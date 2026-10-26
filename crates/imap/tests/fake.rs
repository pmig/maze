//! Tests for the in-memory `FakeImap`, used through the public `ImapSession` API
//! exactly as the sync engine will use it.

use std::time::Duration;

use imap::fake::{FakeFailure, FakeMessage, FakeServer};
use imap::{
    CancellationToken, FlagOp, GmailLabel, IdleOutcome, ImapError, ImapSession, LabelOp,
    QresyncArgs, SessionEvent, UidSet,
};
use mazemail_types::{Flags, FolderRole};
use pretty_assertions::assert_eq;

fn msg(n: u32) -> FakeMessage {
    FakeMessage::simple(
        &format!("Subject {n}"),
        &format!("m{n}@example.org"),
        "Hello",
    )
}

#[tokio::test]
async fn deliver_select_and_fetch() {
    let server = FakeServer::new();
    let uid1 = server.deliver("INBOX", msg(1).flags(Flags::SEEN));
    let uid2 = server.deliver(
        "INBOX",
        msg(2)
            .keywords(["$label1"])
            .part("1", "Hello")
            .internal_date(time::macros::datetime!(2024-05-01 12:00 UTC)),
    );
    assert_eq!((uid1, uid2), (1, 2));

    let mut session = server.connect();
    let info = session.select("inbox", None).await.unwrap();
    assert_eq!(info.exists, 2);
    assert_eq!(info.uidnext, Some(3));
    assert!(info.highest_modseq.is_some());
    assert!(info.allows_new_keywords);

    let headers = session.fetch_headers(&UidSet::all()).await.unwrap();
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0].flags, Flags::SEEN);
    assert_eq!(headers[1].keywords, ["$label1"]);
    assert!(
        headers[1]
            .header
            .expose()
            .ends_with(b"charset=utf-8\r\n\r\n")
    );
    assert_eq!(
        headers[1].internal_date.unwrap().year(),
        2024,
        "internal date is kept"
    );
    assert_eq!(headers[0].body.as_ref().unwrap().text_parts().count(), 1);
    assert!(headers[0].gmail.is_none());

    assert_eq!(&session.fetch_part(2, "1").await.unwrap()[..], b"Hello");
    assert_eq!(&session.fetch_part(2, "TEXT").await.unwrap()[..], b"Hello");
    assert!(
        session
            .fetch_raw(1)
            .await
            .unwrap()
            .starts_with(b"Message-ID: <m1@")
    );
    assert!(matches!(
        session.fetch_raw(9).await,
        Err(ImapError::MessageNotFound { uid: 9 })
    ));
    assert!(matches!(
        session.fetch_part(2, "3.1").await,
        Err(ImapError::PartNotFound { .. })
    ));
    assert_eq!(session.uid_search_all().await.unwrap().to_string(), "1:2");
}

#[tokio::test]
async fn message_commands_need_a_selected_folder() {
    let server = FakeServer::new();
    let mut session = server.connect();
    assert!(matches!(
        session.uid_search_all().await,
        Err(ImapError::NoFolderSelected)
    ));
    assert!(matches!(
        session.select("Nope", None).await,
        Err(ImapError::No { .. })
    ));
}

#[tokio::test]
async fn condstore_changes_and_qresync_select() {
    let server = FakeServer::new();
    for n in 1..=4 {
        server.deliver("INBOX", msg(n));
    }
    let mut session = server.connect();
    let info = session.select("INBOX", None).await.unwrap();
    let modseq = info.highest_modseq.unwrap();

    // Another client changes flags and expunges.
    server.set_flags("INBOX", 2, Flags::SEEN | Flags::FLAGGED, &["Work"]);
    server.expunge("INBOX", 3);

    let changes = session.fetch_changes(modseq).await.unwrap();
    assert_eq!(changes.changed.len(), 1);
    assert_eq!(changes.changed[0].uid, 2);
    assert_eq!(changes.changed[0].flags, Flags::SEEN | Flags::FLAGGED);
    assert_eq!(changes.changed[0].keywords, ["Work"]);
    assert_eq!(changes.vanished.to_string(), "3");

    // A fresh session resyncs with QRESYNC.
    let mut other = server.connect();
    let info2 = other
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
    assert!(info2.qresync_applied);
    assert_eq!(info2.vanished.to_string(), "3");
    assert_eq!(info2.changed.iter().map(|c| c.uid).collect::<Vec<_>>(), [2]);
    assert!(info2.highest_modseq.unwrap() > modseq);
}

