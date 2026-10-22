//! Shared helpers: a store in a temp dir, a fake server behind a switchable
//! session factory, fast timings and polling waits.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use imap::fake::{FakeMessage, FakeServer};
use mazemail_types::{AccountId, Flags, FolderId, MessageId};
use store::{AccountKind, Folder, MessageDetail, NewAccount, Queries, Store, StoreConfig};
use sync::{
    AccountHandle, AccountSyncConfig, BoxSession, OfflinePolicy, SessionFactory, StoreIngest,
    SyncDeps, SyncError, SyncStatus, SyncTiming, spawn_account,
};
use tempfile::TempDir;
use vault::{KEY_LEN, KeyId, MasterKey, Vault};

/// Hands out `FakeImap` sessions; can be switched "offline".
#[derive(Debug)]
pub(crate) struct FakeFactory {
    pub(crate) server: FakeServer,
    pub(crate) online: AtomicBool,
    pub(crate) connects: AtomicUsize,
}

#[async_trait]
impl SessionFactory for FakeFactory {
    async fn connect(&self) -> sync::Result<BoxSession> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        if !self.online.load(Ordering::SeqCst) {
            return Err(SyncError::Imap(imap::ImapError::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "connection refused",
            ))));
        }
        Ok(Box::new(self.server.connect()))
    }
}

pub(crate) struct Harness {
    _dir: TempDir,
    pub(crate) store: Store,
    pub(crate) account: AccountId,
    pub(crate) factory: Arc<FakeFactory>,
}

impl Harness {
    pub(crate) async fn new(server: FakeServer) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut config = StoreConfig::new(dir.path());
        config.gc_interval = None;
        config.app_version = "1.0.0-test".to_owned();
        let vault = Vault::from_master_key(MasterKey::from_bytes(KeyId::FIRST, [7; KEY_LEN]));
        let store = Store::open(&config, &vault).unwrap();
        let account = store
            .write(|w| {
                w.create_account(&NewAccount {
                    kind: AccountKind::Imap,
                    name: "Test".to_owned(),
                    config_json: "{}".to_owned(),
                    ordinal: 0,
                })
            })
            .await
            .unwrap();
        let factory = Arc::new(FakeFactory {
            server: server.clone(),
            online: AtomicBool::new(true),
            connects: AtomicUsize::new(0),
        });
        Self {
            _dir: dir,
            store,
            account,
            factory,
        }
    }

    /// Fast timers, no background bodies, polling effectively off.
    pub(crate) fn config(&self) -> AccountSyncConfig {
        let mut config =
            AccountSyncConfig::new(self.account).with_offline(OfflinePolicy::HeadersOnly, 90);
        config.timing = SyncTiming {
            idle_reissue: Duration::from_secs(25 * 60),
            poll_interval: Duration::from_secs(3600),
            open_folder_poll: Duration::from_secs(3600),
            uid_diff_interval: Duration::from_secs(3600),
            coalesce_window: Duration::from_millis(5),
            reconnect_initial: Duration::from_millis(30),
            reconnect_max: Duration::from_millis(200),
        };
        config
    }

    pub(crate) fn spawn(&self, config: AccountSyncConfig) -> AccountHandle {
        spawn_account(
            config,
            SyncDeps {
                store: self.store.clone(),
                ingest: Arc::new(StoreIngest::new(self.store.clone())),
                sessions: self.factory.clone(),
            },
        )
    }

    pub(crate) fn folder(&self, remote_name: &str) -> Folder {
        self.store
            .reader()
            .unwrap()
            .folder_by_remote_name(self.account, remote_name)
            .unwrap()
            .unwrap_or_else(|| panic!("no local folder {remote_name:?}"))
    }

    pub(crate) fn try_folder(&self, remote_name: &str) -> Option<Folder> {
        self.store
            .reader()
            .unwrap()
            .folder_by_remote_name(self.account, remote_name)
            .unwrap()
    }

    pub(crate) fn uids(&self, remote_name: &str) -> Vec<(u32, Flags)> {
        let folder = self.folder(remote_name).id;
        self.store.reader().unwrap().folder_uids(folder).unwrap()
    }

    pub(crate) fn message_at(&self, remote_name: &str, uid: u32) -> MessageId {
        let folder = self.folder(remote_name).id;
        self.store
            .reader()
            .unwrap()
            .message_by_uid(folder, uid)
            .unwrap()
            .unwrap_or_else(|| panic!("no message at {remote_name}:{uid}"))
    }

    pub(crate) fn message(&self, id: MessageId) -> MessageDetail {
        self.store.reader().unwrap().message(id).unwrap().unwrap()
    }

    pub(crate) fn folder_id(&self, remote_name: &str) -> FolderId {
        self.folder(remote_name).id
    }
}

/// A message with sender, date and Message-ID `m{n}@example.org`.
pub(crate) fn mail(n: u32) -> FakeMessage {
    FakeMessage::new(format!(
        "From: Sender {n} <s{n}@example.org>\r\nTo: me@example.org\r\n\
         Subject: Message {n}\r\nMessage-ID: <m{n}@example.org>\r\n\
         Date: Thu, 22 Oct 2026 10:{:02}:00 +0000\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\nBody of message {n}\r\n",
        n % 60
    ))
}

/// Polls `check` until it holds (10 s limit).
pub(crate) async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits until the engine reports `Idle` (10 s limit).
pub(crate) async fn wait_idle(handle: &AccountHandle) {
    wait_status(handle, "idle", |s| *s == SyncStatus::Idle).await;
}

pub(crate) async fn wait_status(
    handle: &AccountHandle,
    what: &str,
    check: impl Fn(&SyncStatus) -> bool,
) {
    let mut rx = handle.status_watch();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if check(&rx.borrow_and_update()) {
                return;
            }
            if rx.changed().await.is_err() {
                panic!("engine stopped while waiting for {what}");
            }
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "timed out waiting for status {what}; last: {:?}",
        handle.status()
    );
}

/// Commands logged since `from`.
pub(crate) fn log_since(server: &FakeServer, from: usize) -> Vec<String> {
    server.command_log().into_iter().skip(from).collect()
}
