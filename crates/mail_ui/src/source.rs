//! The data interface of the mail UI.
//!
//! The UI never talks to the store. Everything it shows comes through [`MailSource`]:
//! the folder tree, the thread (or, with threads off, message) keys of a folder, rows
//! for the visible range, the reply tree of an expanded thread, and the message to
//! display. Reads are synchronous and cheap (PLAN §4.1: the core answers
//! them from a reader connection); slow ones (text search) run on a background
//! thread, which is why the trait is `Send + Sync`.
//!
//! The `mazemail` binary implements it over the core (`CoreMailSource`);
//! [`crate::FakeMailSource`] implements it with synthetic data.

use std::sync::Arc;

use gpui::SharedString;
pub use html_view::Part;
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
    /// Folders the user marked as favorites (without subfolders). The folder pane
    /// orders and groups them by their place in `accounts`.
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

/// The message ids of an unthreaded list in display order. Keys only, like
/// [`ThreadKeys`].
pub type MessageKeys = Arc<[MessageId]>;

/// What a list is sorted by: a column of the table, or the time the server received
/// the message. Threads sort by what their row shows (their newest message's date,
/// sender and size, their subject, whether any message is starred, …); the source
/// sorts, never the view, so a list of any length is in order before rows are fetched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SortBy {
    /// The date (a thread: its newest message's).
    #[default]
    Date,
    /// When the server received the message, else its date.
    Received,
    /// Starred or not.
    Star,
    /// Unread or not.
    Unread,
    /// With attachments or without.
    Attachment,
    /// The subject, ignoring `Re:`/`Fwd:`/`AW:`/`WG:` prefixes and case.
    Subject,
    /// The Correspondents column: the sender, or the recipients of mail you sent.
    Correspondents,
    /// The sender's name, else address.
    Sender,
    /// The recipients.
    Recipients,
    /// Tag names.
    Tags,
    /// The account's name.
    Account,
    /// Size.
    Size,
}

impl SortBy {
    /// Every order, as the sort menu lists them.
    pub const ALL: [Self; 12] = [
        Self::Date,
        Self::Received,
        Self::Star,
        Self::Unread,
        Self::Attachment,
        Self::Subject,
        Self::Correspondents,
        Self::Sender,
        Self::Recipients,
        Self::Tags,
        Self::Account,
        Self::Size,
    ];

    /// The name in menus.
    pub fn label(self) -> &'static str {
        match self {
            Self::Date => "Date",
            Self::Received => "Received",
            Self::Star => "Star",
            Self::Unread => "Unread",
            Self::Attachment => "Attachment",
            Self::Subject => "Subject",
            Self::Correspondents => "Correspondents",
            Self::Sender => "Sender",
            Self::Recipients => "Recipients",
            Self::Tags => "Tags",
            Self::Account => "Account",
            Self::Size => "Size",
        }
    }

    /// The JSON name (`ui_state.json`, `thread_list.sort` in `settings.json`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::Received => "received",
            Self::Star => "star",
            Self::Unread => "unread",
            Self::Attachment => "attachment",
            Self::Subject => "subject",
            Self::Correspondents => "correspondents",
            Self::Sender => "sender",
            Self::Recipients => "recipients",
            Self::Tags => "tags",
            Self::Account => "account",
            Self::Size => "size",
        }
    }

    /// The direction a first click on the column sorts in, as in Thunderbird: dates
    /// newest first, sizes largest first, starred, unread and attachments first, text
    /// A to Z.
    pub fn descending_first(self) -> bool {
        matches!(
            self,
            Self::Date | Self::Received | Self::Star | Self::Unread | Self::Attachment | Self::Size
        )
    }

    /// The two directions, descending first: "Newest First" / "Oldest First", "A to Z"
    /// / "Z to A" (as descending / ascending), ….
    pub fn direction_labels(self) -> (&'static str, &'static str) {
        match self {
            Self::Date | Self::Received => ("Newest First", "Oldest First"),
            Self::Star => ("Starred First", "Unstarred First"),
            Self::Unread => ("Unread First", "Read First"),
            Self::Attachment => ("With Attachments First", "Without Attachments First"),
            Self::Size => ("Largest First", "Smallest First"),
            Self::Subject
            | Self::Correspondents
            | Self::Sender
            | Self::Recipients
            | Self::Tags
            | Self::Account => ("Z to A", "A to Z"),
        }
    }
}

