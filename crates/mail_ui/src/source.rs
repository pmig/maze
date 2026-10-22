//! The data interface of the mail UI.
//!
//! The UI never talks to the store. Everything it shows comes through [`MailSource`]:
//! the folder tree, the thread keys of a folder, rows for the visible range, and the
//! message to display. Reads are synchronous and cheap (PLAN §4.1: the core answers
//! them from a reader connection); slow ones (text search) run on a background
//! thread, which is why the trait is `Send + Sync`.
//!
//! [`crate::FakeMailSource`] implements it with synthetic data until the core does.

use std::sync::Arc;

use gpui::SharedString;
use mazemail_types::{
    AccountId, Address, Flags, FolderId, FolderRole, MessageId, Sensitive, ThreadId,
};
use time::OffsetDateTime;

/// Identifies a list of threads the UI can show.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FolderKey {
    /// One folder of one account.
    Folder(FolderId),
    /// A unified folder: the folders with this role across all accounts.
    Unified(FolderRole),
}

impl FolderKey {
    /// A stable string id, used as the folder pane's tree item id.
    pub fn tree_id(&self) -> SharedString {
        match self {
            Self::Folder(id) => format!("folder:{id}").into(),
            Self::Unified(role) => format!("unified:{}", role.as_str()).into(),
        }
    }
}

/// What the folder pane shows: Favorites, Unified Folders and Accounts (CONCEPT §8.2).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FolderTree {
    /// Folders the user marked as favorites.
    pub favorites: Vec<FolderEntry>,
    /// Unified Inbox, Drafts, Sent, …
    pub unified: Vec<FolderEntry>,
    /// Every account with its folder hierarchy.
    pub accounts: Vec<AccountEntry>,
}

impl FolderTree {
    /// Finds the entry for `key` anywhere in the tree.
    pub fn find(&self, key: &FolderKey) -> Option<&FolderEntry> {
        fn walk<'a>(entries: &'a [FolderEntry], key: &FolderKey) -> Option<&'a FolderEntry> {
            entries.iter().find_map(|entry| {
                if &entry.key == key {
                    Some(entry)
                } else {
                    walk(&entry.children, key)
                }
            })
        }
        walk(&self.favorites, key)
            .or_else(|| walk(&self.unified, key))
            .or_else(|| {
                self.accounts
                    .iter()
                    .find_map(|account| walk(&account.folders, key))
            })
    }
}

/// An account and its folders.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountEntry {
    /// The account.
    pub id: AccountId,
    /// The name the user gave it, e.g. "Gmail (personal)".
    pub name: SharedString,
    /// What kind of account it is (decides the icon).
    pub kind: AccountKind,
    /// Top-level folders, Inbox first.
    pub folders: Vec<FolderEntry>,
}

/// The kind of an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountKind {
    /// Gmail over IMAP with Gmail extensions.
    Gmail,
    /// Microsoft 365 / Outlook.com.
    Microsoft,
    /// Any other IMAP server.
    Imap,
    /// Thunderbird's Local Folders.
    Local,
}

/// A folder in the folder pane.
#[derive(Debug, Clone, PartialEq)]
pub struct FolderEntry {
    /// What selecting it shows.
    pub key: FolderKey,
    /// Display name.
    pub name: SharedString,
    /// Special-use role, if any (decides the icon).
    pub role: Option<FolderRole>,
    /// Subfolders.
    pub children: Vec<FolderEntry>,
}

/// The thread ids of a list in display order (newest first). Keys only: rows are
/// fetched for the visible range (PLAN §8, "keys-only thread list").
pub type ThreadKeys = Arc<[ThreadId]>;

/// Which threads a list shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ThreadFilter {
    /// Case-insensitive search terms; every whitespace-separated term must occur in
    /// the subject or the sender.
    pub text: Option<String>,
    /// Only unread threads.
    pub unread: bool,
    /// Only starred threads.
    pub starred: bool,
    /// Only threads with attachments.
    pub attachments: bool,
}

impl ThreadFilter {
    /// Whether the filter lets everything through.
    pub fn is_empty(&self) -> bool {
        self.text.as_deref().is_none_or(str::is_empty)
            && !self.unread
            && !self.starred
            && !self.attachments
    }

    /// Whether a row passes the filter.
    pub fn matches(&self, row: &ThreadRow) -> bool {
        if self.unread && row.flags.contains(Flags::SEEN) {
            return false;
        }
        if self.starred && !row.flags.contains(Flags::FLAGGED) {
            return false;
        }
        if self.attachments && !row.has_attachments {
            return false;
        }
        match self.text.as_deref().map(str::trim) {
            None | Some("") => true,
            Some(text) => {
                let subject = row.subject.expose().to_lowercase();
                let sender = row.sender.expose().to_lowercase();
                text.to_lowercase()
                    .split_whitespace()
                    .all(|term| subject.contains(term) || sender.contains(term))
            }
        }
    }
}

/// Where a list of threads comes from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ThreadScope {
    /// One folder or unified folder.
    Folder(FolderKey),
    /// Every folder of every account (global search).
    AllFolders,
}

/// One row of the message list.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadRow {
    /// The thread.
    pub id: ThreadId,
    /// Subject of the thread.
    pub subject: Sensitive<SharedString>,
    /// Display name (or address) of the newest message's sender.
    pub sender: Sensitive<SharedString>,
    /// Date of the newest message.
    pub date: OffsetDateTime,
    /// Flags of the thread (unread if any message is unread).
    pub flags: Flags,
    /// Whether any message has attachments.
    pub has_attachments: bool,
    /// Number of messages in the thread.
    pub message_count: u32,
    /// Size of the newest message in bytes.
    pub size: u64,
}

