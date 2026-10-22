//! Global search and the Quick Filter.

use mazemail_core::{
    Error, ListQuery, ListScope, QuickFilter, SearchOpts, SearchSort, SearchThread,
};
use mazemail_types::{Flags, MessageId, Sensitive, ThreadId};
use time::UtcOffset;

use crate::common::{self, Fixture, Place, TestCore};

struct Corpus {
    t: TestCore,
    f: Fixture,
    anna: (MessageId, ThreadId),
    bob: (MessageId, ThreadId),
    german: (MessageId, ThreadId),
    lunch: (MessageId, ThreadId),
}

async fn corpus() -> Corpus {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let anna = common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 1),
        common::raw_message(
            "anna@example.com",
            "Anna Example <anna@example.com>",
            "me@work.example",
            "Invoice for September",
            "Mon, 6 Oct 2025 10:00:00 +0000",
            "Please find the invoice attached. The budget review is next week.",
        ),
    )
    .await;
    let bob = common::ingest(
        &t.core,
        Place::new(f.work, f.projects, 2).flags(Flags::SEEN | Flags::FLAGGED),
        common::raw_message(
            "bob@example.com",
            "Bob Builder <bob@builder.example>",
            "me@work.example",
            "Another invoice",
            "Tue, 7 Jan 2025 10:00:00 +0000",
            "Invoice number 42 for the scaffolding.",
        ),
    )
    .await;
    let german = common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 3).flags(Flags::SEEN),
        common::raw_message(
            "carla@example.com",
            "Carla <carla@example.de>",
            "me@work.example",
            "Rechnungen und Unterlagen",
            "Wed, 8 Oct 2025 10:00:00 +0000",
            "Hallo zusammen, anbei die Rechnungen für die Besprechung am Donnerstag. \
             Bitte gebt mir bis morgen Bescheid, ob der Termin für euch passt.",
        ),
    )
    .await;
    let lunch = common::ingest(
        &t.core,
        Place::new(f.gmail, f.g_inbox, 1).gmail(9),
        common::raw_message(
            "dan@example.com",
            "Dan <dan@example.org>",
            "me@gmail.example",
            "Lunch on Friday?",
            "Thu, 9 Oct 2025 12:00:00 +0000",
            "Shall we try the new place around the corner?",
        ),
    )
    .await;
    t.core.commit_index().await.unwrap();
    Corpus {
        t,
        f,
        anna,
        bob,
        german,
        lunch,
    }
}

