//! Pending-op payloads: what [`CoreHandle::apply`](crate::CoreHandle::apply) queues for
//! the sync engine to replay against the server (PLAN §4.5, "Pending ops").
//!
//! The payload type belongs to the `sync` crate (`sync::ops::OpPayload`), which owns
//! the replay semantics. Until it lands on `main`, core writes this private mirror of
//! it; swapping in the real type is a change to this file only (`OpPayload` and
//! `Envelope` become re-exports or thin conversions, the builders stay).
//!
//! # Format (`pending_op.payload_json`)
//!
//! A JSON object with a payload version `v` and an internal tag `op` that equals the
//! `pending_op.kind` column:
//!
//! ```json
//! {"v":1,"op":"move","targets":[{"message":42,"folder":3,"mailbox":"INBOX","uid":1234,
//!   "gm_msgid":null,"rfc_message_id":"abc@example.com"}],"to":{"id":7,"remote_name":"Archive/2026"}}
//! ```
//!
//! - `targets` are the message copies on the server the op acts on, captured before
//!   the optimistic local apply: the folder's server name and the UID (absent for
//!   copies that haven't reached the server yet, for example after an earlier move;
//!   sync then resolves by Message-ID or `X-GM-MSGID`).
//! - Flags travel as IMAP names (`\Seen`, `$Junk`), tags as lowercase IMAP keywords,
//!   both as add/remove sets (idempotent to replay).
//! - One op per account and command, in command order; ops are enqueued in the same
//!   transaction as the local apply and the audit entry.
//!
//! Variants: `set_flags`, `move`, `copy`, `delete` (to Trash, or `permanent`),
//! `archive` (to a folder, or Gmail's "remove the `\Inbox` label"), `create_folder`,
//! `rename_folder`, `delete_folder`. `junk`, `not_junk` and `append_draft` exist in
//! `sync`'s enum and aren't produced by core yet.

use mazemail_types::{Flags, FolderId, MessageId};
use serde::{Deserialize, Serialize};
use store::{Folder, Location, MessageDetail};

/// Version of the payload format.
pub(crate) const PAYLOAD_VERSION: u32 = 1;

/// A folder as the server knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FolderRef {
    /// The local folder.
    pub(crate) id: FolderId,
    /// Its server name (IMAP mailbox or Gmail label); `None` for local folders.
    pub(crate) remote_name: Option<String>,
}

impl FolderRef {
    pub(crate) fn of(folder: &Folder) -> Self {
        Self {
            id: folder.id,
            remote_name: folder.remote_name.clone(),
        }
    }
}

/// One copy of a message on the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OpTarget {
    pub(crate) message: MessageId,
    pub(crate) folder: FolderId,
    /// The folder's server name.
    pub(crate) mailbox: Option<String>,
    /// UID in that mailbox, if known.
    pub(crate) uid: Option<u32>,
    /// Gmail's message id.
    pub(crate) gm_msgid: Option<u64>,
    /// `Message-ID` without angle brackets, for the fallback search.
    pub(crate) rfc_message_id: Option<String>,
}

impl OpTarget {
    pub(crate) fn new(
        detail: &MessageDetail,
        location: &Location,
        folder: Option<&Folder>,
    ) -> Self {
        Self {
            message: detail.id,
            folder: location.folder,
            mailbox: folder.and_then(|f| f.remote_name.clone()),
            uid: location.uid,
            gm_msgid: detail.gm_msgid,
            rfc_message_id: detail.rfc_message_id.clone(),
        }
    }
}

/// Where an archive goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ArchiveTarget {
    /// Move to this folder (the identity's archive folder, or its year/month
    /// subfolder).
    Folder { folder: FolderRef },
    /// Gmail: remove the `\Inbox` label (`-X-GM-LABELS (\Inbox)`).
    GmailRemoveInbox,
}

