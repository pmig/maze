//! Blob reference counts and garbage collection.

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use mazemail_types::BlobId;
use store::{AccountKind, BodyState, GcOptions, GcReport, Queries, Store};

use crate::common::{self, message};

fn gc(store: &Store, options: GcOptions) -> GcReport {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(store.collect_garbage(options))
        .unwrap()
}

fn refs(store: &Store, id: BlobId) -> Option<i64> {
    // The blob table isn't part of the API; read it through a raw keyed connection.
    let conn = rusqlite::Connection::open(store.layout().database()).unwrap();
    let key = common::vault(1).sqlcipher_pragma_key();
    conn.execute_batch(&format!("PRAGMA key = \"{}\";", key.expose_secret()))
        .unwrap();
    conn.query_row("SELECT refs FROM blob WHERE id = ?1", [&id.0], |r| r.get(0))
        .ok()
}

#[test]
fn references_are_counted_by_every_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let raw = b"From: a@example.com\r\nSubject: x\r\n\r\nshared body".to_vec();
    let staged = store.blobs().put(&raw).unwrap();
    let id = staged.id();
    let (a, b) = (
        message(f.account, f.inbox, 1, 1),
        message(f.account, f.inbox, 2, 2),
    );
    let s = staged.clone();
    let (a, b) = store
        .write_blocking(move |w| {
            let a = w.upsert_message(&a)?.message;
            let b = w.upsert_message(&b)?.message;
            w.set_message_body(a, &s, BodyState::Full)?;
            w.set_message_body(b, &s, BodyState::Full)?;
            Ok((a, b))
        })
        .unwrap();
    let s = staged.clone();
    let (outbox, agent) = store
        .write_blocking(move |w| {
            let outbox = w.outbox_enqueue(None, &s, "out@x", 0, None)?;
            let agent = w.create_agent_thread("claude", None, None)?;
            w.set_agent_transcript(agent, &s)?;
            Ok((outbox, agent))
        })
        .unwrap();
    drop(staged);
    assert_eq!(refs(&store, id), Some(4));

    store
        .write_blocking(move |w| w.clear_message_body(a))
        .unwrap();
    store.write_blocking(move |w| w.delete_message(b)).unwrap();
    store
        .write_blocking(move |w| w.outbox_remove(outbox))
        .unwrap();
    assert_eq!(refs(&store, id), Some(1));
    assert_eq!(gc(&store, GcOptions::full()), GcReport::default());
    assert!(store.blobs().exists(&id));

    store
        .write_blocking(move |w| w.delete_agent_thread(agent))
        .unwrap();
    assert_eq!(refs(&store, id), Some(0));
    let report = gc(&store, GcOptions::full());
    assert_eq!(report.rows_deleted, 1);
    assert_eq!(report.files_deleted, 1);
    assert_eq!(refs(&store, id), None);
    assert!(!store.blobs().exists(&id));
}

#[test]
fn a_missing_blob_resets_the_body_state() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let staged = store.blobs().put(b"raw").unwrap();
    let id = staged.id();
    let m = message(f.account, f.inbox, 1, 1);
    let out = store
        .write_blocking(move |w| {
            let out = w.upsert_message(&m)?;
            w.set_message_body(out.message, &staged, BodyState::Full)?;
            Ok(out)
        })
        .unwrap();
    fs::remove_file(store.blobs().path(&id)).unwrap();
    assert!(matches!(store.blobs().read(&id), Err(store::Error::BlobMissing(x)) if x == id));
    let reset = store.write_blocking(move |w| w.blob_missing(id)).unwrap();
    assert_eq!(reset, 1);
    let detail = store
        .reader()
        .unwrap()
        .message(out.message)
        .unwrap()
        .unwrap();
    assert_eq!(detail.blob, None);
    assert_eq!(detail.body_state, BodyState::None);
}

#[test]
fn orphans_are_collected_only_when_old() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    // An orphan: written, never referenced.
    let orphan = store.blobs().put(b"never committed").unwrap().id();
    // A referenced blob, also old.
    let kept = store.blobs().put(b"committed").unwrap();
    let kept_id = kept.id();
    let m = message(f.account, f.inbox, 1, 1);
    store
        .write_blocking(move |w| {
            let out = w.upsert_message(&m)?;
            w.set_message_body(out.message, &kept, BodyState::Full)
        })
        .unwrap();
    let stale_tmp = tmp.path().join("mail/tmp/crashed.tmp");
    fs::write(&stale_tmp, b"partial").unwrap();

    // Young: nothing happens.
    assert_eq!(gc(&store, GcOptions::full()), GcReport::default());
    assert!(store.blobs().exists(&orphan));

    // 25 hours later.
    let later = GcOptions {
        now: SystemTime::now() + Duration::from_secs(25 * 3600),
        ..GcOptions::full()
    };
    let report = gc(&store, later);
    assert_eq!(report.orphans_deleted, 1);
    assert_eq!(report.tmp_deleted, 1);
    assert!(!store.blobs().exists(&orphan));
    assert!(store.blobs().exists(&kept_id));
    assert!(!stale_tmp.exists());
    // Without `orphans`, the scan doesn't run at all.
    assert_eq!(gc(&store, GcOptions::default()), GcReport::default());
}

