//! Row types and inputs of the store's API.
//!
//! Types that carry mail content or addresses implement `Debug` by hand and print only
//! ids and counts, so they're safe in logs (AGENTS.md).

use std::fmt;

use mazemail_types::{
    AccountId, Address, BlobId, FolderId, FolderRole, IdentityId, MessageId, TagId, ThreadId,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unix time in seconds (UTC). Every timestamp in the store uses it.
pub type UnixTime = i64;

macro_rules! local_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

local_id!(
    /// A queued server operation (`pending_op`).
    OpId
);
local_id!(
    /// An outbox item.
    OutboxId
);
local_id!(
    /// A local API token.
    TokenId
);
local_id!(
    /// An agent proposal.
    ProposalId
);
local_id!(
    /// One command inside a proposal.
    ProposalItemId
);
local_id!(
    /// An audit log entry.
    AuditId
);
local_id!(
    /// An agent conversation.
    AgentThreadId
);
local_id!(
    /// An SMTP server.
    SmtpServerId
);
local_id!(
    /// A signature.
    SignatureId
);
local_id!(
    /// An identity rule.
    IdentityRuleId
);
local_id!(
    /// A filter rule.
    RuleId
);
local_id!(
    /// A saved search (virtual folder).
    SavedSearchId
);
local_id!(
    /// An address book entry.
    ContactId
);

/// Prints the listed fields and elides the rest.
macro_rules! redacted_debug {
    ($ty:ident { $($field:ident),* $(,)? }) => {
        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($ty))
                    $(.field(stringify!($field), &self.$field))*
                    .finish_non_exhaustive()
            }
        }
    };
}

/// Implements `as_str`/`parse` for a fieldless enum stored as TEXT.
macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $text:literal),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $($(#[$vmeta])* #[serde(rename = $text)] $variant),*
        }

        impl $name {
            /// The value stored in the database.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),*
                }
            }

            /// Parses the database value.
            pub fn parse(s: &str) -> Option<Self> {
                match s {
                    $($text => Some(Self::$variant),)*
                    _ => None,
                }
            }
        }

        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(self.as_str().into())
            }
        }

        impl rusqlite::types::FromSql for $name {
            fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                let text = value.as_str()?;
                Self::parse(text).ok_or_else(|| rusqlite::types::FromSqlError::Other(
                    format!("unknown {} {text:?}", stringify!($name)).into(),
                ))
            }
        }
    };
}

// ---------------------------------------------------------------------------------------
// Accounts, folders, identities

text_enum!(
    /// The protocol family of an account.
    AccountKind {
        Imap = "imap",
        Gmail = "gmail",
        M365 = "m365",
        Local = "local",
        Jmap = "jmap",
    }
);

/// An account.
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    pub id: AccountId,
    /// Stable for the account's lifetime and never reused; generated when the account
    /// is created. Keychain secrets are keyed by it (`auth` crate).
    pub uuid: Uuid,
    pub kind: AccountKind,
    /// The name shown in the folder pane (often an address).
    pub name: String,
    /// Connection settings as JSON (owned by the connector).
    pub config_json: String,
    /// Connector sync state as JSON.
    pub sync_state_json: Option<String>,
    pub ordinal: i64,
}
redacted_debug!(Account {
    id,
    uuid,
    kind,
    ordinal
});

/// Input for creating an account.
#[derive(Clone, PartialEq, Eq)]
pub struct NewAccount {
    pub kind: AccountKind,
    pub name: String,
    pub config_json: String,
    pub ordinal: i64,
}
redacted_debug!(NewAccount { kind, ordinal });

/// Input for creating a folder or a Gmail label folder.
#[derive(Clone, PartialEq, Eq)]
pub struct NewFolder {
    pub account: AccountId,
    pub parent: Option<FolderId>,
    /// The server's name (IMAP mailbox name or Gmail label); `None` for local folders.
    pub remote_name: Option<String>,
    pub display_name: String,
    pub role: Option<FolderRole>,
    /// A Gmail label shown as a folder (its locations have no UIDs).
    pub is_label: bool,
    pub sync_policy: Option<String>,
    pub ordinal: i64,
}
redacted_debug!(NewFolder {
    account,
    parent,
    role,
    is_label
});

