//! The pending-op payload format: what `mazemail_core` enqueues and the sync engine
//! replays (PLAN §4.5 "Pending ops").
//!
//! # Contract
//!
//! 1. Core persists the op with [`OpPayload::enqueue`] (inside the same store write job
//!    as the optimistic local change, so both commit together), then applies the
//!    change locally, then sends [`SyncCommand::ApplyOp`](crate::SyncCommand::ApplyOp).
//! 2. The account's engine replays the queue strictly in order
//!    ([`store::Queries::next_pending_op`]). Success removes the op; a server `NO` counts
//!    an attempt and retries with backoff ([`store::retry_backoff_secs`] plus jitter);
//!    after [`store::MAX_OP_ATTEMPTS`] the op is stuck and reported as
//!    [`OpEvent::Stuck`](crate::OpEvent::Stuck). Connection losses don't count.
//! 3. Messages are addressed by [`LocationRef`]: the message, the folder the op acts on
//!    and the location's UID **at enqueue time**. Capture the UID before the optimistic
//!    apply (a local move drops the source location). A `uid` of `None` means "not on
//!    the server yet" (e.g. the target of an earlier move); the engine fills it in when
//!    an earlier op learns it (COPYUID/APPENDUID) and otherwise looks it up in the
//!    store at replay time.
//! 4. Destination locations: for `move`, `copy` and `append_draft`, core creates the
//!    destination location optimistically with `uid: None`
//!    ([`store::WriteTx::move_location`], [`store::WriteTx::add_location`]); the engine
//!    sets the UID from COPYUID/APPENDUID, or matches the message by Message-ID in the
//!    destination when the server doesn't report UIDs.
//!
//! # Format
//!
//! The `pending_op.kind` column holds the op name ([`OpPayload::kind`]), and
//! `payload_json` a self-describing JSON object with a format version `v` and the op
//! name in `op`. Flags are IMAP names; folders and messages are raw store ids.
//!
//! ```json
//! {"v":1,"op":"set_flags","targets":[
//!   {"message":17,"folder":3,"uid":4711,"flags":["\\Seen","\\Flagged"],"keywords":["$label1"]}]}
//! {"v":1,"op":"move","items":[{"message":17,"folder":3,"uid":4711}],"to":9}
//! {"v":1,"op":"copy","items":[{"message":17,"folder":3,"uid":4711}],"to":9}
//! {"v":1,"op":"delete","items":[{"message":17,"folder":3,"uid":4711}],"permanent":false,"trash":5}
//! {"v":1,"op":"archive","items":[{"message":17,"folder":3,"uid":4711}],"target":{"folder":12}}
//! {"v":1,"op":"archive","items":[{"message":17,"folder":3,"uid":null}],"target":"gmail_remove_inbox"}
//! {"v":1,"op":"junk","items":[{"message":17,"folder":3,"uid":4711}],"to":6}
//! {"v":1,"op":"not_junk","items":[{"message":17,"folder":6,"uid":12}],"to":3}
//! {"v":1,"op":"labels","messages":[17],"add":[21],"remove":[22]}
//! {"v":1,"op":"create_folder","name":"Projects/2026","folder":30,"subscribe":true}
//! {"v":1,"op":"rename_folder","folder":30,"from":"Projects/2026","to":"Projects/Done"}
//! {"v":1,"op":"delete_folder","folder":30,"name":"Projects/Done"}
//! {"v":1,"op":"subscribe","name":"Lists/rust","subscribed":true}
//! {"v":1,"op":"append_draft","folder":4,"blob":"<64 hex chars>","flags":["\\Seen","\\Draft"],
//!  "message":88,"replaces":{"message":87,"folder":4,"uid":31}}
//! ```
//!
//! # Semantics per op
//!
//! | op | IMAP | Gmail (ADR-007) |
//! | --- | --- | --- |
//! | `set_flags` | `UID STORE … FLAGS.SILENT (…)` with the absolute flags + keywords per location (idempotent) | same, on the message's All Mail UID |
//! | `move` | `UID MOVE` (COPY + `\Deleted` + `UID EXPUNGE` without MOVE); COPYUID sets the target UID | label → label: `+X-GM-LABELS` target, `-X-GM-LABELS` source; to Trash/Spam: `UID MOVE` from All Mail; from Trash/Spam: `UID MOVE` to the target mailbox |
//! | `copy` | `UID COPY` | label: `+X-GM-LABELS`; mailbox: `UID COPY` |
//! | `delete` | `permanent` or already in `trash`: `+FLAGS (\Deleted)` + `UID EXPUNGE`; else move to `trash` | move to `[Gmail]/Trash`; permanent in Trash/Spam |
//! | `archive` | move to `target` folder (the identity's archive folder, computed by core incl. granularity) | `-X-GM-LABELS (\Inbox)` |
//! | `junk` / `not_junk` | `+$Junk -$NotJunk` (or the reverse), then move to `to` if given | `UID MOVE` All Mail → `[Gmail]/Spam` / Spam → `INBOX` |
//! | `labels` | — (fails: not a Gmail account) | `+X-GM-LABELS` / `-X-GM-LABELS` |
//! | `create_folder` | `CREATE` (+ `SUBSCRIBE`); exists already = done | creates a label |
//! | `rename_folder` | `RENAME`; source gone and target present = done | renames a label |
//! | `delete_folder` | `DELETE`; gone already = done | deletes a label |
//! | `subscribe` | `SUBSCRIBE` / `UNSUBSCRIBE` | same |
//! | `append_draft` | `APPEND` the blob with the flags; APPENDUID sets the UID; then expunges `replaces` | same |
//!
//! `append_draft` is the only op that is not idempotent: if Maze dies between the
//! server's `OK` and removing the op, the draft is appended twice.

