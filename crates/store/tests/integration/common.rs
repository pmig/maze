//! Shared helpers for the integration tests.

use std::path::Path;

use mazemail_types::{AccountId, Address, Flags, FolderId, FolderRole};
use store::{
    AccountKind, NewAccount, NewFolder, NewLocation, NewMessage, Store, StoreConfig,
    ThreadAssignment,
};
use vault::{KEY_LEN, KeyId, MasterKey, Vault};

pub(crate) fn vault(seed: u8) -> Vault {
    Vault::from_master_key(MasterKey::from_bytes(KeyId::FIRST, [seed; KEY_LEN]))
}

pub(crate) fn config(dir: &Path) -> StoreConfig {
    let mut config = StoreConfig::new(dir);
    config.gc_interval = None;
    config.app_version = "1.0.0-test".to_owned();
    config
}

pub(crate) fn open(dir: &Path) -> Store {
    Store::open(&config(dir), &vault(1)).unwrap()
}

/// Closes the store from a synchronous test.
pub(crate) fn close(store: Store) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(store.close()).unwrap();
}

pub(crate) struct Fixture {
    pub(crate) account: AccountId,
    pub(crate) inbox: FolderId,
    pub(crate) archive: FolderId,
    pub(crate) sent: FolderId,
}

pub(crate) fn fixture(store: &Store, kind: AccountKind) -> Fixture {
    store
        .write_blocking(move |w| {
            let account = w.create_account(&NewAccount {
                kind,
                name: "Work".to_owned(),
                config_json: "{}".to_owned(),
                ordinal: 0,
            })?;
            let inbox = w.create_folder(
                &NewFolder::new(account, "INBOX", "Inbox").with_role(FolderRole::Inbox),
            )?;
            let archive = w.create_folder(
                &NewFolder::new(account, "Archive", "Archive").with_role(FolderRole::Archive),
            )?;
            let sent = w.create_folder(
                &NewFolder::new(account, "Sent", "Sent").with_role(FolderRole::Sent),
            )?;
            Ok(Fixture {
                account,
                inbox,
                archive,
                sent,
            })
        })
        .unwrap()
}

/// A plain message in one folder, in its own thread.
pub(crate) fn message(account: AccountId, folder: FolderId, uid: u32, date: i64) -> NewMessage {
    let mut m = NewMessage::new(account, date);
    m.rfc_message_id = Some(format!("msg-{uid}-{date}@example.com"));
    m.subject = Some(format!("Subject {uid}"));
    m.from = Some(Address::new(
        Some("Sender"),
        format!("sender{uid}@example.com"),
    ));
    m.thread = ThreadAssignment::New;
    m.locations = vec![NewLocation::new(folder, Some(uid), Flags::empty())];
    m
}