impl NewFolder {
    /// A top-level folder with the given names and no role.
    pub fn new(account: AccountId, remote_name: &str, display_name: &str) -> Self {
        Self {
            account,
            parent: None,
            remote_name: Some(remote_name.to_owned()),
            display_name: display_name.to_owned(),
            role: None,
            is_label: false,
            sync_policy: None,
            ordinal: 0,
        }
    }

    /// Sets the role.
    pub fn with_role(mut self, role: FolderRole) -> Self {
        self.role = Some(role);
        self
    }

    /// Marks it as a Gmail label folder.
    pub fn label(mut self) -> Self {
        self.is_label = true;
        self
    }
}

/// IMAP synchronization state of a folder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderSyncState {
    pub uidvalidity: Option<u32>,
    pub uidnext: Option<u32>,
    pub highestmodseq: Option<u64>,
}

/// A folder with its sync state and counts.
#[derive(Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: FolderId,
    pub account: AccountId,
    pub parent: Option<FolderId>,
    pub remote_name: Option<String>,
    pub display_name: String,
    pub role: Option<FolderRole>,
    pub is_label: bool,
    pub sync: FolderSyncState,
    pub sync_policy: Option<String>,
    /// Locations without `\Seen` in this folder.
    pub unread: u64,
    /// Locations in this folder.
    pub total: u64,
    pub ordinal: i64,
    pub favorite: bool,
}
redacted_debug!(Folder {
    id,
    account,
    parent,
    role,
    is_label,
    unread,
    total
});

/// Changes to a folder's editable attributes; `None` leaves a field as it is.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct FolderPatch {
    pub parent: Option<Option<FolderId>>,
    pub remote_name: Option<Option<String>>,
    pub display_name: Option<String>,
    pub role: Option<Option<FolderRole>>,
    pub sync_policy: Option<Option<String>>,
    pub ordinal: Option<i64>,
    pub favorite: Option<bool>,
}
redacted_debug!(FolderPatch {
    parent,
    role,
    ordinal,
    favorite
});

/// A folder and its children, for the folder pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderNode {
    pub folder: Folder,
    pub children: Vec<FolderNode>,
}

text_enum!(
    /// Transport security of an SMTP server.
    SmtpSecurity {
        Tls = "tls",
        StartTls = "starttls",
        None = "none",
    }
);

text_enum!(
    /// SMTP authentication mechanism; the secret itself is in the keychain.
    SmtpAuth {
        None = "none",
        Plain = "plain",
        Login = "login",
        OAuth2 = "oauth2",
    }
);

text_enum!(
    /// Whether the server files a Sent copy itself (PLAN §4.5).
    SavesSent {
        /// Gmail and Microsoft 365: yes; others: no.
        Auto = "auto",
        Yes = "yes",
        No = "no",
    }
);

/// An SMTP server. Passwords and tokens are in the keychain.
#[derive(Clone, PartialEq, Eq)]
pub struct SmtpServer {
    pub id: SmtpServerId,
    pub name: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub security: SmtpSecurity,
    pub auth: SmtpAuth,
    pub username: Option<String>,
    pub saves_sent: SavesSent,
}
redacted_debug!(SmtpServer {
    id,
    port,
    security,
    auth,
    saves_sent
});

/// Input for an SMTP server ([`SmtpServer`] without the id).
#[derive(Clone, PartialEq, Eq)]
pub struct SmtpServerInput {
    pub name: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub security: SmtpSecurity,
    pub auth: SmtpAuth,
    pub username: Option<String>,
    pub saves_sent: SavesSent,
}
redacted_debug!(SmtpServerInput {
    port,
    security,
    auth,
    saves_sent
});

/// A signature with HTML and plain-text variants.
#[derive(Clone, PartialEq, Eq)]
pub struct Signature {
    pub id: SignatureId,
    pub name: String,
    pub body_html: Option<String>,
    pub body_text: Option<String>,
    pub ordinal: i64,
    pub created: UnixTime,
    pub updated: UnixTime,
}
redacted_debug!(Signature { id, ordinal });

/// Input for a signature.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SignatureInput {
    pub name: String,
    pub body_html: Option<String>,
    pub body_text: Option<String>,
    pub ordinal: i64,
}
redacted_debug!(SignatureInput { ordinal });

