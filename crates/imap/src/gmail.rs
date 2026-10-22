//! Gmail IMAP extensions (`X-GM-EXT-1`): labels, message ids, thread ids.
//!
//! Maze syncs `[Gmail]/All Mail`, Trash and Spam only (ADR-007) and turns each
//! message's `X-GM-LABELS` into folder memberships. This module parses and encodes
//! labels; [`ImapSession::store_gmail_labels`](crate::ImapSession::store_gmail_labels)
//! changes them.

use std::fmt;

use mazemail_types::FolderRole;

use crate::types::{Caps, StoreMode};
use crate::utf7;

/// Whether the server is Gmail (advertises `X-GM-EXT-1`).
pub fn is_gmail(caps: &Caps) -> bool {
    caps.gmail()
}

/// One Gmail label from `X-GM-LABELS`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GmailLabel {
    /// `\Inbox`.
    Inbox,
    /// `\Sent`.
    Sent,
    /// `\Draft`.
    Draft,
    /// `\Starred`.
    Starred,
    /// `\Important`.
    Important,
    /// `\Spam`.
    Spam,
    /// `\Trash`.
    Trash,
    /// Another system label (`\Something`), stored without the backslash.
    System(String),
    /// A user label, decoded from modified UTF-7, e.g. `Work/Projects`.
    User(String),
}

impl GmailLabel {
    /// Parse a label as delivered in `X-GM-LABELS` (already unquoted).
    ///
    /// Labels starting with `\` are system labels; anything else is a user label in
    /// modified UTF-7. Names that are not valid modified UTF-7 are kept verbatim.
    pub fn parse(raw: &str) -> Self {
        if let Some(system) = raw.strip_prefix('\\') {
            match system.to_ascii_lowercase().as_str() {
                "inbox" => Self::Inbox,
                "sent" => Self::Sent,
                "draft" | "drafts" => Self::Draft,
                "starred" => Self::Starred,
                "important" => Self::Important,
                "spam" => Self::Spam,
                "trash" => Self::Trash,
                _ => Self::System(system.to_owned()),
            }
        } else {
            Self::User(utf7::decode_lossy(raw))
        }
    }

    /// The folder role a system label stands for.
    pub fn role(&self) -> Option<FolderRole> {
        Some(match self {
            Self::Inbox => FolderRole::Inbox,
            Self::Sent => FolderRole::Sent,
            Self::Draft => FolderRole::Drafts,
            Self::Starred => FolderRole::Flagged,
            Self::Important => FolderRole::Important,
            Self::Spam => FolderRole::Junk,
            Self::Trash => FolderRole::Trash,
            Self::System(_) | Self::User(_) => return None,
        })
    }

    /// The system label for a folder role, if Gmail has one.
    pub fn for_role(role: FolderRole) -> Option<Self> {
        Some(match role {
            FolderRole::Inbox => Self::Inbox,
            FolderRole::Sent => Self::Sent,
            FolderRole::Drafts => Self::Draft,
            FolderRole::Flagged => Self::Starred,
            FolderRole::Important => Self::Important,
            FolderRole::Junk => Self::Spam,
            FolderRole::Trash => Self::Trash,
            FolderRole::Archive | FolderRole::All => return None,
        })
    }

    /// Whether this is a system label (starts with `\` on the wire).
    pub fn is_system(&self) -> bool {
        !matches!(self, Self::User(_))
    }

    /// The label as it goes into a `STORE … X-GM-LABELS (…)` list: system labels as
    /// atoms (`\Inbox`), user labels as quoted modified UTF-7 strings.
    pub fn to_wire(&self) -> String {
        match self {
            Self::User(name) => quote(&utf7::encode(name)),
            other => format!("\\{}", other.system_name()),
        }
    }

    fn system_name(&self) -> &str {
        match self {
            Self::Inbox => "Inbox",
            Self::Sent => "Sent",
            Self::Draft => "Draft",
            Self::Starred => "Starred",
            Self::Important => "Important",
            Self::Spam => "Spam",
            Self::Trash => "Trash",
            Self::System(name) | Self::User(name) => name,
        }
    }
}

