//! Data types exchanged through [`ImapSession`](crate::ImapSession).

use std::collections::BTreeSet;

use bytes::Bytes;
use mazemail_types::{Flags, FolderRole, Sensitive};
use time::OffsetDateTime;

use crate::gmail::GmailLabel;
use crate::uid_set::UidSet;

/// The server's capability list (RFC 3501 §7.2.1), with typed accessors for the
/// extensions Maze uses. Names are compared case-insensitively.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caps {
    atoms: BTreeSet<String>,
}

impl Caps {
    /// Build from raw capability names such as `IMAP4rev1`, `AUTH=PLAIN`, `MOVE`.
    pub fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            atoms: names
                .into_iter()
                .map(|n| n.as_ref().to_ascii_uppercase())
                .collect(),
        }
    }

    /// Whether the server lists the capability `name` (case-insensitive).
    pub fn has(&self, name: &str) -> bool {
        self.atoms.contains(&name.to_ascii_uppercase())
    }

    /// All capability names, upper-cased, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.atoms.iter().map(String::as_str)
    }

    /// SASL mechanisms from `AUTH=` capabilities, upper-cased.
    pub fn auth_mechanisms(&self) -> impl Iterator<Item = &str> {
        self.atoms.iter().filter_map(|a| a.strip_prefix("AUTH="))
    }

    /// Whether `AUTH=<mechanism>` is offered.
    pub fn has_auth(&self, mechanism: &str) -> bool {
        self.has(&format!("AUTH={mechanism}"))
    }

    /// `IDLE` (RFC 2177).
    pub fn idle(&self) -> bool {
        self.has("IDLE")
    }

    /// `CONDSTORE` (RFC 7162). Implied by `QRESYNC`.
    pub fn condstore(&self) -> bool {
        self.has("CONDSTORE") || self.has("QRESYNC")
    }

    /// `QRESYNC` (RFC 7162).
    pub fn qresync(&self) -> bool {
        self.has("QRESYNC")
    }

    /// `MOVE` (RFC 6851).
    pub fn has_move(&self) -> bool {
        self.has("MOVE")
    }

    /// `UIDPLUS` (RFC 4315): `APPENDUID`, `COPYUID`, `UID EXPUNGE`.
    pub fn uidplus(&self) -> bool {
        self.has("UIDPLUS")
    }

    /// `SPECIAL-USE` (RFC 6154).
    pub fn special_use(&self) -> bool {
        self.has("SPECIAL-USE")
    }

    /// `LIST-EXTENDED` (RFC 5258).
    pub fn list_extended(&self) -> bool {
        self.has("LIST-EXTENDED")
    }

    /// `ENABLE` (RFC 5161).
    pub fn enable(&self) -> bool {
        self.has("ENABLE")
    }

    /// `ID` (RFC 2971).
    pub fn id(&self) -> bool {
        self.has("ID")
    }

    /// `COMPRESS=DEFLATE` (RFC 4978).
    pub fn compress_deflate(&self) -> bool {
        self.has("COMPRESS=DEFLATE")
    }

    /// `ESEARCH` (RFC 4731). Advertised only; this client does not use it (see crate docs).
    pub fn esearch(&self) -> bool {
        self.has("ESEARCH")
    }

    /// `NOTIFY` (RFC 5465). Advertised only; this client does not use it (see crate docs).
    pub fn notify(&self) -> bool {
        self.has("NOTIFY")
    }

    /// `LITERAL+` (RFC 7888): non-synchronizing literals of any size.
    pub fn literal_plus(&self) -> bool {
        self.has("LITERAL+")
    }

    /// `STARTTLS`.
    pub fn starttls(&self) -> bool {
        self.has("STARTTLS")
    }

    /// `LOGINDISABLED`: the `LOGIN` command must not be used.
    pub fn login_disabled(&self) -> bool {
        self.has("LOGINDISABLED")
    }

    /// Gmail's IMAP extensions (`X-GM-EXT-1`): labels, message and thread ids.
    pub fn gmail(&self) -> bool {
        self.has("X-GM-EXT-1")
    }
}