/// A sending identity (CONCEPT §8.6): From, Reply-To, signature, SMTP server and folders
/// switched as one bundle.
#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    pub id: IdentityId,
    pub account: AccountId,
    pub input: IdentityInput,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("id", &self.id)
            .field("account", &self.account)
            .field("input", &self.input)
            .finish()
    }
}

/// The editable part of an identity.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct IdentityInput {
    pub label: Option<String>,
    pub display_name: Option<String>,
    pub email: String,
    pub reply_to: Option<String>,
    pub signature: Option<SignatureId>,
    pub smtp: Option<SmtpServerId>,
    pub sent_folder: Option<FolderId>,
    pub drafts_folder: Option<FolderId>,
    pub archive_folder: Option<FolderId>,
    /// 0 single folder, 1 yearly, 2 monthly (Thunderbird's archive granularity).
    pub archive_granularity: i64,
    pub color: Option<String>,
    /// Address patterns this identity answers for (`*@distr.example`), as JSON.
    pub match_patterns_json: Option<String>,
    pub is_default: bool,
    pub ordinal: i64,
}
redacted_debug!(IdentityInput {
    signature,
    smtp,
    sent_folder,
    drafts_folder,
    archive_folder,
    is_default,
    ordinal
});

text_enum!(
    /// What an identity rule matches on.
    IdentityRuleKind {
        Recipient = "recipient",
        Domain = "domain",
        Folder = "folder",
        List = "list",
    }
);

/// A rule that picks an identity when replying or composing.
#[derive(Clone, PartialEq, Eq)]
pub struct IdentityRule {
    pub id: IdentityRuleId,
    pub identity: IdentityId,
    pub kind: IdentityRuleKind,
    pub pattern: String,
    pub ordinal: i64,
}
redacted_debug!(IdentityRule {
    id,
    identity,
    kind,
    ordinal
});

/// A tag: an IMAP keyword with a name and color.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub id: TagId,
    /// Lowercase IMAP keyword (`$label1`, `todo`, …).
    pub keyword: String,
    pub name: String,
    pub color: Option<String>,
    pub ordinal: i64,
}

// ---------------------------------------------------------------------------------------
// Messages

/// How much of a message body is stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BodyState {
    /// Headers only.
    #[default]
    None = 0,
    /// The text parts (large messages opened before the full download).
    TextParts = 1,
    /// The full raw message.
    Full = 2,
}

impl BodyState {
    pub(crate) fn from_i64(value: i64) -> Self {
        match value {
            1 => Self::TextParts,
            2 => Self::Full,
            _ => Self::None,
        }
    }
}

/// How the store picks the thread of a new message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadAssignment {
    /// Use this thread (for example one computed by the JWZ threader in
    /// `mazemail_mime`).
    Existing(ThreadId),
    /// Always start a new thread.
    New,
    /// Let the store decide: Gmail's `X-GM-THRID`; else the threads of messages this
    /// one refers to (References, In-Reply-To), that refer to it, or that share a
    /// referenced ancestor, merging them when there are several; else, if
    /// `by_subject`, a recent thread (30 days) with the same subject key; else new.
    Resolve {
        /// Fall back to the subject key. Callers usually set it only for replies.
        by_subject: bool,
    },
}

/// One copy of a message in a folder (or a Gmail label).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLocation {
    pub folder: FolderId,
    /// `None` for Gmail label folders and moves that haven't reached the server.
    pub uid: Option<u32>,
    pub modseq: Option<u64>,
    pub flags: mazemail_types::Flags,
    /// IMAP keywords; stored lowercase, deduplicated and sorted.
    pub keywords: Vec<String>,
}

impl NewLocation {
    /// A location with flags and no keywords.
    pub fn new(folder: FolderId, uid: Option<u32>, flags: mazemail_types::Flags) -> Self {
        Self {
            folder,
            uid,
            modseq: None,
            flags,
            keywords: Vec::new(),
        }
    }
}

