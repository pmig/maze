//! Argument types: the `clap` structs the CLI parses and the `serde` types sent as
//! `args` on the wire.
//!
//! Most commands use one struct for both. Commands that read local files (`draft`,
//! `propose`) or write them (`attachment save`) have a separate `…CliArgs` struct:
//! the CLI reads and writes those files itself and sends contents, because Maze runs
//! outside the agent sandbox and must never open paths on a sandboxed caller's behalf.
//! Fields that can hold mail data (queries, addresses, subjects, bodies) are
//! [`Sensitive`] so they never show up in `Debug` output.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use clap::{ArgGroup, Args, ValueEnum};
use mazemail_types::{AccountId, FolderId, MicrosoftTenant, PublicId as _, Sensitive};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::pubid;
use crate::token::{Profile, TokenId, Ttl};

/// Default `--limit` for `search`.
pub const DEFAULT_SEARCH_LIMIT: u32 = 50;
/// Default `--limit` for `contacts`.
pub const DEFAULT_CONTACTS_LIMIT: u32 = 20;

fn default_search_limit() -> u32 {
    DEFAULT_SEARCH_LIMIT
}

fn default_contacts_limit() -> u32 {
    DEFAULT_CONTACTS_LIMIT
}

fn default_profile() -> Profile {
    Profile::ReadOnly
}

fn yes() -> bool {
    true
}

/// `mazemail status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusArgs {}

/// `mazemail help`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelpArgs {
    /// A command (`draft new`) or `agents` for the AGENTS.md text.
    #[arg(value_name = "TOPIC")]
    #[serde(default)]
    pub topic: Vec<String>,
}

/// `mazemail login` and `mazemail token create`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoginArgs {
    /// Profile of the new token.
    #[arg(long, value_enum, default_value_t = Profile::ReadOnly)]
    #[serde(default = "default_profile")]
    pub profile: Profile,
    /// How long the token is valid: 30m, 8h, 7d, 1h30m (at most 30d).
    #[arg(long, default_value_t)]
    #[serde(default)]
    pub ttl: Ttl,
    /// A label shown in Settings and the audit log.
    #[arg(long)]
    #[serde(default)]
    pub label: Option<String>,
    /// Limit the token to this account (repeatable; default: every account whose AI
    /// policy allows agents).
    #[arg(long = "account", value_name = "ACCOUNT")]
    #[serde(default, with = "pubid::vec")]
    #[schemars(with = "Vec<String>")]
    pub accounts: Vec<AccountId>,
    /// Limit the token to this folder (repeatable; default: all folders).
    #[arg(long = "folder", value_name = "FOLDER")]
    #[serde(default, with = "pubid::vec")]
    #[schemars(with = "Vec<String>")]
    pub folders: Vec<FolderId>,
    /// Enable `mazemail send` for the token (every send still asks).
    #[arg(long)]
    #[serde(default)]
    pub allow_send: bool,
}

impl Default for LoginArgs {
    fn default() -> Self {
        Self {
            profile: Profile::ReadOnly,
            ttl: Ttl::default(),
            label: None,
            accounts: Vec::new(),
            folders: Vec::new(),
            allow_send: false,
        }
    }
}

/// `mazemail token list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenListArgs {}

/// `mazemail token revoke`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenRevokeArgs {
    /// The token's id (`tk_…`, from `mazemail token list`).
    #[arg(value_name = "TOKEN_ID")]
    pub id: TokenId,
}

/// `mazemail search`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    /// The query: words plus `from:`, `to:`, `subject:`, `has:attachment`, `is:unread`,
    /// `tag:`, `in:`, `list:`, `after:`, `before:`, `older_than:`.
    #[arg(value_name = "QUERY")]
    #[schemars(with = "String")]
    pub query: Sensitive<String>,
    /// Maximum number of results.
    #[arg(long, default_value_t = DEFAULT_SEARCH_LIMIT)]
    #[serde(default = "default_search_limit")]
    pub limit: u32,
    /// Print only message ids, one per line (for `$(…)`).
    #[arg(long)]
    #[serde(skip)]
    pub ids: bool,
}