/// A mailbox from `LIST`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFolder {
    /// Full name, decoded from modified UTF-7. Pass this to the other
    /// [`ImapSession`](crate::ImapSession) methods.
    pub name: String,
    /// The name exactly as the server sent it (modified UTF-7).
    pub raw_name: String,
    /// Hierarchy delimiter, `None` for a flat namespace.
    pub delimiter: Option<char>,
    /// Name attributes as sent, e.g. `\HasNoChildren`, `\Sent`, `\Noselect`.
    pub attributes: Vec<String>,
    /// Role from SPECIAL-USE attributes, or [`FolderRole::Inbox`] for `INBOX`.
    pub role: Option<FolderRole>,
    /// `false` for `\Noselect` / `\NonExistent` placeholders.
    pub selectable: bool,
    /// Whether the folder is subscribed (from `LSUB`).
    pub subscribed: bool,
}

impl RemoteFolder {
    /// Whether the folder has the attribute `attr` (case-insensitive, e.g. `\Noselect`).
    pub fn has_attribute(&self, attr: &str) -> bool {
        self.attributes.iter().any(|a| a.eq_ignore_ascii_case(attr))
    }

    /// The last hierarchy component, for display.
    pub fn leaf_name(&self) -> &str {
        match self.delimiter {
            Some(d) => self.name.rsplit(d).next().unwrap_or(&self.name),
            None => &self.name,
        }
    }
}

/// Known state for a `SELECT … (QRESYNC (uidvalidity modseq [known-uids]))` (RFC 7162 §3.2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QresyncArgs {
    /// The UIDVALIDITY the client's cache was built with.
    pub uidvalidity: u32,
    /// The highest mod-sequence the client has seen.
    pub highest_modseq: u64,
    /// Optional known UIDs. Usually left out: on large folders it can exceed command
    /// length limits, and RFC 7162 allows omitting it.
    pub known_uids: Option<UidSet>,
}

/// The result of `SELECT`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectInfo {
    /// `UIDVALIDITY`. If it differs from the cached value, every cached UID is invalid.
    pub uidvalidity: u32,
    /// `UIDNEXT`, if the server sent it.
    pub uidnext: Option<u32>,
    /// `HIGHESTMODSEQ`, or `None` without CONDSTORE or when the folder reports `NOMODSEQ`.
    pub highest_modseq: Option<u64>,
    /// Number of messages.
    pub exists: u32,
    /// Number of `\Recent` messages.
    pub recent: u32,
    /// Sequence number of the first unseen message, if sent.
    pub first_unseen: Option<u32>,
    /// Flags defined in the folder (`FLAGS`).
    pub flags: Vec<String>,
    /// Flags that can be changed permanently (`PERMANENTFLAGS`), without `\*`.
    pub permanent_flags: Vec<String>,
    /// Whether new keywords can be created (`\*` in `PERMANENTFLAGS`).
    pub allows_new_keywords: bool,
    /// Whether the folder was opened read-only (`[READ-ONLY]`).
    pub read_only: bool,
    /// Whether the server accepted the QRESYNC parameters, i.e. `vanished` and
    /// `changed` describe everything since the given mod-sequence.
    pub qresync_applied: bool,
    /// UIDs expunged since the given mod-sequence (`VANISHED (EARLIER)`), QRESYNC only.
    pub vanished: UidSet,
    /// Messages whose flags changed since the given mod-sequence, QRESYNC only.
    pub changed: Vec<FlagChange>,
}

/// Flags of one message, from a flags fetch, a CHANGEDSINCE fetch or a QRESYNC select.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagChange {
    /// Message UID.
    pub uid: u32,
    /// System flags and well-known keywords (`$Forwarded`, `$Junk`, `$NotJunk`).
    pub flags: Flags,
    /// Other keywords (tags), as sent.
    pub keywords: Vec<String>,
    /// The message's mod-sequence, with CONDSTORE.
    pub modseq: Option<u64>,
    /// Gmail labels, when connected to Gmail.
    pub gmail_labels: Option<Vec<GmailLabel>>,
}