/// The order of a list: what it is sorted by and which way. In `ui_state.json` it is
/// `"<sort by>_<asc|desc>"` (`"subject_asc"`); the names before all columns were
/// sortable, `"newest_first"` and `"oldest_first"`, still read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ListSort {
    /// What the list is sorted by.
    pub by: SortBy,
    /// Largest, newest, starred or Z first.
    pub descending: bool,
}

impl Default for ListSort {
    fn default() -> Self {
        Self::NEWEST_FIRST
    }
}

impl ListSort {
    /// By date, newest first (the default).
    pub const NEWEST_FIRST: Self = Self {
        by: SortBy::Date,
        descending: true,
    };

    /// By date, oldest first.
    pub const OLDEST_FIRST: Self = Self {
        by: SortBy::Date,
        descending: false,
    };

    /// Sorted by `by`, in the direction a first click sorts in.
    pub fn by(by: SortBy) -> Self {
        Self {
            by,
            descending: by.descending_first(),
        }
    }

    /// The other direction.
    pub fn toggled(self) -> Self {
        Self {
            descending: !self.descending,
            ..self
        }
    }

    /// What a click on the header of the column sorted by `by` does: the other
    /// direction if the list is sorted by it already, else sort by it.
    pub fn clicked(self, by: SortBy) -> Self {
        if self.by == by {
            self.toggled()
        } else {
            Self::by(by)
        }
    }

    /// Whether this is a date order (the cheap one: straight from an index).
    pub fn is_by_date(self) -> bool {
        self.by == SortBy::Date
    }

    /// "Subject, A to Z".
    pub fn describe(self) -> String {
        let (descending, ascending) = self.by.direction_labels();
        let direction = if self.descending {
            descending
        } else {
            ascending
        };
        format!("{}, {direction}", self.by.label())
    }

    /// The JSON name, `"subject_asc"`.
    pub fn name(self) -> String {
        let direction = if self.descending { "desc" } else { "asc" };
        format!("{}_{direction}", self.by.key())
    }

    /// Reads [`ListSort::name`] (and the old names `newest_first` and `oldest_first`).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "newest_first" => return Some(Self::NEWEST_FIRST),
            "oldest_first" => return Some(Self::OLDEST_FIRST),
            _ => {}
        }
        let (by, direction) = name.rsplit_once('_')?;
        let descending = match direction {
            "desc" => true,
            "asc" => false,
            _ => return None,
        };
        let by = SortBy::ALL.into_iter().find(|b| b.key() == by)?;
        Some(Self { by, descending })
    }
}

impl From<ListSort> for String {
    fn from(sort: ListSort) -> Self {
        sort.name()
    }
}

impl TryFrom<String> for ListSort {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::parse(&name).ok_or_else(|| format!("unknown sort order {name:?}"))
    }
}