impl ThreadRow {
    /// Whether the thread has unread messages.
    pub fn is_unread(&self) -> bool {
        !self.flags.contains(Flags::SEEN)
    }

    /// Whether the thread is starred (`\Flagged`).
    pub fn is_starred(&self) -> bool {
        self.flags.contains(Flags::FLAGGED)
    }
}

/// A message ready to display.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageDocument {
    /// The thread it belongs to.
    pub thread: ThreadId,
    /// The message.
    pub message: MessageId,
    /// Subject.
    pub subject: Sensitive<SharedString>,
    /// Sender.
    pub from: Sensitive<Address>,
    /// Recipients.
    pub to: Sensitive<Vec<Address>>,
    /// Carbon-copy recipients.
    pub cc: Sensitive<Vec<Address>>,
    /// Date sent.
    pub date: OffsetDateTime,
    /// The body to render.
    pub body: MessageBody,
    /// Attachments, in message order.
    pub attachments: Vec<AttachmentInfo>,
}

/// The body of a message.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageBody {
    /// Sanitized reader-mode HTML (PLAN §4.8), rendered with gpui-kit's `TextView`.
    Html(Sensitive<SharedString>),
    /// Plain text.
    PlainText(Sensitive<SharedString>),
}

/// An attachment of a message.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentInfo {
    /// File name.
    pub name: Sensitive<SharedString>,
    /// Size in bytes.
    pub size: u64,
}

/// Direction of a search through a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Towards higher indexes (older messages).
    Forward,
    /// Towards lower indexes (newer messages).
    Backward,
}

/// What [`MailSource::find`] looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadPredicate {
    /// A thread with unread messages.
    Unread,
}

impl ThreadPredicate {
    /// Whether `row` matches.
    pub fn matches(self, row: &ThreadRow) -> bool {
        match self {
            Self::Unread => row.is_unread(),
        }
    }
}

/// Sync state shown in the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncState {
    /// Nothing to do.
    Idle,
    /// Synchronizing an account.
    Syncing {
        /// The account's display name.
        account: SharedString,
        /// Messages done.
        done: u64,
        /// Messages total.
        total: u64,
    },
}

/// Connection and queue state shown in the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceStatus {
    /// Sync progress.
    pub sync: SyncState,
    /// Messages waiting in the outbox.
    pub outbox: u32,
    /// Whether Maze is online.
    pub online: bool,
}

/// Errors from a [`MailSource`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// The thread doesn't exist (any more).
    #[error("thread {0} not found")]
    ThreadNotFound(ThreadId),
    /// The folder doesn't exist (any more).
    #[error("folder not found")]
    FolderNotFound,
    /// The backend couldn't answer.
    #[error("mail backend unavailable: {0}")]
    Unavailable(String),
}

/// Everything the mail UI reads and changes. Implemented by the core later, and by
/// [`crate::FakeMailSource`] now.
pub trait MailSource: Send + Sync + 'static {
    /// The folder pane content.
    fn folder_tree(&self) -> FolderTree;

    /// Unread threads in a folder (cached counts, cheap).
    fn unread_count(&self, folder: &FolderKey) -> u64;

    /// Thread keys of a scope in display order, newest first, after `filter`.
    ///
    /// With a text filter this can take a while on large folders; call it off the
    /// main thread then.
    fn thread_keys(
        &self,
        scope: &ThreadScope,
        filter: &ThreadFilter,
    ) -> Result<ThreadKeys, SourceError>;

    /// Rows for `keys`, in the same order. Threads that no longer exist are left out.
    fn thread_rows(&self, keys: &[ThreadId]) -> Vec<ThreadRow>;

    /// The index of the next thread in `keys` matching `predicate`, starting after
    /// (or before) `from`. `from = None` starts at the edge of the list.
    ///
    /// The default scans rows in chunks; a backend with an index answers directly.
    fn find(
        &self,
        keys: &[ThreadId],
        from: Option<usize>,
        direction: Direction,
        predicate: ThreadPredicate,
    ) -> Option<usize> {
        const CHUNK: usize = 512;
        match direction {
            Direction::Forward => {
                let mut start = from.map_or(0, |ix| ix + 1);
                while start < keys.len() {
                    let end = (start + CHUNK).min(keys.len());
                    let rows = self.thread_rows(&keys[start..end]);
                    if let Some(hit) = rows.iter().find(|row| predicate.matches(row)) {
                        return keys[start..end]
                            .iter()
                            .position(|id| *id == hit.id)
                            .map(|offset| start + offset);
                    }
                    start = end;
                }
                None
            }
            Direction::Backward => {
                let mut end = from.unwrap_or(keys.len()).min(keys.len());
                while end > 0 {
                    let start = end.saturating_sub(CHUNK);
                    let rows = self.thread_rows(&keys[start..end]);
                    if let Some(hit) = rows.iter().rev().find(|row| predicate.matches(row)) {
                        return keys[start..end]
                            .iter()
                            .position(|id| *id == hit.id)
                            .map(|offset| start + offset);
                    }
                    end = start;
                }
                None
            }
        }
    }

    /// The newest message of a thread, ready to display.
    fn message(&self, thread: ThreadId) -> Result<MessageDocument, SourceError>;

    /// Adds and removes flags on threads (mark read, star, …).
    fn set_flags(&self, threads: &[ThreadId], add: Flags, remove: Flags)
    -> Result<(), SourceError>;

    /// Moves threads to their account's Archives folder.
    fn archive(&self, threads: &[ThreadId]) -> Result<(), SourceError>;

    /// Connection, sync and outbox state.
    fn status(&self) -> SourceStatus;
}