/// A message as the sync engine or importer sees it: the header summary and its
/// locations.
#[derive(Clone, PartialEq, Eq)]
pub struct NewMessage {
    pub account: AccountId,
    /// `Message-ID` without angle brackets.
    pub rfc_message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub date: UnixTime,
    pub received: Option<UnixTime>,
    pub from: Option<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub delivered_to: Option<String>,
    pub subject: Option<String>,
    /// Normalized subject (no `Re:`/`Fwd:`), for threading by subject.
    pub subject_key: Option<String>,
    pub snippet: Option<String>,
    pub size: Option<u64>,
    pub has_attachments: bool,
    pub list_id: Option<String>,
    pub bodystructure_json: Option<String>,
    pub gm_msgid: Option<u64>,
    pub gm_thrid: Option<u64>,
    pub auth_json: Option<String>,
    pub identity_hint: Option<IdentityId>,
    pub thread: ThreadAssignment,
    pub locations: Vec<NewLocation>,
    /// Remove this message's locations in Gmail label folders that aren't listed in
    /// `locations` (the label set from `X-GM-LABELS` is complete).
    pub replace_labels: bool,
}
redacted_debug!(NewMessage {
    account,
    date,
    has_attachments,
    gm_msgid,
    gm_thrid,
    thread,
    locations,
    replace_labels
});

impl NewMessage {
    /// A message with only the required fields set; the rest is empty.
    pub fn new(account: AccountId, date: UnixTime) -> Self {
        Self {
            account,
            rfc_message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            date,
            received: None,
            from: None,
            to: Vec::new(),
            cc: Vec::new(),
            delivered_to: None,
            subject: None,
            subject_key: None,
            snippet: None,
            size: None,
            has_attachments: false,
            list_id: None,
            bodystructure_json: None,
            gm_msgid: None,
            gm_thrid: None,
            auth_json: None,
            identity_hint: None,
            thread: ThreadAssignment::Resolve { by_subject: false },
            locations: Vec::new(),
            replace_labels: false,
        }
    }
}

/// What the full body says about a stored message, for
/// [`WriteTx::update_body_summary`](crate::WriteTx::update_body_summary) (header-first
/// sync once the body arrives). Every field is optional: `None` keeps the stored value,
/// `Some` replaces it.
#[derive(Clone, Default)]
pub struct BodySummaryUpdate {
    pub snippet: Option<String>,
    /// Uncompressed size of the raw message in bytes.
    pub size: Option<u64>,
    /// Feeds the thread and folder attachment counts.
    pub has_attachments: Option<bool>,
    pub bodystructure_json: Option<String>,
    /// The new body. The message's previous body (if any) loses a reference.
    pub blob: Option<crate::StagedBlob>,
    /// The new body state; with a `blob` and no state, [`BodyState::Full`]. A state
    /// other than [`BodyState::None`] needs a body (given or stored).
    pub body_state: Option<BodyState>,
}

impl fmt::Debug for BodySummaryUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BodySummaryUpdate")
            .field("size", &self.size)
            .field("has_attachments", &self.has_attachments)
            .field("blob", &self.blob.as_ref().map(crate::StagedBlob::id))
            .field("body_state", &self.body_state)
            .finish_non_exhaustive()
    }
}

/// What [`WriteTx::upsert_message`](crate::WriteTx::upsert_message) did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpsertOutcome {
    pub message: MessageId,
    pub thread: ThreadId,
    /// False if an existing message was updated (Gmail dedup, known UID, or a detached
    /// message reattached).
    pub created: bool,
}

/// A message location as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub folder: FolderId,
    pub uid: Option<u32>,
    pub modseq: Option<u64>,
    pub flags: mazemail_types::Flags,
    pub keywords: Vec<String>,
}

/// Everything the store knows about a message.
#[derive(Clone, PartialEq, Eq)]
pub struct MessageDetail {
    pub id: MessageId,
    pub account: AccountId,
    pub thread: ThreadId,
    pub blob: Option<BlobId>,
    pub body_state: BodyState,
    pub rfc_message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub date: UnixTime,
    pub received: Option<UnixTime>,
    pub from: Option<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub delivered_to: Option<String>,
    pub subject: Option<String>,
    pub snippet: Option<String>,
    pub size: Option<u64>,
    pub has_attachments: bool,
    pub list_id: Option<String>,
    pub bodystructure_json: Option<String>,
    pub gm_msgid: Option<u64>,
    pub gm_thrid: Option<u64>,
    pub auth_json: Option<String>,
    pub identity_hint: Option<IdentityId>,
    pub locations: Vec<Location>,
    pub tags: Vec<TagId>,
    /// Set while the message has no location (after an expunge or UIDVALIDITY reset).
    pub detached_at: Option<UnixTime>,
}
redacted_debug!(MessageDetail {
    id,
    account,
    thread,
    blob,
    body_state,
    date,
    locations,
    tags
});

