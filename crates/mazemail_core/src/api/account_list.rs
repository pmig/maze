//! `local_api`'s [`AccountList`]: the accounts in the store. A token for every account
//! sees all of them, including accounts added later.

use std::collections::BTreeSet;
use std::fmt;

use local_api::AccountList;
use mazemail_types::AccountId;

use crate::handle::CoreHandle;

/// Every account of `core`. See the [module docs](self).
#[derive(Clone)]
pub struct CoreAccountList {
    core: CoreHandle,
}

impl fmt::Debug for CoreAccountList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreAccountList").finish_non_exhaustive()
    }
}

impl CoreAccountList {
    /// The accounts of `core`.
    pub fn new(core: CoreHandle) -> Self {
        Self { core }
    }
}

impl AccountList for CoreAccountList {
    fn all_accounts(&self) -> BTreeSet<AccountId> {
        match self.core.accounts() {
            Ok(accounts) => accounts.iter().map(|account| account.id).collect(),
            Err(error) => {
                // Fail closed: nothing is in scope while the store can't be read.
                tracing::warn!(%error, "cannot read the accounts for a request");
                BTreeSet::new()
            }
        }
    }
}
