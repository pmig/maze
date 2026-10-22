//! Folder list sync: creation, renames (by UIDVALIDITY), deletion, roles.

use imap::fake::FakeServer;
use mazemail_types::FolderRole;
use pretty_assertions::assert_eq;
use store::Queries;
use sync::SyncScope;

use crate::common::{Harness, mail, wait_for, wait_idle};

#[tokio::test(flavor = "multi_thread")]
async fn folder_changes_on_the_server_are_mirrored() {
    let server = FakeServer::new();
    server.add_folder("Lists", None);
    server.add_folder("Lists/rust", None);
    server.add_folder("Old", None);
    server.deliver("Lists/rust", mail(1));
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    let rust = h.folder("Lists/rust");
    assert_eq!(rust.display_name, "rust");
    assert_eq!(rust.parent, Some(h.folder_id("Lists")));
    let message = h.message_at("Lists/rust", 1);

    // Another client renames and deletes folders.
    let mut other = server.connect();
    use imap::ImapSession;
    other.rename("Lists/rust", "Lists/rust-lang").await.unwrap();
    other.delete("Old").await.unwrap();
    handle.sync_now(SyncScope::Account).unwrap();
    wait_for("the rename", || h.try_folder("Lists/rust-lang").is_some()).await;
    wait_idle(&handle).await;

    let renamed = h.folder("Lists/rust-lang");
    assert_eq!(renamed.id, rust.id, "a rename keeps the folder");
    assert_eq!(renamed.display_name, "rust-lang");
    assert!(h.try_folder("Lists/rust").is_none());
    assert!(h.try_folder("Old").is_none());
    // Its messages stayed where they were.
    assert_eq!(h.message_at("Lists/rust-lang", 1), message);
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn roles_are_guessed_without_special_use() {
    let server = FakeServer::empty(&["IMAP4rev1", "IDLE", "MOVE", "UIDPLUS", "CONDSTORE"]);
    server.add_folder("INBOX", None);
    server.add_folder("Sent Items", None);
    server.add_folder("Deleted Items", None);
    server.add_folder("Notes", None);
    let h = Harness::new(server.clone()).await;
    let handle = h.spawn(h.config());
    wait_idle(&handle).await;
    assert_eq!(h.folder("INBOX").role, Some(FolderRole::Inbox));
    assert_eq!(h.folder("Sent Items").role, Some(FolderRole::Sent));
    assert_eq!(h.folder("Deleted Items").role, Some(FolderRole::Trash));
    assert_eq!(h.folder("Notes").role, None);
    let all = h.store.reader().unwrap().folders(Some(h.account)).unwrap();
    assert_eq!(all.len(), 4);
    handle.shutdown().await;
}