#[tokio::test]
async fn uidvalidity_reset_invalidates_qresync() {
    let server = FakeServer::new();
    server.deliver("INBOX", msg(1));
    server.deliver("INBOX", msg(2));
    server.expunge("INBOX", 1);
    let mut session = server.connect();
    let before = session.select("INBOX", None).await.unwrap();
    let new_validity = server.reset_uidvalidity("INBOX");
    assert_ne!(new_validity, before.uidvalidity);
    let after = session
        .select(
            "INBOX",
            Some(QresyncArgs {
                uidvalidity: before.uidvalidity,
                highest_modseq: before.highest_modseq.unwrap(),
                known_uids: None,
            }),
        )
        .await
        .unwrap();
    assert_eq!(after.uidvalidity, new_validity);
    assert!(!after.qresync_applied);
    assert_eq!(session.uid_search_all().await.unwrap().to_string(), "1");
}

#[tokio::test]
async fn unsolicited_events_with_and_without_qresync() {
    let server = FakeServer::new();
    for n in 1..=3 {
        server.deliver("INBOX", msg(n));
    }
    let mut session = server.connect();
    session.select("INBOX", None).await.unwrap();
    server.expunge("INBOX", 2);
    server.set_flags("INBOX", 3, Flags::SEEN, &[]);
    server.deliver("INBOX", msg(4));
    session.noop().await.unwrap();
    let events = session.take_events();
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[0], SessionEvent::Vanished(UidSet::single(2)));
    assert!(matches!(
        events[1],
        SessionEvent::Flags {
            uid: Some(3),
            seq: 2,
            ..
        }
    ));
    assert_eq!(events[2], SessionEvent::Exists(3));

    // Without QRESYNC, expunges are reported by sequence number, highest first.
    let plain = FakeServer::new();
    plain.set_caps(&["IMAP4rev1", "IDLE", "UIDPLUS"]);
    for n in 1..=4 {
        plain.deliver("INBOX", msg(n));
    }
    let mut session = plain.connect();
    session.select("INBOX", None).await.unwrap();
    plain.expunge("INBOX", 2);
    plain.expunge("INBOX", 4);
    session.noop().await.unwrap();
    assert_eq!(
        session.take_events(),
        [SessionEvent::Expunge(4), SessionEvent::Expunge(2)]
    );
}

#[tokio::test]
async fn own_silent_store_is_not_reported() {
    let server = FakeServer::new();
    server.deliver("INBOX", msg(1));
    let mut session = server.connect();
    session.select("INBOX", None).await.unwrap();
    session
        .store_flags(
            &UidSet::single(1),
            &FlagOp::add(Flags::SEEN).with_keywords(["Work"]),
        )
        .await
        .unwrap();
    session.noop().await.unwrap();
    assert!(session.take_events().is_empty());
    let info = &server.messages("INBOX")[0];
    assert_eq!(info.flags, Flags::SEEN);
    assert_eq!(info.keywords, ["Work"]);

    session
        .store_flags(&UidSet::single(1), &FlagOp::set(Flags::FLAGGED))
        .await
        .unwrap();
    assert_eq!(server.messages("INBOX")[0].flags, Flags::FLAGGED);
    assert!(server.messages("INBOX")[0].keywords.is_empty());
    assert!(matches!(
        session
            .store_flags(
                &UidSet::single(1),
                &FlagOp::add(Flags::empty()).with_keywords(["no spaces"])
            )
            .await,
        Err(ImapError::InvalidInput(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn idle_wakes_on_delivery_times_out_and_cancels() {
    let server = FakeServer::new();
    let mut session = server.connect();
    session.select("INBOX", None).await.unwrap();

    let other = server.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        other.deliver("INBOX", msg(1));
    });
    let outcome = session
        .idle(CancellationToken::new(), Duration::from_secs(25 * 60))
        .await
        .unwrap();
    assert_eq!(outcome, IdleOutcome::NewData);
    assert_eq!(session.take_events(), [SessionEvent::Exists(1)]);

    // Changes in other folders don't wake the session.
    let other = server.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        other.deliver("Archive", msg(2));
    });
    let outcome = session
        .idle(CancellationToken::new(), Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(outcome, IdleOutcome::Timeout);

    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        trigger.cancel();
    });
    let outcome = session.idle(cancel, Duration::from_secs(60)).await.unwrap();
    assert_eq!(outcome, IdleOutcome::Cancelled);
}

