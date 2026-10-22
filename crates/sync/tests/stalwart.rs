//! The engine against a real Stalwart server (the setup of `crates/imap/tests/support`).
//!
//! Ignored by default because it downloads and runs a server binary:
//!
//! ```sh
//! MAZEMAIL_TEST_IMAP=1 cargo nextest run -p sync --run-ignored only syncs_a_real_server
//! ```

// Tests may unwrap (AGENTS.md); clippy only knows that for `#[test]` functions.
#![allow(clippy::unwrap_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use imap::{ImapAuth, ImapClient, ImapConfig, ImapSession, Security};
use mazemail_types::Flags;
use store::{AccountKind, BodyState, NewAccount, Queries, Store, StoreConfig};
use support::Stalwart;
use sync::ops::{FlagTarget, OpPayload};
use sync::{
    AccountSyncConfig, BodyPriority, CredentialError, CredentialProvider, ImapSessionFactory,
    OfflinePolicy, StoreIngest, SyncDeps, SyncStatus, spawn_account,
};
use vault::{KEY_LEN, KeyId, MasterKey, Vault};

struct Password;

#[async_trait]
impl CredentialProvider for Password {
    async fn credentials(&self) -> Result<ImapAuth, CredentialError> {
        Ok(ImapAuth::password(support::USER, support::PASSWORD))
    }
}

fn imap_config(port: u16) -> ImapConfig {
    ImapConfig::new(
        "127.0.0.1",
        port,
        ImapAuth::password(support::USER, support::PASSWORD),
    )
    .with_security(Security::PlainInsecure)
    .with_command_timeout(Duration::from_secs(10))
}

fn message(n: u32) -> Vec<u8> {
    format!(
        "From: Alice <alice@example.org>\r\nTo: Bob <bob@example.org>\r\nSubject: Message {n}\r\n\
         Message-ID: <sync-{n}@example.org>\r\nDate: Thu, 22 Oct 2026 10:00:00 +0000\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\nHello {n}\r\n"
    )
    .into_bytes()
}

async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !check() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Stalwart; set MAZEMAIL_TEST_IMAP=1 and run ignored tests"]
#[allow(clippy::print_stderr)]
async fn syncs_a_real_server() {
    if !support::enabled() {
        eprintln!("skipping: MAZEMAIL_TEST_IMAP is not 1");
        return;
    }
    let server = Stalwart::start().unwrap();
    let mut other = ImapClient::connect(&imap_config(server.port))
        .await
        .unwrap();
    for n in 1..=5 {
        other
            .append("INBOX", &message(n), Flags::empty())
            .await
            .unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let mut store_config = StoreConfig::new(dir.path());
    store_config.gc_interval = None;
    let vault = Vault::from_master_key(MasterKey::from_bytes(KeyId::FIRST, [9; KEY_LEN]));
    let store = Store::open(&store_config, &vault).unwrap();
    let account = store
        .write(|w| {
            w.create_account(&NewAccount {
                kind: AccountKind::Imap,
                name: "Stalwart".into(),
                config_json: "{}".into(),
                ordinal: 0,
            })
        })
        .await
        .unwrap();
    let mut config = AccountSyncConfig::new(account).with_offline(OfflinePolicy::HeadersOnly, 90);
    config.timing.coalesce_window = Duration::from_millis(5);
    let handle = spawn_account(
        config,
        SyncDeps {
            store: store.clone(),
            ingest: Arc::new(StoreIngest::new(store.clone())),
            sessions: Arc::new(ImapSessionFactory::new(
                imap_config(server.port),
                Arc::new(Password),
            )),
        },
    );
    let inbox = || {
        store
            .reader()
            .unwrap()
            .folder_by_remote_name(account, "INBOX")
            .unwrap()
    };
    wait_for("the initial sync", || {
        handle.status() == SyncStatus::Idle && inbox().is_some_and(|f| f.total == 5)
    })
    .await;
    let inbox_id = inbox().unwrap().id;

    // New mail through IDLE.
    other
        .append("INBOX", &message(6), Flags::empty())
        .await
        .unwrap();
    wait_for("IDLE delivery", || inbox().is_some_and(|f| f.total == 6)).await;

    // A body.
    let uids = store.reader().unwrap().folder_uids(inbox_id).unwrap();
    let first = store
        .reader()
        .unwrap()
        .message_by_uid(inbox_id, uids[0].0)
        .unwrap()
        .unwrap();
    assert_eq!(
        handle
            .fetch_body(first, BodyPriority::Interactive)
            .await
            .unwrap(),
        BodyState::Full
    );

    // An op: mark read.
    let uid = uids[0].0;
    let op = store
        .write(move |w| {
            OpPayload::set_flags(vec![FlagTarget {
                message: first,
                folder: inbox_id,
                uid: Some(uid),
                flags: Flags::SEEN,
                keywords: Vec::new(),
            }])
            .enqueue(w, account, "user")
        })
        .await
        .unwrap();
    handle.apply_op(op).unwrap();
    wait_for("the op", || {
        store
            .reader()
            .unwrap()
            .pending_ops(account)
            .unwrap()
            .is_empty()
    })
    .await;
    other.select("INBOX", None).await.unwrap();
    let flags = other.fetch_flags(&imap::UidSet::single(uid)).await.unwrap();
    assert!(flags[0].flags.contains(Flags::SEEN));
    handle.shutdown().await;
    let _ = other.logout().await;
}
