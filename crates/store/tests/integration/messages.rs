//! Message upsert, Gmail dedup, locations, flags, tags, threading.

use mazemail_types::{Flags, FolderRole};
use pretty_assertions::assert_eq;
use store::{
    AccountKind, BodyState, Error, NewFolder, NewLocation, NewMessage, Queries, ThreadAssignment,
};

use crate::common::{self, message};

#[test]
fn gmail_message_is_stored_once_with_own_flags_per_label() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Gmail);
    let (all, work, trash, inbox) = store
        .write_blocking(move |w| {
            let all = w.create_folder(
                &NewFolder::new(f.account, "[Gmail]/All Mail", "All Mail")
                    .with_role(FolderRole::All),
            )?;
            let work = w.create_folder(&NewFolder::new(f.account, "Work", "Work").label())?;
            let trash = w.create_folder(
                &NewFolder::new(f.account, "[Gmail]/Trash", "Trash").with_role(FolderRole::Trash),
            )?;
            // Gmail's \Inbox is a label folder.
            let inbox = w.create_folder(
                &NewFolder::new(f.account, "\\Inbox", "Inbox")
                    .with_role(FolderRole::Inbox)
                    .label(),
            )?;
            Ok((all, work, trash, inbox))
        })
        .unwrap();

    let gmail = move |locations: Vec<NewLocation>| {
        let mut m = NewMessage::new(f.account, 1_700_000_000);
        m.rfc_message_id = Some("abc@mail.gmail.com".to_owned());
        m.subject = Some("Quarterly numbers".to_owned());
        m.gm_msgid = Some(1_234_567_890_123);
        m.gm_thrid = Some(42);
        m.thread = ThreadAssignment::Resolve { by_subject: false };
        m.replace_labels = true;
        m.locations = locations;
        m
    };

    let first = gmail(vec![
        NewLocation::new(all, Some(10), Flags::SEEN),
        NewLocation::new(inbox, None, Flags::SEEN | Flags::FLAGGED),
        NewLocation::new(work, None, Flags::empty()),
    ]);
    let a = store
        .write_blocking(move |w| w.upsert_message(&first))
        .unwrap();
    assert!(a.created);

    // Seen again from All Mail with one label removed: same message, labels replaced.
    let second = gmail(vec![
        NewLocation::new(all, Some(10), Flags::SEEN),
        NewLocation::new(work, None, Flags::ANSWERED),
    ]);
    let b = store
        .write_blocking(move |w| w.upsert_message(&second))
        .unwrap();
    assert_eq!(b.message, a.message);
    assert_eq!(b.thread, a.thread);
    assert!(!b.created);

    // Seen in Trash (a real folder with its own UID): adds a location, keeps labels.
    let mut third = gmail(vec![NewLocation::new(trash, Some(3), Flags::SEEN)]);
    third.replace_labels = false;
    let c = store
        .write_blocking(move |w| w.upsert_message(&third))
        .unwrap();
    assert_eq!(c.message, a.message);

    let reader = store.reader().unwrap();
    let detail = reader.message(a.message).unwrap().unwrap();
    let locations: Vec<_> = detail
        .locations
        .iter()
        .map(|l| (l.folder, l.uid, l.flags))
        .collect();
    let mut expected = vec![
        (all, Some(10), Flags::SEEN),
        (work, None, Flags::ANSWERED),
        (trash, Some(3), Flags::SEEN),
    ];
    expected.sort_by_key(|l| l.0);
    assert_eq!(locations, expected);
    assert_eq!(detail.effective_flags(), Flags::ANSWERED);
    assert_eq!(
        reader
            .message_by_gm_msgid(f.account, 1_234_567_890_123)
            .unwrap(),
        Some(a.message)
    );

    // Counts: Inbox lost the message, Work has it unread.
    assert_eq!(reader.folder(inbox).unwrap().unwrap().total, 0);
    let work_folder = reader.folder(work).unwrap().unwrap();
    assert_eq!((work_folder.total, work_folder.unread), (1, 1));
    let all_folder = reader.folder(all).unwrap().unwrap();
    assert_eq!((all_folder.total, all_folder.unread), (1, 0));

    // One message row for the account.
    let ids = reader
        .message_ids_in_scope(&store::MessageScope::Account(f.account))
        .unwrap();
    assert_eq!(ids, vec![a.message]);

    // Another message in the same Gmail thread joins it.
    let mut reply = gmail(vec![NewLocation::new(all, Some(11), Flags::empty())]);
    reply.gm_msgid = Some(99);
    reply.rfc_message_id = Some("reply@mail.gmail.com".to_owned());
    let r = store
        .write_blocking(move |w| w.upsert_message(&reply))
        .unwrap();
    assert!(r.created);
    assert_eq!(r.thread, a.thread);
}

