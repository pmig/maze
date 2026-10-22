//! Event batching and the sync status seam.

use std::time::Duration;

use mazemail_core::{CoreEventBatch, SyncState, SyncStatus};
use mazemail_types::{Address, Flags, ThreadId};
use store::{NewLocation, NewMessage, ThreadAssignment};

use crate::common;

#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_writes_arrives_as_few_batches_with_counts() {
    // A wider window than the default frame keeps this independent of machine load:
    // whatever arrives within it must come as one batch.
    let dir = tempfile::tempdir().unwrap();
    let keys = std::sync::Arc::new(vault::MemoryKeyStore::new());
    let mut config = common::config(dir.path(), &keys);
    config.event_window = Duration::from_millis(250);
    let core = mazemail_core::CoreHandle::open(config).await.unwrap();
    let t = common::TestCore {
        core,
        keys,
        sync: std::sync::Arc::default(),
        dir,
    };
    let f = common::fixture(&t.core).await;
    let mut events = t.core.events();
    // Let the fixture's own batch go by.
    common::wait_for(&mut events, |b| b.folders_changed.contains(&f.inbox)).await;

    const WRITES: u32 = 40;
    let writes = (0..WRITES).map(|n| {
        let store = t.core.store().clone();
        let (account, inbox) = (f.work, f.inbox);
        tokio::spawn(async move {
            let mut m = NewMessage::new(account, 1_700_000_000 + i64::from(n));
            m.subject = Some(format!("Burst {n}"));
            m.from = Some(Address::new(None::<String>, "burst@example.com"));
            m.thread = ThreadAssignment::New;
            m.locations = vec![NewLocation::new(inbox, Some(100 + n), Flags::empty())];
            store
                .write(move |w| w.upsert_message(&m))
                .await
                .unwrap()
                .thread
        })
    });
    let mut threads: Vec<ThreadId> = Vec::new();
    for write in writes {
        threads.push(write.await.unwrap());
    }

    let mut batches: Vec<CoreEventBatch> = Vec::new();
    let mut seen: Vec<ThreadId> = Vec::new();
    while seen.len() < threads.len() {
        let batch = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        for (folder, changed) in &batch.threads_changed {
            if *folder == f.inbox {
                seen.extend(changed.iter().copied());
            }
        }
        seen.sort();
        seen.dedup();
        batches.push(batch);
    }
    eprintln!("{WRITES} writes arrived in {} batches", batches.len());
    assert!(
        batches.len() < WRITES as usize / 2,
        "{} batches for {WRITES} writes",
        batches.len()
    );
    let last_counts = batches
        .iter()
        .rev()
        .find_map(|b| b.counts.iter().find(|c| c.folder == f.inbox))
        .unwrap();
    assert_eq!(last_counts.total, u64::from(WRITES));
    assert_eq!(last_counts.unread, u64::from(WRITES));
    for batch in &batches {
        assert!(!batch.is_empty());
        let mut sorted = batch.folders_changed.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted, batch.folders_changed, "sorted, no duplicates");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_status_and_index_progress_reach_the_batches() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let mut events = t.core.events();
    let sink = t.core.sync_status_sink();
    assert!(sink.report(SyncStatus {
        account: f.work,
        state: SyncState::Syncing {
            folder: Some(f.inbox),
            done: 1,
            total: Some(10),
        },
    }));
    assert!(sink.report(SyncStatus {
        account: f.work,
        state: SyncState::Idle,
    }));
    let batch = common::wait_for(&mut events, |b| !b.sync_status.is_empty()).await;
    // Within one window only the latest state per account survives.
    assert_eq!(
        batch.sync_status,
        vec![SyncStatus {
            account: f.work,
            state: SyncState::Idle
        }]
    );

    common::ingest(
        &t.core,
        common::Place::new(f.work, f.inbox, 1),
        common::raw_message(
            "p@example.com",
            "anna@example.com",
            "me@work.example",
            "Progress",
            "Mon, 6 Oct 2025 10:00:00 +0000",
            "Body",
        ),
    )
    .await;
    // The indexer commits on its own within the commit delay (1 s) and says so.
    let batch = common::wait_for(&mut events, |b| b.index.is_some_and(|i| i.indexed == 1)).await;
    assert_eq!(batch.index.unwrap().pending, 0);
}