/// Which threads a list shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ThreadFilter {
    /// Case-insensitive search terms; every whitespace-separated term must occur in
    /// the subject, the sender or the recipients.
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
        self.passes(
            row.flags,
            row.has_attachments,
            [&row.subject, &row.sender, &row.recipients],
        )
    }

    /// Whether a message row passes the filter.
    pub fn matches_message(&self, row: &MessageRow) -> bool {
        self.passes(
            row.flags,
            row.has_attachments,
            [&row.subject, &row.sender, &row.recipients],
        )
    }

    fn passes(&self, flags: Flags, attachments: bool, text: [&Sensitive<SharedString>; 3]) -> bool {
        if self.unread && flags.contains(Flags::SEEN) {
            return false;
        }
        if self.starred && !flags.contains(Flags::FLAGGED) {
            return false;
        }
        if self.attachments && !attachments {
            return false;
        }
        match self.text.as_deref().map(str::trim) {
            None | Some("") => true,
            Some(filter) => {
                let haystack: Vec<String> =
                    text.iter().map(|t| t.expose().to_lowercase()).collect();
                filter
                    .to_lowercase()
                    .split_whitespace()
                    .all(|term| haystack.iter().any(|h| h.contains(term)))
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
    /// Recipients of the newest message, display names joined with ", ".
    pub recipients: Sensitive<SharedString>,
    /// The newest message is mail you sent: the Correspondents column shows its
    /// recipients (with a "sent" arrow) instead of the sender, as in Thunderbird.
    pub outgoing: bool,
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
    /// The account's name.
    pub account: SharedString,
    /// Tag names.
    pub tags: Vec<SharedString>,
}

impl ThreadRow {
    /// What the Correspondents column shows: the recipients of mail you sent, else the
    /// sender.
    pub fn correspondents(&self) -> &Sensitive<SharedString> {
        if self.outgoing {
            &self.recipients
        } else {
            &self.sender
        }
    }
}

/// One message row: a message of an unthreaded list or of an expanded thread.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageRow {
    /// The message.
    pub id: MessageId,
    /// Its thread.
    pub thread: ThreadId,
    /// Subject.
    pub subject: Sensitive<SharedString>,
    /// Display name (or address) of the sender.
    pub sender: Sensitive<SharedString>,
    /// Recipients, display names joined with ", ".
    pub recipients: Sensitive<SharedString>,
    /// Mail you sent (see [`ThreadRow::outgoing`]).
    pub outgoing: bool,
    /// Date.
    pub date: OffsetDateTime,
    /// Flags.
    pub flags: Flags,
    /// Whether it has attachments.
    pub has_attachments: bool,
    /// Size in bytes.
    pub size: u64,
    /// The account's name.
    pub account: SharedString,
    /// Tag names.
    pub tags: Vec<SharedString>,
}

impl MessageRow {
    /// What the Correspondents column shows (see [`ThreadRow::correspondents`]).
    pub fn correspondents(&self) -> &Sensitive<SharedString> {
        if self.outgoing {
            &self.recipients
        } else {
            &self.sender
        }
    }

    /// Whether the message is unread.
    pub fn is_unread(&self) -> bool {
        !self.flags.contains(Flags::SEEN)
    }
}

/// One message of a thread's reply tree, in display order (depth first, replies by
/// date).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeNode {
    /// The message.
    pub message: MessageId,
    /// The message it replies to, if that is in the tree.
    pub parent: Option<MessageId>,
    /// 0 for the thread's first messages, 1 for their replies, …
    pub depth: u16,
}

/// The reply tree of a thread, see [`MailSource::thread_tree`].
pub type ThreadTree = Arc<[TreeNode]>;

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
    /// Sender verification (DKIM, SPF, DMARC).
    pub verification: Verification,
    /// The mail client the sender used, if the headers give it away.
    pub client: Sensitive<Option<SenderClient>>,
    /// The body's links, in reading order: what the link preview shows when one is
    /// hovered (its target, and a warning for deceptive ones).
    pub links: Sensitive<Vec<BodyLink>>,
    /// The sanitized HTML for the original mode (ADR-020); `None` for plain-text mail,
    /// which never uses the engine.
    pub original: Option<OriginalHtml>,
}

/// A link of a message body: the text the reader sees and where it really goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyLink {
    /// The visible text (or a linked image's alt text).
    pub text: SharedString,
    /// The target (`href`).
    pub href: SharedString,
    /// Set when the link is deceptive: its text shows another domain than the one it
    /// opens (the phishing banner's check).
    pub deception: Option<Deception>,
}

/// What a deceptive link shows and where it goes: registrable domains, decoded from
/// punycode (`paypal.com`, `evil.example`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deception {
    /// The domain the link's text shows.
    pub shown: SharedString,
    /// The domain the link opens.
    pub opens: SharedString,
}

/// A message's HTML for the original mode (ADR-020). Its links go through the same
/// policy as the reader's ([`MessageDocument::links`], `links::open_link`).
#[derive(Debug, Clone, PartialEq)]
pub struct OriginalHtml {
    /// The HTML after the sanitizer (PLAN §4.8), before any resource rewrite.
    pub html: Sensitive<Arc<str>>,
    /// Whether the message's remote content decision allows remote resources (policy
    /// `always`, or `ask` with the sender on the "Always for this sender" list).
    pub remote_allowed: bool,
    /// Whether blocked remote resources may be loaded on request (policy `ask`): the
    /// view then offers "Load images" and "Always for this sender".
    pub remote_ask: bool,
    /// Tracking pixels the sanitizer removed.
    pub trackers_removed: usize,
}