#[test]
fn uidvalidity_reset_keeps_bodies_and_reattaches() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let staged = store
        .blobs()
        .put(b"From: a@example.com\r\n\r\nbody")
        .unwrap();
    let blob = staged.id();
    let m = message(f.account, f.inbox, 7, 1000);
    let m2 = m.clone();
    let outcome = store
        .write_blocking(move |w| {
            let outcome = w.upsert_message(&m)?;
            w.set_message_body(outcome.message, &staged, BodyState::Full)?;
            Ok(outcome)
        })
        .unwrap();

    let removed = store
        .write_blocking(move |w| w.reset_folder(f.inbox, 555))
        .unwrap();
    assert_eq!(removed, 1);
    {
        let reader = store.reader().unwrap();
        let folder = reader.folder(f.inbox).unwrap().unwrap();
        assert_eq!(folder.sync.uidvalidity, Some(555));
        assert_eq!((folder.total, folder.unread), (0, 0));
        let detail = reader.message(outcome.message).unwrap().unwrap();
        assert!(detail.locations.is_empty());
        assert!(detail.detached_at.is_some());
        assert_eq!(detail.blob, Some(blob));
        let rows = reader.thread_rows(&[outcome.thread], None).unwrap();
        assert_eq!(rows[0].message_count, 0);
    }

    // Re-sync: new UID, same Message-ID → the detached message comes back with its body.
    let mut again = m2;
    again.locations = vec![NewLocation::new(f.inbox, Some(1), Flags::SEEN)];
    let back = store
        .write_blocking(move |w| w.upsert_message(&again))
        .unwrap();
    assert_eq!(back.message, outcome.message);
    assert!(!back.created);
    let reader = store.reader().unwrap();
    let detail = reader.message(outcome.message).unwrap().unwrap();
    assert_eq!(detail.detached_at, None);
    assert_eq!(detail.blob, Some(blob));
    assert_eq!(detail.body_state, BodyState::Full);
    assert_eq!(reader.folder(f.inbox).unwrap().unwrap().total, 1);
    assert_eq!(
        store.blobs().read(&blob).unwrap(),
        b"From: a@example.com\r\n\r\nbody"
    );
}

#[test]
fn expunge_detaches_and_purge_deletes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let staged = store.blobs().put(b"raw message").unwrap();
    let blob = staged.id();
    let a = message(f.account, f.inbox, 1, 100);
    let b = message(f.account, f.inbox, 2, 200);
    let (a, b) = store
        .write_blocking(move |w| {
            let a = w.upsert_message(&a)?;
            let b = w.upsert_message(&b)?;
            w.set_message_body(a.message, &staged, BodyState::Full)?;
            Ok((a, b))
        })
        .unwrap();

    let affected = store
        .write_blocking(move |w| w.expunge(f.inbox, &[1, 99]))
        .unwrap();
    assert_eq!(affected, vec![a.message]);
    {
        let reader = store.reader().unwrap();
        assert_eq!(reader.folder(f.inbox).unwrap().unwrap().total, 1);
        assert_eq!(reader.message_by_uid(f.inbox, 1).unwrap(), None);
        assert_eq!(reader.message_by_uid(f.inbox, 2).unwrap(), Some(b.message));
        assert!(
            reader
                .message(a.message)
                .unwrap()
                .unwrap()
                .detached_at
                .is_some()
        );
    }

    let purged = store
        .write_blocking(move |w| w.purge_detached(f.account, i64::MAX))
        .unwrap();
    assert_eq!(purged, 1);
    let reader = store.reader().unwrap();
    assert!(reader.message(a.message).unwrap().is_none());
    assert!(reader.thread_rows(&[a.thread], None).unwrap().is_empty());
    drop(reader);

    // The body lost its last reference; GC removes it.
    let report = store
        .write_blocking(|_| Ok(()))
        .and_then(|()| {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(store.collect_garbage(store::GcOptions::default()))
        })
        .unwrap();
    assert_eq!(report.rows_deleted, 1);
    assert!(!store.blobs().exists(&blob));
}

