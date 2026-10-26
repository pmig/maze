//! Integration tests of `ImapClient` against a real Stalwart server.
//!
//! Ignored by default because they download and run a server binary. Run them with
//!
//! ```sh
//! MAZEMAIL_TEST_IMAP=1 cargo nextest run -p imap --run-ignored only
//! ```

mod support;

use std::time::Duration;

use imap::{
    CancellationToken, FlagOp, IdleOutcome, ImapAuth, ImapClient, ImapConfig, ImapSession,
    QresyncArgs, Security, UidSet,
};
use mazemail_types::{Flags, FolderRole};
use support::Stalwart;

#[allow(clippy::print_stderr)] // Tell the developer why the test did nothing.
fn server() -> Option<Stalwart> {
    if !support::enabled() {
        eprintln!("skipping: MAZEMAIL_TEST_IMAP is not 1");
        return None;
    }
    Some(Stalwart::start().expect("start stalwart"))
}

async fn connect(server: &Stalwart) -> ImapClient {
    let mut config = ImapConfig::new(
        "127.0.0.1",
        server.port,
        ImapAuth::password(support::USER, support::PASSWORD),
    )
    .with_security(Security::PlainInsecure)
    .with_command_timeout(Duration::from_secs(10));
    config.client_id = Some(vec![("name".into(), "Maze tests".into())]);
    ImapClient::connect(&config).await.expect("connect")
}

fn message(subject: &str, id: &str) -> Vec<u8> {
    format!(
        "From: Alice <alice@example.org>\r\nTo: Bob <bob@example.org>\r\nSubject: {subject}\r\n\
         Message-ID: <{id}@example.org>\r\nDate: Thu, 22 Oct 2026 10:00:00 +0000\r\n\
         MIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
         --b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHello\r\n\
         --b\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hello</p>\r\n--b--\r\n"
    )
    .into_bytes()
}

#[tokio::test]
#[ignore = "needs a local Stalwart; set MAZEMAIL_TEST_IMAP=1 and run ignored tests"]
async fn capabilities_and_folders() {
    let Some(server) = server() else { return };
    let mut s = connect(&server).await;
    let caps = s.capabilities().await.unwrap();
    assert!(caps.idle() && caps.has_move() && caps.uidplus() && caps.condstore());
    assert!(s.qresync_enabled(), "Stalwart supports QRESYNC");

    s.create("Projekte/Entwürfe").await.unwrap();
    let folders = s.list().await.unwrap();
    let inbox = folders.iter().find(|f| f.name == "INBOX").unwrap();
    assert_eq!(inbox.role, Some(FolderRole::Inbox));
    assert!(folders.iter().any(|f| f.role == Some(FolderRole::Sent)));
    assert!(folders.iter().any(|f| f.role == Some(FolderRole::Trash)));
    let created = folders
        .iter()
        .find(|f| f.name == "Projekte/Entwürfe")
        .unwrap();
    assert_eq!(created.raw_name, "Projekte/Entw&APw-rfe");

    s.subscribe("Projekte/Entwürfe", true).await.unwrap();
    assert!(
        s.list()
            .await
            .unwrap()
            .iter()
            .any(|f| f.name == "Projekte/Entwürfe" && f.subscribed)
    );
    s.rename("Projekte/Entwürfe", "Projekte/Archiv 2026")
        .await
        .unwrap();
    s.delete("Projekte/Archiv 2026").await.unwrap();
    assert!(
        !s.list()
            .await
            .unwrap()
            .iter()
            .any(|f| f.name.starts_with("Projekte/A"))
    );
    s.logout().await.unwrap();
}

#[tokio::test]
#[ignore = "needs a local Stalwart; set MAZEMAIL_TEST_IMAP=1 and run ignored tests"]
async fn append_fetch_store_and_search() {
    let Some(server) = server() else { return };
    let mut s = connect(&server).await;
    let appended = s
        .append("INBOX", &message("First", "m1"), Flags::SEEN, &[])
        .await
        .unwrap()
        .expect("APPENDUID");
    s.append("INBOX", &message("Second", "m2"), Flags::empty(), &[])
        .await
        .unwrap();

    let info = s.select("INBOX", None).await.unwrap();
    assert_eq!(info.uidvalidity, appended.uidvalidity);
    assert_eq!(info.exists, 2);
    assert!(info.highest_modseq.is_some());

    let headers = s.fetch_headers(&UidSet::all()).await.unwrap();
    assert_eq!(headers.len(), 2);
    let first = &headers[0];
    assert_eq!(first.uid, appended.uid);
    assert_eq!(first.flags, Flags::SEEN);
    assert!(
        first
            .header
            .expose()
            .windows(14)
            .any(|w| w == b"Subject: First")
    );
    assert!(first.internal_date.is_some());
    assert!(first.modseq.is_some());
    let body = first.body.as_ref().unwrap();
    let sections: Vec<&str> = body.text_parts().map(|p| p.section.as_str()).collect();
    assert_eq!(sections, ["1", "2"]);

    let html = s.fetch_part(first.uid, "2").await.unwrap();
    assert_eq!(&html[..], b"<p>Hello</p>");
    let raw = s.fetch_raw(first.uid).await.unwrap();
    assert_eq!(&raw[..], &message("First", "m1")[..]);

    let all = s.uid_search_all().await.unwrap();
    assert_eq!(all.len(), Some(2));

    s.store_flags(
        &all,
        &FlagOp::add(Flags::FLAGGED | Flags::FORWARDED).with_keywords(["Work"]),
    )
    .await
    .unwrap();
    let flags = s.fetch_flags(&all).await.unwrap();
    assert!(
        flags
            .iter()
            .all(|f| f.flags.contains(Flags::FLAGGED | Flags::FORWARDED))
    );
    assert!(flags.iter().all(|f| f.keywords == ["Work"]));

    // Absolute set replaces everything, including keywords.
    s.store_flags(&UidSet::single(first.uid), &FlagOp::set(Flags::SEEN))
        .await
        .unwrap();
    let flags = s.fetch_flags(&UidSet::single(first.uid)).await.unwrap();
    assert_eq!(flags[0].flags, Flags::SEEN);
    assert!(flags[0].keywords.is_empty());

    let folders = s.list().await.unwrap();
    let sent = folders
        .iter()
        .find(|f| f.role == Some(FolderRole::Sent))
        .unwrap();
    let status = s.status(&sent.name).await.unwrap();
    assert_eq!(status.messages, Some(0));
    assert!(status.uidvalidity.is_some() && status.highest_modseq.is_some());
}

