//! Performance of the thread list at 1M messages (PLAN §5 M1 exit criteria: list keys
//! query < 50 ms, measured with encryption on).
//!
//! Ignored by default; run in release:
//!
//! ```sh
//! cargo nextest run -p store --release --run-ignored only --no-capture perf
//! ```

#![allow(clippy::print_stderr)]

use std::time::{Duration, Instant};

use mazemail_types::{Address, Flags, MessageId, ThreadId};
use store::{
    AccountKind, ListFilter, ListKey, ListScope, MessageScope, MetadataFilter, NewLocation,
    NewMessage, Page, Queries, SortOrder, Store, ThreadAssignment,
};

use crate::common;

const BIG: u32 = 1_000_000;
const SMALL: u32 = 100_000;
const JOB: u32 = 5_000;

fn synthetic(
    account: mazemail_types::AccountId,
    folder: mazemail_types::FolderId,
    i: u32,
) -> NewMessage {
    let mut m = NewMessage::new(account, 1_500_000_000 + i64::from(i) * 37);
    m.rfc_message_id = Some(format!("{i}.perf@example.com"));
    m.subject = Some(format!("Synthetic message number {i}"));
    m.from = Some(Address::new(
        Some("Perf Sender"),
        format!("sender{}@example.com", i % 5000),
    ));
    m.to = vec![Address::new(None::<String>, "me@example.com")];
    m.snippet = Some("Lorem ipsum dolor sit amet, consectetur adipiscing elit".to_owned());
    m.size = Some(4096);
    m.has_attachments = i.is_multiple_of(11);
    m.thread = ThreadAssignment::New;
    let flags = if i.is_multiple_of(4) {
        Flags::empty()
    } else {
        Flags::SEEN
    };
    m.locations = vec![NewLocation::new(folder, Some(i + 1), flags)];
    m
}

fn fill(
    store: &Store,
    account: mazemail_types::AccountId,
    folder: mazemail_types::FolderId,
    count: u32,
) -> Duration {
    let started = Instant::now();
    let mut next = 0;
    while next < count {
        let end = (next + JOB).min(count);
        let range = next..end;
        let runtime_free = store.clone();
        runtime_free
            .write_blocking(move |w| {
                for i in range {
                    w.upsert_message(&synthetic(account, folder, i))?;
                }
                Ok(())
            })
            .unwrap();
        next = end;
    }
    started.elapsed()
}

fn time<R>(label: &str, runs: u32, mut f: impl FnMut() -> R) -> (Duration, R) {
    // One warm-up run, then the median of `runs`.
    let mut result = f();
    let mut times = Vec::new();
    for _ in 0..runs {
        let started = Instant::now();
        result = f();
        times.push(started.elapsed());
    }
    times.sort();
    let median = times[times.len() / 2];
    eprintln!(
        "{label:<60} {:>9.2} ms (median of {runs})",
        median.as_secs_f64() * 1000.0
    );
    (median, result)
}

