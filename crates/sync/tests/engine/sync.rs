//! Folder passes: initial sync, restart deltas, IDLE, CONDSTORE, expunges,
//! UIDVALIDITY, offline and recovery.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use imap::fake::FakeServer;
use mazemail_types::{Flags, FolderRole};
use pretty_assertions::assert_eq;
use store::BodyState;
use sync::{BodyPriority, SyncScope, SyncStatus};

use crate::common::{Harness, log_since, mail, wait_for, wait_idle, wait_status};

/// Caps of a CONDSTORE server without QRESYNC.
const CONDSTORE_CAPS: &[&str] = &[
    "IMAP4rev1",
    "IDLE",
    "MOVE",
    "UIDPLUS",
    "CONDSTORE",
    "SPECIAL-USE",
];

#[tokio::test(flavor = "multi_thread")]
async fn initial_sync_of_several_folders_fetches_newest_first() {
    let server = FakeServer::new();
    server.add_folder("Projects", None);
    for n in 1..=5 {
        let flags = if n <= 2 { Flags::SEEN } else { Flags::empty() };
        server.deliver("INBOX", mail(n).flags(flags));
    }
    for n in 10..13 {
        server.deliver("Sent", mail(n).flags(Flags::SEEN));
    }
    for n in 20..22 {
        server.deliver("Projects", mail(n));
    }
    let h = Harness::new(server.clone()).await;
    let mut config = h.config();
    config.header_batch = 2;
    let handle = h.spawn(config);
    let mut events = handle.subscribe();
    wait_idle(&handle).await;

    let inbox = h.folder("INBOX");
    assert_eq!(inbox.role, Some(FolderRole::Inbox));
    assert_eq!(inbox.display_name, "Inbox");
    assert_eq!((inbox.total, inbox.unread), (5, 3));
    let sent = h.folder("Sent");
    assert_eq!(sent.role, Some(FolderRole::Sent));
    assert_eq!((sent.total, sent.unread), (3, 0));
    assert_eq!(h.folder("Projects").total, 2);
    for name in ["Drafts", "Trash", "Junk", "Archive"] {
        assert_eq!(h.folder(name).total, 0, "{name}");
    }
    assert_eq!(inbox.sync.uidnext, Some(6));
    assert!(inbox.sync.highestmodseq.is_some());

    // Newest first, in batches of two.
    let log = server.command_log();
    let start = log.iter().position(|c| c == "SELECT INBOX").unwrap();
    let fetches: Vec<&String> = log[start..]
        .iter()
        .take_while(|c| !c.starts_with("SELECT") || *c == "SELECT INBOX")
        .filter(|c| c.ends_with("HEADERS"))
        .collect();
    assert_eq!(
        fetches,
        vec![
            "UID FETCH 4:5 HEADERS",
            "UID FETCH 2:3 HEADERS",
            "UID FETCH 1 HEADERS"
        ]
    );

    // The events announced every INBOX message. They are coalesced, so the last batch
    // can arrive a moment after the actor went idle: wait for it instead of polling.
    let mut added = BTreeSet::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while added.len() < 5 {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(batch)) => {
                if let Some(set) = batch.added.get(&inbox.id) {
                    added.extend(set.iter().copied());
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(_)) | Err(_) => break,
        }
    }
    assert_eq!(added.len(), 5);
    handle.shutdown().await;
    assert_eq!(handle.status(), SyncStatus::Stopped);
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_fetches_only_the_delta() {
    let server = FakeServer::new();
    for n in 1..=4 {
        server.deliver("INBOX", mail(n));
    }
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    handle.shutdown().await;

    // Meanwhile: new mail, a flag change and an expunge by another client.
    server.deliver("INBOX", mail(5));
    server.set_flags("INBOX", 1, Flags::SEEN | Flags::FLAGGED, &["$label1"]);
    server.expunge("INBOX", 2);
    let before = server.command_log().len();
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;

    let log = log_since(&server, before);
    let header_fetches: Vec<&String> = log.iter().filter(|c| c.ends_with("HEADERS")).collect();
    assert_eq!(header_fetches, vec!["UID FETCH 5 HEADERS"], "{log:?}");
    assert!(!log.iter().any(|c| c.starts_with("UID SEARCH")), "{log:?}");
    let uids = h.uids("INBOX");
    assert_eq!(
        uids.iter().map(|(u, _)| *u).collect::<Vec<_>>(),
        vec![1, 3, 4, 5]
    );
    assert_eq!(uids[0].1, Flags::SEEN | Flags::FLAGGED);
    let first = h.message(h.message_at("INBOX", 1));
    assert_eq!(first.locations[0].keywords, vec!["$label1".to_owned()]);
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn new_mail_arrives_through_idle() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail(1));
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    // Wait until the IDLE session is in place.
    wait_for("IDLE", || server.command_log().iter().any(|c| c == "IDLE")).await;

    server.deliver("INBOX", mail(2));
    wait_for("the new message", || h.uids("INBOX").len() == 2).await;
    // Polling is off (1 h), so IDLE delivered it.
    server.set_flags("INBOX", 2, Flags::SEEN, &[]);
    wait_for("the flag change", || {
        h.uids("INBOX")
            .iter()
            .any(|(u, f)| *u == 2 && f.contains(Flags::SEEN))
    })
    .await;
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn condstore_picks_up_flags_from_another_client() {
    let server = FakeServer::new();
    server.set_caps(CONDSTORE_CAPS);
    for n in 1..=3 {
        server.deliver("INBOX", mail(n));
    }
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    let inbox = h.folder_id("INBOX");

    server.set_flags("INBOX", 3, Flags::FLAGGED | Flags::ANSWERED, &[]);
    let before = server.command_log().len();
    handle.sync_now(SyncScope::Folder(inbox)).unwrap();
    wait_for("the flag change", || {
        h.uids("INBOX")
            .iter()
            .any(|(u, f)| *u == 3 && *f == Flags::FLAGGED | Flags::ANSWERED)
    })
    .await;
    let log = log_since(&server, before);
    assert!(log.iter().any(|c| c.contains("CHANGEDSINCE")), "{log:?}");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn qresync_vanished_removes_expunged_messages() {
    let server = FakeServer::new();
    for n in 1..=3 {
        server.deliver("INBOX", mail(n));
    }
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    let removed = h.message_at("INBOX", 2);

    server.expunge("INBOX", 2);
    let before = server.command_log().len();
    handle
        .sync_now(SyncScope::Folder(h.folder_id("INBOX")))
        .unwrap();
    wait_for("the expunge", || h.uids("INBOX").len() == 2).await;
    let log = log_since(&server, before);
    assert!(!log.iter().any(|c| c.starts_with("UID SEARCH")), "{log:?}");
    assert!(h.message(removed).detached_at.is_some());
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn uid_search_diff_finds_expunges_without_qresync() {
    let server = FakeServer::new();
    server.set_caps(CONDSTORE_CAPS);
    for n in 1..=4 {
        server.deliver("INBOX", mail(n));
    }
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;

    server.expunge("INBOX", 1);
    server.expunge("INBOX", 3);
    let before = server.command_log().len();
    handle
        .sync_now(SyncScope::Folder(h.folder_id("INBOX")))
        .unwrap();
    wait_for("the expunges", || h.uids("INBOX").len() == 2).await;
    let log = log_since(&server, before);
    assert!(log.iter().any(|c| c == "UID SEARCH ALL"), "{log:?}");
    assert_eq!(
        h.uids("INBOX").iter().map(|(u, _)| *u).collect::<Vec<_>>(),
        vec![2, 4]
    );
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn flag_diff_without_condstore() {
    let server = FakeServer::new();
    server.set_caps(&["IMAP4rev1", "IDLE", "MOVE", "UIDPLUS", "SPECIAL-USE"]);
    for n in 1..=3 {
        server.deliver("INBOX", mail(n));
    }
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;

    server.set_flags("INBOX", 1, Flags::SEEN, &[]);
    server.expunge("INBOX", 2);
    server.deliver("INBOX", mail(4));
    handle
        .sync_now(SyncScope::Folder(h.folder_id("INBOX")))
        .unwrap();
    wait_for("the diff", || {
        let uids = h.uids("INBOX");
        uids.len() == 3 && uids[0] == (1, Flags::SEEN) && uids[2].0 == 4
    })
    .await;
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn uidvalidity_change_resets_the_folder_and_keeps_bodies() {
    let server = FakeServer::new();
    for n in 1..=3 {
        server.deliver("INBOX", mail(n));
    }
    server.expunge("INBOX", 1);
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    let message = h.message_at("INBOX", 3);
    let state = handle
        .fetch_body(message, BodyPriority::Interactive)
        .await
        .unwrap();
    assert_eq!(state, BodyState::Full);
    let old_validity = h.folder("INBOX").sync.uidvalidity;

    // The server renumbers: UIDs 2, 3 become 1, 2.
    let validity = server.reset_uidvalidity("INBOX");
    let mut events = handle.subscribe();
    handle
        .sync_now(SyncScope::Folder(h.folder_id("INBOX")))
        .unwrap();
    wait_for("the reset", || {
        h.folder("INBOX").sync.uidvalidity == Some(validity) && h.uids("INBOX").len() == 2
    })
    .await;
    assert_ne!(old_validity, Some(validity));
    assert_eq!(
        h.uids("INBOX").iter().map(|(u, _)| *u).collect::<Vec<_>>(),
        vec![1, 2]
    );
    // The same message, found again by Message-ID, with its body.
    assert_eq!(h.message_at("INBOX", 2), message);
    assert_eq!(h.message(message).body_state, BodyState::Full);
    let mut reset_seen = false;
    while let Ok(batch) =
        tokio::time::timeout(std::time::Duration::from_millis(200), events.recv()).await
    {
        reset_seen |= batch.unwrap().folders_reset.contains(&h.folder_id("INBOX"));
    }
    assert!(reset_seen);
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_then_recovery() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail(1));
    let h = Harness::new(server.clone()).await;
    h.factory.online.store(false, Ordering::SeqCst);
    let handle = h.spawn(h.config());
    wait_status(&handle, "offline", |s| {
        matches!(s, SyncStatus::Offline { .. })
    })
    .await;
    let text = handle.status().text();
    assert!(text.starts_with("Offline: network error"), "{text}");
    // Opening a message while offline fails fast.
    assert!(h.try_folder("INBOX").is_none());

    h.factory.online.store(true, Ordering::SeqCst);
    wait_idle(&handle).await;
    assert_eq!(h.uids("INBOX").len(), 1);
    assert!(h.factory.connects.load(Ordering::SeqCst) >= 2);

    // A connection drop mid-session: the job is retried on a new session.
    server.deliver("INBOX", mail(2));
    server.fail_command("SELECT", imap::fake::FakeFailure::ConnectionLost);
    handle
        .sync_now(SyncScope::Folder(h.folder_id("INBOX")))
        .unwrap();
    wait_for("the retried pass", || h.uids("INBOX").len() == 2).await;
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn network_change_reconnects_and_syncs() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail(1));
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    let connects = h.factory.connects.load(Ordering::SeqCst);
    server.deliver("Archive", mail(2));
    handle.network_changed().unwrap();
    wait_for("the archive pass", || h.uids("Archive").len() == 1).await;
    assert!(h.factory.connects.load(Ordering::SeqCst) > connects);
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_polling_finds_changes_in_other_folders() {
    let server = FakeServer::new();
    let h = Harness::new(server.clone()).await;
    let mut config = h.config();
    config.timing.poll_interval = std::time::Duration::from_millis(100);
    let handle = h.spawn(config);
    wait_idle(&handle).await;
    server.deliver("Archive", mail(1));
    wait_for("the polled change", || h.uids("Archive").len() == 1).await;
    let log = server.command_log();
    assert!(log.iter().any(|c| c == "STATUS Archive"), "{log:?}");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn watched_folders_are_polled_with_the_open_folder() {
    let server = FakeServer::new();
    server.deliver("Archive", mail(1));
    let h = Harness::new(server.clone()).await;
    let mut config = h.config();
    // Only the 30 s poll is fast here; the poll of the other folders never comes.
    config.timing.open_folder_poll = std::time::Duration::from_millis(100);
    let handle = h.spawn(config);
    wait_idle(&handle).await;
    handle.watch_folders(vec![h.folder_id("Archive")]).unwrap();
    server.deliver("Archive", mail(2));
    wait_for("the watched folder's new mail", || {
        h.uids("Archive").len() == 2
    })
    .await;
    let log = server.command_log();
    assert!(log.iter().any(|c| c == "STATUS Archive"), "{log:?}");
    handle.shutdown().await;
}

/// The activity snapshot shows the running pass with its header progress, then
/// nothing running and the time of the finished round.
#[tokio::test(flavor = "multi_thread")]
async fn activity_shows_folder_progress_and_the_last_round() {
    use sync::FolderPhase;

    let server = FakeServer::new();
    server.deliver("INBOX", mail(1));
    server.add_folder("Bulk", None);
    let h = Harness::new(server.clone()).await;
    let mut config = h.config();
    config.workers = 1;
    config.header_batch = 10;
    let handle = h.spawn(config);
    wait_idle(&handle).await;
    let first = handle.activity();
    assert!(first.folders.is_empty(), "{first:?}");
    assert!(first.last_synced.is_some(), "the first round finished");

    for n in 0..3000 {
        server.deliver("Bulk", mail(1000 + n));
    }
    let bulk = h.folder_id("Bulk");
    handle.sync_now(SyncScope::Folder(bulk)).unwrap();
    wait_for("header progress of Bulk", || {
        handle.activity().folders.iter().any(|f| {
            f.folder == bulk
                && f.phase == FolderPhase::FetchingHeaders
                && f.total == Some(3000)
                && f.done > 0
                && f.done < 3000
        })
    })
    .await;
    wait_idle(&handle).await;
    let done = handle.activity();
    assert!(done.folders.is_empty(), "{done:?}");
    assert!(done.last_synced >= first.last_synced);
    handle.shutdown().await;
}
