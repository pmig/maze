//! A message being written (PLAN §4.10): what the composer edits, the core stores as a
//! local draft and turns into MIME when it is sent.
//!
//! The body is Markdown. Sending produces `multipart/alternative` with the Markdown
//! source as `text/plain` and rendered HTML (see `mazemail_mime::compose`).

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Address, IdentityId, MessageId};

/// Maze's website, linked from the "Sent with Maze" line (no tracking parameters).
pub const MAZE_WEBSITE: &str = "https://mazemail.dev";

/// The line added below messages whose [`Draft::support_maze`] is set (the plain-text
/// part; the HTML part links "Maze").
pub const SENT_WITH_MAZE: &str = "Sent with Maze (https://mazemail.dev)";

/// What a draft answers or forwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum DraftOrigin {
    /// A reply to the sender.
    Reply(MessageId),
    /// A reply to everyone.
    ReplyAll(MessageId),
    /// A forward.
    Forward(MessageId),
}

impl DraftOrigin {
    /// The message replied to or forwarded.
    pub fn message(self) -> MessageId {
        match self {
            Self::Reply(m) | Self::ReplyAll(m) | Self::Forward(m) => m,
        }
    }
}

/// A file attached to a draft. The file is read when the message is sent; its content
/// then goes into the (encrypted) outbox blob.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DraftAttachment {
    /// Where the file is.
    pub path: PathBuf,
    /// The file name the recipient sees.
    pub name: String,
    /// Size in bytes when it was attached.
    pub size: u64,
}

impl fmt::Debug for DraftAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DraftAttachment")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// A message being written. `Debug` leaves out addresses and content.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    /// The sending identity; `None` until one is picked (the account's default is used).
    pub identity: Option<IdentityId>,
    /// Recipients.
    pub to: Vec<Address>,
    /// Copy recipients.
    pub cc: Vec<Address>,
    /// Blind copy recipients (never written into the message headers).
    pub bcc: Vec<Address>,
    /// The subject.
    pub subject: String,
    /// The body, Markdown.
    pub body_markdown: String,
    /// `In-Reply-To` (a Message-ID without angle brackets).
    pub in_reply_to: Option<String>,
    /// `References`, oldest first (Message-IDs without angle brackets).
    pub references: Vec<String>,
    /// Attached files.
    pub attachments: Vec<DraftAttachment>,
    /// What this draft answers or forwards.
    pub origin: Option<DraftOrigin>,
    /// The draft's random two-word name (`brave-otter`), given when it is created; its
    /// tab is titled with it. Empty for drafts from before names.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Add [`SENT_WITH_MAZE`] below the message when it is sent (the composer's "Add
    /// “Sent with Maze”" checkbox; new drafts take `compose.support_maze`). Like the
    /// configured footer, it is added at send time, never to the body you edit.
    #[serde(default = "yes")]
    pub support_maze: bool,
}

fn yes() -> bool {
    true
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            identity: None,
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: String::new(),
            body_markdown: String::new(),
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
            origin: None,
            name: String::new(),
            support_maze: true,
        }
    }
}

impl fmt::Debug for Draft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Draft")
            .field("identity", &self.identity)
            .field("to", &self.to.len())
            .field("cc", &self.cc.len())
            .field("bcc", &self.bcc.len())
            .field("attachments", &self.attachments.len())
            .field("origin", &self.origin)
            .field("support_maze", &self.support_maze)
            .finish_non_exhaustive()
    }
}

impl Draft {
    /// Every recipient (To, Cc and Bcc).
    pub fn recipients(&self) -> impl Iterator<Item = &Address> {
        self.to.iter().chain(&self.cc).chain(&self.bcc)
    }

    /// Whether nothing has been written: no recipients, subject, body or attachments
    /// (the name doesn't count).
    pub fn is_blank(&self) -> bool {
        self.recipients().next().is_none()
            && self.subject.trim().is_empty()
            && self.body_markdown.trim().is_empty()
            && self.attachments.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_content_and_json_round_trips() {
        let draft = Draft {
            identity: Some(IdentityId(3)),
            to: vec![Address::new(Some("Anna"), "anna@example.com")],
            subject: "Secret plans".into(),
            body_markdown: "Hello **Anna**".into(),
            origin: Some(DraftOrigin::Reply(MessageId(9))),
            name: "brave-otter".into(),
            attachments: vec![DraftAttachment {
                path: "/tmp/plan.pdf".into(),
                name: "plan.pdf".into(),
                size: 12,
            }],
            ..Draft::default()
        };
        let debug = format!("{draft:?}");
        assert!(!debug.contains("anna") && !debug.contains("Secret") && !debug.contains("plan"));
        let json = serde_json::to_string(&draft).unwrap();
        assert_eq!(serde_json::from_str::<Draft>(&json).unwrap(), draft);
        assert!(!draft.is_blank());
        assert!(Draft::default().is_blank());
        assert_eq!(draft.origin.unwrap().message(), MessageId(9));
    }

    #[test]
    fn sent_with_maze_is_on_unless_turned_off() {
        assert!(Draft::default().support_maze);
        // Drafts serialized before the field existed keep the default.
        let old: Draft = serde_json::from_str(
            r#"{"identity":null,"to":[],"cc":[],"bcc":[],"subject":"","body_markdown":"",
                "in_reply_to":null,"references":[],"attachments":[],"origin":null}"#,
        )
        .unwrap();
        assert!(old.support_maze);
        let off = Draft {
            support_maze: false,
            ..Draft::default()
        };
        let json = serde_json::to_string(&off).unwrap();
        assert!(!serde_json::from_str::<Draft>(&json).unwrap().support_maze);
        assert!(SENT_WITH_MAZE.ends_with(&format!("({MAZE_WEBSITE})")));
    }
}
