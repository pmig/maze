//! Header sync throughput with the fake server (ignored; run in release):
//!
//! ```sh
//! cargo nextest run -p sync --release --run-ignored only perf --no-capture
//! ```

// Tests may unwrap and print (AGENTS.md).
#![allow(clippy::unwrap_used, clippy::print_stderr)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use imap::fake::{FakeMessage, FakeServer};
use mazemail_types::MessageId;
use store::{AccountKind, BodyState, NewAccount, Queries, Store, StoreConfig, ThreadAssignment};
use sync::{
    AccountSyncConfig, BoxSession, HeaderInput, IngestError, Ingested, MessageIngest,
    OfflinePolicy, SessionFactory, StoreIngest, SyncDeps, SyncStatus, new_message, spawn_account,
};
use vault::{KEY_LEN, KeyId, MasterKey, Vault};

struct Factory(FakeServer);

#[async_trait]
impl SessionFactory for Factory {
    async fn connect(&self) -> sync::Result<BoxSession> {
        Ok(Box::new(self.0.connect()))
    }
}

/// Realistic-ish messages: threads of five (each replies to the thread's first).
/// `StoreIngest` without thread resolution: measures the engine and the store's
/// inserts without the store's References lookup.
struct NoThreadIngest(StoreIngest, Store);

#[async_trait]
impl MessageIngest for NoThreadIngest {
    async fn ingest_headers(&self, input: HeaderInput) -> Result<Ingested, IngestError> {
        Ok(self.ingest_headers_batch(vec![input]).await?.remove(0))
    }

    async fn ingest_headers_batch(
        &self,
        inputs: Vec<HeaderInput>,
    ) -> Result<Vec<Ingested>, IngestError> {
        let messages: Vec<_> = inputs
            .iter()
            .map(|i| {
                let mut m = new_message(i);
                m.thread = ThreadAssignment::New;
                m
            })
            .collect();
        Ok(self
            .1
            .write_weighted(messages.len() * 4, move |w| {
                messages
                    .iter()
                    .map(|m| {
                        w.upsert_message(m).map(|o| Ingested {
                            message: o.message,
                            created: o.created,
                        })
                    })
                    .collect::<store::Result<Vec<_>>>()
            })
            .await?)
    }

    async fn attach_body(
        &self,
        message: MessageId,
        raw: Bytes,
        state: BodyState,
    ) -> Result<(), IngestError> {
        self.0.attach_body(message, raw, state).await
    }
}

fn message(n: u32) -> FakeMessage {
    let root = n - n % 5;
    FakeMessage::new(format!(
        "From: Sender {n} <sender{n}@example.org>\r\nTo: Me <me@example.org>\r\n\
         Subject: Status report number {n} for the weekly sync\r\n\
         Message-ID: <perf-{n}@example.org>\r\nIn-Reply-To: <perf-{}@example.org>\r\n\
         Date: Thu, 22 Oct 2026 10:00:00 +0000\r\nList-Id: <team.example.org>\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\nBody {n}\r\n",
        root
    ))
}

async fn initial_sync(count: u32, resolve_threads: bool) -> Duration {
    let server = FakeServer::new();
    for n in 1..=count {
        server.deliver("INBOX", message(n));
    }
    let dir = tempfile::tempdir().unwrap();
    let mut config = StoreConfig::new(dir.path());
    config.gc_interval = None;
    let vault = Vault::from_master_key(MasterKey::from_bytes(KeyId::FIRST, [3; KEY_LEN]));
    let store = Store::open(&config, &vault).unwrap();
    let account = store
        .write(|w| {
            w.create_account(&NewAccount {
                kind: AccountKind::Imap,
                name: "Perf".into(),
                config_json: "{}".into(),
                ordinal: 0,
            })
        })
        .await
        .unwrap();
    let config = AccountSyncConfig::new(account).with_offline(OfflinePolicy::HeadersOnly, 90);
    let started = Instant::now();
    let handle = spawn_account(
        config,
        SyncDeps {
            store: store.clone(),
            ingest: if resolve_threads {
                Arc::new(StoreIngest::new(store.clone()))
            } else {
                Arc::new(NoThreadIngest(
                    StoreIngest::new(store.clone()),
                    store.clone(),
                ))
            },
            sessions: Arc::new(Factory(server)),
        },
    );
    let mut status = handle.status_watch();
    loop {
        if *status.borrow_and_update() == SyncStatus::Idle {
            break;
        }
        status.changed().await.unwrap();
    }
    let elapsed = started.elapsed();
    let inbox = store
        .reader()
        .unwrap()
        .folder_by_remote_name(account, "INBOX")
        .unwrap()
        .unwrap();
    assert_eq!(inbox.total, u64::from(count));
    handle.shutdown().await;
    elapsed
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "performance measurement; run in release with --run-ignored"]
async fn perf_initial_sync_of_a_50k_folder() {
    let runs = [
        (5_000u32, false),
        (50_000, false),
        (5_000, true),
        (20_000, true),
    ];
    for (count, resolve) in runs {
        let elapsed = initial_sync(count, resolve).await;
        eprintln!(
            "initial sync of {count} messages{}: {:.2} s ({:.0} messages/s)",
            if resolve {
                " with store thread resolution"
            } else {
                ""
            },
            elapsed.as_secs_f64(),
            f64::from(count) / elapsed.as_secs_f64()
        );
    }
}

/// A small version that runs in CI, as a smoke test of the bulk path.
#[tokio::test(flavor = "multi_thread")]
async fn initial_sync_of_2000_messages() {
    let elapsed = initial_sync(2_000, true).await;
    assert!(elapsed < Duration::from_secs(60), "{elapsed:?}");
}
