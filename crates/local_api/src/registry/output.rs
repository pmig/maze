//! Output types: the `data` of a command's final response, and what `--json` prints.
//!
//! `mazemail_core` builds these; their JSON Schemas document the output shapes in
//! AGENTS.md. Within a protocol version fields are only ever added. Ids are always the
//! short public form (`m_…`, `t_…`, `d_…`, `f_…`, `a_…`).

use std::path::PathBuf;

use mazemail_types::Sensitive;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use super::args::ShowFormat;
use crate::permission::{Access, Permission};
use crate::token::{Profile, TokenId, TokenInfo, TokenScope};

/// `status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StatusOutput {
    /// Always `true` (otherwise the CLI exits with code 5).
    pub running: bool,
    /// Maze's version.
    pub version: String,
    /// Protocol major version.
    pub protocol: u32,
    /// Whether the request carried a valid token.
    pub signed_in: bool,
    /// The token, when signed in.
    pub token: Option<TokenInfo>,
    /// Why a presented token was not accepted ("the session token has expired").
    pub token_error: Option<String>,
    /// Details from the app (accounts, sync state; only when signed in).
    pub app: Value,
}

/// `help`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HelpOutput {
    /// Every command.
    pub commands: Vec<HelpEntry>,
}

/// One command in `help`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HelpEntry {
    /// As typed after `mazemail` (`draft new`).
    pub command: String,
    /// One-line summary.
    pub summary: String,
    /// Permission class.
    pub permission: Permission,
    /// Whether the command needs a token.
    pub requires_token: bool,
    /// What happens with the caller's token (`null` without a token).
    pub access: Option<Access>,
}

/// `login`, `token create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoginOutput {
    /// The new token. Shown once; Maze keeps only a hash.
    pub token: String,
    /// `export MAZEMAIL_SESSION=…`, for `eval "$(mazemail login)"`.
    pub export: String,
    /// The token's public id.
    pub id: TokenId,
    /// The approved profile.
    pub profile: Profile,
    /// The approved scope (`null` = all).
    pub scope: TokenScope,
    /// Whether `send` is enabled (always asks).
    pub allow_send: bool,
    /// Expiry (RFC 3339).
    #[serde(with = "time::serde::rfc3339::option")]
    #[schemars(with = "Option<String>")]
    pub expires_at: Option<OffsetDateTime>,
}

/// `token revoke`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RevokeOutput {
    /// The token's id.
    pub id: TokenId,
    /// `false` if it was already revoked.
    pub revoked: bool,
}

/// One `search` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchHit {
    /// Message id.
    pub id: String,
    /// Thread id.
    pub thread: String,
    /// Account id.
    pub account: String,
    /// Folder id.
    pub folder: Option<String>,
    /// Date (RFC 3339).
    #[serde(with = "time::serde::rfc3339")]
    #[schemars(with = "String")]
    pub date: OffsetDateTime,
    /// Sender ("Name <address>").
    #[schemars(with = "String")]
    pub from: Sensitive<String>,
    /// Subject.
    #[schemars(with = "String")]
    pub subject: Sensitive<String>,
    /// Start of the body.
    #[schemars(with = "Option<String>")]
    pub snippet: Option<Sensitive<String>>,
    /// Unread.
    pub unread: bool,
    /// Flagged.
    pub flagged: bool,
    /// Tag names.
    pub tags: Vec<String>,
    /// Has attachments.
    pub has_attachments: bool,
}

/// `show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ShowOutput {
    /// The requested message or thread id.
    pub id: String,
    /// Thread id.
    pub thread: String,
    /// Format of `content`.
    pub format: ShowFormat,
    /// The message or whole thread rendered in `format`.
    #[schemars(with = "String")]
    pub content: Sensitive<String>,
    /// The messages shown, oldest first.
    pub messages: Vec<MessageMeta>,
}

/// Metadata of one message in `show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MessageMeta {
    /// Message id.
    pub id: String,
    /// Sender.
    #[schemars(with = "String")]
    pub from: Sensitive<String>,
    /// Recipients.
    #[schemars(with = "Vec<String>")]
    pub to: Vec<Sensitive<String>>,
    /// Date (RFC 3339).
    #[serde(with = "time::serde::rfc3339")]
    #[schemars(with = "String")]
    pub date: OffsetDateTime,
    /// Subject.
    #[schemars(with = "String")]
    pub subject: Sensitive<String>,
    /// Attachments.
    pub attachments: Vec<AttachmentMeta>,
}