#[tokio::test]
async fn move_copy_and_fallbacks() {
    let server = FakeServer::new();
    for n in 1..=3 {
        server.deliver("INBOX", msg(n));
    }
    let mut session = server.connect();
    session.select("INBOX", None).await.unwrap();
    let copy = session
        .r#move(&UidSet::range(1, 2), "Archive")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(copy.pairs().collect::<Vec<_>>(), [(1, 1), (2, 2)]);
    assert_eq!(server.messages("Archive").len(), 2);
    assert_eq!(
        session.take_events(),
        [SessionEvent::Vanished(UidSet::range(1, 2))]
    );
    let copied = session
        .copy(&UidSet::single(3), "Trash")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(copied.destination_of(3), Some(1));
    assert_eq!(server.messages("INBOX").len(), 1);
    assert!(matches!(
        session.r#move(&UidSet::single(3), "Missing").await,
        Err(ImapError::No { code: Some(code), .. }) if code == "TRYCREATE"
    ));

    // No MOVE, but UIDPLUS: copy + expunge.
    server.set_caps(&["IMAP4rev1", "UIDPLUS", "CONDSTORE"]);
    let copy = session.r#move(&UidSet::single(3), "Junk").await.unwrap();
    assert!(copy.is_some());
    assert!(server.messages("INBOX").is_empty());

    // Neither MOVE nor UIDPLUS: copy, message stays marked \Deleted, no COPYUID.
    server.set_caps(&["IMAP4rev1"]);
    session.select("Archive", None).await.unwrap();
    let copy = session.r#move(&UidSet::single(1), "INBOX").await.unwrap();
    assert!(copy.is_none());
    assert_eq!(server.messages("Archive")[0].flags, Flags::DELETED);
    assert_eq!(server.messages("INBOX").len(), 1);
    assert!(matches!(
        session.expunge_uids(&UidSet::single(1)).await,
        Err(ImapError::Unsupported("UIDPLUS"))
    ));
}

#[tokio::test]
async fn expunge_only_removes_deleted() {
    let server = FakeServer::new();
    server.deliver("INBOX", msg(1).flags(Flags::DELETED));
    server.deliver("INBOX", msg(2));
    let mut session = server.connect();
    session.select("INBOX", None).await.unwrap();
    session.expunge_uids(&UidSet::range(1, 2)).await.unwrap();
    assert_eq!(
        server
            .messages("INBOX")
            .iter()
            .map(|m| m.uid)
            .collect::<Vec<_>>(),
        [2]
    );
}