impl MessageDetail {
    /// The flags the UI shows: `\Seen` only if every location is seen, any other flag if
    /// some location has it (PLAN §4.3).
    pub fn effective_flags(&self) -> mazemail_types::Flags {
        effective_flags(self.locations.iter().map(|l| l.flags))
    }
}

/// Combines per-location flags: `\Seen` needs every location, other flags any.
pub fn effective_flags(
    locations: impl IntoIterator<Item = mazemail_types::Flags>,
) -> mazemail_types::Flags {
    use mazemail_types::Flags;
    let mut any = Flags::empty();
    let mut all_seen = true;
    let mut count = 0;
    for flags in locations {
        any |= flags;
        all_seen &= flags.contains(Flags::SEEN);
        count += 1;
    }
    if count == 0 || !all_seen {
        any.remove(Flags::SEEN);
    }
    any
}

// ---------------------------------------------------------------------------------------
// Thread lists

/// What a thread list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListScope {
    /// One folder or Gmail label.
    Folder(FolderId),
    /// A unified folder: every folder with this role, across accounts.
    Unified(FolderRole),
    /// Every thread of an account.
    Account(AccountId),
}

/// Sort order of a thread list (by the thread's newest message in the scope).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SortOrder {
    #[default]
    NewestFirst,
    OldestFirst,
}

/// Quick-filter toggles; all set toggles must match.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ListFilter {
    pub unread: bool,
    pub flagged: bool,
    pub has_attachment: bool,
    pub tag: Option<TagId>,
}

/// The sort key of a thread in a list: the date of its newest message in the scope.
pub type SortKey = UnixTime;

/// One entry of a thread list snapshot (16 bytes), also the keyset cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListKey {
    pub thread: ThreadId,
    pub key: SortKey,
}

/// A page request for [`Queries::thread_list_keys`](crate::Queries::thread_list_keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// Continue after this key (the last key of the previous page).
    pub after: Option<ListKey>,
    pub limit: usize,
}

impl Page {
    /// The first `limit` keys.
    pub fn first(limit: usize) -> Self {
        Self { after: None, limit }
    }

    /// The `limit` keys after `key`.
    pub fn after(key: ListKey, limit: usize) -> Self {
        Self {
            after: Some(key),
            limit,
        }
    }
}

/// A sender shown in a thread row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    pub name: Option<String>,
    pub email: String,
}

/// The data of one visible row of the thread list.
#[derive(Clone, PartialEq, Eq)]
pub struct ThreadRow {
    pub id: ThreadId,
    pub account: AccountId,
    pub subject: Option<String>,
    /// Distinct senders, oldest first (at most 8).
    pub participants: Vec<Participant>,
    /// Date of the newest message (in the context folder, if given).
    pub date: UnixTime,
    pub message_count: u32,
    pub unread_count: u32,
    pub flagged: bool,
    pub has_attachments: bool,
    pub tags: Vec<TagId>,
    /// Snippet of the newest message.
    pub snippet: Option<String>,
    /// The newest message.
    pub last_message: Option<MessageId>,
}
redacted_debug!(ThreadRow {
    id,
    account,
    date,
    message_count,
    unread_count,
    flagged,
    has_attachments,
    tags
});

// ---------------------------------------------------------------------------------------
// Search filters

/// Metadata conditions applied to search candidates in SQL (PLAN §4.4 "Mutable
/// fields"). Unset fields don't filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataFilter {
    /// The message has a location in one of these folders.
    pub folders: Option<Vec<FolderId>>,
    pub account: Option<AccountId>,
    /// The message has a location in a folder with this role.
    pub in_role: Option<FolderRole>,
    /// Flags that must be set (effective flags, see [`effective_flags`]).
    pub flags_set: mazemail_types::Flags,
    /// Flags that must not be set.
    pub flags_unset: mazemail_types::Flags,
    /// Tags the message must all have.
    pub tags: Vec<TagId>,
}