/// Writers keep staging, referencing and dropping blobs from a small pool of contents
/// while the collector runs in a loop with an orphan age of zero. Every committed
/// reference must still have its file.
#[test]
fn gc_never_deletes_a_blob_a_concurrent_insert_references() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let stop = Arc::new(AtomicBool::new(false));

    let collector = {
        let store = store.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let mut runs = 0;
            while !stop.load(Ordering::Relaxed) {
                let options = GcOptions {
                    orphan_min_age: Duration::ZERO,
                    ..GcOptions::full()
                };
                runtime.block_on(store.collect_garbage(options)).unwrap();
                runs += 1;
            }
            runs
        })
    };

    let workers: Vec<_> = (0..4u32)
        .map(|worker| {
            let store = store.clone();
            std::thread::spawn(move || {
                for i in 0..150u32 {
                    let content = format!("shared content {}", (i + worker) % 5).into_bytes();
                    let staged = store.blobs().put(&content).unwrap();
                    let uid = worker * 1000 + i + 1;
                    let m = message(f.account, f.inbox, uid, i64::from(i));
                    let delete = i % 3 == 0;
                    store
                        .write_blocking(move |w| {
                            let out = w.upsert_message(&m)?;
                            w.set_message_body(out.message, &staged, BodyState::Full)?;
                            if delete {
                                w.delete_message(out.message)?;
                            }
                            Ok(())
                        })
                        .unwrap();
                    if i % 2 == 0 {
                        // Staged but never referenced: an orphan right away.
                        drop(
                            store
                                .blobs()
                                .put(format!("orphan {worker} {i}").as_bytes())
                                .unwrap(),
                        );
                    }
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    let runs = collector.join().unwrap();
    assert!(runs > 0);

    let reader = store.reader().unwrap();
    let ids = reader
        .message_ids_in_scope(&store::MessageScope::Account(f.account))
        .unwrap();
    assert_eq!(ids.len(), 4 * 100);
    for id in ids {
        let blob = reader.message(id).unwrap().unwrap().blob.unwrap();
        let body = store.blobs().read(&blob).unwrap();
        assert!(body.starts_with(b"shared content"));
    }
}

#[test]
fn the_writer_makes_staged_blobs_durable_before_committing() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let staged = store
        .blobs()
        .stage(b"From: a@example.com\r\n\r\nstaged")
        .unwrap();
    assert!(!staged.is_durable());
    let (s, m) = (staged.clone(), message(f.account, f.inbox, 1, 1));
    let id = store
        .write_blocking(move |w| {
            let id = w.upsert_message(&m)?.message;
            w.set_message_body(id, &s, BodyState::Full)?;
            Ok(id)
        })
        .unwrap();
    assert!(staged.is_durable());
    let detail = store.reader().unwrap().message(id).unwrap().unwrap();
    assert_eq!(
        store.blobs().read(&detail.blob.unwrap()).unwrap(),
        b"From: a@example.com\r\n\r\nstaged"
    );
    common::close(store);
}

#[test]
fn a_blob_that_cannot_be_made_durable_rolls_the_commit_back() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let staged = store.blobs().stage(b"doomed").unwrap();
    // The staging file disappears (a failing disk, a cleaner run amok): the rename
    // fails, so the row that would point at the blob must not commit.
    let hex = staged.id().to_hex();
    for entry in fs::read_dir(store.layout().tmp_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with(&hex)
        {
            fs::remove_file(path).unwrap();
        }
    }
    let m = message(f.account, f.inbox, 1, 1);
    let err = store
        .write_blocking(move |w| {
            let id = w.upsert_message(&m)?.message;
            w.set_message_body(id, &staged, BodyState::Full)
        })
        .unwrap_err();
    assert!(matches!(err, store::Error::BlobsNotDurable(_)), "{err:?}");
    let reader = store.reader().unwrap();
    assert_eq!(reader.folder_uids(f.inbox).unwrap(), Vec::new());
    drop(reader);
    common::close(store);
}