use std::fmt;

use mazemail_types::{AccountId, BlobId, Flags, FolderId, MessageId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use store::{OpId, PendingOp, WriteTx};
use thiserror::Error;

/// The payload format version written by this build.
pub const OP_PAYLOAD_VERSION: u32 = 1;

/// A message location an op acts on (see the module docs, rule 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LocationRef {
    /// The message.
    pub message: MessageId,
    /// The folder (or Gmail label) of the location.
    pub folder: FolderId,
    /// The location's UID when the op was enqueued, if it had one.
    pub uid: Option<u32>,
}

impl LocationRef {
    /// A location reference.
    pub fn new(message: MessageId, folder: FolderId, uid: Option<u32>) -> Self {
        Self {
            message,
            folder,
            uid,
        }
    }
}

/// The absolute flags and keywords one location should have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlagTarget {
    /// The message.
    pub message: MessageId,
    /// The folder of the location.
    pub folder: FolderId,
    /// The location's UID at enqueue time.
    pub uid: Option<u32>,
    /// All system flags and well-known keywords the location should have.
    #[serde(with = "flag_names")]
    pub flags: Flags,
    /// All other keywords (tags) the location should have.
    #[serde(default)]
    pub keywords: Vec<String>,
}

impl FlagTarget {
    /// The location this target refers to.
    pub fn location(&self) -> LocationRef {
        LocationRef::new(self.message, self.folder, self.uid)
    }
}

/// Where archived messages go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveTarget {
    /// Move to this folder (the identity's archive folder, PLAN §4.2).
    Folder(FolderId),
    /// Gmail: remove the `\Inbox` label.
    GmailRemoveInbox,
}

