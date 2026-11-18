//! What a request may touch: a token's scope resolved against the accounts that exist.
//!
//! A token covers every account or a list of them (`mazemail login --accounts`).
//! `local_api` asks an [`AccountList`] (implemented by `mazemail_core`) on every
//! request, so a token for every account also covers accounts added later, and an
//! account that was removed drops out of every scope.

use std::collections::BTreeSet;
use std::sync::RwLock;

use mazemail_types::{AccountId, FolderId};

use crate::token::TokenScope;

/// The accounts that exist.
pub trait AccountList: Send + Sync {
    /// Every account. Called on every request, so it should be a cheap lookup.
    fn all_accounts(&self) -> BTreeSet<AccountId>;
}

/// A fixed, replaceable set of accounts, for tests.
#[derive(Debug, Default)]
pub struct StaticAccountList {
    accounts: RwLock<BTreeSet<AccountId>>,
}

impl StaticAccountList {
    /// Exactly `accounts`. The default has none.
    pub fn new(accounts: impl IntoIterator<Item = AccountId>) -> Self {
        Self {
            accounts: RwLock::new(accounts.into_iter().collect()),
        }
    }

    /// Replaces the accounts.
    pub fn set(&self, accounts: impl IntoIterator<Item = AccountId>) {
        *self
            .accounts
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = accounts.into_iter().collect();
    }
}

impl AccountList for StaticAccountList {
    fn all_accounts(&self) -> BTreeSet<AccountId> {
        self.accounts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// What a request may actually touch: the token's scope intersected with the accounts
/// that exist. `mazemail_core` checks every message, thread, folder and account a
/// command touches against it.
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

    /// `scope` resolved against the `existing` accounts.
    pub fn resolve(scope: &TokenScope, existing: &BTreeSet<AccountId>) -> Self {
        let accounts = match &scope.accounts {
            None => existing.clone(),
            Some(accounts) => accounts.intersection(existing).copied().collect(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[i64]) -> BTreeSet<AccountId> {
        ids.iter().copied().map(AccountId).collect()
    }

    #[test]
    fn effective_scope_follows_the_existing_accounts() {
        let existing = set(&[1, 2]);
        let all = EffectiveScope::resolve(&TokenScope::all(), &existing);
        assert_eq!(all.accounts(), &existing);

        let scope = TokenScope {
            accounts: Some(set(&[2, 3])),
            folders: Some([FolderId(9)].into()),
        };
        let effective = EffectiveScope::resolve(&scope, &existing);
        assert_eq!(effective.accounts(), &set(&[2]));
        assert!(effective.allows_folder(AccountId(2), FolderId(9)));
        assert!(!effective.allows_folder(AccountId(2), FolderId(8)));
        assert!(!effective.allows_folder(AccountId(3), FolderId(9)));

        // Without accounts there is nothing in scope.
        assert!(EffectiveScope::resolve(&scope, &BTreeSet::new()).is_empty());
        assert!(EffectiveScope::none().is_empty());
    }

    #[test]
    fn static_account_list_is_replaceable() {
        let list = StaticAccountList::default();
        assert!(list.all_accounts().is_empty());
        list.set([AccountId(4)]);
        assert_eq!(list.all_accounts(), set(&[4]));
    }
}