#[tokio::test]
async fn append_returns_appenduid() {
    let server = FakeServer::new();
    let mut session = server.connect();
    let appended = session
        .append("Sent", b"Subject: sent\r\n\r\nbody", Flags::SEEN, &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(appended.uid, 1);
    assert_eq!(Some(appended.uidvalidity), server.uidvalidity("Sent"));
    assert_eq!(server.messages("Sent")[0].flags, Flags::SEEN);
    assert!(matches!(
        session.append("Nope", b"x", Flags::empty(), &[]).await,
        Err(ImapError::No { code: Some(code), .. }) if code == "TRYCREATE"
    ));
    server.set_caps(&["IMAP4rev1"]);
    assert_eq!(
        session
            .append("Sent", b"x", Flags::empty(), &[])
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn folder_management_and_status() {
    let server = FakeServer::new();
    let mut session = server.connect();
    let folders = session.list().await.unwrap();
    assert_eq!(folders[0].name, "INBOX");
    assert_eq!(folders[0].role, Some(FolderRole::Inbox));
    let sent = folders.iter().find(|f| f.name == "Sent").unwrap();
    assert_eq!(sent.role, Some(FolderRole::Sent));
    assert!(sent.has_attribute("\\Sent"));

    session.create("Projekte").await.unwrap();
    session.create("Projekte/Ä").await.unwrap();
    assert!(matches!(
        session.create("Projekte").await,
        Err(ImapError::No { .. })
    ));
    session.rename("Projekte", "Work").await.unwrap();
    assert!(server.folder_names().contains(&"Work/Ä".to_owned()));
    session.subscribe("Work/Ä", true).await.unwrap();
    let folders = session.list().await.unwrap();
    let child = folders.iter().find(|f| f.name == "Work/Ä").unwrap();
    assert!(child.subscribed);
    assert_eq!(child.raw_name, "Work/&AMQ-");
    session.delete("Work/Ä").await.unwrap();
    assert!(matches!(
        session.delete("INBOX").await,
        Err(ImapError::No { .. })
    ));

    server.deliver("INBOX", msg(1).flags(Flags::SEEN));
    server.deliver("INBOX", msg(2));
    let status = session.status("INBOX").await.unwrap();
    assert_eq!(status.messages, Some(2));
    assert_eq!(status.unseen, Some(1));
    assert_eq!(status.uidnext, Some(3));
    assert!(status.highest_modseq.is_some());
    assert!(
        server
            .command_log()
            .iter()
            .any(|c| c == "RENAME Projekte Work")
    );
}

#[tokio::test]
async fn gmail_labels() {
    let server = FakeServer::gmail();
    let all = "[Gmail]/All Mail";
    server.deliver(
        all,
        msg(1)
            .labels([GmailLabel::Inbox, GmailLabel::User("Work".into())])
            .thread_id(77),
    );
    let mut session = server.connect();
    let caps = session.capabilities().await.unwrap();
    assert!(caps.gmail() && !caps.qresync());
    let folders = session.list().await.unwrap();
    let gmail_parent = folders.iter().find(|f| f.name == "[Gmail]").unwrap();
    assert!(!gmail_parent.selectable);
    assert_eq!(
        folders.iter().find(|f| f.name == all).unwrap().role,
        Some(FolderRole::All)
    );

    let info = session.select(all, None).await.unwrap();
    let headers = session.fetch_headers(&UidSet::all()).await.unwrap();
    let gmail = headers[0].gmail.as_ref().unwrap();
    assert_eq!(gmail.thread_id, 77);
    assert_eq!(gmail.labels.len(), 2);

    session
        .store_gmail_labels(&UidSet::single(1), &LabelOp::archive())
        .await
        .unwrap();
    assert_eq!(
        server.messages(all)[0].labels,
        [GmailLabel::User("Work".into())]
    );
    let changes = session
        .fetch_changes(info.highest_modseq.unwrap())
        .await
        .unwrap();
    assert_eq!(
        changes.changed[0].gmail_labels.as_deref(),
        Some(&[GmailLabel::User("Work".into())][..])
    );
    assert!(changes.vanished.is_empty(), "no QRESYNC on Gmail");

    let plain = FakeServer::new();
    let mut session = plain.connect();
    session.select("INBOX", None).await.unwrap();
    assert!(matches!(
        session
            .store_gmail_labels(&UidSet::single(1), &LabelOp::archive())
            .await,
        Err(ImapError::Unsupported(_))
    ));
}

#[tokio::test]
async fn injected_failures() {
    let server = FakeServer::new();
    server.deliver("INBOX", msg(1));
    let mut session = server.connect();
    session.select("INBOX", None).await.unwrap();

    server.fail_command("UID MOVE", FakeFailure::No("over quota".into()));
    let err = session
        .r#move(&UidSet::single(1), "Archive")
        .await
        .unwrap_err();
    assert!(err.is_recoverable());
    // The NO was consumed; the retry works.
    session.r#move(&UidSet::single(1), "Archive").await.unwrap();

    server.fail_next(FakeFailure::ConnectionLost);
    assert!(matches!(
        session.noop().await,
        Err(ImapError::ConnectionLost(_))
    ));
    assert!(matches!(session.noop().await, Err(ImapError::Broken)));

    // A new session works.
    let mut fresh = server.connect();
    server.fail_next(FakeFailure::Timeout);
    assert!(matches!(fresh.list().await, Err(ImapError::Timeout { .. })));
    let mut fresh = server.connect();
    fresh.list().await.unwrap();
}

#[tokio::test]
async fn usable_as_trait_object() {
    let server = FakeServer::new();
    server.deliver("INBOX", msg(1));
    let mut session: Box<dyn ImapSession> = Box::new(server.connect());
    session.select("INBOX", None).await.unwrap();
    assert_eq!(session.fetch_flags(&UidSet::all()).await.unwrap().len(), 1);
    session.logout().await.unwrap();
    assert!(matches!(session.noop().await, Err(ImapError::Broken)));
}