/// The sender's mail client, as the message header shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderClient {
    /// Normalized name, e.g. "Thunderbird 128.3 (Linux)".
    pub name: SharedString,
    /// The header it was recognized by, e.g. "X-Mailer: iPhone Mail (22B83)".
    pub header: SharedString,
}

/// The overall result of sender verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The sender's domain authenticated the message.
    Verified,
    /// Nothing proves or disproves the sender.
    Unverified,
    /// The sender's domain says it didn't send this message.
    Suspicious,
}

/// Sender verification as the message header shows it (CONCEPT §8.4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Verification {
    /// Being checked, or waiting for the body.
    #[default]
    Pending,
    /// Can't be checked (no DNS resolver).
    Unavailable,
    /// Checked.
    Checked {
        /// The verdict.
        verdict: Verdict,
        /// The DKIM result ("pass", "fail", "key no longer published", …).
        dkim: SharedString,
        /// The SPF result.
        spf: SharedString,
        /// The DMARC result.
        dmarc: SharedString,
        /// One or two sentences explaining the verdict.
        explanation: SharedString,
    },
}

/// The body of a message.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageBody {
    /// Sanitized reader-mode HTML (PLAN §4.8), rendered with gpui-kit's `TextView`.
    Html(Sensitive<SharedString>),
    /// The reader document as Markdown, rendered natively with gpui-kit's `TextView`.
    /// The source must escape mail text and must not use image syntax: the view
    /// renders what it gets, and remote content is never loaded (PLAN §4.8).
    Markdown(Sensitive<SharedString>),
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

    /// Whether a message row matches.
    pub fn matches_message(self, row: &MessageRow) -> bool {
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
    /// No account syncs (none added yet, or only Local Folders of an older Maze).
    NoAccounts,
    /// What the sync engine says, for states without progress ("Connecting…",
    /// "Offline: … Maze will try again in 30 seconds.").
    Message {
        /// The sentence.
        text: SharedString,
        /// Whether it needs attention (offline, sign-in required, an error).
        problem: bool,
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

/// What changed in the data behind the window, from the backend's change events.
/// [`MailWindow::apply_changes`](crate::MailWindow::apply_changes) re-reads only what
/// is affected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DataChanges {
    /// Folders or accounts were added, renamed or removed: rebuild the folder pane.
    pub folders: bool,
    /// Unread counts changed: re-render the folder pane's badges.
    pub counts: bool,
    /// Threads were added, moved or removed: re-read the open list's keys.
    pub threads: bool,
    /// Threads whose rows or messages changed: re-show the message if it belongs to
    /// one of them.
    pub threads_updated: Vec<ThreadId>,
    /// Messages whose data changed (body arrived, sender verified): re-show the
    /// message if it is one of them.
    pub messages_updated: Vec<MessageId>,
    /// The sync or connection status changed: re-render the status bar.
    pub status: bool,
    /// Changes were lost (or the backend just became available): reload everything.
    pub everything: bool,
}

impl DataChanges {
    /// Everything changed.
    pub fn everything() -> Self {
        Self {
            everything: true,
            ..Self::default()
        }
    }

    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Adds `other`'s changes.
    pub fn merge(&mut self, other: Self) {
        self.folders |= other.folders;
        self.counts |= other.counts;
        self.threads |= other.threads;
        self.everything |= other.everything;
        self.status |= other.status;
        for message in other.messages_updated {
            if !self.messages_updated.contains(&message) {
                self.messages_updated.push(message);
            }
        }
        for thread in other.threads_updated {
            if !self.threads_updated.contains(&thread) {
                self.threads_updated.push(thread);
            }
        }
    }
}

/// Errors from a [`MailSource`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// The thread doesn't exist (any more).
    #[error("thread {0} not found")]
    ThreadNotFound(ThreadId),
    /// The message doesn't exist (any more).
    #[error("message {0} not found")]
    MessageNotFound(MessageId),
    /// The folder doesn't exist (any more).
    #[error("folder not found")]
    FolderNotFound,
    /// The backend couldn't answer.
    #[error("mail backend unavailable: {0}")]
    Unavailable(String),
}