#[test]
#[ignore = "inserts 1.1M messages; run in release (see module docs)"]
fn thread_list_at_one_million_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);

    let insert_big = fill(&store, f.account, f.inbox, BIG);
    eprintln!(
        "insert {BIG} messages into one folder: {:.1} s ({:.0} msg/s)",
        insert_big.as_secs_f64(),
        f64::from(BIG) / insert_big.as_secs_f64()
    );
    let insert_small = fill(&store, f.account, f.archive, SMALL);
    eprintln!(
        "insert {SMALL} messages into a second folder: {:.1} s",
        insert_small.as_secs_f64()
    );

    let reader = store.reader().unwrap();
    let all = ListFilter::default();
    let newest = SortOrder::NewestFirst;
    let inbox = ListScope::Folder(f.inbox);

    let (first_page, keys) = time("keys, 1M folder, first keyset page (200k)", 9, || {
        reader
            .thread_list_keys(inbox, newest, &all, Page::first(200_000))
            .unwrap()
    });
    assert_eq!(keys.len(), 200_000);

    let (full, keys) = time("keys, 1M folder, all 1M keys (5 pages of 200k)", 5, || {
        let mut out: Vec<ListKey> = Vec::with_capacity(BIG as usize);
        let mut page = Page::first(200_000);
        loop {
            let chunk = reader.thread_list_keys(inbox, newest, &all, page).unwrap();
            let done = chunk.len() < 200_000;
            if let Some(last) = chunk.last() {
                page = Page::after(*last, 200_000);
            }
            out.extend(chunk);
            if done {
                return out;
            }
        }
    });
    assert_eq!(keys.len(), BIG as usize);
    assert!(
        keys.windows(2)
            .all(|w| (w[0].key, w[0].thread) > (w[1].key, w[1].thread))
    );

    let (_, deep) = time(
        "keys, 1M folder, page of 200 at offset 900k (keyset)",
        9,
        || {
            reader
                .thread_list_keys(inbox, newest, &all, Page::after(keys[900_000], 200))
                .unwrap()
        },
    );
    assert_eq!(deep[0], keys[900_001]);

    time(
        "keys, 1M folder, unread filter, first page (200k)",
        5,
        || {
            reader
                .thread_list_keys(
                    inbox,
                    newest,
                    &ListFilter {
                        unread: true,
                        ..all
                    },
                    Page::first(200_000),
                )
                .unwrap()
        },
    );
    time(
        "keys, account scope (1.1M threads), first page (200k)",
        5,
        || {
            reader
                .thread_list_keys(
                    ListScope::Account(f.account),
                    newest,
                    &all,
                    Page::first(200_000),
                )
                .unwrap()
        },
    );

    let visible: Vec<ThreadId> = keys[..60].iter().map(|k| k.thread).collect();
    let (rows, _) = time("rows for 60 visible threads", 21, || {
        reader.thread_rows(&visible, Some(f.inbox)).unwrap()
    });

    // Folder switch: keys (first page) + the visible rows of the new folder.
    let archive = ListScope::Folder(f.archive);
    let (switch_small, _) = time(
        "folder switch to the 100k folder (all keys + 60 rows)",
        9,
        || {
            let keys = reader
                .thread_list_keys(archive, newest, &all, Page::first(200_000))
                .unwrap();
            let ids: Vec<ThreadId> = keys[..60].iter().map(|k| k.thread).collect();
            (
                keys.len(),
                reader.thread_rows(&ids, Some(f.archive)).unwrap().len(),
            )
        },
    );
    let (switch_big, _) = time(
        "folder switch to the 1M folder (first 200k keys + 60 rows)",
        9,
        || {
            let keys = reader
                .thread_list_keys(inbox, newest, &all, Page::first(200_000))
                .unwrap();
            let ids: Vec<ThreadId> = keys[..60].iter().map(|k| k.thread).collect();
            (
                keys.len(),
                reader.thread_rows(&ids, Some(f.inbox)).unwrap().len(),
            )
        },
    );

    // Search support: 10k candidates through the metadata filter.
    let candidates: Vec<MessageId> = (0..10_000).map(|i| MessageId(i * 97 + 3)).collect();
    let (filter10k, passing) = time(
        "filter_message_ids, 10k candidates, unread + folder",
        9,
        || {
            reader
                .filter_message_ids(
                    &candidates,
                    &MetadataFilter {
                        folders: Some(vec![f.inbox]),
                        flags_unset: Flags::SEEN,
                        ..MetadataFilter::default()
                    },
                )
                .unwrap()
        },
    );
    assert!(!passing.is_empty());
    time("message_ids_in_scope, 100k folder", 5, || {
        reader
            .message_ids_in_scope(&MessageScope::Folder(f.archive))
            .unwrap()
    });

    drop(reader);

    // Writes: one flag change (the optimistic local apply of "mark read"), and marking
    // 10k messages read, both including the commit.
    let (one_write, ()) = time("write: mark 1 message read (commit included)", 9, || {
        store
            .write_blocking(|w| w.set_flags(&[MessageId(5)], Flags::SEEN, Flags::empty()))
            .unwrap();
    });
    let batch: Vec<MessageId> = (0..10_000).map(|i| MessageId(i * 7 + 11)).collect();
    let (bulk_write, ()) = time("write: mark 10k messages read (commit included)", 3, || {
        let batch = batch.clone();
        store
            .write_blocking(move |w| w.set_flags(&batch, Flags::SEEN, Flags::empty()))
            .unwrap();
    });

    let mut size = 0;
    for name in ["mazemail.db", "mazemail.db-wal"] {
        size += std::fs::metadata(tmp.path().join("mail").join(name))
            .map(|m| m.len())
            .unwrap_or(0);
    }
    eprintln!("database size: {:.0} MB", size as f64 / 1e6);
    common::close(store);

    eprintln!(
        "summary: first page {:.1} ms, all keys {:.1} ms, rows {:.2} ms, switch 100k {:.1} ms, \
         switch 1M {:.1} ms, filter 10k {:.1} ms, 1 write {:.1} ms, 10k flag writes {:.0} ms",
        first_page.as_secs_f64() * 1e3,
        full.as_secs_f64() * 1e3,
        rows.as_secs_f64() * 1e3,
        switch_small.as_secs_f64() * 1e3,
        switch_big.as_secs_f64() * 1e3,
        filter10k.as_secs_f64() * 1e3,
        one_write.as_secs_f64() * 1e3,
        bulk_write.as_secs_f64() * 1e3,
    );
    if !cfg!(debug_assertions) {
        assert!(first_page < Duration::from_millis(50), "first page budget");
        assert!(
            switch_small < Duration::from_millis(50),
            "folder switch budget"
        );
    }
}
