//! Opening and closing: single instance, lost key, crash guard, owned runtime.

use std::sync::Arc;

use mazemail_core::{CoreHandle, Error, RuntimeConfig};
use vault::{KeyId, KeyStore, MasterKey, MemoryKeyStore};

use crate::common::{self, Place};

#[tokio::test(flavor = "multi_thread")]
async fn a_second_instance_is_refused_until_the_first_shuts_down() {
    let t = common::open().await;
    match CoreHandle::open(common::config(t.dir.path(), &t.keys)).await {
        Err(Error::AlreadyRunning { pid, data_dir }) => {
            assert_eq!(pid, Some(std::process::id()));
            assert_eq!(data_dir, t.dir.path());
        }
        other => panic!("expected AlreadyRunning, got {other:?}"),
    }
    t.core.shutdown().await.unwrap();
    // Idempotent, and the handle is closed afterwards.
    t.core.shutdown().await.unwrap();
    assert!(matches!(t.core.accounts(), Err(Error::Closed)));

    let again = CoreHandle::open(common::config(t.dir.path(), &t.keys))
        .await
        .unwrap();
    again.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_key_offers_recovery_and_never_deletes_data() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let raw = common::raw_message(
        "keep@example.com",
        "anna@example.com",
        "me@work.example",
        "Keep me",
        "Mon, 6 Oct 2025 10:00:00 +0000",
        "Body",
    );
    common::ingest(&t.core, Place::new(f.work, f.inbox, 1), raw).await;
    let recovery = t
        .keys
        .load()
        .unwrap()
        .unwrap()
        .to_recovery_key()
        .expose_secret()
        .to_owned();
    t.core.shutdown().await.unwrap();

    // The keychain lost the key: the data stays, and the core offers the way out.
    let empty = Arc::new(MemoryKeyStore::new());
    let lost = match CoreHandle::open(common::config(t.dir.path(), &empty)).await {
        Err(Error::MissingKeyForExistingData(lost)) => lost,
        other => panic!("expected MissingKeyForExistingData, got {other:?}"),
    };
    assert_eq!(lost.data_dir(), t.dir.path());
    assert!(!empty.contains_key(), "no key may be created silently");
    // The directory stays locked while the choice is pending.
    assert!(matches!(
        CoreHandle::open(common::config(t.dir.path(), &empty)).await,
        Err(Error::AlreadyRunning { .. })
    ));

    // A well-formed but wrong key is refused and hands the choice back.
    let wrong = MasterKey::generate(KeyId::FIRST).unwrap().to_recovery_key();
    let failed = lost
        .enter_recovery_key(wrong.expose_secret())
        .await
        .unwrap_err();
    assert!(matches!(failed.error, Error::WrongRecoveryKey));
    assert!(!empty.contains_key());
    let lost = failed.lost_key.unwrap();

    // Garbage isn't a recovery key at all.
    let failed = lost.enter_recovery_key("not a key").await.unwrap_err();
    assert!(matches!(failed.error, Error::Vault(_)));
    let lost = failed.lost_key.unwrap();

    let core = lost.enter_recovery_key(&recovery).await.unwrap();
    assert!(empty.contains_key(), "the recovered key is stored");
    assert_eq!(core.accounts().unwrap().len(), 2);
    core.shutdown().await.unwrap();

    // Start fresh: the old data is moved aside, not deleted.
    let fresh_keys = Arc::new(MemoryKeyStore::new());
    let lost = match CoreHandle::open(common::config(t.dir.path(), &fresh_keys)).await {
        Err(Error::MissingKeyForExistingData(lost)) => lost,
        other => panic!("expected MissingKeyForExistingData, got {other:?}"),
    };
    let (core, moved_to) = lost.start_fresh().await.unwrap();
    assert!(moved_to.join(store::DATABASE_FILE).exists());
    assert!(moved_to.starts_with(t.dir.path()));
    assert!(core.accounts().unwrap().is_empty());
    assert!(fresh_keys.contains_key());
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_crash_guard_counts_launches_that_never_became_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    for expected in 0..4 {
        let core = CoreHandle::open(common::config(dir.path(), &keys))
            .await
            .unwrap();
        assert_eq!(core.crash_guard().unhealthy_launches, expected);
        assert_eq!(
            core.should_offer_rollback(),
            expected >= mazemail_core::ROLLBACK_THRESHOLD
        );
        assert!(dir.path().join(mazemail_core::SENTINEL_FILE).exists());
        // Shut down without becoming healthy (as if the window never rendered).
        core.shutdown().await.unwrap();
    }
    let core = CoreHandle::open(common::config(dir.path(), &keys))
        .await
        .unwrap();
    assert!(core.should_offer_rollback());
    core.mark_healthy().unwrap();
    assert!(!dir.path().join(mazemail_core::SENTINEL_FILE).exists());
    core.shutdown().await.unwrap();

    let core = CoreHandle::open(common::config(dir.path(), &keys))
        .await
        .unwrap();
    assert_eq!(core.crash_guard().unhealthy_launches, 0);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owned_runtime_runs_the_core_and_stops_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let mut config = common::config(dir.path(), &keys);
    config.runtime = RuntimeConfig::Owned {
        worker_threads: Some(2),
    };
    let core = CoreHandle::open(config).await.unwrap();
    let f = common::fixture(&core).await;
    let results = core
        .search("anything", mazemail_core::SearchOpts::default())
        .await
        .unwrap();
    assert_eq!(results.total_messages, 0);
    assert_eq!(core.folders().unwrap().len(), 3, "{f:?}");
    core.shutdown().await.unwrap();
}

#[test]
fn open_works_without_an_ambient_runtime() {
    // The GUI calls `open` from GPUI's executor, not from Tokio.
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let mut config = common::config_without_runtime(dir.path(), &keys);
    config.runtime = RuntimeConfig::Owned {
        worker_threads: Some(1),
    };
    let executor = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    // A current-thread runtime without IO or timers stands in for a foreign executor:
    // everything the core does runs on its own runtime.
    executor.block_on(async {
        let core = CoreHandle::open(config).await.unwrap();
        assert!(core.accounts().unwrap().is_empty());
        core.shutdown().await.unwrap();
    });
}