/// One server operation waiting to be replayed (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OpPayload {
    /// Set the absolute flags and keywords of locations.
    SetFlags {
        /// One entry per location.
        targets: Vec<FlagTarget>,
    },
    /// Move messages to a folder.
    Move {
        /// The source locations.
        items: Vec<LocationRef>,
        /// The target folder or label.
        to: FolderId,
    },
    /// Copy messages to a folder (Gmail: add a label).
    Copy {
        /// The source locations.
        items: Vec<LocationRef>,
        /// The target folder or label.
        to: FolderId,
    },
    /// Delete messages: to the trash, or for good.
    Delete {
        /// The locations.
        items: Vec<LocationRef>,
        /// Expunge instead of moving to the trash.
        permanent: bool,
        /// The account's trash folder (ignored on Gmail).
        trash: Option<FolderId>,
    },
    /// Archive messages.
    Archive {
        /// The locations.
        items: Vec<LocationRef>,
        /// Where to.
        target: ArchiveTarget,
    },
    /// Mark as junk (`$Junk`) and move to the junk folder.
    Junk {
        /// The locations.
        items: Vec<LocationRef>,
        /// The junk folder; `None` only marks (Gmail: `[Gmail]/Spam` regardless).
        to: Option<FolderId>,
    },
    /// Mark as not junk (`$NotJunk`) and move back.
    NotJunk {
        /// The locations.
        items: Vec<LocationRef>,
        /// Where to move them (usually the inbox); `None` only marks.
        to: Option<FolderId>,
    },
    /// Gmail: add and remove labels.
    Labels {
        /// The messages.
        messages: Vec<MessageId>,
        /// Label folders to add.
        add: Vec<FolderId>,
        /// Label folders to remove.
        remove: Vec<FolderId>,
    },
    /// Create a folder (Gmail: a label).
    CreateFolder {
        /// The full server name, with the server's hierarchy delimiter.
        name: String,
        /// The local folder created optimistically, if any.
        folder: Option<FolderId>,
        /// Subscribe it after creating it.
        subscribe: bool,
    },
    /// Rename a folder.
    RenameFolder {
        /// The local folder, if any.
        folder: Option<FolderId>,
        /// The old full server name.
        from: String,
        /// The new full server name.
        to: String,
    },
    /// Delete a folder.
    DeleteFolder {
        /// The local folder, if it still exists.
        folder: Option<FolderId>,
        /// The full server name.
        name: String,
    },
    /// Subscribe or unsubscribe a folder.
    Subscribe {
        /// The full server name.
        name: String,
        /// Subscribe (`true`) or unsubscribe.
        subscribed: bool,
    },
    /// Upload a draft (or any message) to a folder.
    AppendDraft {
        /// The target folder (the identity's Drafts folder).
        folder: FolderId,
        /// The raw message in the blob store.
        #[serde(with = "blob_hex")]
        blob: BlobId,
        /// Flags to store it with (`\Draft` is added).
        #[serde(with = "flag_names")]
        flags: Flags,
        /// The local message whose location in `folder` gets the new UID.
        message: Option<MessageId>,
        /// The previous version of the draft, expunged after the upload.
        replaces: Option<LocationRef>,
    },
}

/// A payload that can't be decoded.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpPayloadError {
    /// Not valid JSON for any op.
    #[error("malformed op payload: {0}")]
    Json(#[from] serde_json::Error),
    /// Written by a newer Maze.
    #[error("op payload version {0} is newer than this version of Maze understands")]
    UnsupportedVersion(u32),
    /// The `kind` column and the payload's `op` disagree.
    #[error("op kind {kind:?} doesn't match its payload ({payload:?})")]
    KindMismatch {
        /// The `kind` column.
        kind: String,
        /// The payload's op name.
        payload: &'static str,
    },
}

#[derive(Serialize)]
struct EnvelopeOut<'a> {
    v: u32,
    #[serde(flatten)]
    payload: &'a OpPayload,
}

#[derive(Deserialize)]
struct EnvelopeIn {
    #[serde(default = "first_version")]
    v: u32,
    #[serde(flatten)]
    payload: OpPayload,
}

fn first_version() -> u32 {
    1
}