/// Output formats of `show`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, ValueEnum, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ShowFormat {
    /// Markdown with headers, quotes and links (default).
    #[default]
    Md,
    /// Plain text.
    Text,
    /// The raw RFC 5322 message.
    Raw,
}

/// `mazemail show`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShowArgs {
    /// A message (`m_…`) or thread (`t_…`) id.
    #[arg(value_name = "ID")]
    pub id: String,
    /// Output format.
    #[arg(long, value_enum, default_value_t)]
    #[serde(default)]
    pub format: ShowFormat,
}

/// `mazemail folders`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FoldersArgs {
    /// Only this account's folders.
    #[arg(long, value_name = "ACCOUNT")]
    #[serde(default, with = "pubid::option")]
    #[schemars(with = "Option<String>")]
    pub account: Option<AccountId>,
}

/// `mazemail tags`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TagsArgs {}

/// `mazemail contacts`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContactsArgs {
    /// Part of a name or address.
    #[arg(value_name = "QUERY")]
    #[schemars(with = "String")]
    pub query: Sensitive<String>,
    /// Maximum number of results.
    #[arg(long, default_value_t = DEFAULT_CONTACTS_LIMIT)]
    #[serde(default = "default_contacts_limit")]
    pub limit: u32,
}

/// `mazemail identities`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IdentitiesArgs {
    /// Only this account's identities.
    #[arg(long, value_name = "ACCOUNT")]
    #[serde(default, with = "pubid::option")]
    #[schemars(with = "Option<String>")]
    pub account: Option<AccountId>,
}

/// `mazemail attachment save` as typed on the command line.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct AttachmentSaveCliArgs {
    /// The attachment's id (from `mazemail show`).
    #[arg(value_name = "ID")]
    pub id: String,
    /// Directory to save into (default: the current directory).
    #[arg(long, value_name = "DIR")]
    pub to: Option<PathBuf>,
}

/// `attachment.save` on the wire. Maze returns the content and the CLI writes the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachmentSaveArgs {
    /// The attachment's id.
    pub id: String,
}

/// A file attached to a draft, sent inline (base64).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachmentUpload {
    /// File name.
    #[schemars(with = "String")]
    pub name: Sensitive<String>,
    /// MIME type, if known; Maze sniffs it otherwise.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Content, standard base64.
    #[schemars(with = "String")]
    pub data: Sensitive<String>,
}

/// `mazemail draft new` as typed on the command line.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct DraftNewCliArgs {
    /// Reply to this message (`m_…`).
    #[arg(long, value_name = "ID", conflicts_with = "forward")]
    pub reply_to: Option<String>,
    /// Reply to all recipients (with --reply-to).
    #[arg(long, requires = "reply_to")]
    pub reply_all: bool,
    /// Forward this message (`m_…`).
    #[arg(long, value_name = "ID")]
    pub forward: Option<String>,
    /// Identity to write from; picked automatically from the recipients and the
    /// replied-to message when omitted.
    #[arg(long, value_name = "IDENTITY")]
    pub identity: Option<String>,
    /// Recipient (repeatable).
    #[arg(long, value_name = "ADDRESS")]
    pub to: Vec<Sensitive<String>>,
    /// Cc recipient (repeatable).
    #[arg(long, value_name = "ADDRESS")]
    pub cc: Vec<Sensitive<String>>,
    /// Bcc recipient (repeatable).
    #[arg(long, value_name = "ADDRESS")]
    pub bcc: Vec<Sensitive<String>>,
    /// Subject (default for replies: "Re: …").
    #[arg(long)]
    pub subject: Option<Sensitive<String>>,
    /// Read the body (Markdown) from a file, `-` for stdin.
    #[arg(long, value_name = "FILE", conflicts_with = "body")]
    pub body_file: Option<PathBuf>,
    /// The body (Markdown).
    #[arg(long)]
    pub body: Option<Sensitive<String>>,
    /// Attach a file (repeatable).
    #[arg(long, value_name = "FILE")]
    pub attach: Vec<PathBuf>,
}

