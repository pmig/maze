//! The per-account AI policy hook (CONCEPT §10.4, PLAN §4.12).
//!
//! Accounts whose AI policy is _off_ or _local models only_ are never in the scope of a
//! token. `local_api` asks an [`AccountPolicy`] (implemented by `mazemail_core` from the
//! settings) when a token is minted, and again on every request, so turning the policy
//! off takes effect immediately for existing tokens.

use std::collections::BTreeSet;
use std::sync::RwLock;

use mazemail_types::{AccountId, FolderId};

use crate::token::TokenScope;

/// Which accounts external agents may access.
pub trait AccountPolicy: Send + Sync {
    /// Accounts whose AI policy is "any agent". Called on every request, so it should be
    /// a cheap in-memory lookup.
    fn agent_accounts(&self) -> BTreeSet<AccountId>;
}

/// A fixed, replaceable set of allowed accounts, for tests and headless setups.
#[derive(Debug, Default)]
pub struct StaticAccountPolicy {
    accounts: RwLock<BTreeSet<AccountId>>,
}

impl StaticAccountPolicy {
    /// Allows exactly `accounts`. The default allows none (AI is off by default).
    pub fn new(accounts: impl IntoIterator<Item = AccountId>) -> Self {
        Self {
            accounts: RwLock::new(accounts.into_iter().collect()),
        }
    }

    /// Replaces the allowed accounts.
    pub fn set(&self, accounts: impl IntoIterator<Item = AccountId>) {
        *self
            .accounts
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = accounts.into_iter().collect();
    }
}

impl AccountPolicy for StaticAccountPolicy {
    fn agent_accounts(&self) -> BTreeSet<AccountId> {
        self.accounts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// What a request may actually touch: the token's scope intersected with the accounts
/// the AI policy allows right now. `mazemail_core` checks every message, thread, folder
/// and account a command touches against it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveScope {
    accounts: BTreeSet<AccountId>,
    folders: Option<BTreeSet<FolderId>>,
}

impl EffectiveScope {
    /// Nothing at all (requests without a token).
    pub fn none() -> Self {
        Self::default()
    }

    /// `scope` restricted to `allowed` accounts.
    pub fn resolve(scope: &TokenScope, allowed: &BTreeSet<AccountId>) -> Self {
        let accounts = match &scope.accounts {
            None => allowed.clone(),
            Some(accounts) => accounts.intersection(allowed).copied().collect(),
        };
        Self {
            accounts,
            folders: scope.folders.clone(),
        }
    }

    /// The accounts in scope.
    pub fn accounts(&self) -> &BTreeSet<AccountId> {
        &self.accounts
    }

    /// The folders in scope, `None` for every folder of the accounts in scope.
    pub fn folders(&self) -> Option<&BTreeSet<FolderId>> {
        self.folders.as_ref()
    }

    /// Whether `account` is in scope.
    pub fn allows_account(&self, account: AccountId) -> bool {
        self.accounts.contains(&account)
    }

    /// Whether `folder` of `account` is in scope.
    pub fn allows_folder(&self, account: AccountId, folder: FolderId) -> bool {
        self.allows_account(account)
            && self
                .folders
                .as_ref()
                .is_none_or(|folders| folders.contains(&folder))
    }

    /// Whether nothing is in scope.
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty() || self.folders.as_ref().is_some_and(BTreeSet::is_empty)
    }
}

/// The accounts of `scope` that the policy excludes (empty for an "all accounts" scope,
/// which follows the policy automatically).
pub fn excluded_accounts(scope: &TokenScope, allowed: &BTreeSet<AccountId>) -> Vec<AccountId> {
    scope
        .accounts
        .iter()
        .flatten()
        .filter(|account| !allowed.contains(account))
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[i64]) -> BTreeSet<AccountId> {
        ids.iter().copied().map(AccountId).collect()
    }

    #[test]
    fn effective_scope_follows_policy() {
        let allowed = set(&[1, 2]);
        let all = EffectiveScope::resolve(&TokenScope::all(), &allowed);
        assert_eq!(all.accounts(), &allowed);

        let scope = TokenScope {
            accounts: Some(set(&[2, 3])),
            folders: Some([FolderId(9)].into()),
        };
        let effective = EffectiveScope::resolve(&scope, &allowed);
        assert_eq!(effective.accounts(), &set(&[2]));
        assert!(effective.allows_folder(AccountId(2), FolderId(9)));
        assert!(!effective.allows_folder(AccountId(2), FolderId(8)));
        assert!(!effective.allows_folder(AccountId(3), FolderId(9)));
        assert_eq!(excluded_accounts(&scope, &allowed), vec![AccountId(3)]);
        assert!(excluded_accounts(&TokenScope::all(), &allowed).is_empty());

        // Turning the policy off for every account empties the scope.
        assert!(EffectiveScope::resolve(&scope, &BTreeSet::new()).is_empty());
        assert!(EffectiveScope::none().is_empty());
    }

    #[test]
    fn static_policy_is_replaceable() {
        let policy = StaticAccountPolicy::default();
        assert!(policy.agent_accounts().is_empty());
        policy.set([AccountId(4)]);
        assert_eq!(policy.agent_accounts(), set(&[4]));
    }
}
