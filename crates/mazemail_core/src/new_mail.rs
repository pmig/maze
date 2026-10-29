//! New mail, for notifications (CONCEPT §16).
//!
//! Every account's sync batches go through a [`NewMailFilter`]. A message counts as
//! new mail when it
//!
//! - was added to a folder with the Inbox role (Gmail's INBOX label too) or to a
//!   favorite folder (`folder_pane.favorites`) that isn't the Sent folder,
//! - is unread there,
//! - wasn't sent from one of the account's identities, and
//! - reached the server ([`MessageDetail::received`], else its `Date`) after the
//!   account's sync started in this session, less [`ARRIVAL_SLACK`] for clock skew.
//!
//! The last rule keeps notifications quiet during a first sync, after a UIDVALIDITY
//! reset and when old mail moves back into the Inbox: those messages arrived at the
//! server before. Mail that arrives while the computer sleeps counts once it syncs
//! after the wake. Each message is reported once per session.
//!
//! [`CoreHandle::new_mail`](crate::CoreHandle::new_mail) broadcasts the result; the
//! GUI turns it into system notifications and applies the `notifications` settings.

use std::collections::{BTreeSet, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use mazemail_types::{
    AccountId, Address, Flags, FolderId, FolderRole, MessageId, Sensitive, ThreadId,
};
use store::{MessageDetail, Queries, UnixTime};
use sync::SyncEventBatch;
use tokio::sync::broadcast;

/// Mail that reached the server up to this long before sync started still counts as
/// new: the server's clock and this computer's may differ a little.
pub const ARRIVAL_SLACK: i64 = 5 * 60;

/// Capacity of the broadcast channel; a receiver further behind misses notifications.
const BROADCAST_CAPACITY: usize = 64;

/// One newly arrived message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMail {
    /// The message.
    pub message: MessageId,
    /// Its thread.
    pub thread: ThreadId,
    /// Its account.
    pub account: AccountId,
    /// The account's name ("Work").
    pub account_name: String,
    /// The folder it arrived in: an Inbox or a favorite.
    pub folder: FolderId,
    /// The sender.
    pub from: Sensitive<Option<Address>>,
    /// The subject.
    pub subject: Sensitive<Option<String>>,
    /// When it reached the server (Unix seconds).
    pub received: UnixTime,
}

/// New mail of one sync batch, in the order the server delivered it.
pub type NewMailBatch = std::sync::Arc<[NewMail]>;

/// The broadcast side of the new-mail stream.
pub(crate) fn channel() -> broadcast::Sender<NewMailBatch> {
    broadcast::channel(BROADCAST_CAPACITY).0
}

/// The seconds since the Unix epoch now.
pub(crate) fn unix_now() -> UnixTime {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Picks the new mail out of one account's sync batches. See the [module docs](self).
#[derive(Debug)]
pub(crate) struct NewMailFilter {
    account: AccountId,
    /// Mail that reached the server before this isn't new.
    since: UnixTime,
    /// Messages reported already.
    reported: HashSet<MessageId>,
}

impl NewMailFilter {
    /// A filter for `account` whose sync starts at `started` (Unix seconds).
    pub(crate) fn new(account: AccountId, started: UnixTime) -> Self {
        Self {
            account,
            since: started.saturating_sub(ARRIVAL_SLACK),
            reported: HashSet::new(),
        }
    }

    /// The new mail in `batch`, read from `reader`; `favorites` are the account's
    /// favorite folders.
    pub(crate) fn select(
        &mut self,
        reader: &impl Queries,
        batch: &SyncEventBatch,
        favorites: &BTreeSet<FolderId>,
    ) -> store::Result<Vec<NewMail>> {
        let mut found = Vec::new();
        let mut own: Option<HashSet<String>> = None;
        let mut account_name: Option<String> = None;
        for (folder, messages) in &batch.added {
            if batch.folders_reset.contains(folder) {
                continue;
            }
            let role = match reader.folder(*folder)? {
                Some(f) => f.role,
                None => continue,
            };
            if !notifies(role, favorites.contains(folder)) {
                continue;
            }
            for &message in messages {
                if self.reported.contains(&message) {
                    continue;
                }
                let Some(detail) = reader.message(message)? else {
                    continue;
                };
                if !self.is_new(&detail, *folder) {
                    continue;
                }
                let own = match &own {
                    Some(own) => own,
                    None => own.insert(own_addresses(reader, self.account)?),
                };
                if detail
                    .from
                    .as_ref()
                    .is_some_and(|from| own.contains(&from.email.to_lowercase()))
                {
                    continue;
                }
                let account_name = match &account_name {
                    Some(name) => name.clone(),
                    None => account_name
                        .insert(
                            reader
                                .account(self.account)?
                                .map(|a| a.name)
                                .unwrap_or_default(),
                        )
                        .clone(),
                };
                self.reported.insert(message);
                found.push(NewMail {
                    message,
                    thread: detail.thread,
                    account: detail.account,
                    account_name,
                    folder: *folder,
                    received: arrival(&detail),
                    from: Sensitive::new(detail.from),
                    subject: Sensitive::new(detail.subject),
                });
            }
        }
        found.sort_by_key(|m| (m.received, m.message));
        Ok(found)
    }

    /// Unread in `folder` and arrived after sync started.
    fn is_new(&self, detail: &MessageDetail, folder: FolderId) -> bool {
        let unread = detail
            .locations
            .iter()
            .any(|l| l.folder == folder && !l.flags.contains(Flags::SEEN));
        unread && arrival(detail) >= self.since
    }
}

/// Whether new mail in a folder with `role` is notified: in an Inbox, and in favorites
/// other than Sent.
fn notifies(role: Option<FolderRole>, favorite: bool) -> bool {
    role == Some(FolderRole::Inbox) || (favorite && role != Some(FolderRole::Sent))
}

/// When a message reached the server: INTERNALDATE, else its `Date` header.
fn arrival(detail: &MessageDetail) -> UnixTime {
    detail.received.unwrap_or(detail.date)
}

/// The addresses of an account's identities, lowercased.
fn own_addresses(reader: &impl Queries, account: AccountId) -> store::Result<HashSet<String>> {
    Ok(reader
        .identities(Some(account))?
        .into_iter()
        .map(|identity| identity.input.email.to_lowercase())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inboxes_and_favorites_but_not_sent_notify() {
        assert!(notifies(Some(FolderRole::Inbox), false));
        assert!(notifies(None, true));
        assert!(notifies(Some(FolderRole::Archive), true));
        assert!(!notifies(Some(FolderRole::Sent), true));
        assert!(!notifies(None, false));
        assert!(!notifies(Some(FolderRole::Archive), false));
    }
}