/// A narrow scope turned into an id set before searching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageScope {
    Folder(FolderId),
    Folders(Vec<FolderId>),
    Role(FolderRole),
    Account(AccountId),
    Thread(ThreadId),
}

// ---------------------------------------------------------------------------------------
// Pending ops, outbox, send log

text_enum!(
    /// State of a pending op.
    OpState {
        /// Waiting for (another) attempt.
        Queued = "queued",
        /// Failed too often; shown in the activity log with Retry and Discard.
        Stuck = "stuck",
    }
);

/// A server operation waiting to be replayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOp {
    pub id: OpId,
    pub account: AccountId,
    pub kind: String,
    pub payload_json: String,
    pub origin: String,
    pub state: OpState,
    pub attempts: u32,
    pub next_attempt: UnixTime,
    pub last_error: Option<String>,
    pub created: UnixTime,
}

text_enum!(
    /// State of an outbox item.
    OutboxState {
        Queued = "queued",
        Sending = "sending",
        Sent = "sent",
        Failed = "failed",
        /// After a rollback: waits until you press Send again (PLAN §4.15).
        Frozen = "frozen",
    }
);

/// A message waiting to be sent (or sent, until it is removed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxItem {
    pub id: OutboxId,
    pub identity: Option<IdentityId>,
    pub blob: Option<BlobId>,
    pub rfc_message_id: String,
    pub send_at: UnixTime,
    pub state: OutboxState,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created: UnixTime,
}

/// A send log entry: the SMTP server accepted this message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentReceipt {
    pub rfc_message_id: String,
    pub sent_at: UnixTime,
    pub identity: Option<IdentityId>,
    pub outbox: Option<OutboxId>,
}

// ---------------------------------------------------------------------------------------
// Tokens, proposals, audit, agents

/// A local API token. Only the hash of the secret is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiToken {
    pub id: TokenId,
    pub hash: [u8; 32],
    /// `read-only`, `triage` or `writer`.
    pub profile: String,
    pub scope_json: String,
    pub expires: Option<UnixTime>,
    pub label: Option<String>,
    pub created: UnixTime,
    pub last_used: Option<UnixTime>,
    pub revoked: Option<UnixTime>,
}

impl ApiToken {
    /// Not revoked and not expired at `now`.
    pub fn is_valid_at(&self, now: UnixTime) -> bool {
        self.revoked.is_none() && self.expires.is_none_or(|expires| now < expires)
    }
}

/// Input for a new token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApiToken {
    pub hash: [u8; 32],
    pub profile: String,
    pub scope_json: String,
    pub expires: Option<UnixTime>,
    pub label: Option<String>,
}

text_enum!(
    /// State of a proposal.
    ProposalState {
        Pending = "pending",
        Approved = "approved",
        Rejected = "rejected",
        /// Some items approved, some rejected.
        Partial = "partial",
        Applied = "applied",
    }
);

text_enum!(
    /// State of a proposal item.
    ProposalItemState {
        Pending = "pending",
        Approved = "approved",
        Rejected = "rejected",
        Applied = "applied",
        Failed = "failed",
    }
);

/// A batch of commands an agent proposed for review.
#[derive(Clone, PartialEq, Eq)]
pub struct Proposal {
    pub id: ProposalId,
    pub token: Option<TokenId>,
    pub title: String,
    pub state: ProposalState,
    pub created: UnixTime,
    pub decided: Option<UnixTime>,
    pub items: Vec<ProposalItem>,
}
redacted_debug!(Proposal {
    id,
    token,
    state,
    created,
    items
});

/// One proposed command.
#[derive(Clone, PartialEq, Eq)]
pub struct ProposalItem {
    pub id: ProposalItemId,
    pub command_json: String,
    pub reason: Option<String>,
    pub state: ProposalItemState,
}
redacted_debug!(ProposalItem { id, state });

/// Input for one proposed command.
#[derive(Clone, PartialEq, Eq)]
pub struct NewProposalItem {
    pub command_json: String,
    pub reason: Option<String>,
}
redacted_debug!(NewProposalItem {});

/// An audit log entry.
#[derive(Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub id: AuditId,
    pub at: UnixTime,
    pub origin: String,
    pub action: String,
    pub detail_json: Option<String>,
}
redacted_debug!(AuditEntry {
    id,
    at,
    origin,
    action
});