impl OpPayload {
    /// The op name, stored in `pending_op.kind` and as `op` in the JSON.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SetFlags { .. } => "set_flags",
            Self::Move { .. } => "move",
            Self::Copy { .. } => "copy",
            Self::Delete { .. } => "delete",
            Self::Archive { .. } => "archive",
            Self::Junk { .. } => "junk",
            Self::NotJunk { .. } => "not_junk",
            Self::Labels { .. } => "labels",
            Self::CreateFolder { .. } => "create_folder",
            Self::RenameFolder { .. } => "rename_folder",
            Self::DeleteFolder { .. } => "delete_folder",
            Self::Subscribe { .. } => "subscribe",
            Self::AppendDraft { .. } => "append_draft",
        }
    }

    /// The JSON stored in `pending_op.payload_json`.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&EnvelopeOut {
            v: OP_PAYLOAD_VERSION,
            payload: self,
        })
    }

    /// Decodes `payload_json`.
    pub fn from_json(json: &str) -> Result<Self, OpPayloadError> {
        let envelope: EnvelopeIn = serde_json::from_str(json)?;
        if envelope.v > OP_PAYLOAD_VERSION {
            return Err(OpPayloadError::UnsupportedVersion(envelope.v));
        }
        Ok(envelope.payload)
    }

    /// Decodes a queued op, checking that its `kind` matches.
    pub fn from_pending(op: &PendingOp) -> Result<Self, OpPayloadError> {
        let payload = Self::from_json(&op.payload_json)?;
        if payload.kind() != op.kind {
            return Err(OpPayloadError::KindMismatch {
                kind: op.kind.clone(),
                payload: payload.kind(),
            });
        }
        Ok(payload)
    }

    /// Persists the op for `account` inside a store write job. `origin` is the
    /// mutation's origin as text (`user`, `rule:<id>`, `agent:<token id>`, `cli`).
    pub fn enqueue(
        &self,
        w: &mut WriteTx<'_>,
        account: AccountId,
        origin: &str,
    ) -> store::Result<OpId> {
        let json = self.to_json()?;
        w.enqueue_op(account, self.kind(), &json, origin)
    }

    /// Absolute flags for locations.
    pub fn set_flags(targets: Vec<FlagTarget>) -> Self {
        Self::SetFlags { targets }
    }

    /// Move to `to`.
    pub fn move_to(items: Vec<LocationRef>, to: FolderId) -> Self {
        Self::Move { items, to }
    }

    /// Copy to `to`.
    pub fn copy_to(items: Vec<LocationRef>, to: FolderId) -> Self {
        Self::Copy { items, to }
    }

    /// Move to the trash (or delete for good if the items are in the trash).
    pub fn trash(items: Vec<LocationRef>, trash: Option<FolderId>) -> Self {
        Self::Delete {
            items,
            permanent: false,
            trash,
        }
    }

    /// Delete for good.
    pub fn delete_permanently(items: Vec<LocationRef>) -> Self {
        Self::Delete {
            items,
            permanent: true,
            trash: None,
        }
    }

    /// Archive to a folder, or (Gmail) remove `\Inbox`.
    pub fn archive(items: Vec<LocationRef>, target: ArchiveTarget) -> Self {
        Self::Archive { items, target }
    }

    /// The source locations this op acts on (empty for folder ops and `labels`).
    pub fn locations(&self) -> Vec<LocationRef> {
        match self {
            Self::SetFlags { targets } => targets.iter().map(FlagTarget::location).collect(),
            Self::Move { items, .. }
            | Self::Copy { items, .. }
            | Self::Delete { items, .. }
            | Self::Archive { items, .. }
            | Self::Junk { items, .. }
            | Self::NotJunk { items, .. } => items.clone(),
            Self::AppendDraft { replaces, .. } => replaces.iter().copied().collect(),
            Self::Labels { .. }
            | Self::CreateFolder { .. }
            | Self::RenameFolder { .. }
            | Self::DeleteFolder { .. }
            | Self::Subscribe { .. } => Vec::new(),
        }
    }

    /// Fills in UIDs learned later: every location of `message` in `folder` without
    /// a UID gets `uid`. Returns whether anything changed.
    pub fn fill_uid(&mut self, message: MessageId, folder: FolderId, uid: u32) -> bool {
        let mut changed = false;
        let mut fill = |m: MessageId, f: FolderId, u: &mut Option<u32>| {
            if m == message && f == folder && u.is_none() {
                *u = Some(uid);
                changed = true;
            }
        };
        match self {
            Self::SetFlags { targets } => {
                for t in targets {
                    fill(t.message, t.folder, &mut t.uid);
                }
            }
            Self::Move { items, .. }
            | Self::Copy { items, .. }
            | Self::Delete { items, .. }
            | Self::Archive { items, .. }
            | Self::Junk { items, .. }
            | Self::NotJunk { items, .. } => {
                for i in items {
                    fill(i.message, i.folder, &mut i.uid);
                }
            }
            Self::AppendDraft {
                replaces: Some(r), ..
            } => fill(r.message, r.folder, &mut r.uid),
            _ => {}
        }
        changed
    }

    /// Folders this op needs to exist on the server (so a folder-list sync doesn't
    /// delete their optimistic local copies before the op ran).
    pub(crate) fn protected_folders(&self) -> Vec<FolderId> {
        match self {
            Self::CreateFolder {
                folder: Some(f), ..
            }
            | Self::RenameFolder {
                folder: Some(f), ..
            } => vec![*f],
            _ => Vec::new(),
        }
    }

    /// Server folder names this op creates or renames to.
    pub(crate) fn protected_names(&self) -> Vec<String> {
        match self {
            Self::CreateFolder { name, .. } => vec![name.clone()],
            Self::RenameFolder { to, .. } => vec![to.clone()],
            _ => Vec::new(),
        }
    }

    /// Folders that may receive locations without UIDs from this op.
    pub(crate) fn destinations(&self) -> Vec<FolderId> {
        match self {
            Self::Move { to, .. } | Self::Copy { to, .. } => vec![*to],
            Self::Junk { to: Some(to), .. } | Self::NotJunk { to: Some(to), .. } => vec![*to],
            Self::Archive {
                target: ArchiveTarget::Folder(f),
                ..
            } => vec![*f],
            Self::Delete {
                trash: Some(t),
                permanent: false,
                ..
            } => vec![*t],
            Self::AppendDraft { folder, .. } => vec![*folder],
            _ => Vec::new(),
        }
    }
}