#[test]
fn flags_and_moves_update_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let m = message(f.account, f.inbox, 1, 100);
    let out = store.write_blocking(move |w| w.upsert_message(&m)).unwrap();

    store
        .write_blocking(move |w| {
            w.set_flags(&[out.message], Flags::SEEN | Flags::FLAGGED, Flags::empty())
        })
        .unwrap();
    {
        let r = store.reader().unwrap();
        let inbox = r.folder(f.inbox).unwrap().unwrap();
        assert_eq!((inbox.total, inbox.unread), (1, 0));
        let row = &r.thread_rows(&[out.thread], Some(f.inbox)).unwrap()[0];
        assert!(row.flagged);
        assert_eq!(row.unread_count, 0);
    }

    // Optimistic move, then the server reports the new UID.
    store
        .write_blocking(move |w| {
            w.move_location(out.message, f.inbox, f.archive, None)?;
            w.set_location_uid(out.message, f.archive, 77)
        })
        .unwrap();
    let r = store.reader().unwrap();
    assert_eq!(r.folder(f.inbox).unwrap().unwrap().total, 0);
    assert_eq!(r.folder(f.archive).unwrap().unwrap().total, 1);
    assert_eq!(r.message_by_uid(f.archive, 77).unwrap(), Some(out.message));
    let detail = r.message(out.message).unwrap().unwrap();
    assert_eq!(detail.locations[0].flags, Flags::SEEN | Flags::FLAGGED);

    // Server flag change by UID.
    drop(r);
    let found = store
        .write_blocking(move |w| {
            w.update_location_flags(f.archive, 77, Flags::empty(), None, Some(9))
        })
        .unwrap();
    assert_eq!(found, Some(out.message));
    let r = store.reader().unwrap();
    assert_eq!(r.folder(f.archive).unwrap().unwrap().unread, 1);
    assert_eq!(
        r.folder_uids(f.archive).unwrap(),
        vec![(77, Flags::empty())]
    );
}

#[test]
fn keywords_become_tags() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let mut m = message(f.account, f.inbox, 1, 100);
    m.locations[0].keywords = vec!["$Label1".to_owned(), "custom".to_owned()];
    let out = store.write_blocking(move |w| w.upsert_message(&m)).unwrap();

    let reader = store.reader().unwrap();
    let important = reader.tag_by_keyword("$label1").unwrap().unwrap();
    let detail = reader.message(out.message).unwrap().unwrap();
    assert_eq!(detail.tags, vec![important.id]);
    assert_eq!(detail.locations[0].keywords, vec!["$label1", "custom"]);
    drop(reader);

    // Defining a tag for an existing keyword tags the messages that carry it.
    let custom = store
        .write_blocking(|w| w.create_tag("Custom", "Custom", Some("#123456"), 9))
        .unwrap();
    let reader = store.reader().unwrap();
    let mut tags = reader.message(out.message).unwrap().unwrap().tags;
    tags.sort();
    assert_eq!(tags, vec![important.id, custom]);
    let row = &reader.thread_rows(&[out.thread], None).unwrap()[0];
    assert_eq!(row.tags.len(), 2);
    drop(reader);

    // Tag commands write keywords on every location.
    store
        .write_blocking(move |w| w.set_tags(&[out.message], &[], &[important.id]))
        .unwrap();
    let reader = store.reader().unwrap();
    let detail = reader.message(out.message).unwrap().unwrap();
    assert_eq!(detail.tags, vec![custom]);
    assert_eq!(detail.locations[0].keywords, vec!["custom"]);
}