/// The mirror of `sync::ops::OpPayload`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum OpPayload {
    SetFlags {
        targets: Vec<OpTarget>,
        add: Vec<String>,
        remove: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        add_keywords: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        remove_keywords: Vec<String>,
    },
    Move {
        targets: Vec<OpTarget>,
        to: FolderRef,
    },
    Copy {
        targets: Vec<OpTarget>,
        to: FolderRef,
    },
    Delete {
        targets: Vec<OpTarget>,
        permanent: bool,
        trash: Option<FolderRef>,
    },
    Archive {
        targets: Vec<OpTarget>,
        target: ArchiveTarget,
    },
    CreateFolder {
        folder: FolderRef,
        parent: Option<FolderRef>,
    },
    RenameFolder {
        folder: FolderRef,
        from_name: Option<String>,
    },
    DeleteFolder {
        folder: FolderRef,
    },
}

impl OpPayload {
    /// Flag changes as IMAP names.
    pub(crate) fn set_flags(targets: Vec<OpTarget>, add: Flags, remove: Flags) -> Self {
        Self::SetFlags {
            targets,
            add: names(add),
            remove: names(remove),
            add_keywords: Vec::new(),
            remove_keywords: Vec::new(),
        }
    }

    /// Keyword (tag) changes.
    pub(crate) fn set_keywords(
        targets: Vec<OpTarget>,
        add_keywords: Vec<String>,
        remove_keywords: Vec<String>,
    ) -> Self {
        Self::SetFlags {
            targets,
            add: Vec::new(),
            remove: Vec::new(),
            add_keywords,
            remove_keywords,
        }
    }

    /// The `pending_op.kind` value (equal to the `op` tag).
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::SetFlags { .. } => "set_flags",
            Self::Move { .. } => "move",
            Self::Copy { .. } => "copy",
            Self::Delete { .. } => "delete",
            Self::Archive { .. } => "archive",
            Self::CreateFolder { .. } => "create_folder",
            Self::RenameFolder { .. } => "rename_folder",
            Self::DeleteFolder { .. } => "delete_folder",
        }
    }

    /// The `pending_op.payload_json` value.
    pub(crate) fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&Envelope {
            v: PAYLOAD_VERSION,
            op: self.clone(),
        })
    }
}

/// The versioned wrapper stored in `payload_json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Envelope {
    pub(crate) v: u32,
    #[serde(flatten)]
    pub(crate) op: OpPayload,
}

fn names(flags: Flags) -> Vec<String> {
    flags.to_imap().into_iter().map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_is_versioned_and_tagged() {
        let payload = OpPayload::Move {
            targets: vec![OpTarget {
                message: MessageId(42),
                folder: FolderId(3),
                mailbox: Some("INBOX".to_owned()),
                uid: Some(1234),
                gm_msgid: None,
                rfc_message_id: Some("abc@example.com".to_owned()),
            }],
            to: FolderRef {
                id: FolderId(7),
                remote_name: Some("Archive".to_owned()),
            },
        };
        let json: serde_json::Value = serde_json::from_str(&payload.to_json().unwrap()).unwrap();
        assert_eq!(json["v"], 1);
        assert_eq!(json["op"], "move");
        assert_eq!(json["op"], payload.kind());
        assert_eq!(json["targets"][0]["uid"], 1234);
        assert_eq!(json["to"]["remote_name"], "Archive");
        let back: Envelope = serde_json::from_value(json).unwrap();
        assert_eq!(back.op, payload);
    }

    #[test]
    fn flags_travel_as_imap_names() {
        let payload = OpPayload::set_flags(Vec::new(), Flags::SEEN, Flags::FLAGGED);
        let json: serde_json::Value = serde_json::from_str(&payload.to_json().unwrap()).unwrap();
        assert_eq!(json["add"][0], "\\Seen");
        assert_eq!(json["remove"][0], "\\Flagged");
        assert!(json.get("add_keywords").is_none());
        let archive = OpPayload::Archive {
            targets: Vec::new(),
            target: ArchiveTarget::GmailRemoveInbox,
        };
        let json: serde_json::Value = serde_json::from_str(&archive.to_json().unwrap()).unwrap();
        assert_eq!(json["target"]["kind"], "gmail_remove_inbox");
    }
}
