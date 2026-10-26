//! "Sync activity": what each account's sync is doing right now
//! ([`CoreHandle::sync_activity`]).
//!
//! The sync engine's in-memory activity (folder passes with their step and header
//! progress, op replay, the body queue, the last finished round, see
//! [`sync::SyncActivity`]) joined with the store's view of the work that waits:
//! pending ops (count, stuck ones, the oldest) and outbox messages. A snapshot is
//! cheap (a few small reads); the UI takes one a few times a second while the
//! popover is open, and nothing is pushed per message.

use std::collections::HashMap;

use mazemail_types::{AccountId, FolderId, IdentityId};
use store::{AccountKind, OpState, OutboxState, Queries as _};
use time::OffsetDateTime;

pub use sync::FolderPhase;

use crate::folders::folder_path;
use crate::handle::CoreHandle;
use crate::sync_bridge::SyncStatus;
use crate::{AccountConfig, Result};

/// A folder the account's sync is working on or will work on next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderProgress {
    /// The folder.
    pub folder: FolderId,
    /// Its path in the folder pane ("Archive/2025"). Shown to the user only, never
    /// logged.
    pub name: String,
    /// What the pass is doing.
    pub phase: FolderPhase,
    /// Work done in this phase (headers fetched).
    pub done: u64,
    /// Work in this phase, if known.
    pub total: Option<u64>,
}

/// What one account's sync is doing and what waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountActivity {
    /// The account.
    pub account: AccountId,
    /// Its name in the folder pane.
    pub name: String,
    /// Its kind.
    pub kind: AccountKind,
    /// The sync state; `None` when the account isn't synced.
    pub sync: Option<SyncStatus>,
    /// Running passes first, then waiting ones.
    pub folders: Vec<FolderProgress>,
    /// Pending ops are being replayed on the server right now.
    pub replaying_ops: bool,
    /// Message bodies waiting to be downloaded for offline reading.
    pub bodies_queued: usize,
    /// When the last full round finished (since Maze started).
    pub last_synced: Option<OffsetDateTime>,
    /// Changes (moves, flags, appends) waiting to be replayed on the server.
    pub pending_ops: usize,
    /// Of those, the ones that failed too often and wait for Retry or Discard.
    pub stuck_ops: usize,
    /// When the oldest waiting change was made.
    pub oldest_op: Option<OffsetDateTime>,
    /// Outbox messages of the account's identities waiting to be sent (or failed).
    pub outbox: usize,
}

impl CoreHandle {
    /// What every account's sync is doing right now (see the [module docs](self)),
    /// in folder pane order.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn sync_activity(&self) -> Result<Vec<AccountActivity>> {
        let reader = self.store().reader()?;
        let engine = self.engine();
        let identities: HashMap<IdentityId, AccountId> = reader
            .identities(None)?
            .into_iter()
            .map(|identity| (identity.id, identity.account))
            .collect();
        let mut outbox: HashMap<AccountId, usize> = HashMap::new();
        for item in reader.outbox()? {
            if matches!(item.state, OutboxState::Sent) {
                continue;
            }
            if let Some(account) = item.identity.and_then(|id| identities.get(&id)) {
                *outbox.entry(*account).or_default() += 1;
            }
        }
        let mut out = Vec::new();
        for account in reader.accounts()? {
            // Accounts without a server (legacy Local Folders) have nothing to sync.
            if AccountConfig::parse(&account.config_json).is_none() {
                continue;
            }
            let activity = engine
                .as_ref()
                .and_then(|engine| engine.activity(account.id))
                .unwrap_or_default();
            let folders = if activity.folders.is_empty() {
                Vec::new()
            } else {
                let all = reader.folders(Some(account.id))?;
                activity
                    .folders
                    .iter()
                    .map(|f| FolderProgress {
                        folder: f.folder,
                        name: folder_path(&all, f.folder).unwrap_or_default(),
                        phase: f.phase,
                        done: f.done,
                        total: f.total,
                    })
                    .collect()
            };
            let ops = reader.pending_ops(account.id)?;
            out.push(AccountActivity {
                account: account.id,
                name: account.name.clone(),
                kind: account.kind,
                sync: engine.as_ref().and_then(|e| e.status(account.id)),
                folders,
                replaying_ops: activity.replaying_ops,
                bodies_queued: activity.bodies_queued,
                last_synced: activity
                    .last_synced
                    .and_then(|t| OffsetDateTime::from_unix_timestamp(t).ok()),
                pending_ops: ops.len(),
                stuck_ops: ops.iter().filter(|op| op.state == OpState::Stuck).count(),
                oldest_op: ops
                    .iter()
                    .map(|op| op.created)
                    .min()
                    .and_then(|t| OffsetDateTime::from_unix_timestamp(t).ok()),
                outbox: outbox.get(&account.id).copied().unwrap_or_default(),
            });
        }
        Ok(out)
    }
}