#[test]
fn resolver_threads_by_references_and_merges() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let make = move |uid: u32, id: &str, refs: &[&str]| {
        let mut m = message(f.account, f.inbox, uid, 1000 + i64::from(uid));
        m.rfc_message_id = Some(id.to_owned());
        m.in_reply_to = refs.last().map(|r| (*r).to_owned());
        m.references = refs.iter().map(|r| (*r).to_owned()).collect();
        m.thread = ThreadAssignment::Resolve { by_subject: false };
        m
    };
    // C and B arrive before their parent A; they only share the missing parent.
    let c = make(3, "c@x", &["a@x"]);
    let b = make(2, "b@x", &["a@x"]);
    let unrelated = make(4, "z@x", &[]);
    let (c, b, z) = store
        .write_blocking(move |w| {
            Ok((
                w.upsert_message(&c)?,
                w.upsert_message(&b)?,
                w.upsert_message(&unrelated)?,
            ))
        })
        .unwrap();
    assert_eq!(
        c.thread, b.thread,
        "siblings with a missing parent share a thread"
    );
    assert_ne!(z.thread, c.thread);

    // D replies to the unrelated message *and* to A's thread: the threads merge.
    let d = make(5, "d@x", &["a@x", "z@x"]);
    let d = store.write_blocking(move |w| w.upsert_message(&d)).unwrap();
    assert_eq!(d.thread, c.thread.min(z.thread));
    let reader = store.reader().unwrap();
    let rows = reader.thread_rows(&[c.thread, z.thread], None).unwrap();
    assert_eq!(rows.len(), 1, "the merged-away thread is gone");
    assert_eq!(rows[0].message_count, 4);
    assert_eq!(reader.folder(f.inbox).unwrap().unwrap().total, 4);
    assert_eq!(
        reader
            .threads_for_rfc_ids(f.account, &["a@x"])
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        vec![d.thread]
    );
    drop(reader);

    // The late parent joins too.
    let a = make(1, "a@x", &[]);
    let a = store.write_blocking(move |w| w.upsert_message(&a)).unwrap();
    assert_eq!(a.thread, d.thread);

    // Subject fallback only when asked.
    let mut s1 = message(f.account, f.inbox, 10, 5000);
    s1.subject_key = Some("lunch".to_owned());
    let mut s2 = message(f.account, f.inbox, 11, 5100);
    s2.subject_key = Some("lunch".to_owned());
    s2.thread = ThreadAssignment::Resolve { by_subject: true };
    let mut s3 = message(f.account, f.inbox, 12, 5200);
    s3.subject_key = Some("lunch".to_owned());
    s3.thread = ThreadAssignment::Resolve { by_subject: false };
    let (s1, s2, s3) = store
        .write_blocking(move |w| {
            Ok((
                w.upsert_message(&s1)?,
                w.upsert_message(&s2)?,
                w.upsert_message(&s3)?,
            ))
        })
        .unwrap();
    assert_eq!(s2.thread, s1.thread);
    assert_ne!(s3.thread, s1.thread);
}

#[test]
fn invalid_input_is_rejected_and_rolled_back() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let other = common::fixture(&store, AccountKind::Imap);
    let mut m = message(f.account, f.inbox, 1, 100);
    m.locations
        .push(NewLocation::new(other.inbox, Some(1), Flags::empty()));
    let err = store
        .write_blocking(move |w| w.upsert_message(&m))
        .unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err:?}");

    // A job that fails after writing leaves nothing behind.
    let m = message(f.account, f.inbox, 2, 100);
    let err = store
        .write_blocking(move |w| {
            w.upsert_message(&m)?;
            Err::<(), _>(Error::Invalid("abort"))
        })
        .unwrap_err();
    assert!(matches!(err, Error::Invalid("abort")));
    let reader = store.reader().unwrap();
    assert_eq!(reader.message_by_uid(f.inbox, 2).unwrap(), None);
    assert_eq!(reader.folder(f.inbox).unwrap().unwrap().total, 0);
}

#[test]
fn a_reused_uid_moves_to_the_message_the_server_names() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Gmail);
    let mut first = message(f.account, f.archive, 5, 1);
    first.gm_msgid = Some(1);
    let mut second = message(f.account, f.inbox, 9, 2);
    second.gm_msgid = Some(2);
    let (a, b) = store
        .write_blocking(move |w| Ok((w.upsert_message(&first)?, w.upsert_message(&second)?)))
        .unwrap();
    // The server now says UID 5 in Archive is message 2 (found by X-GM-MSGID).
    let mut moved = message(f.account, f.archive, 5, 2);
    moved.gm_msgid = Some(2);
    let out = store
        .write_blocking(move |w| w.upsert_message(&moved))
        .unwrap();
    assert_eq!(out.message, b.message);
    let reader = store.reader().unwrap();
    assert_eq!(
        reader.message_by_uid(f.archive, 5).unwrap(),
        Some(b.message)
    );
    let old = reader.message(a.message).unwrap().unwrap();
    assert!(old.locations.is_empty());
    assert!(old.detached_at.is_some());
    assert_eq!(reader.folder(f.archive).unwrap().unwrap().total, 1);
}
