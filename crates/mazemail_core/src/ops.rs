//! Pending-op payloads: what [`CoreHandle::apply`](crate::CoreHandle::apply) queues for
//! the sync engine to replay against the server (PLAN §4.5, "Pending ops").
//!
//! The format belongs to the `sync` crate, which owns the replay semantics; it is
//! re-exported here ([`OpPayload`] and its parts, documented in `sync::ops`). This
//! module only adds the small builders [`crate::commands`] uses.
//!
//! # How core follows the enqueue contract of `sync::ops`
//!
//! - Every command runs as one store write job: ops are enqueued with
//!   [`OpPayload::enqueue`] *before* the optimistic local change, in the same
//!   transaction, together with the audit entry.
//! - Locations ([`LocationRef`]) are captured from the message as it is before the
//!   change, so they carry the UID the server knows (a local move drops the source
//!   location).
//! - Destination locations of moves, copies and archives are created locally with
//!   `uid: None`; sync fills in the UID from COPYUID or a Message-ID search.
//! - Flags are absolute: [`FlagTarget`] holds every flag and keyword a location should
//!   have after the change, so replaying an op twice is harmless.
//! - One op per account and command, in command order. Folder operations carry the
//!   full server names (`create_folder`, `rename_folder`, `delete_folder`).

use mazemail_types::Flags;
use store::{Location, MessageDetail};

pub use sync::ops::{
    ArchiveTarget, FlagTarget, LocationRef, OP_PAYLOAD_VERSION, OpPayload, OpPayloadError,
};

/// The server copy `location` of `detail`, with its UID as it is now.
pub(crate) fn location_ref(detail: &MessageDetail, location: &Location) -> LocationRef {
    LocationRef::new(detail.id, location.folder, location.uid)
}

/// The absolute flags and keywords `location` of `detail` should have.
pub(crate) fn flag_target(
    detail: &MessageDetail,
    location: &Location,
    flags: Flags,
    keywords: Vec<String>,
) -> FlagTarget {
    FlagTarget {
        message: detail.id,
        folder: location.folder,
        uid: location.uid,
        flags,
        keywords,
    }
}

/// `present` with `add` added and `remove` removed, sorted and without duplicates.
pub(crate) fn edit_keywords(present: &[String], add: &[String], remove: &[String]) -> Vec<String> {
    let mut out: Vec<String> = present
        .iter()
        .chain(add)
        .filter(|k| !remove.contains(k))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use mazemail_types::{FolderId, MessageId};

    use super::*;

    #[test]
    fn keywords_are_edited_as_sets() {
        let present = vec!["$label1".to_owned(), "todo".to_owned()];
        assert_eq!(
            edit_keywords(
                &present,
                &["later".to_owned(), "todo".to_owned()],
                &["$label1".to_owned()]
            ),
            vec!["later".to_owned(), "todo".to_owned()]
        );
        assert!(edit_keywords(&[], &[], &["x".to_owned()]).is_empty());
    }

    #[test]
    fn payloads_use_the_sync_format() {
        let json = OpPayload::move_to(
            vec![LocationRef::new(MessageId(17), FolderId(3), Some(4711))],
            FolderId(9),
        )
        .to_json()
        .unwrap();
        assert_eq!(
            json,
            r#"{"v":1,"op":"move","items":[{"message":17,"folder":3,"uid":4711}],"to":9}"#
        );
    }
}