impl fmt::Display for GmailLabel {
    /// System labels print with their backslash (`\Inbox`), user labels as decoded names.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(name) => f.write_str(name),
            other => write!(f, "\\{}", other.system_name()),
        }
    }
}

/// A label change for [`ImapSession::store_gmail_labels`](crate::ImapSession::store_gmail_labels).
///
/// Gmail semantics (PLAN §4.5): archive = remove `\Inbox`; move to a label = add the
/// target label and remove the source label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelOp {
    /// Set, add or remove.
    pub mode: StoreMode,
    /// The labels.
    pub labels: Vec<GmailLabel>,
}

impl LabelOp {
    /// Add labels.
    pub fn add(labels: impl IntoIterator<Item = GmailLabel>) -> Self {
        Self {
            mode: StoreMode::Add,
            labels: labels.into_iter().collect(),
        }
    }

    /// Remove labels.
    pub fn remove(labels: impl IntoIterator<Item = GmailLabel>) -> Self {
        Self {
            mode: StoreMode::Remove,
            labels: labels.into_iter().collect(),
        }
    }

    /// Archive: remove `\Inbox`.
    pub fn archive() -> Self {
        Self::remove([GmailLabel::Inbox])
    }

    /// The parenthesized wire list, e.g. `(\Inbox "Work")`.
    pub(crate) fn wire_list(&self) -> String {
        let items: Vec<String> = self.labels.iter().map(GmailLabel::to_wire).collect();
        format!("({})", items.join(" "))
    }
}

/// Quote a string for IMAP (`"…"` with `\` and `"` escaped). The input must not
/// contain CR or LF; callers validate or encode first.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_system_and_user_labels() {
        assert_eq!(GmailLabel::parse("\\Inbox"), GmailLabel::Inbox);
        assert_eq!(GmailLabel::parse("\\Important"), GmailLabel::Important);
        assert_eq!(GmailLabel::parse("\\Sent"), GmailLabel::Sent);
        assert_eq!(GmailLabel::parse("\\Draft"), GmailLabel::Draft);
        assert_eq!(GmailLabel::parse("\\Starred"), GmailLabel::Starred);
        assert_eq!(
            GmailLabel::parse("\\Muted"),
            GmailLabel::System("Muted".into())
        );
        assert_eq!(
            GmailLabel::parse("Work/Projects"),
            GmailLabel::User("Work/Projects".into())
        );
        // Modified UTF-7 user labels are decoded.
        assert_eq!(
            GmailLabel::parse("&AMQ-pfel &- Birnen"),
            GmailLabel::User("Äpfel & Birnen".into())
        );
        // Invalid modified UTF-7 stays verbatim.
        assert_eq!(GmailLabel::parse("a&b"), GmailLabel::User("a&b".into()));
    }

    #[test]
    fn roles_round_trip() {
        for label in [
            GmailLabel::Inbox,
            GmailLabel::Sent,
            GmailLabel::Draft,
            GmailLabel::Starred,
            GmailLabel::Important,
            GmailLabel::Spam,
            GmailLabel::Trash,
        ] {
            let role = label.role().unwrap();
            assert_eq!(GmailLabel::for_role(role), Some(label));
        }
        assert_eq!(GmailLabel::User("x".into()).role(), None);
        assert_eq!(GmailLabel::for_role(FolderRole::All), None);
    }

    #[test]
    fn wire_encoding() {
        assert_eq!(GmailLabel::Inbox.to_wire(), "\\Inbox");
        assert_eq!(GmailLabel::System("Muted".into()).to_wire(), "\\Muted");
        assert_eq!(
            GmailLabel::User("Äpfel \"bio\"".into()).to_wire(),
            "\"&AMQ-pfel \\\"bio\\\"\""
        );
        let op = LabelOp::add([GmailLabel::Inbox, GmailLabel::User("Work".into())]);
        assert_eq!(op.wire_list(), "(\\Inbox \"Work\")");
        assert_eq!(LabelOp::archive().mode, StoreMode::Remove);
    }

    #[test]
    fn display() {
        assert_eq!(GmailLabel::Inbox.to_string(), "\\Inbox");
        assert_eq!(GmailLabel::User("Work".into()).to_string(), "Work");
    }
}