/// Result of [`ImapSession::fetch_changes`](crate::ImapSession::fetch_changes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    /// Messages changed since the given mod-sequence.
    pub changed: Vec<FlagChange>,
    /// UIDs expunged since the given mod-sequence. Only filled with QRESYNC enabled;
    /// otherwise diff [`uid_search_all`](crate::ImapSession::uid_search_all) results.
    pub vanished: UidSet,
}

/// Header-level data of one message, for the message list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderFetch {
    /// Message UID.
    pub uid: u32,
    /// System flags and well-known keywords.
    pub flags: Flags,
    /// Other keywords (tags).
    pub keywords: Vec<String>,
    /// `RFC822.SIZE`.
    pub size: u32,
    /// `INTERNALDATE` (arrival time at the server).
    pub internal_date: Option<OffsetDateTime>,
    /// Mod-sequence, with CONDSTORE.
    pub modseq: Option<u64>,
    /// The raw RFC 5322 header block (`BODY.PEEK[HEADER]`), including the blank line.
    pub header: Sensitive<Bytes>,
    /// MIME structure summary (`BODYSTRUCTURE`).
    pub body: Option<BodySummary>,
    /// Gmail message id, thread id and labels.
    pub gmail: Option<GmailMeta>,
}

/// Gmail-specific message metadata (`X-GM-MSGID`, `X-GM-THRID`, `X-GM-LABELS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmailMeta {
    /// Stable message id across all labels.
    pub msg_id: u64,
    /// Gmail thread id.
    pub thread_id: u64,
    /// Labels, including system labels such as `\Inbox`.
    pub labels: Vec<GmailLabel>,
}

/// Flattened summary of a message's MIME tree: the leaf parts with their IMAP
/// section numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BodySummary {
    /// Leaf parts in document order. `message/rfc822` parts are leaves (not descended).
    pub parts: Vec<BodyPart>,
}

impl BodySummary {
    /// Displayable text parts (`text/plain`, `text/html`) that are not attachments.
    pub fn text_parts(&self) -> impl Iterator<Item = &BodyPart> {
        self.parts.iter().filter(|p| {
            !p.is_attachment && p.mime_type == "text" && matches!(&*p.subtype, "plain" | "html")
        })
    }

    /// Whether any part is an attachment.
    pub fn has_attachments(&self) -> bool {
        self.parts.iter().any(|p| p.is_attachment)
    }
}

/// One leaf of a [`BodySummary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyPart {
    /// IMAP section, e.g. `1`, `1.2`, `2.1.3`; pass to
    /// [`fetch_part`](crate::ImapSession::fetch_part).
    pub section: String,
    /// Lower-case top-level type, e.g. `text`.
    pub mime_type: String,
    /// Lower-case subtype, e.g. `plain`.
    pub subtype: String,
    /// `charset` parameter, if any.
    pub charset: Option<String>,
    /// Lower-case `Content-Transfer-Encoding`, e.g. `base64`.
    pub encoding: String,
    /// Encoded size in octets.
    pub size: u32,
    /// Lower-case disposition (`inline`, `attachment`), if any.
    pub disposition: Option<String>,
    /// File name from the disposition or the `name` parameter.
    pub filename: Option<Sensitive<String>>,
    /// `Content-ID`, if any.
    pub content_id: Option<String>,
    /// Heuristic: an attachment rather than displayable body content.
    pub is_attachment: bool,
}

/// How [`ImapSession::store_flags`](crate::ImapSession::store_flags) applies flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreMode {
    /// Replace all flags (`FLAGS`). Idempotent, preferred for replaying pending ops.
    Set,
    /// Add flags (`+FLAGS`).
    Add,
    /// Remove flags (`-FLAGS`).
    Remove,
}

impl StoreMode {
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Set => "",
            Self::Add => "+",
            Self::Remove => "-",
        }
    }
}

/// A flag change for `UID STORE`: system flags plus keywords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagOp {
    /// Set, add or remove.
    pub mode: StoreMode,
    /// System flags and well-known keywords.
    pub flags: Flags,
    /// Other keywords (IMAP atoms, e.g. `$label1`, `Work`).
    pub keywords: Vec<String>,
}

