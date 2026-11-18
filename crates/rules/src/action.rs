//! Actions: what a rule does with a matching message.

use std::fmt;

use serde_json::{Value, json};

use crate::thunderbird;

/// What an action does.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// Move to the folder named by the value (a path in the message's account).
    MoveToFolder,
    /// Copy to the folder named by the value.
    CopyToFolder,
    /// Add the tag named by the value (an IMAP keyword or a tag name).
    AddTag,
    /// Mark read.
    MarkRead,
    /// Mark unread.
    MarkUnread,
    /// Flag (star).
    MarkFlagged,
    /// Archive, as the Archive command does.
    Archive,
    /// Move to the account's Trash. Never deletes for good.
    Trash,
    /// Report junk: mark it and move it to the Junk folder.
    Junk,
    /// Run no further rules on the message.
    Stop,
    /// Something Maze doesn't run (kept as written).
    Other(String),
}

impl ActionKind {
    /// The stored name.
    pub fn as_str(&self) -> &str {
        match self {
            Self::MoveToFolder => "move_to_folder",
            Self::CopyToFolder => "copy_to_folder",
            Self::AddTag => "add_tag",
            Self::MarkRead => "mark_read",
            Self::MarkUnread => "mark_unread",
            Self::MarkFlagged => "mark_flagged",
            Self::Archive => "archive",
            Self::Trash => "trash",
            Self::Junk => "junk",
            Self::Stop => "stop",
            Self::Other(name) => name,
        }
    }

    /// The action with this stored name.
    pub fn parse(name: &str) -> Self {
        match name {
            "move_to_folder" => Self::MoveToFolder,
            "copy_to_folder" => Self::CopyToFolder,
            "add_tag" => Self::AddTag,
            "mark_read" => Self::MarkRead,
            "mark_unread" => Self::MarkUnread,
            "mark_flagged" => Self::MarkFlagged,
            "archive" => Self::Archive,
            "trash" => Self::Trash,
            "junk" => Self::Junk,
            "stop" => Self::Stop,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Whether the action takes the message out of the folder it is in. A rule runs at
    /// most one of them, after its other actions, and no rule runs after it.
    pub fn moves(&self) -> bool {
        matches!(
            self,
            Self::MoveToFolder | Self::Archive | Self::Trash | Self::Junk
        )
    }

    /// Whether the action needs a value (a folder or a tag).
    pub fn needs_value(&self) -> bool {
        matches!(self, Self::MoveToFolder | Self::CopyToFolder | Self::AddTag)
    }

    /// The order within a rule: flags and tags, then copies, then the move.
    pub(crate) fn rank(&self) -> u8 {
        match self {
            Self::CopyToFolder => 1,
            kind if kind.moves() => 2,
            _ => 0,
        }
    }
}

/// One action of a rule.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Action {
    /// What it does.
    pub kind: ActionKind,
    /// The folder or tag, for actions that need one.
    pub value: Option<String>,
}

impl Action {
    /// An action without a value.
    pub fn new(kind: ActionKind) -> Self {
        Self { kind, value: None }
    }

    /// An action with a folder or tag.
    pub fn with_value(kind: ActionKind, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: Some(value.into()),
        }
    }

    /// The value, if it isn't empty.
    pub fn value(&self) -> Option<&str> {
        self.value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
    }

    pub(crate) fn to_json(&self) -> Value {
        match &self.value {
            Some(value) => json!({ "action": self.kind.as_str(), "value": value }),
            None => json!({ "action": self.kind.as_str() }),
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.value().unwrap_or("?");
        match &self.kind {
            ActionKind::MoveToFolder => write!(f, "move to {value}"),
            ActionKind::CopyToFolder => write!(f, "copy to {value}"),
            ActionKind::AddTag => write!(f, "tag {value}"),
            ActionKind::MarkRead => f.write_str("mark read"),
            ActionKind::MarkUnread => f.write_str("mark unread"),
            ActionKind::MarkFlagged => f.write_str("flag"),
            ActionKind::Archive => f.write_str("archive"),
            ActionKind::Trash => f.write_str("move to Trash"),
            ActionKind::Junk => f.write_str("mark as junk"),
            ActionKind::Stop => f.write_str("stop"),
            ActionKind::Other(name) => write!(f, "{name} (not supported)"),
        }
    }
}

/// Reads the stored JSON array; actions the Thunderbird import wrote before the rules
/// engine (Thunderbird names, folder URIs) are converted.
pub(crate) fn from_json(value: &Value) -> Result<Vec<Action>, String> {
    let items = value.as_array().ok_or("expected an array")?;
    items
        .iter()
        .map(|item| {
            let name = item
                .get("action")
                .and_then(Value::as_str)
                .ok_or("an action has no `action`")?;
            let value = item.get("value").and_then(Value::as_str);
            Ok(match ActionKind::parse(name) {
                ActionKind::Other(name) => thunderbird::legacy_action(&name, value),
                kind => {
                    let value = match (&kind, value) {
                        (ActionKind::MoveToFolder | ActionKind::CopyToFolder, Some(value)) => Some(
                            thunderbird::folder_path(value).unwrap_or_else(|| value.to_owned()),
                        ),
                        (_, value) => value.map(str::to_owned),
                    };
                    Action { kind, value }
                }
            })
        })
        .collect()
}