/// `draft.new` on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftNewArgs {
    /// Message id to reply to.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Reply to all recipients.
    #[serde(default)]
    pub reply_all: bool,
    /// Message id to forward.
    #[serde(default)]
    pub forward: Option<String>,
    /// Identity name, address or id; automatic when absent.
    #[serde(default)]
    pub identity: Option<String>,
    /// Recipients.
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub to: Vec<Sensitive<String>>,
    /// Cc recipients.
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub cc: Vec<Sensitive<String>>,
    /// Bcc recipients.
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub bcc: Vec<Sensitive<String>>,
    /// Subject.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub subject: Option<Sensitive<String>>,
    /// Body (Markdown).
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub body: Option<Sensitive<String>>,
    /// Attachments.
    #[serde(default)]
    pub attachments: Vec<AttachmentUpload>,
}

/// `mazemail draft edit` as typed on the command line.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct DraftEditCliArgs {
    /// The draft's id (`d_…`).
    #[arg(value_name = "ID")]
    pub id: String,
    /// Identity to write from.
    #[arg(long, value_name = "IDENTITY")]
    pub identity: Option<String>,
    /// Replace the recipients (repeatable).
    #[arg(long, value_name = "ADDRESS")]
    pub to: Vec<Sensitive<String>>,
    /// Replace the Cc recipients (repeatable).
    #[arg(long, value_name = "ADDRESS")]
    pub cc: Vec<Sensitive<String>>,
    /// Replace the Bcc recipients (repeatable).
    #[arg(long, value_name = "ADDRESS")]
    pub bcc: Vec<Sensitive<String>>,
    /// Replace the subject.
    #[arg(long)]
    pub subject: Option<Sensitive<String>>,
    /// Replace the body with a file's content (Markdown), `-` for stdin.
    #[arg(long, value_name = "FILE", conflicts_with = "body")]
    pub body_file: Option<PathBuf>,
    /// Replace the body (Markdown).
    #[arg(long)]
    pub body: Option<Sensitive<String>>,
    /// Add an attachment (repeatable).
    #[arg(long, value_name = "FILE")]
    pub attach: Vec<PathBuf>,
}

/// `draft.edit` on the wire. Absent fields stay unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DraftEditArgs {
    /// The draft's id.
    pub id: String,
    /// New identity.
    #[serde(default)]
    pub identity: Option<String>,
    /// New recipients.
    #[serde(default)]
    #[schemars(with = "Option<Vec<String>>")]
    pub to: Option<Vec<Sensitive<String>>>,
    /// New Cc recipients.
    #[serde(default)]
    #[schemars(with = "Option<Vec<String>>")]
    pub cc: Option<Vec<Sensitive<String>>>,
    /// New Bcc recipients.
    #[serde(default)]
    #[schemars(with = "Option<Vec<String>>")]
    pub bcc: Option<Vec<Sensitive<String>>>,
    /// New subject.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub subject: Option<Sensitive<String>>,
    /// New body (Markdown).
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub body: Option<Sensitive<String>>,
    /// Attachments to add.
    #[serde(default)]
    pub attachments: Vec<AttachmentUpload>,
}

/// `mazemail archive`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArchiveArgs {
    /// Message (`m_…`) or thread (`t_…`) ids.
    #[arg(value_name = "ID", required = true)]
    pub ids: Vec<String>,
}

/// `mazemail move`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoveArgs {
    /// Target folder id (`f_…`, from `mazemail folders`).
    #[arg(long, value_name = "FOLDER")]
    #[serde(with = "pubid::one")]
    #[schemars(with = "String")]
    pub to: FolderId,
    /// Message (`m_…`) or thread (`t_…`) ids.
    #[arg(value_name = "ID", required = true)]
    pub ids: Vec<String>,
}

