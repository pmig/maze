//! `local_api`'s [`AccountPolicy`] from the settings (CONCEPT §10.4).
//!
//! `agent.accounts` maps account names to `off`, `local_only` or `any`; accounts not
//! listed are `off`. External agents (everything that reaches Maze through the local
//! API) only ever see accounts set to `any`. The settings are watched, so changing the
//! policy takes effect on the next request, for existing tokens too.

use std::collections::BTreeSet;
use std::fmt;

use local_api::AccountPolicy;
use mazemail_types::AccountId;
use settings::AgentAccess;

use crate::handle::CoreHandle;

/// Allows the accounts whose `agent.accounts` entry is `any`. See the
/// [module docs](self).
#[derive(Clone)]
pub struct SettingsAccountPolicy {
    core: CoreHandle,
}

impl fmt::Debug for SettingsAccountPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsAccountPolicy")
            .finish_non_exhaustive()
    }
}

impl SettingsAccountPolicy {
    /// The policy of `core`'s settings and accounts.
    pub fn new(core: CoreHandle) -> Self {
        Self { core }
    }
}

impl AccountPolicy for SettingsAccountPolicy {
    fn agent_accounts(&self) -> BTreeSet<AccountId> {
        let settings = self.core.settings();
        match self.core.accounts() {
            Ok(accounts) => accounts
                .iter()
                .filter(|account| {
                    settings.settings.agent.access_for(&account.name) == AgentAccess::Any
                })
                .map(|account| account.id)
                .collect(),
            Err(error) => {
                // Fail closed: no account is shared while the store can't be read.
                tracing::warn!(%error, "cannot read accounts for the agent policy");
                BTreeSet::new()
            }
        }
    }
}
