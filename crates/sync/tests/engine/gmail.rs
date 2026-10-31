//! Gmail (ADR-007): All Mail, Trash and Spam are mailboxes; labels are locations.

use imap::GmailLabel;
use imap::fake::FakeServer;
use mazemail_types::{Flags, FolderRole};
use pretty_assertions::assert_eq;
use store::Queries;
use sync::ops::{ArchiveTarget, LocationRef, OpPayload};
use sync::{OpEvent, SyncScope};

use crate::common::{Harness, mail, wait_for, wait_idle};

const ALL_MAIL: &str = "[Gmail]/All Mail";

#[tokio::test(flavor = "multi_thread")]
async fn labels_become_locations_of_one_message() {
    let server = FakeServer::gmail();
    server.add_folder("Work", None);
    server.deliver(
        ALL_MAIL,
        mail(1)
            .flags(Flags::SEEN)
            .labels([GmailLabel::Inbox, GmailLabel::User("Work".into())]),
    );
    server.deliver(
        ALL_MAIL,
        mail(2).labels([GmailLabel::Inbox, GmailLabel::Important]),
    );
    server.deliver(
        ALL_MAIL,
        mail(3).labels([GmailLabel::User("Hidden".into())]),
    );
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;

    // Only the three mailboxes were selected.
    let selected: Vec<String> = server
        .command_log()
        .into_iter()
        .filter(|c| c.starts_with("SELECT"))
        .collect();
    assert!(
        selected.iter().all(|c| c == "SELECT [Gmail]/All Mail"
            || c == "SELECT [Gmail]/Trash"
            || c == "SELECT [Gmail]/Spam"),
        "{selected:?}"
    );

    let inbox = h.folder("INBOX");
    assert!(inbox.is_label);
    assert_eq!(inbox.role, Some(FolderRole::Inbox));
    let work = h.folder("Work");
    assert!(work.is_label);
    assert!(!h.folder(ALL_MAIL).is_label);
    assert_eq!(h.folder(ALL_MAIL).total, 3);
    assert_eq!((inbox.total, inbox.unread), (2, 1));
    assert_eq!(work.total, 1);
    // A label hidden from IMAP got a folder on the fly.
    assert!(h.folder("Hidden").is_label);

    let first = h.message(h.message_at(ALL_MAIL, 1));
    assert_eq!(first.gm_msgid, Some(server.messages(ALL_MAIL)[0].gm_msgid));
    let folders: Vec<_> = first.locations.iter().map(|l| l.folder).collect();
    assert_eq!(folders.len(), 3);
    assert!(folders.contains(&inbox.id) && folders.contains(&work.id));
    assert!(first.locations.iter().all(|l| l.flags == Flags::SEEN));
    let label_uids: Vec<_> = first
        .locations
        .iter()
        .filter(|l| l.folder != h.folder_id(ALL_MAIL))
        .map(|l| l.uid)
        .collect();
    assert_eq!(label_uids, vec![None, None]);
    // One row per message.
    let rows = h
        .store
        .reader()
        .unwrap()
        .messages_by_rfc_id(h.account, "m1@example.org")
        .unwrap();
    assert_eq!(rows.len(), 1);

    // Another client adds a label and flags the message: both follow.
    server.set_labels(
        ALL_MAIL,
        2,
        vec![GmailLabel::Inbox, GmailLabel::User("Work".into())],
    );
    server.set_flags(ALL_MAIL, 2, Flags::FLAGGED, &[]);
    handle
        .sync_now(SyncScope::Folder(h.folder_id("Work")))
        .unwrap();
    // Labels and flags are applied in separate writes: wait for both.
    wait_for("the label and flag change", || {
        h.folder("Work").total == 2
            && h.message(h.message_at(ALL_MAIL, 2))
                .locations
                .iter()
                .all(|l| l.flags == Flags::FLAGGED)
    })
    .await;
    let second = h.message(h.message_at(ALL_MAIL, 2));
    assert_eq!(
        second.locations.len(),
        3,
        "All Mail, Inbox, Work (Important removed)"
    );
    assert!(second.locations.iter().all(|l| l.flags == Flags::FLAGGED));
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_removes_the_inbox_label() {
    let server = FakeServer::gmail();
    server.deliver(ALL_MAIL, mail(1).labels([GmailLabel::Inbox]));
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    let message = h.message_at(ALL_MAIL, 1);
    let inbox = h.folder_id("INBOX");
    assert_eq!(h.folder("INBOX").total, 1);

    // Core: enqueue + optimistic local apply.
    let account = h.account;
    let mut events = handle.subscribe();
    let op = h
        .store
        .write(move |w| {
            let op = OpPayload::archive(
                vec![LocationRef::new(message, inbox, None)],
                ArchiveTarget::GmailRemoveInbox,
            )
            .enqueue(w, account, "user")?;
            w.remove_location(message, inbox)?;
            Ok(op)
        })
        .await
        .unwrap();
    handle.apply_op(op).unwrap();
    wait_for("the archive", || {
        server.messages(ALL_MAIL)[0].labels.is_empty()
    })
    .await;
    wait_for("the op to finish", || {
        h.store
            .reader()
            .unwrap()
            .pending_ops(account)
            .unwrap()
            .is_empty()
    })
    .await;
    wait_idle(&handle).await;
    assert_eq!(h.folder("INBOX").total, 0);
    assert_eq!(h.message(message).locations.len(), 1, "still in All Mail");
    let log = server.command_log();
    assert!(
        log.iter().any(|c| c.contains("X-GM-LABELS Remove")),
        "{log:?}"
    );
    let mut done = false;
    while let Ok(batch) = events.try_recv() {
        done |= batch.ops.contains(&OpEvent::Done(op));
    }
    assert!(done);
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_moves_to_trash_and_the_message_stays_one() {
    let server = FakeServer::gmail();
    server.deliver(
        ALL_MAIL,
        mail(1).labels([GmailLabel::Inbox, GmailLabel::User("Work".into())]),
    );
    server.add_folder("Work", None);
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    let message = h.message_at(ALL_MAIL, 1);
    let inbox = h.folder_id("INBOX");
    let account = h.account;
    let op = h
        .store
        .write(move |w| {
            OpPayload::trash(vec![LocationRef::new(message, inbox, None)], None)
                .enqueue(w, account, "user")
        })
        .await
        .unwrap();
    handle.apply_op(op).unwrap();
    wait_for("the move to Trash", || {
        server.messages("[Gmail]/Trash").len() == 1
    })
    .await;
    assert!(server.messages(ALL_MAIL).is_empty());
    wait_for("the local state", || {
        let d = h.message(message);
        d.locations.len() == 1 && d.locations[0].folder == h.folder_id("[Gmail]/Trash")
    })
    .await;
    assert_eq!(h.folder("Work").total, 0);
    handle.shutdown().await;
}

/// Builds before schema v8 dropped Gmail's quoted system labels (`"\\Inbox"`), so
/// Inbox, Sent, Starred and Important stayed empty. The v8 migration sets All Mail's
/// HIGHESTMODSEQ to 1; the next pass fetches every message's labels and fills them.
#[tokio::test(flavor = "multi_thread")]
async fn a_reset_modseq_fetches_all_labels_again() {
    let server = FakeServer::gmail();
    server.deliver(
        ALL_MAIL,
        mail(1).labels([GmailLabel::Inbox, GmailLabel::Starred]),
    );
    server.deliver(
        ALL_MAIL,
        mail(2).labels([GmailLabel::Sent, GmailLabel::Important]),
    );
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    let labels = [
        "INBOX",
        "[Gmail]/Sent Mail",
        "[Gmail]/Starred",
        "[Gmail]/Important",
    ];
    assert!(labels.iter().all(|l| h.folder(l).total == 1));

    // What an affected database looks like: the messages without their system labels.
    let all_mail = h.folder(ALL_MAIL);
    let messages = [h.message_at(ALL_MAIL, 1), h.message_at(ALL_MAIL, 2)];
    let label_ids: Vec<_> = labels.iter().map(|l| h.folder_id(l)).collect();
    h.store
        .write(move |w| {
            for message in messages {
                for &label in &label_ids {
                    w.remove_location(message, label)?;
                }
            }
            Ok(())
        })
        .await
        .unwrap();
    assert!(labels.iter().all(|l| h.folder(l).total == 0));

    // The migration's reset; the server's state is unchanged.
    let sync = store::FolderSyncState {
        highestmodseq: Some(1),
        ..all_mail.sync
    };
    h.store
        .write(move |w| w.set_folder_sync_state(all_mail.id, sync))
        .await
        .unwrap();
    handle
        .sync_now(SyncScope::Folder(h.folder_id(ALL_MAIL)))
        .unwrap();
    wait_for("the labels", || {
        labels.iter().all(|l| h.folder(l).total == 1)
    })
    .await;
    assert_eq!(
        h.message(messages[0]).locations.len(),
        3,
        "All Mail, Inbox, Starred"
    );
    handle.shutdown().await;
}