fn threads(results: &[SearchThread]) -> Vec<ThreadId> {
    results.iter().map(|t| t.thread).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn text_metadata_and_mixed_queries() {
    let c = corpus().await;
    let search = |q: &'static str| {
        let core = c.t.core.clone();
        async move { core.search(q, SearchOpts::default()).await.unwrap() }
    };

    let invoice = search("invoice").await;
    assert_eq!(invoice.total_messages, 2);
    let mut found = threads(&invoice.threads);
    found.sort();
    let mut expected = vec![c.anna.1, c.bob.1];
    expected.sort();
    assert_eq!(found, expected);
    assert!(invoice.warnings.is_empty());

    // Text AND a mutable flag: the flag is filtered in SQLite.
    assert_eq!(
        threads(&search("invoice is:unread").await.threads),
        vec![c.anna.1]
    );
    assert_eq!(
        threads(&search("invoice is:starred").await.threads),
        vec![c.bob.1]
    );
    assert_eq!(
        threads(&search("from:bob invoice").await.threads),
        vec![c.bob.1]
    );
    // Folder by name and by role, account by name.
    assert_eq!(threads(&search("in:projects").await.threads), vec![c.bob.1]);
    assert_eq!(search("in:inbox").await.total_messages, 3);
    assert_eq!(
        threads(&search("account:gmail").await.threads),
        vec![c.lunch.1]
    );
    assert_eq!(search("in:nonexistent").await.total_messages, 0);
    // Mixed OR across index and metadata.
    let mixed = search("lunch OR is:starred").await;
    let mut found = threads(&mixed.threads);
    found.sort();
    let mut expected = vec![c.bob.1, c.lunch.1];
    expected.sort();
    assert_eq!(found, expected);
    // Negation of metadata.
    assert_eq!(search("invoice -in:inbox").await.total_messages, 1);
    // Stemming: the German plural finds the German message.
    assert_eq!(threads(&search("rechnung").await.threads), vec![c.german.1]);
    // Dates in the user's time zone.
    assert_eq!(search("before:2025-06-01").await.total_messages, 1);
    assert_eq!(search("after:2025-10-07").await.total_messages, 2);

    let parse_error = c.t.core.search("from:(", SearchOpts::default()).await;
    assert!(
        matches!(parse_error, Err(Error::Query(_))),
        "{parse_error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn snippets_facets_scope_and_order() {
    let c = corpus().await;
    let results =
        c.t.core
            .search(
                "invoice",
                SearchOpts {
                    facets: true,
                    sort: SearchSort::NewestFirst,
                    ..SearchOpts::default()
                },
            )
            .await
            .unwrap();
    assert_eq!(threads(&results.threads), vec![c.anna.1, c.bob.1]);
    let first = &results.threads[0];
    assert_eq!(first.best_message, c.anna.0);
    let subject = first.subject.as_ref().unwrap();
    assert_eq!(subject.fragment, "Invoice for September");
    assert_eq!(subject.highlighted, vec![0..7]);
    let body = first.body.as_ref().unwrap();
    assert!(body.fragment.contains("invoice"), "{:?}", body.fragment);
    assert!(!body.highlighted.is_empty());

    let facets = results.facets.unwrap();
    let senders: Vec<&str> = facets.senders.iter().map(|(s, _)| s.as_str()).collect();
    assert!(senders.contains(&"anna@example.com"), "{senders:?}");
    assert!(senders.contains(&"bob@builder.example"), "{senders:?}");
    assert_eq!(facets.with_attachment + facets.without_attachment, 2);
    let mut folders = facets.folders.clone();
    folders.sort();
    let mut expected = vec![(c.f.inbox, 1), (c.f.projects, 1)];
    expected.sort();
    assert_eq!(folders, expected);
    assert_eq!(facets.histogram.iter().map(|(_, n)| n).sum::<u64>(), 2);

    let scoped =
        c.t.core
            .search(
                "invoice",
                SearchOpts {
                    scope: Some(ListScope::Folder(c.f.projects)),
                    ..SearchOpts::default()
                },
            )
            .await
            .unwrap();
    assert_eq!(threads(&scoped.threads), vec![c.bob.1]);

    let paged =
        c.t.core
            .search(
                "invoice",
                SearchOpts {
                    limit: 1,
                    offset: 1,
                    sort: SearchSort::OldestFirst,
                    snippets: false,
                    ..SearchOpts::default()
                },
            )
            .await
            .unwrap();
    assert_eq!(paged.total_threads, 2);
    assert_eq!(threads(&paged.threads), vec![c.anna.1]);
    assert!(paged.threads[0].subject.is_none());

    // A metadata-only query still gets plain snippets.
    let starred =
        c.t.core
            .search("is:starred", SearchOpts::default())
            .await
            .unwrap();
    assert_eq!(
        starred.threads[0].subject.as_ref().unwrap().fragment,
        "Another invoice"
    );
    assert!(
        starred.threads[0]
            .subject
            .as_ref()
            .unwrap()
            .highlighted
            .is_empty()
    );

    // Dates follow the time zone: 2025-10-08 10:00 UTC is the 9th in UTC+14.
    let far_east =
        c.t.core
            .search(
                "after:2025-10-09",
                SearchOpts {
                    utc_offset: Some(UtcOffset::from_hms(14, 0, 0).unwrap()),
                    ..SearchOpts::default()
                },
            )
            .await
            .unwrap();
    assert_eq!(far_east.total_messages, 2);
    let utc =
        c.t.core
            .search("after:2025-10-09", SearchOpts::default())
            .await
            .unwrap();
    assert_eq!(utc.total_messages, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_quick_filter_adds_body_matches_from_the_index() {
    let c = corpus().await;
    let query = |text: &str, unread: bool| ListQuery {
        quick_filter: QuickFilter {
            unread,
            text: Some(Sensitive::new(text.to_owned())),
            ..QuickFilter::default()
        },
        ..ListQuery::new(ListScope::Folder(c.f.inbox))
    };

    // "budget" is only in a body: nothing at once, the thread after the index step.
    let instant = c.t.core.thread_list(&query("budget", false)).unwrap();
    assert!(instant.is_empty());
    assert!(instant.body_matches_pending());
    let full =
        c.t.core
            .quick_filter_body(&query("budget", false))
            .await
            .unwrap();
    assert_eq!(full.thread_ids(0..10), vec![c.anna.1]);
    assert!(!full.body_matches_pending());

    // Sender matches show at once; the body step keeps them.
    let instant = c.t.core.thread_list(&query("carla", false)).unwrap();
    assert_eq!(instant.thread_ids(0..10), vec![c.german.1]);
    let full =
        c.t.core
            .quick_filter_body(&query("carla", false))
            .await
            .unwrap();
    assert_eq!(full.thread_ids(0..10), vec![c.german.1]);

    // Toggles and text combine; the index step stays inside the folder.
    let unread =
        c.t.core
            .quick_filter_body(&query("invoice", true))
            .await
            .unwrap();
    assert_eq!(unread.thread_ids(0..10), vec![c.anna.1]);
    let elsewhere =
        c.t.core
            .quick_filter_body(&query("scaffolding", false))
            .await
            .unwrap();
    assert!(elsewhere.is_empty(), "bob's message is in another folder");
}