/// `mazemail tag`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[command(group(ArgGroup::new("change").required(true).multiple(true).args(["add", "remove"])))]
#[serde(deny_unknown_fields)]
pub struct TagArgs {
    /// Tag to add (repeatable).
    #[arg(long, value_name = "TAG")]
    #[serde(default)]
    pub add: Vec<String>,
    /// Tag to remove (repeatable).
    #[arg(long, value_name = "TAG")]
    #[serde(default)]
    pub remove: Vec<String>,
    /// Message (`m_…`) or thread (`t_…`) ids.
    #[arg(value_name = "ID", required = true)]
    pub ids: Vec<String>,
}

/// `mazemail flag`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlagArgs {
    /// Remove the flag instead.
    #[arg(long)]
    #[serde(default)]
    pub clear: bool,
    /// Message (`m_…`) or thread (`t_…`) ids.
    #[arg(value_name = "ID", required = true)]
    pub ids: Vec<String>,
}

/// `mazemail read`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    /// Mark as unread instead.
    #[arg(long)]
    #[serde(default)]
    pub unread: bool,
    /// Message (`m_…`) or thread (`t_…`) ids.
    #[arg(value_name = "ID", required = true)]
    pub ids: Vec<String>,
}

/// `mazemail propose` as typed on the command line.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ProposeCliArgs {
    /// JSON file with the actions (an array, or `{ "title": …, "actions": [...] }`),
    /// `-` for stdin.
    #[arg(long, value_name = "FILE")]
    pub file: PathBuf,
    /// Title shown above the proposal in Maze.
    #[arg(long)]
    pub title: Option<String>,
}

/// `propose` on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposeArgs {
    /// Title shown above the proposal.
    #[serde(default)]
    pub title: Option<String>,
    /// The proposed actions, reviewed one by one in Maze.
    pub actions: Vec<ProposedAction>,
}

/// One item of a batch proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProposedAction {
    /// Archive messages or threads.
    Archive {
        /// Message or thread ids.
        ids: Vec<String>,
        /// Why, shown in the review UI.
        #[serde(default)]
        #[schemars(with = "Option<String>")]
        reason: Option<Sensitive<String>>,
    },
    /// Move messages or threads to a folder.
    Move {
        /// Message or thread ids.
        ids: Vec<String>,
        /// Target folder id.
        #[serde(with = "pubid::one")]
        #[schemars(with = "String")]
        to: FolderId,
        /// Why, shown in the review UI.
        #[serde(default)]
        #[schemars(with = "Option<String>")]
        reason: Option<Sensitive<String>>,
    },
    /// Add or remove tags.
    Tag {
        /// Message or thread ids.
        ids: Vec<String>,
        /// Tags to add.
        #[serde(default)]
        add: Vec<String>,
        /// Tags to remove.
        #[serde(default)]
        remove: Vec<String>,
        /// Why, shown in the review UI.
        #[serde(default)]
        #[schemars(with = "Option<String>")]
        reason: Option<Sensitive<String>>,
    },
    /// Set or clear the flag.
    Flag {
        /// Message or thread ids.
        ids: Vec<String>,
        /// `true` to flag, `false` to clear.
        #[serde(default = "yes")]
        flagged: bool,
        /// Why, shown in the review UI.
        #[serde(default)]
        #[schemars(with = "Option<String>")]
        reason: Option<Sensitive<String>>,
    },
    /// Mark read or unread.
    MarkRead {
        /// Message or thread ids.
        ids: Vec<String>,
        /// `true` for read, `false` for unread.
        #[serde(default = "yes")]
        read: bool,
        /// Why, shown in the review UI.
        #[serde(default)]
        #[schemars(with = "Option<String>")]
        reason: Option<Sensitive<String>>,
    },
    /// Unsubscribe from the mailing lists of these messages.
    Unsubscribe {
        /// Message or thread ids.
        ids: Vec<String>,
        /// Why, shown in the review UI.
        #[serde(default)]
        #[schemars(with = "Option<String>")]
        reason: Option<Sensitive<String>>,
    },
}

