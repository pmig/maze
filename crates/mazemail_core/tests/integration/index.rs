//! Index maintenance: consistency check, repair, automatic deletes, full rebuild.

use mazemail_core::{IndexDocument, SearchOpts};
use mazemail_types::{AccountId, MessageId, Sensitive, ThreadId};

use crate::common::{self, Place};

fn raw(n: u32, subject: &str) -> Vec<u8> {
    common::raw_message(
        &format!("i{n}@example.com"),
        "anna@example.com",
        "me@work.example",
        subject,
        "Mon, 6 Oct 2025 10:00:00 +0000",
        "Searchable body text about zeppelins",
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn check_finds_and_repairs_differences_and_deletes_follow_the_store() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let (a, _) = common::ingest(&t.core, Place::new(f.work, f.inbox, 1), raw(1, "One")).await;
    let (b, _) = common::ingest(&t.core, Place::new(f.work, f.inbox, 2), raw(2, "Two")).await;

    let check = t.core.check_index(false).await.unwrap();
    assert!(check.is_consistent(), "{check:?}");
    assert_eq!((check.store_messages, check.indexed_documents), (2, 2));

    // A document for a message the store doesn't have, and one indexed twice.
    let sink = t.core.index_sink();
    sink.add(IndexDocument {
        message_id: MessageId(9999),
        thread_id: ThreadId(9999),
        account_id: AccountId(1),
        date: 0,
        size: 0,
        from: Vec::new(),
        to: Vec::new(),
        cc: Vec::new(),
        subject: Sensitive::new("ghost".to_owned()),
        body_text: Sensitive::new(String::new()),
        attachment_names: Vec::new(),
        list_id: None,
        has_attachment: false,
        language: None,
    })
    .unwrap();
    let check = t.core.check_index(false).await.unwrap();
    assert_eq!(check.stale, vec![MessageId(9999)]);
    assert!(check.missing.is_empty());
    assert!(!check.repaired);

    let repaired = t.core.check_index(true).await.unwrap();
    assert!(repaired.repaired);
    assert!(t.core.check_index(false).await.unwrap().is_consistent());

    // A message deleted in the store leaves the index on its own.
    let mut events = t.core.events();
    t.core
        .store()
        .write(move |w| w.delete_message(a))
        .await
        .unwrap();
    common::wait_for(&mut events, |e| e.messages_removed.contains(&a)).await;
    let check = t.core.check_index(false).await.unwrap();
    assert!(check.is_consistent(), "{check:?}");
    assert_eq!(check.store_messages, 1);
    let found = t
        .core
        .search("zeppelins", SearchOpts::default())
        .await
        .unwrap();
    assert_eq!(found.threads.len(), 1);
    assert_eq!(found.threads[0].best_message, b);
}

#[tokio::test(flavor = "multi_thread")]
async fn reindex_rebuilds_from_store_and_blobs() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    common::ingest(&t.core, Place::new(f.work, f.inbox, 1), raw(1, "With body")).await;
    common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 2).headers_only(),
        raw(2, "Headers only"),
    )
    .await;
    let mut events = t.core.events();
    let report = t.core.reindex_all().await.unwrap();
    assert_eq!(report.indexed, 2);
    assert_eq!(report.headers_only, 1);
    assert_eq!(report.unreadable, 0);
    let batch = common::wait_for(&mut events, |e| {
        e.index
            .is_some_and(|i| i.indexed == 2 && i.rebuild.is_none())
    })
    .await;
    assert_eq!(batch.index.unwrap().pending, 0);
    assert!(t.core.check_index(false).await.unwrap().is_consistent());
    // The body text came from the blob; the header-only message is found by subject.
    let body = t
        .core
        .search("zeppelins", SearchOpts::default())
        .await
        .unwrap();
    assert_eq!(body.total_messages, 1);
    let subject = t
        .core
        .search("subject:headers", SearchOpts::default())
        .await
        .unwrap();
    assert_eq!(subject.total_messages, 1);
}