/// Which audit entries to return, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditQuery {
    pub since: Option<UnixTime>,
    pub before_id: Option<AuditId>,
    /// Exact origin, or a prefix ending in `:` (`"agent:"`).
    pub origin: Option<String>,
    pub limit: usize,
}

/// An agent conversation (PLAN §4.12).
#[derive(Clone, PartialEq, Eq)]
pub struct AgentThread {
    pub id: AgentThreadId,
    pub agent_id: String,
    pub title: Option<String>,
    pub mail_thread: Option<ThreadId>,
    pub acp_session_id: Option<String>,
    pub transcript: Option<BlobId>,
    pub created: UnixTime,
    pub updated: UnixTime,
}
redacted_debug!(AgentThread {
    id,
    agent_id,
    mail_thread,
    transcript,
    created,
    updated
});

// ---------------------------------------------------------------------------------------
// Contacts, rules, saved searches

/// An address book entry.
#[derive(Clone, PartialEq, Eq)]
pub struct Contact {
    pub id: ContactId,
    pub book: String,
    pub display_name: Option<String>,
    pub email: String,
    pub frequency: u32,
    pub last_used: Option<UnixTime>,
    pub vcard: Option<String>,
}
redacted_debug!(Contact {
    id,
    book,
    frequency,
    last_used
});

text_enum!(
    /// When a rule runs (CONCEPT §14).
    RuleTrigger {
        ArrivalBeforeJunk = "arrival_before_junk",
        Arrival = "arrival",
        Manual = "manual",
        AfterSend = "after_send",
        Periodic = "periodic",
    }
);

/// A filter rule; conditions and actions are JSON owned by the `rules` crate.
#[derive(Clone, PartialEq, Eq)]
pub struct Rule {
    pub id: RuleId,
    pub input: RuleInput,
    pub created: UnixTime,
    pub updated: UnixTime,
}

impl fmt::Debug for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rule")
            .field("id", &self.id)
            .field("input", &self.input)
            .finish_non_exhaustive()
    }
}

/// The editable part of a rule.
#[derive(Clone, PartialEq, Eq)]
pub struct RuleInput {
    /// `None` applies to every account.
    pub account: Option<AccountId>,
    pub name: String,
    pub ordinal: i64,
    pub enabled: bool,
    pub trigger: RuleTrigger,
    pub condition_json: String,
    pub actions_json: String,
}
redacted_debug!(RuleInput {
    account,
    ordinal,
    enabled,
    trigger
});

/// A saved search (Thunderbird virtual folder).
#[derive(Clone, PartialEq, Eq)]
pub struct SavedSearch {
    pub id: SavedSearchId,
    pub name: String,
    pub query: String,
    /// Folders searched; `None` means all.
    pub folders: Option<Vec<FolderId>>,
    pub ordinal: i64,
    pub created: UnixTime,
}
redacted_debug!(SavedSearch {
    id,
    folders,
    ordinal
});

#[cfg(test)]
mod tests {
    use mazemail_types::Flags;

    use super::*;

    #[test]
    fn effective_flags_need_every_location_seen() {
        assert_eq!(effective_flags([]), Flags::empty());
        assert_eq!(
            effective_flags([Flags::SEEN, Flags::SEEN | Flags::FLAGGED]),
            Flags::SEEN | Flags::FLAGGED
        );
        assert_eq!(
            effective_flags([Flags::SEEN, Flags::ANSWERED]),
            Flags::ANSWERED
        );
    }

    #[test]
    fn debug_output_is_redacted() {
        let mut message = NewMessage::new(AccountId(1), 0);
        message.subject = Some("Quarterly numbers".to_owned());
        message.from = Some(Address::new(Some("Anna"), "anna@example.com"));
        let debug = format!("{message:?}");
        assert!(!debug.contains("Quarterly"), "{debug}");
        assert!(!debug.contains("anna@"), "{debug}");
        assert!(debug.contains("account"), "{debug}");
    }

    #[test]
    fn text_enums_round_trip() {
        for kind in [AccountKind::Imap, AccountKind::Gmail, AccountKind::Jmap] {
            assert_eq!(AccountKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(
            RuleTrigger::parse("after_send"),
            Some(RuleTrigger::AfterSend)
        );
        assert_eq!(OutboxState::parse("bogus"), None);
    }
}