impl FlagOp {
    /// Replace the message's flags with `flags`.
    pub fn set(flags: Flags) -> Self {
        Self {
            mode: StoreMode::Set,
            flags,
            keywords: Vec::new(),
        }
    }

    /// Add `flags`.
    pub fn add(flags: Flags) -> Self {
        Self {
            mode: StoreMode::Add,
            flags,
            keywords: Vec::new(),
        }
    }

    /// Remove `flags`.
    pub fn remove(flags: Flags) -> Self {
        Self {
            mode: StoreMode::Remove,
            flags,
            keywords: Vec::new(),
        }
    }

    /// Also apply these keywords.
    #[must_use]
    pub fn with_keywords<I, S>(mut self, keywords: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.keywords.extend(keywords.into_iter().map(Into::into));
        self
    }

    /// All IMAP names this op sends, in a stable order.
    pub fn imap_names(&self) -> Vec<String> {
        self.flags
            .to_imap()
            .into_iter()
            .map(str::to_owned)
            .chain(self.keywords.iter().cloned())
            .collect()
    }
}

/// UIDs assigned by a `COPY`/`MOVE` (`COPYUID`, RFC 4315).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyUid {
    /// UIDVALIDITY of the destination folder.
    pub uidvalidity: u32,
    /// Source UIDs, ascending.
    pub source: Vec<u32>,
    /// Destination UIDs, in the same order as `source`.
    pub destination: Vec<u32>,
}

impl CopyUid {
    /// `(source, destination)` pairs.
    pub fn pairs(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.source
            .iter()
            .copied()
            .zip(self.destination.iter().copied())
    }

    /// The destination UID of a source UID.
    pub fn destination_of(&self, source_uid: u32) -> Option<u32> {
        self.pairs().find(|(s, _)| *s == source_uid).map(|(_, d)| d)
    }
}

/// The UID assigned by `APPEND` (`APPENDUID`, RFC 4315).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendUid {
    /// UIDVALIDITY of the destination folder.
    pub uidvalidity: u32,
    /// UID of the new message.
    pub uid: u32,
}

/// The result of `STATUS`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderStatus {
    /// Number of messages.
    pub messages: Option<u32>,
    /// Number of messages without `\Seen`.
    pub unseen: Option<u32>,
    /// Next UID to be assigned.
    pub uidnext: Option<u32>,
    /// UIDVALIDITY.
    pub uidvalidity: Option<u32>,
    /// HIGHESTMODSEQ, with CONDSTORE.
    pub highest_modseq: Option<u64>,
}

/// How [`ImapSession::idle`](crate::ImapSession::idle) ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleOutcome {
    /// The server reported changes; see [`ImapSession::take_events`](crate::ImapSession::take_events).
    NewData,
    /// The timeout elapsed without changes. Re-issue IDLE (RFC 2177 recommends ≤ 29 min).
    Timeout,
    /// The cancellation token fired.
    Cancelled,
}

/// Unsolicited server data, collected while commands run and while idling.
///
/// Refers to the selected folder at the time it was received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// `EXISTS`: the folder now has this many messages.
    Exists(u32),
    /// `RECENT`: this many messages have `\Recent`.
    Recent(u32),
    /// `EXPUNGE`: the message with this sequence number was removed (later sequence
    /// numbers shift down by one).
    Expunge(u32),
    /// `VANISHED`: these UIDs were removed (QRESYNC).
    Vanished(UidSet),
    /// A `FETCH` with flags, e.g. another client changed flags.
    Flags {
        /// Sequence number.
        seq: u32,
        /// UID, when the server included it (always with CONDSTORE/QRESYNC enabled).
        uid: Option<u32>,
        /// System flags and well-known keywords.
        flags: Flags,
        /// Other keywords.
        keywords: Vec<String>,
        /// Mod-sequence, with CONDSTORE.
        modseq: Option<u64>,
    },
    /// An `[ALERT]` the user must see (RFC 3501 §7.1).
    Alert(String),
    /// Too many events piled up without being taken; some were dropped. Do a full
    /// resync of the selected folder.
    Overflow,
}