#[tokio::test]
#[ignore = "needs a local Stalwart; set MAZEMAIL_TEST_IMAP=1 and run ignored tests"]
async fn condstore_and_qresync_resync() {
    let Some(server) = server() else { return };
    let mut a = connect(&server).await;
    for n in 0..4 {
        a.append(
            "INBOX",
            &message("Msg", &format!("q{n}")),
            Flags::empty(),
            &[],
        )
        .await
        .unwrap();
    }
    let info = a.select("INBOX", None).await.unwrap();
    let modseq = info.highest_modseq.unwrap();
    let uids: Vec<u32> = a
        .uid_search_all()
        .await
        .unwrap()
        .iter_up_to(u32::MAX)
        .collect();

    // Another session changes a flag and expunges a message.
    let mut b = connect(&server).await;
    b.select("INBOX", None).await.unwrap();
    b.store_flags(&UidSet::single(uids[0]), &FlagOp::add(Flags::SEEN))
        .await
        .unwrap();
    b.store_flags(&UidSet::single(uids[1]), &FlagOp::add(Flags::DELETED))
        .await
        .unwrap();
    b.expunge_uids(&UidSet::single(uids[1])).await.unwrap();

    let changes = a.fetch_changes(modseq).await.unwrap();
    assert!(
        changes
            .changed
            .iter()
            .any(|c| c.uid == uids[0] && c.flags.contains(Flags::SEEN))
    );
    assert!(changes.vanished.contains(uids[1]));

    let mut c = connect(&server).await;
    let resync = c
        .select(
            "INBOX",
            Some(QresyncArgs {
                uidvalidity: info.uidvalidity,
                highest_modseq: modseq,
                known_uids: None,
            }),
        )
        .await
        .unwrap();
    assert!(resync.qresync_applied);
    assert!(resync.vanished.contains(uids[1]));
    assert!(resync.changed.iter().any(|ch| ch.uid == uids[0]));
}

#[tokio::test]
#[ignore = "needs a local Stalwart; set MAZEMAIL_TEST_IMAP=1 and run ignored tests"]
async fn move_and_copy_report_copyuid() {
    let Some(server) = server() else { return };
    let mut s = connect(&server).await;
    s.create("Archive 2026").await.unwrap();
    for n in 0..3 {
        s.append(
            "INBOX",
            &message("Move me", &format!("mv{n}")),
            Flags::empty(),
            &[],
        )
        .await
        .unwrap();
    }
    s.select("INBOX", None).await.unwrap();
    let uids = s.uid_search_all().await.unwrap();
    let first_two: UidSet = uids.iter_up_to(u32::MAX).take(2).collect();
    let moved = s
        .r#move(&first_two, "Archive 2026")
        .await
        .unwrap()
        .expect("COPYUID");
    assert_eq!(moved.source.len(), 2);
    assert_eq!(moved.destination.len(), 2);
    assert_eq!(s.uid_search_all().await.unwrap().len(), Some(1));

    let rest = s.uid_search_all().await.unwrap();
    let copied = s
        .copy(&rest, "Archive 2026")
        .await
        .unwrap()
        .expect("COPYUID");
    assert_eq!(copied.uidvalidity, moved.uidvalidity);

    let status = s.status("Archive 2026").await.unwrap();
    assert_eq!(status.messages, Some(3));
}

#[tokio::test]
#[ignore = "needs a local Stalwart; set MAZEMAIL_TEST_IMAP=1 and run ignored tests"]
async fn idle_sees_new_mail_from_another_session() {
    let Some(server) = server() else { return };
    let mut watcher = connect(&server).await;
    watcher.select("INBOX", None).await.unwrap();
    let mut writer = connect(&server).await;

    let idle = tokio::spawn(async move {
        let outcome = watcher
            .idle(CancellationToken::new(), Duration::from_secs(20))
            .await
            .unwrap();
        (outcome, watcher.take_events())
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    writer
        .append("INBOX", &message("Ping", "idle1"), Flags::empty(), &[])
        .await
        .unwrap();
    let (outcome, events) = idle.await.unwrap();
    assert_eq!(outcome, IdleOutcome::NewData);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, imap::SessionEvent::Exists(1))),
        "{events:?}"
    );

    // Timeout and cancellation leave the session usable.
    let mut watcher = connect(&server).await;
    watcher.select("INBOX", None).await.unwrap();
    let outcome = watcher
        .idle(CancellationToken::new(), Duration::from_millis(300))
        .await
        .unwrap();
    assert_eq!(outcome, IdleOutcome::Timeout);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger.cancel();
    });
    assert_eq!(
        watcher.idle(cancel, Duration::from_secs(20)).await.unwrap(),
        IdleOutcome::Cancelled
    );
    watcher.noop().await.unwrap();
}