/// An attachment listed by `show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentMeta {
    /// Attachment id, for `mazemail attachment save`.
    pub id: String,
    /// File name.
    #[schemars(with = "String")]
    pub filename: Sensitive<String>,
    /// MIME type.
    pub content_type: String,
    /// Size in bytes.
    pub size: u64,
}

/// One folder in `folders`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FolderInfo {
    /// Folder id.
    pub id: String,
    /// Account id.
    pub account: String,
    /// Display name.
    pub name: String,
    /// Full path (`Archive/2026`).
    pub path: String,
    /// Special-use role (`inbox`, `sent`, `drafts`, `trash`, `junk`, `archive`, …).
    pub role: Option<String>,
    /// Unread messages.
    pub unread: u64,
    /// All messages.
    pub total: u64,
}

/// One tag in `tags`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TagInfo {
    /// Tag id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Color (`#rrggbb`).
    pub color: Option<String>,
}

/// One contact in `contacts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContactInfo {
    /// Display name.
    #[schemars(with = "Option<String>")]
    pub name: Option<Sensitive<String>>,
    /// Address.
    #[schemars(with = "String")]
    pub email: Sensitive<String>,
}

/// One identity in `identities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IdentityInfo {
    /// Identity id.
    pub id: String,
    /// Account id.
    pub account: String,
    /// Display name.
    #[schemars(with = "String")]
    pub name: Sensitive<String>,
    /// Address.
    #[schemars(with = "String")]
    pub email: Sensitive<String>,
    /// The account's default identity.
    pub default: bool,
}

/// What Maze returns for `attachment.save`; the CLI writes the file and prints
/// [`AttachmentSaved`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentData {
    /// Attachment id.
    pub id: String,
    /// File name as sent (the CLI sanitizes it).
    #[schemars(with = "String")]
    pub filename: Sensitive<String>,
    /// MIME type.
    pub content_type: String,
    /// Size in bytes.
    pub size: u64,
    /// Content, standard base64.
    #[schemars(with = "String")]
    pub data: Sensitive<String>,
}

/// `attachment save`, as the CLI prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentSaved {
    /// Attachment id.
    pub id: String,
    /// Where the file was written.
    pub path: PathBuf,
    /// MIME type.
    pub content_type: String,
    /// Size in bytes.
    pub size: u64,
}

/// `draft new`, `draft edit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DraftOutput {
    /// Draft id.
    pub id: String,
    /// The identity it is written from.
    pub identity: Option<String>,
    /// Recipients.
    #[schemars(with = "Vec<String>")]
    pub to: Vec<Sensitive<String>>,
    /// Subject.
    #[schemars(with = "String")]
    pub subject: Sensitive<String>,
}

/// How a state-changing command was handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MutationOutcome {
    /// Done.
    Applied,
    /// Waiting for review in Maze.
    Proposed,
}

/// `archive`, `move`, `tag`, `flag`, `read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MutationOutput {
    /// Applied directly, or a proposal.
    pub outcome: MutationOutcome,
    /// The proposal's id, if proposed.
    pub proposal: Option<String>,
    /// Message ids affected.
    pub affected: Vec<String>,
}

/// `propose`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProposalOutput {
    /// The proposal's id.
    pub proposal: String,
    /// Number of items.
    pub items: u32,
}

/// `rule add`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuleOutput {
    /// Rule id.
    pub id: String,
    /// Name.
    pub name: String,
}

/// `send`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SendOutput {
    /// The sent draft.
    pub draft: String,
    /// Outbox item id (sending happens in the background).
    pub outbox: String,
}

/// `export`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExportOutput {
    /// Where the export was written.
    pub path: PathBuf,
    /// Messages exported.
    pub messages: u64,
}

/// `import`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ImportOutput {
    /// Messages imported.
    pub messages: u64,
    /// Folders created.
    pub folders: u64,
}
