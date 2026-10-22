//! Pending-op replay: translation, UID remapping, idempotency, failures, stuck ops.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use imap::fake::{FakeFailure, FakeServer};
use mazemail_types::Flags;
use pretty_assertions::assert_eq;
use store::{OpState, Queries};
use sync::ops::{FlagTarget, LocationRef, OpPayload};
use sync::{Clock, OpEvent};

use crate::common::{Harness, mail, wait_for, wait_idle};

fn ops_empty(h: &Harness) -> bool {
    h.store
        .reader()
        .unwrap()
        .pending_ops(h.account)
        .unwrap()
        .is_empty()
}

#[tokio::test(flavor = "multi_thread")]
async fn flags_and_move_replay_with_copyuid() {
    let server = FakeServer::new();
    for n in 1..=3 {
        server.deliver("INBOX", mail(n));
    }
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    let (inbox, archive) = (h.folder_id("INBOX"), h.folder_id("Archive"));
    let (m1, m2) = (h.message_at("INBOX", 1), h.message_at("INBOX", 2));
    let account = h.account;

    // Core: mark m1 read and flagged, move m2 to Archive (optimistic local apply).
    let ops = h
        .store
        .write(move |w| {
            let a = OpPayload::set_flags(vec![FlagTarget {
                message: m1,
                folder: inbox,
                uid: Some(1),
                flags: Flags::SEEN | Flags::FLAGGED,
                keywords: vec!["todo".into()],
            }])
            .enqueue(w, account, "user")?;
            w.set_flags(&[m1], Flags::SEEN | Flags::FLAGGED, Flags::empty())?;
            let b = OpPayload::move_to(vec![LocationRef::new(m2, inbox, Some(2))], archive)
                .enqueue(w, account, "user")?;
            w.move_location(m2, inbox, archive, None)?;
            // A later op on the moved message, before its new UID is known.
            let c = OpPayload::set_flags(vec![FlagTarget {
                message: m2,
                folder: archive,
                uid: None,
                flags: Flags::SEEN,
                keywords: Vec::new(),
            }])
            .enqueue(w, account, "user")?;
            Ok(vec![a, b, c])
        })
        .await
        .unwrap();
    handle.apply_op(ops[2]).unwrap();
    wait_for("the queue to drain", || ops_empty(&h)).await;
    wait_idle(&handle).await;

    let first = &server.messages("INBOX")[0];
    assert_eq!(first.flags, Flags::SEEN | Flags::FLAGGED);
    assert_eq!(first.keywords, vec!["todo".to_owned()]);
    let archived = server.messages("Archive");
    assert_eq!(archived.len(), 1);
    assert_eq!(
        archived[0].flags,
        Flags::SEEN,
        "the follow-up op found the new UID"
    );
    // Locally: the moved location got the server's UID; no duplicate message.
    assert_eq!(h.message_at("Archive", archived[0].uid), m2);
    assert_eq!(h.uids("Archive").len(), 1);
    assert_eq!(h.uids("INBOX").len(), 2);
    let rows = h
        .store
        .reader()
        .unwrap()
        .messages_by_rfc_id(account, "m2@example.org")
        .unwrap();
    assert_eq!(rows, vec![m2]);
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn move_without_uidplus_finds_the_copy_by_message_id() {
    let server = FakeServer::new();
    server.set_caps(&[
        "IMAP4rev1",
        "IDLE",
        "MOVE",
        "CONDSTORE",
        "QRESYNC",
        "SPECIAL-USE",
    ]);
    server.deliver("INBOX", mail(1));
    server.deliver("Archive", mail(50));
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    let (inbox, archive) = (h.folder_id("INBOX"), h.folder_id("Archive"));
    let m1 = h.message_at("INBOX", 1);
    let account = h.account;
    let op = h
        .store
        .write(move |w| {
            let op = OpPayload::move_to(vec![LocationRef::new(m1, inbox, Some(1))], archive)
                .enqueue(w, account, "user")?;
            w.move_location(m1, inbox, archive, None)?;
            Ok(op)
        })
        .await
        .unwrap();
    handle.apply_op(op).unwrap();
    wait_for("the queue to drain", || ops_empty(&h)).await;
    wait_idle(&handle).await;
    let new_uid = server
        .messages("Archive")
        .iter()
        .map(|m| m.uid)
        .max()
        .unwrap();
    assert_eq!(h.message_at("Archive", new_uid), m1);
    assert_eq!(h.uids("Archive").len(), 2);
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_back_off_and_get_stuck_after_five_attempts() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail(1));
    let h = Harness::new(server.clone()).await;
    // The engine's clock: real time plus an offset the test moves forward.
    let offset = Arc::new(AtomicI64::new(0));
    let real_now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    };
    let mut config = h.config();
    let clock_offset = Arc::clone(&offset);
    config.clock = Clock::from_fn(move || real_now() + clock_offset.load(Ordering::SeqCst));
    let handle = h.spawn(config);
    wait_idle(&handle).await;
    let (inbox, archive) = (h.folder_id("INBOX"), h.folder_id("Archive"));
    let m1 = h.message_at("INBOX", 1);
    let account = h.account;
    let op = h
        .store
        .write(move |w| {
            OpPayload::move_to(vec![LocationRef::new(m1, inbox, Some(1))], archive)
                .enqueue(w, account, "user")
        })
        .await
        .unwrap();
    let mut events = handle.subscribe();
    for _ in 0..5 {
        server.fail_command("UID MOVE", FakeFailure::No("quota exceeded".into()));
    }
    handle.apply_op(op).unwrap();
    let pending = |h: &Harness| h.store.reader().unwrap().pending_ops(account).unwrap();
    for attempt in 1..=4u32 {
        wait_for("a failed attempt", || pending(&h)[0].attempts == attempt).await;
        let current = pending(&h).remove(0);
        assert_eq!(current.state, OpState::Queued);
        assert!(
            current
                .last_error
                .as_deref()
                .unwrap()
                .contains("quota exceeded")
        );
        assert!(current.next_attempt >= real_now() + 29);
        // Time passes beyond the backoff; the engine's timer would fire, but the test
        // clock moved, so nudge it.
        offset.fetch_add(2 * 3600, Ordering::SeqCst);
        handle.apply_op(op).unwrap();
    }
    wait_for("the stuck state", || pending(&h)[0].state == OpState::Stuck).await;
    assert_eq!(pending(&h)[0].attempts, 5);
    let mut failed = 0;
    let mut stuck = false;
    while let Ok(Ok(batch)) =
        tokio::time::timeout(std::time::Duration::from_millis(200), events.recv()).await
    {
        for e in &batch.ops {
            match e {
                OpEvent::Failed { op: id, .. } if *id == op => failed += 1,
                OpEvent::Stuck { op: id, error } if *id == op => {
                    assert!(error.contains("quota exceeded"));
                    stuck = true;
                }
                _ => {}
            }
        }
    }
    assert_eq!(failed, 4);
    assert!(stuck);
    // A stuck op blocks nothing: later ops still replay.
    let flagged = h
        .store
        .write(move |w| {
            OpPayload::set_flags(vec![FlagTarget {
                message: m1,
                folder: inbox,
                uid: Some(1),
                flags: Flags::FLAGGED,
                keywords: Vec::new(),
            }])
            .enqueue(w, account, "user")
        })
        .await
        .unwrap();
    handle.apply_op(flagged).unwrap();
    wait_for("the later op", || {
        server.messages("INBOX")[0].flags == Flags::FLAGGED
    })
    .await;
    // Retry from the activity log: the move goes through now.
    h.store.write(move |w| w.op_retry(op)).await.unwrap();
    handle.apply_op(op).unwrap();
    wait_for("the retried move", || server.messages("Archive").len() == 1).await;
    wait_for("the queue to drain", || ops_empty(&h)).await;
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn folder_ops_and_drafts() {
    let server = FakeServer::new();
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    let account = h.account;
    let drafts = h.folder_id("Drafts");
    let raw =
        b"From: me@example.org\r\nSubject: Draft\r\nMessage-ID: <d1@example.org>\r\n\r\nHi\r\n";
    let blob = h.store.blobs().put(raw).unwrap();
    let blob_id = blob.id();

    let ops = h
        .store
        .write(move |w| {
            w.register_blob(&blob)?;
            let create = OpPayload::CreateFolder {
                name: "Projects".into(),
                folder: None,
                subscribe: true,
            }
            .enqueue(w, account, "user")?;
            let rename = OpPayload::RenameFolder {
                folder: None,
                from: "Projects".into(),
                to: "Done".into(),
            }
            .enqueue(w, account, "user")?;
            let append = OpPayload::AppendDraft {
                folder: drafts,
                blob: blob_id,
                flags: Flags::SEEN,
                message: None,
                replaces: None,
            }
            .enqueue(w, account, "user")?;
            Ok(vec![create, rename, append])
        })
        .await
        .unwrap();
    handle.apply_op(ops[0]).unwrap();
    wait_for("the queue to drain", || ops_empty(&h)).await;
    wait_for("the new folder", || h.try_folder("Done").is_some()).await;
    assert!(server.folder_names().contains(&"Done".to_owned()));
    let drafts_on_server = server.messages("Drafts");
    assert_eq!(drafts_on_server.len(), 1);
    assert_eq!(drafts_on_server[0].flags, Flags::SEEN | Flags::DRAFT);
    wait_for("the draft locally", || h.uids("Drafts").len() == 1).await;

    // Replaying the same create again is harmless (idempotent).
    let again = h
        .store
        .write(move |w| {
            OpPayload::CreateFolder {
                name: "Done".into(),
                folder: None,
                subscribe: false,
            }
            .enqueue(w, account, "user")
        })
        .await
        .unwrap();
    handle.apply_op(again).unwrap();
    wait_for("the queue to drain", || ops_empty(&h)).await;
    handle.shutdown().await;
}