/// Everything the mail UI reads and changes. Implemented over the core by the
/// `mazemail` binary, and with synthetic data by [`crate::FakeMailSource`].
pub trait MailSource: Send + Sync + 'static {
    /// The folder pane content.
    fn folder_tree(&self) -> FolderTree;

    /// Unread threads in a folder (cached counts, cheap).
    fn unread_count(&self, folder: &FolderKey) -> u64;

    /// Thread keys of a scope in display order (`sort`), after `filter`.
    ///
    /// With a text filter, or sorted by anything but the date, this can take a while on
    /// large folders; call it off the main thread then.
    fn thread_keys(
        &self,
        scope: &ThreadScope,
        filter: &ThreadFilter,
        sort: ListSort,
    ) -> Result<ThreadKeys, SourceError>;

    /// Rows for `keys`, in the same order. Threads that no longer exist are left out.
    fn thread_rows(&self, keys: &[ThreadId]) -> Vec<ThreadRow>;

    /// Message keys of a scope (threads off: one row per message) in display order,
    /// after `filter`. Like [`MailSource::thread_keys`], slow with a text filter or a
    /// sort other than the date.
    fn message_keys(
        &self,
        scope: &ThreadScope,
        filter: &ThreadFilter,
        sort: ListSort,
    ) -> Result<MessageKeys, SourceError>;

    /// Rows for messages, in the same order. Messages that no longer exist are left
    /// out.
    fn message_rows(&self, keys: &[MessageId]) -> Vec<MessageRow>;

    /// The reply tree of a thread: its messages in the open list's folder (as many as
    /// the thread row's `message_count`), depth first.
    fn thread_tree(&self, thread: ThreadId) -> Result<ThreadTree, SourceError>;

    /// The message count of each thread, in the order given (0 for threads that are
    /// gone): what "expand all" sizes the list with. Can be slow for a million threads;
    /// call it off the main thread then.
    fn message_counts(&self, threads: &[ThreadId]) -> Vec<u32> {
        const CHUNK: usize = 512;
        let mut out = Vec::with_capacity(threads.len());
        for chunk in threads.chunks(CHUNK) {
            let mut rows = self.thread_rows(chunk).into_iter().peekable();
            out.extend(chunk.iter().map(|id| {
                rows.next_if(|row| row.id == *id)
                    .map_or(0, |row| row.message_count)
            }));
        }
        out
    }

    /// The index of the next message in `keys` matching `predicate`, starting after (or
    /// before) `from`, like [`MailSource::find`].
    fn find_message(
        &self,
        keys: &[MessageId],
        from: Option<usize>,
        direction: Direction,
        predicate: ThreadPredicate,
    ) -> Option<usize> {
        const CHUNK: usize = 512;
        let hit_in = |start: usize, end: usize, forward: bool| {
            let rows = self.message_rows(&keys[start..end]);
            let hit = if forward {
                rows.iter().find(|row| predicate.matches_message(row))
            } else {
                rows.iter().rev().find(|row| predicate.matches_message(row))
            }?;
            keys[start..end]
                .iter()
                .position(|id| *id == hit.id)
                .map(|offset| start + offset)
        };
        match direction {
            Direction::Forward => {
                let mut start = from.map_or(0, |ix| ix + 1);
                while start < keys.len() {
                    let end = (start + CHUNK).min(keys.len());
                    if let Some(ix) = hit_in(start, end, true) {
                        return Some(ix);
                    }
                    start = end;
                }
                None
            }
            Direction::Backward => {
                let mut end = from.unwrap_or(keys.len()).min(keys.len());
                while end > 0 {
                    let start = end.saturating_sub(CHUNK);
                    if let Some(ix) = hit_in(start, end, false) {
                        return Some(ix);
                    }
                    end = start;
                }
                None
            }
        }
    }

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

    /// One message, ready to display (a message row of an expanded thread or of an
    /// unthreaded list).
    fn message_document(&self, message: MessageId) -> Result<MessageDocument, SourceError>;

    /// The inline part (`cid:` image) of a message with this Content-ID, for the
    /// original mode. Served from memory, never written to disk.
    fn inline_part(&self, _message: MessageId, _content_id: &str) -> Option<Part> {
        None
    }

    /// A remote resource of a message, fetched by Maze (never by the engine), when the
    /// message's remote content decision allows it, or `once` ("Load images") under
    /// the `ask` policy. `None` when it can't be had.
    fn remote_resource(&self, _message: MessageId, _url: &str, _once: bool) -> Option<Part> {
        None
    }

    /// "Always for this sender": messages from `sender` load remote content without
    /// asking (unless the policy is `never`).
    fn allow_remote_content(&self, _sender: &str) -> Result<(), SourceError> {
        Err(SourceError::Unavailable(
            "remote content exceptions aren't kept here".into(),
        ))
    }

    /// Adds and removes flags on threads (mark read, star, …).
    fn set_flags(&self, threads: &[ThreadId], add: Flags, remove: Flags)
    -> Result<(), SourceError>;

    /// Adds and removes flags on single messages.
    fn set_message_flags(
        &self,
        messages: &[MessageId],
        add: Flags,
        remove: Flags,
    ) -> Result<(), SourceError>;

    /// Moves threads to their account's Archives folder.
    fn archive(&self, threads: &[ThreadId]) -> Result<(), SourceError>;

    /// Moves single messages to their account's Archives folder.
    fn archive_messages(&self, messages: &[MessageId]) -> Result<(), SourceError>;

    /// Reverts the last archive, move or flag change. `false` when there was nothing
    /// to undo.
    fn undo(&self) -> Result<bool, SourceError>;

    /// The account a thread belongs to (Quick Move offers that account's folders).
    fn thread_account(&self, thread: ThreadId) -> Option<AccountId>;

    /// The account a message belongs to.
    fn message_account(&self, message: MessageId) -> Option<AccountId>;

    /// Moves threads into `to`, a folder of their account. `from` is the folder they
    /// are moved out of (the one shown); `None` moves every copy except Gmail's All
    /// Mail. Undoable like [`MailSource::archive`].
    fn move_threads(
        &self,
        threads: &[ThreadId],
        to: FolderId,
        from: Option<FolderId>,
    ) -> Result<(), SourceError>;

    /// Moves single messages into `to`, like [`MailSource::move_threads`].
    fn move_messages(
        &self,
        messages: &[MessageId],
        to: FolderId,
        from: Option<FolderId>,
    ) -> Result<(), SourceError>;

    /// Adds a folder to the favorites or removes it. The next
    /// [`MailSource::folder_tree`] shows the change.
    fn set_favorite(&self, folder: FolderId, favorite: bool) -> Result<(), SourceError>;

    /// Connection, sync and outbox state.
    fn status(&self) -> SourceStatus;

    /// Synchronizes every account now (the title bar's sync button, "Mail: Sync Now").
    /// Only queues the request: progress shows in [`MailSource::status`].
    fn sync_now(&self) -> Result<(), SourceError>;

    /// Text shown instead of the message list when there is no mail to show at all:
    /// while the backend opens, when it failed, or before any account exists. `None`
    /// when mail can be shown (an empty folder still says "No messages").
    fn placeholder(&self) -> Option<SharedString> {
        None
    }

    /// Whether the empty state offers "Add account" (no account exists yet).
    fn offers_add_account(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_names_round_trip_and_read_the_old_names() {
        for by in SortBy::ALL {
            for descending in [false, true] {
                let sort = ListSort { by, descending };
                assert_eq!(ListSort::parse(&sort.name()), Some(sort));
            }
        }
        assert_eq!(
            ListSort::parse("subject_asc"),
            Some(ListSort::by(SortBy::Subject))
        );
        assert_eq!(
            ListSort::parse("oldest_first"),
            Some(ListSort::OLDEST_FIRST)
        );
        assert_eq!(
            ListSort::parse("newest_first"),
            Some(ListSort::NEWEST_FIRST)
        );
        assert_eq!(ListSort::parse("subject"), None);
        assert_eq!(ListSort::parse("color_asc"), None);
        let json = serde_json::to_string(&ListSort::by(SortBy::Size)).unwrap();
        assert_eq!(json, "\"size_desc\"");
    }

    #[test]
    fn a_click_sorts_by_the_column_then_reverses() {
        let sort = ListSort::NEWEST_FIRST;
        let by_subject = sort.clicked(SortBy::Subject);
        assert_eq!(by_subject, ListSort::by(SortBy::Subject));
        assert!(!by_subject.descending, "text starts A to Z");
        assert!(by_subject.clicked(SortBy::Subject).descending);
        assert!(by_subject.clicked(SortBy::Star).descending, "starred first");
        assert_eq!(sort.clicked(SortBy::Date), ListSort::OLDEST_FIRST);
        assert_eq!(ListSort::by(SortBy::Size).describe(), "Size, Largest First");
    }
}