impl fmt::Display for OpPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind())
    }
}

/// Flags as a list of IMAP names (`["\\Seen", "$Junk"]`).
mod flag_names {
    use super::*;

    pub(super) fn serialize<S: Serializer>(flags: &Flags, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(flags.to_imap())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Flags, D::Error> {
        let names: Vec<String> = Vec::deserialize(d)?;
        names.iter().try_fold(Flags::empty(), |acc, name| {
            Flags::from_imap(name)
                .map(|f| acc | f)
                .ok_or_else(|| serde::de::Error::custom(format!("unknown flag {name:?}")))
        })
    }
}

/// A blob id as 64 hex characters.
mod blob_hex {
    use super::*;

    pub(super) fn serialize<S: Serializer>(blob: &BlobId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&blob.to_hex())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BlobId, D::Error> {
        let hex = String::deserialize(d)?;
        BlobId::from_hex(&hex).ok_or_else(|| serde::de::Error::custom("invalid blob id"))
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn loc(m: i64, f: i64, uid: Option<u32>) -> LocationRef {
        LocationRef::new(MessageId(m), FolderId(f), uid)
    }

    fn all_variants() -> Vec<OpPayload> {
        vec![
            OpPayload::set_flags(vec![FlagTarget {
                message: MessageId(17),
                folder: FolderId(3),
                uid: Some(4711),
                flags: Flags::SEEN | Flags::FLAGGED | Flags::JUNK,
                keywords: vec!["$label1".into()],
            }]),
            OpPayload::move_to(vec![loc(17, 3, Some(4711))], FolderId(9)),
            OpPayload::copy_to(vec![loc(17, 3, Some(4711))], FolderId(9)),
            OpPayload::trash(vec![loc(17, 3, Some(4711))], Some(FolderId(5))),
            OpPayload::delete_permanently(vec![loc(17, 5, Some(2))]),
            OpPayload::archive(vec![loc(17, 3, None)], ArchiveTarget::GmailRemoveInbox),
            OpPayload::archive(vec![loc(17, 3, None)], ArchiveTarget::Folder(FolderId(12))),
            OpPayload::Junk {
                items: vec![loc(1, 3, Some(1))],
                to: Some(FolderId(6)),
            },
            OpPayload::NotJunk {
                items: vec![loc(1, 6, Some(1))],
                to: None,
            },
            OpPayload::Labels {
                messages: vec![MessageId(17)],
                add: vec![FolderId(21)],
                remove: vec![FolderId(22)],
            },
            OpPayload::CreateFolder {
                name: "Projects/2026".into(),
                folder: Some(FolderId(30)),
                subscribe: true,
            },
            OpPayload::RenameFolder {
                folder: Some(FolderId(30)),
                from: "a".into(),
                to: "b".into(),
            },
            OpPayload::DeleteFolder {
                folder: None,
                name: "b".into(),
            },
            OpPayload::Subscribe {
                name: "Lists/rust".into(),
                subscribed: false,
            },
            OpPayload::AppendDraft {
                folder: FolderId(4),
                blob: BlobId([0xab; 32]),
                flags: Flags::SEEN | Flags::DRAFT,
                message: Some(MessageId(88)),
                replaces: Some(loc(87, 4, Some(31))),
            },
        ]
    }

    #[test]
    fn every_variant_round_trips() {
        for payload in all_variants() {
            let json = payload.to_json().unwrap();
            assert!(json.starts_with("{\"v\":1,\"op\":\""), "{json}");
            assert!(json.contains(&format!("\"op\":\"{}\"", payload.kind())));
            assert_eq!(OpPayload::from_json(&json).unwrap(), payload);
        }
    }

    #[test]
    fn json_format_is_stable() {
        let json = OpPayload::move_to(vec![loc(17, 3, Some(4711))], FolderId(9))
            .to_json()
            .unwrap();
        assert_eq!(
            json,
            r#"{"v":1,"op":"move","items":[{"message":17,"folder":3,"uid":4711}],"to":9}"#
        );
        let flags = OpPayload::from_json(
            r#"{"v":1,"op":"set_flags","targets":[{"message":1,"folder":2,"uid":3,"flags":["\\Seen","$Forwarded"]}]}"#,
        )
        .unwrap();
        let OpPayload::SetFlags { targets } = flags else {
            panic!("wrong variant")
        };
        assert_eq!(targets[0].flags, Flags::SEEN | Flags::FORWARDED);
        assert!(targets[0].keywords.is_empty());
        // Gmail archive target spelling.
        let archive = OpPayload::archive(vec![], ArchiveTarget::GmailRemoveInbox);
        assert!(
            archive
                .to_json()
                .unwrap()
                .contains(r#""target":"gmail_remove_inbox""#)
        );
    }

    #[test]
    fn rejects_newer_versions_and_garbage() {
        assert!(matches!(
            OpPayload::from_json(r#"{"v":2,"op":"subscribe","name":"x","subscribed":true}"#),
            Err(OpPayloadError::UnsupportedVersion(2))
        ));
        assert!(matches!(
            OpPayload::from_json(r#"{"v":1,"op":"explode"}"#),
            Err(OpPayloadError::Json(_))
        ));
        // A missing version means version 1.
        assert!(OpPayload::from_json(r#"{"op":"subscribe","name":"x","subscribed":true}"#).is_ok());
    }

    #[test]
    fn fill_uid_only_touches_unknown_matching_locations() {
        let mut op = OpPayload::move_to(
            vec![loc(1, 9, None), loc(2, 9, None), loc(1, 8, None)],
            FolderId(3),
        );
        assert!(op.fill_uid(MessageId(1), FolderId(9), 77));
        assert!(!op.fill_uid(MessageId(1), FolderId(9), 78));
        assert_eq!(
            op.locations(),
            vec![loc(1, 9, Some(77)), loc(2, 9, None), loc(1, 8, None)]
        );
    }
}