impl ProposedAction {
    /// The message or thread ids the action covers.
    pub fn ids(&self) -> &[String] {
        match self {
            Self::Archive { ids, .. }
            | Self::Move { ids, .. }
            | Self::Tag { ids, .. }
            | Self::Flag { ids, .. }
            | Self::MarkRead { ids, .. }
            | Self::Unsubscribe { ids, .. } => ids,
        }
    }
}

/// One action of a rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuleAction {
    /// Archive.
    Archive,
    /// Mark read.
    MarkRead,
    /// Flag.
    Flag,
    /// Move to a folder.
    Move {
        /// Target folder id.
        #[serde(with = "pubid::one")]
        #[schemars(with = "String")]
        to: FolderId,
    },
    /// Add a tag.
    Tag {
        /// Tag name.
        tag: String,
    },
}

impl FromStr for RuleAction {
    type Err = RuleActionError;

    /// `archive`, `read`, `flag`, `move:<folder-id>`, `tag:<name>`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (verb, value) = match s.split_once(':') {
            Some((verb, value)) => (verb, Some(value)),
            None => (s, None),
        };
        match (verb, value) {
            ("archive", None) => Ok(Self::Archive),
            ("read", None) => Ok(Self::MarkRead),
            ("flag", None) => Ok(Self::Flag),
            ("move", Some(folder)) => FolderId::parse_public(folder)
                .map(|to| Self::Move { to })
                .map_err(|_| RuleActionError),
            ("tag", Some(tag)) if !tag.is_empty() => Ok(Self::Tag {
                tag: tag.to_owned(),
            }),
            _ => Err(RuleActionError),
        }
    }
}

impl fmt::Display for RuleAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Archive => f.write_str("archive"),
            Self::MarkRead => f.write_str("read"),
            Self::Flag => f.write_str("flag"),
            Self::Move { to } => write!(f, "move:{to}"),
            Self::Tag { tag } => write!(f, "tag:{tag}"),
        }
    }
}

/// A rule action that isn't `archive`, `read`, `flag`, `move:<folder-id>` or
/// `tag:<name>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected archive, read, flag, move:<folder-id> or tag:<name>")]
pub struct RuleActionError;

/// `mazemail rule add`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuleAddArgs {
    /// Name shown in the rules list.
    #[arg(long)]
    pub name: String,
    /// Which messages the rule matches, as a search query.
    #[arg(long, value_name = "QUERY")]
    #[schemars(with = "String")]
    pub when: Sensitive<String>,
    /// What to do (repeatable): archive, read, flag, move:<folder-id>, tag:<name>.
    #[arg(long = "then", value_name = "ACTION", required = true)]
    pub actions: Vec<RuleAction>,
    /// Limit the rule to one account.
    #[arg(long, value_name = "ACCOUNT")]
    #[serde(default, with = "pubid::option")]
    #[schemars(with = "Option<String>")]
    pub account: Option<AccountId>,
    /// Also run the rule on existing messages now.
    #[arg(long)]
    #[serde(default)]
    pub apply_now: bool,
}

/// `mazemail send`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendArgs {
    /// The draft's id (`d_…`).
    #[arg(value_name = "DRAFT")]
    pub draft: String,
}

/// Export formats.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, ValueEnum, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    /// One mbox file.
    #[default]
    Mbox,
    /// A Maildir directory.
    Maildir,
}

/// `mazemail export`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportArgs {
    /// Which messages, as a search query (`in:inbox`, `tag:receipts after:2026-01-01`).
    #[arg(value_name = "QUERY")]
    #[schemars(with = "String")]
    pub query: Sensitive<String>,
    /// Where to write (relative to the current directory). The export is plaintext.
    #[arg(long, value_name = "PATH")]
    pub to: PathBuf,
    /// Format.
    #[arg(long, value_enum, default_value_t)]
    #[serde(default)]
    pub format: ExportFormat,
}

/// Import formats.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ImportFormat {
    /// An mbox file.
    Mbox,
    /// A Maildir directory.
    Maildir,
    /// A Thunderbird profile directory.
    Thunderbird,
}

