//! Bodies: interactive priority, partial fetch of large messages, the offline policy.

use bytes::Bytes;
use imap::fake::{FakeMessage, FakeServer};
use imap::{BodyPart, BodySummary};
use mazemail_types::{MessageId, Sensitive};
use pretty_assertions::assert_eq;
use store::{BodyState, Queries};
use sync::{AccountHandle, BodyPriority, OfflinePolicy, SyncScope, SyncStatus};

use crate::common::{Harness, mail, wait_for, wait_idle};

fn text_part(section: &str, subtype: &str, size: u32) -> BodyPart {
    BodyPart {
        section: section.into(),
        mime_type: "text".into(),
        subtype: subtype.into(),
        charset: Some("utf-8".into()),
        encoding: "7bit".into(),
        size,
        disposition: None,
        filename: None,
        content_id: None,
        is_attachment: false,
    }
}

/// A 400 KB message: a short text part and a large attachment.
fn large_message() -> FakeMessage {
    let attachment = "A".repeat(400 * 1024);
    let raw = format!(
        "From: a@example.org\r\nSubject: Big\r\nMessage-ID: <big@example.org>\r\n\
         MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
         --b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nSee attachment\r\n\
         --b\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"x.bin\"\r\n\r\n\
         {attachment}\r\n--b--\r\n"
    );
    FakeMessage::new(raw)
        .body(BodySummary {
            parts: vec![
                text_part("1", "plain", 16),
                BodyPart {
                    section: "2".into(),
                    mime_type: "application".into(),
                    subtype: "octet-stream".into(),
                    charset: None,
                    encoding: "7bit".into(),
                    size: 400 * 1024,
                    disposition: Some("attachment".into()),
                    filename: Some(Sensitive::new("x.bin".into())),
                    content_id: None,
                    is_attachment: true,
                },
            ],
        })
        .part("1", Bytes::from_static(b"See attachment\r\n"))
}

#[tokio::test(flavor = "multi_thread")]
async fn opening_a_large_message_fetches_text_parts_only() {
    let server = FakeServer::new();
    server.deliver("INBOX", large_message());
    server.deliver("INBOX", mail(2));
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    let big = h.message_at("INBOX", 1);
    let small = h.message_at("INBOX", 2);

    let before = server.command_log().len();
    let state = handle
        .fetch_body(big, BodyPriority::Interactive)
        .await
        .unwrap();
    assert_eq!(state, BodyState::TextParts);
    let log: Vec<String> = server.command_log().into_iter().skip(before).collect();
    assert!(
        log.iter().any(|c| c == "UID FETCH 1 BODY.PEEK[1]"),
        "{log:?}"
    );
    assert!(!log.iter().any(|c| c.ends_with("BODY.PEEK[]")), "{log:?}");
    let detail = h.message(big);
    assert_eq!(detail.body_state, BodyState::TextParts);
    let blob = h.store.blobs().read(&detail.blob.unwrap()).unwrap();
    let parsed = mazemail_mime::parse(&blob).unwrap();
    assert_eq!(parsed.text_body.unwrap().text.trim(), "See attachment");
    // Opening again is instant; a background request completes the body.
    assert_eq!(
        handle
            .fetch_body(big, BodyPriority::Interactive)
            .await
            .unwrap(),
        BodyState::TextParts
    );
    assert_eq!(
        handle
            .fetch_body(big, BodyPriority::Background)
            .await
            .unwrap(),
        BodyState::Full
    );
    // Small messages come whole.
    assert_eq!(
        handle
            .fetch_body(small, BodyPriority::Interactive)
            .await
            .unwrap(),
        BodyState::Full
    );
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_opened_message_jumps_ahead_of_a_long_sync() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail(1));
    server.add_folder("Bulk", None);
    let h = Harness::new(server.clone()).await;
    let mut config = h.config();
    config.workers = 1;
    config.header_batch = 10;
    let handle = h.spawn(config);
    wait_idle(&handle).await;
    let message = h.message_at("INBOX", 1);

    for n in 0..3000 {
        server.deliver("Bulk", mail(1000 + n));
    }
    handle
        .sync_now(SyncScope::Folder(h.folder_id("Bulk")))
        .unwrap();
    wait_for("the bulk sync to start", || h.folder("Bulk").total > 0).await;
    let state = handle
        .fetch_body(message, BodyPriority::Interactive)
        .await
        .unwrap();
    assert_eq!(state, BodyState::Full);
    // The single session served the body before the long pass finished.
    let bulk = h.folder("Bulk");
    assert!(bulk.total < 3000, "pass already done: {}", bulk.total);
    assert_ne!(bulk.sync.uidnext, Some(3001));
    wait_idle(&handle).await;
    assert_eq!(h.folder("Bulk").total, 3000);
    // The resumed pass didn't fetch any header twice.
    let fetched: usize = server
        .command_log()
        .iter()
        .filter(|c| c.ends_with("HEADERS"))
        .count();
    assert!(fetched <= 3000 / 10 + 2, "{fetched} header fetches");
    handle.shutdown().await;
}

/// A message whose `Date:` header is `date_days` days ago and which the server
/// received (`INTERNALDATE`) `received_days` days ago.
fn mail_dated(n: u32, date_days: i64, received_days: i64) -> FakeMessage {
    let now = time::OffsetDateTime::now_utc();
    let date = (now - time::Duration::days(date_days))
        .format(&time::format_description::well_known::Rfc2822)
        .unwrap();
    FakeMessage::new(format!(
        "From: Sender {n} <s{n}@example.org>\r\nTo: me@example.org\r\n\
         Subject: Message {n}\r\nMessage-ID: <m{n}@example.org>\r\nDate: {date}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\nBody of message {n}\r\n"
    ))
    .internal_date(now - time::Duration::days(received_days))
}

/// Waits until `messages` have their bodies and the background queue is empty: the
/// policy queues each pass's (and the backfill's) bodies at once, so whatever it
/// queued together with `messages` has been fetched by then too.
async fn wait_for_bodies(h: &Harness, handle: &AccountHandle, messages: &[MessageId]) {
    wait_for("background bodies", || {
        messages
            .iter()
            .all(|m| h.message(*m).body_state == BodyState::Full)
            && handle.activity().bodies_queued == 0
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_policy_downloads_recent_bodies_in_the_background() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail_dated(1, 200, 200));
    server.deliver("INBOX", mail_dated(2, 3, 3));
    server.deliver("Sent", mail_dated(3, 1, 1));
    let h = Harness::new(server.clone()).await;
    let config = h.config().with_offline(OfflinePolicy::Recent, 90);
    let handle = h.spawn(config);
    wait_idle(&handle).await;
    let old = h.message_at("INBOX", 1);
    let recent = h.message_at("INBOX", 2);
    let sent = h.message_at("Sent", 1);
    wait_for_bodies(&h, &handle, &[recent, sent]).await;
    assert_eq!(h.message(old).body_state, BodyState::None);
    handle.shutdown().await;

    // Everything: the backfill finds the old one after a restart.
    let handle = h.spawn(h.config().with_offline(OfflinePolicy::All, 0));
    wait_for("the backfill", || {
        h.message(old).body_state == BodyState::Full
    })
    .await;
    handle.shutdown().await;
}

/// The messages of [`offline_policy_goes_by_the_message_date`]: `Date:` header and
/// `INTERNALDATE` agree on the first and the last, not on the two in between.
fn mixed_dates() -> FakeServer {
    let server = FakeServer::new();
    server.deliver("INBOX", mail_dated(1, 200, 200));
    // Old, but the server got it yesterday (a migrated mailbox, a re-upload).
    server.deliver("INBOX", mail_dated(2, 200, 1));
    // Recent, with an old `INTERNALDATE`.
    server.deliver("INBOX", mail_dated(3, 3, 200));
    server.deliver("INBOX", mail_dated(4, 3, 3));
    server
}

/// Issue #5: the folder pass judged a message's age by `INTERNALDATE`, the backfill by
/// its date (the `Date:` header, as the list shows it), so each fetched bodies the
/// other left out. Both go by the message date now.
#[tokio::test(flavor = "multi_thread")]
async fn offline_policy_goes_by_the_message_date() {
    // The folder pass: every message is new.
    let h = Harness::new(mixed_dates()).await;
    let handle = h.spawn(h.config().with_offline(OfflinePolicy::Recent, 90));
    wait_idle(&handle).await;
    let ids: Vec<MessageId> = (1..=4).map(|uid| h.message_at("INBOX", uid)).collect();
    wait_for_bodies(&h, &handle, &[ids[2], ids[3]]).await;
    assert_eq!(h.message(ids[0]).body_state, BodyState::None);
    assert_eq!(h.message(ids[1]).body_state, BodyState::None);
    handle.shutdown().await;

    // The backfill: the headers are there, the policy changes to `recent`.
    let h = Harness::new(mixed_dates()).await;
    let handle = h.spawn(h.config().with_offline(OfflinePolicy::HeadersOnly, 0));
    wait_idle(&handle).await;
    handle.shutdown().await;
    let ids: Vec<MessageId> = (1..=4).map(|uid| h.message_at("INBOX", uid)).collect();
    assert!(
        ids.iter()
            .all(|m| h.message(*m).body_state == BodyState::None)
    );
    let handle = h.spawn(h.config().with_offline(OfflinePolicy::Recent, 90));
    wait_for_bodies(&h, &handle, &[ids[2], ids[3]]).await;
    assert_eq!(h.message(ids[0]).body_state, BodyState::None);
    assert_eq!(h.message(ids[1]).body_state, BodyState::None);
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_body_fails_fast_when_offline() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail(1));
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    let message = h.message_at("INBOX", 1);
    handle.shutdown().await;

    h.factory
        .online
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let handle = h.spawn(h.config());
    crate::common::wait_status(&handle, "offline", |s| {
        matches!(s, SyncStatus::Offline { .. })
    })
    .await;
    let result = handle.fetch_body(message, BodyPriority::Interactive).await;
    assert!(
        matches!(result, Err(sync::SyncError::Offline)),
        "{result:?}"
    );
    let reader = h.store.reader().unwrap();
    assert_eq!(
        reader.message(message).unwrap().unwrap().body_state,
        BodyState::None
    );
    handle.shutdown().await;
}