/// `mazemail import`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportArgs {
    /// File or directory to import (relative to the current directory).
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
    /// Format (detected when omitted).
    #[arg(long, value_enum)]
    #[serde(default)]
    pub format: Option<ImportFormat>,
    /// Account to upload mbox and Maildir mail into (may be left out when there is only
    /// one account). Not for Thunderbird profiles, which bring their own accounts.
    #[arg(long, value_name = "ACCOUNT")]
    #[serde(default, with = "pubid::option")]
    #[schemars(with = "Option<String>")]
    pub account: Option<AccountId>,
    /// Folder of the account to upload into, as a path (`Archive/2019`); created on the
    /// server if missing. Default: a top-level folder named after the file.
    #[arg(long, value_name = "PATH")]
    #[serde(default)]
    pub folder: Option<String>,
}

/// `mazemail account list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountListArgs {}

/// Providers with a browser sign-in.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum OAuthProvider {
    /// Gmail and Google Workspace.
    Google,
    /// Outlook.com and Microsoft 365.
    Microsoft,
}

/// Transport security (IMAP or SMTP).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum SecurityArg {
    /// TLS from the first byte (IMAP port 993, SMTP 465).
    Tls,
    /// STARTTLS (IMAP port 143, SMTP 587).
    Starttls,
    /// No encryption: only for local test servers.
    None,
}

/// `mazemail account add` as sent to Maze. The CLI never takes the password as an
/// argument; it reads it from the terminal or stdin ([`AccountAddCliArgs`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountAddArgs {
    /// The account's address.
    #[schemars(with = "String")]
    pub email: Sensitive<String>,
    /// The name in the folder pane (default: the address).
    #[serde(default)]
    pub name: Option<String>,
    /// Sign in with Google or Microsoft in the browser instead of a password.
    #[serde(default)]
    pub oauth: Option<OAuthProvider>,
    /// The Microsoft tenant to sign in with: `common`, `organizations`, `consumers`, a
    /// tenant ID or a domain (default: the `auth.microsoft.tenant` setting). Only with
    /// `oauth: microsoft`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub tenant: Option<MicrosoftTenant>,
    /// IMAP host (default: detected from the address).
    #[serde(default)]
    pub host: Option<String>,
    /// IMAP port (default: 993 for TLS, 143 otherwise).
    #[serde(default)]
    pub port: Option<u16>,
    /// Transport security (default: tls).
    #[serde(default)]
    pub security: Option<SecurityArg>,
    /// Login name (default: the address).
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub username: Option<Sensitive<String>>,
    /// The password; required without `oauth`. Stored encrypted, never logged.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub password: Option<Sensitive<String>>,
    /// SMTP host for sending (default: derived from the address and the IMAP host).
    #[serde(default)]
    pub smtp_host: Option<String>,
    /// SMTP port (default: 465 for TLS, 587 for STARTTLS).
    #[serde(default)]
    pub smtp_port: Option<u16>,
    /// SMTP transport security (default: starttls, or the provider's).
    #[serde(default)]
    pub smtp_security: Option<SecurityArg>,
}

/// `mazemail account add` on the command line.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct AccountAddCliArgs {
    /// The account's address.
    #[arg(value_name = "EMAIL")]
    pub email: String,
    /// The name in the folder pane (default: the address).
    #[arg(long)]
    pub name: Option<String>,
    /// Sign in with Google or Microsoft in the browser (no password).
    #[arg(long, value_enum, conflicts_with_all = ["password_stdin", "host", "port", "security", "username", "smtp_host", "smtp_port", "smtp_security"])]
    pub oauth: Option<OAuthProvider>,
    /// With `--oauth microsoft`: the organization's tenant ID or domain, or `common`,
    /// `organizations`, `consumers` (default: the `auth.microsoft.tenant` setting).
    #[arg(long, value_name = "TENANT", requires = "oauth")]
    pub tenant: Option<MicrosoftTenant>,
    /// IMAP host (default: detected from the address).
    #[arg(long)]
    pub host: Option<String>,
    /// IMAP port (default: 993 for TLS, 143 otherwise).
    #[arg(long)]
    pub port: Option<u16>,
    /// Transport security.
    #[arg(long, value_enum)]
    pub security: Option<SecurityArg>,
    /// Login name (default: the address).
    #[arg(long)]
    pub username: Option<String>,
    /// Read the password from stdin (first line) instead of prompting on the terminal.
    #[arg(long)]
    pub password_stdin: bool,
    /// SMTP host for sending (default: derived from the address and the IMAP host).
    #[arg(long)]
    pub smtp_host: Option<String>,
    /// SMTP port (default: 465 for TLS, 587 for STARTTLS).
    #[arg(long)]
    pub smtp_port: Option<u16>,
    /// SMTP transport security.
    #[arg(long, value_enum)]
    pub smtp_security: Option<SecurityArg>,
}

/// `mazemail account remove`.
#[derive(Debug, Clone, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountRemoveArgs {
    /// The account (`a_…`).
    #[arg(value_name = "ACCOUNT")]
    #[serde(with = "pubid::one")]
    #[schemars(with = "String")]
    pub id: AccountId,
}

/// `mazemail sync`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Args, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SyncArgs {
    /// Only this account (default: every account in scope).
    #[arg(long, value_name = "ACCOUNT")]
    #[serde(default, with = "pubid::option")]
    #[schemars(with = "Option<String>")]
    pub account: Option<AccountId>,
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    #[test]
    fn rule_actions_parse_and_print() {
        for text in ["archive", "read", "flag", "move:f_b", "tag:Receipts"] {
            let action: RuleAction = text.parse().unwrap();
            assert_eq!(action.to_string(), text);
        }
        for bad in ["", "move", "move:m_b", "tag:", "delete", "archive:x"] {
            assert!(bad.parse::<RuleAction>().is_err(), "{bad}");
        }
        assert_eq!(
            serde_json::to_value(RuleAction::Move { to: FolderId(1) }).unwrap(),
            json!({ "action": "move", "to": "f_b" })
        );
    }

    #[test]
    fn proposed_actions_wire_format() {
        let actions: Vec<ProposedAction> = serde_json::from_value(json!([
            { "action": "archive", "ids": ["m_b"], "reason": "newsletter" },
            { "action": "move", "ids": ["t_c"], "to": "f_d" },
            { "action": "mark_read", "ids": ["m_e"] },
        ]))
        .unwrap();
        assert_eq!(actions[1].ids(), ["t_c"]);
        assert!(matches!(
            actions[2],
            ProposedAction::MarkRead { read: true, .. }
        ));
        // Unknown actions and fields are rejected.
        assert!(
            serde_json::from_value::<ProposedAction>(json!({ "action": "delete", "ids": [] }))
                .is_err()
        );
        assert!(
            serde_json::from_value::<ProposedAction>(
                json!({ "action": "archive", "ids": [], "permanent": true })
            )
            .is_err()
        );
    }

    #[test]
    fn login_defaults() {
        let args: LoginArgs = serde_json::from_value(json!({})).unwrap();
        assert_eq!(args, LoginArgs::default());
        let args: LoginArgs = serde_json::from_value(
            json!({ "profile": "triage", "ttl": "1h", "accounts": ["a_c"] }),
        )
        .unwrap();
        assert_eq!(args.profile, Profile::Triage);
        assert_eq!(args.accounts, vec![AccountId(2)]);
        assert_eq!(
            serde_json::to_value(&args).unwrap()["accounts"],
            json!(["a_c"])
        );
    }

    #[test]
    fn sensitive_fields_stay_out_of_debug() {
        let args = SearchArgs {
            query: Sensitive::new("from:anna@example.com".into()),
            limit: 5,
            ids: false,
        };
        assert!(!format!("{args:?}").contains("anna"));
        // `ids` is CLI-only.
        assert_eq!(
            serde_json::to_value(&args).unwrap(),
            json!({ "query": "from:anna@example.com", "limit": 5 })
        );
    }
}
