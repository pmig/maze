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

#[tokio::test(flavor = "multi_thread")]
async fn typed_searches_finish_the_last_word_and_tolerate_typos() {
    let c = corpus().await;
    let interactive = SearchOpts {
        interactive: true,
        ..SearchOpts::default()
    };
    let search = |query: &'static str, opts: SearchOpts| {
        let core = c.t.core.clone();
        async move { core.search(query, opts).await.unwrap() }
    };

    // The word being typed matches as a prefix; exact searches don't.
    let typed = search("invo", interactive.clone()).await;
    assert_eq!(typed.total_messages, 2);
    assert!(!typed.approximate);
    assert_eq!(
        search("invo", SearchOpts::default()).await.total_messages,
        0
    );

    // A typo: nothing exact, so the search tolerates it and says so.
    let typo = search("invocie", interactive.clone()).await;
    assert_eq!(typo.total_messages, 2);
    assert!(typo.approximate);
    let exact = search("invocie", SearchOpts::default()).await;
    assert_eq!(exact.total_messages, 0, "the CLI and agents search exactly");
    assert!(!exact.approximate);

    // Exact matches win: no typo tolerance when something matches.
    let budget = search("budget", interactive).await;
    assert_eq!(threads(&budget.threads), vec![c.anna.1]);
    assert!(!budget.approximate);
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_mail_leaves_the_results_unless_trash_is_searched() {
    let c = corpus().await;
    let search = |query: &'static str, scope: Option<ListScope>| {
        let core = c.t.core.clone();
        async move {
            core.search(
                query,
                SearchOpts {
                    scope,
                    ..SearchOpts::default()
                },
            )
            .await
            .unwrap()
        }
    };
    assert_eq!(search("invoice", None).await.total_messages, 2);

    // Deleting the results (to Trash) takes them out of the search.
    c.t.core
        .apply(
            mazemail_core::MailCommand::Delete {
                ids: vec![c.anna.0, c.bob.0],
                permanent: false,
            },
            mazemail_core::Origin::User,
        )
        .await
        .unwrap();
    assert_eq!(search("invoice", None).await.total_messages, 0);

    // They're found when Trash is asked for: by the query or by the scope.
    assert_eq!(search("invoice in:trash", None).await.total_messages, 2);
    assert_eq!(
        search("invoice", Some(ListScope::Folder(c.f.trash)))
            .await
            .total_messages,
        2
    );
}

#[tokio::test]
async fn saved_searches_are_named_queries_whose_results_follow_the_mail() {
    let c = corpus().await;
    let core = &c.t.core;
    assert!(core.saved_searches().unwrap().is_empty());

    let invoices = core
        .create_saved_search(" Invoices ", "invoice")
        .await
        .unwrap();
    assert_eq!(invoices.name, "Invoices");
    let lunch = core
        .create_saved_search("Lunch", "subject:lunch")
        .await
        .unwrap();
    assert!(lunch.ordinal > invoices.ordinal, "new ones go last");
    let names: Vec<String> = core
        .saved_searches()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["Invoices", "Lunch"]);

    // Names are unique ignoring case; queries must parse.
    for (name, query) in [("invoices", "x"), ("", "x"), ("Other", "  ")] {
        let error = core.create_saved_search(name, query).await.unwrap_err();
        assert!(matches!(error, Error::InvalidCommand(_)), "{error}");
    }
    let error = core
        .create_saved_search("Broken", "from:(anna")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Query(_)), "{error}");

    // Found by id or by name.
    assert_eq!(
        core.find_saved_search(&invoices.id.to_string()).unwrap().id,
        invoices.id
    );
    assert_eq!(core.find_saved_search("LUNCH").unwrap().id, lunch.id);
    assert!(matches!(
        core.find_saved_search("nope"),
        Err(Error::NotFound { .. })
    ));

    let opts = || SearchOpts {
        sort: SearchSort::NewestFirst,
        ..SearchOpts::default()
    };
    let results = core.search_saved(invoices.id, None, opts()).await.unwrap();
    assert_eq!(threads(&results.threads), [c.anna.1, c.bob.1]);
    // The Quick Filter narrows it.
    let narrowed = core
        .search_saved(invoices.id, Some("scaffolding"), opts())
        .await
        .unwrap();
    assert_eq!(threads(&narrowed.threads), [c.bob.1]);

    // Results follow the mail: Anna's invoice read, a saved unread search drops it.
    let unread = core
        .create_saved_search("Unread invoices", "invoice is:unread")
        .await
        .unwrap();
    let before = core.search_saved(unread.id, None, opts()).await.unwrap();
    assert_eq!(threads(&before.threads), [c.anna.1]);
    core.apply(
        mazemail_core::MailCommand::SetFlags {
            ids: vec![c.anna.0],
            add: Flags::SEEN,
            remove: Flags::empty(),
        },
        mazemail_core::Origin::User,
    )
    .await
    .unwrap();
    let after = core.search_saved(unread.id, None, opts()).await.unwrap();
    assert!(after.threads.is_empty());

    // Rename and edit.
    let renamed = core
        .update_saved_search(
            invoices.id,
            mazemail_core::SavedSearchChange {
                name: Some("Bills".into()),
                query: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (renamed.name.as_str(), renamed.query.as_str()),
        ("Bills", "invoice")
    );
    let error = core
        .update_saved_search(
            invoices.id,
            mazemail_core::SavedSearchChange {
                name: Some("lunch".into()),
                query: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidCommand(_)), "{error}");
    core.update_saved_search(
        invoices.id,
        mazemail_core::SavedSearchChange {
            name: None,
            query: Some("from:bob".into()),
        },
    )
    .await
    .unwrap();
    let edited = core.search_saved(invoices.id, None, opts()).await.unwrap();
    assert_eq!(threads(&edited.threads), [c.bob.1]);

    // Delete.
    core.delete_saved_search(lunch.id).await.unwrap();
    assert!(matches!(
        core.delete_saved_search(lunch.id).await,
        Err(Error::NotFound { .. })
    ));
    assert!(matches!(
        core.search_saved(lunch.id, None, opts()).await,
        Err(Error::NotFound { .. })
    ));
    assert_eq!(core.saved_searches().unwrap().len(), 2);
}

/// A domain's mail spread over every folder of two accounts, more of it than the
/// search's small-set shortcuts cover: newest first and without a limit (the main
/// window's search list), every thread outside Trash shows, the newest first.
#[tokio::test(flavor = "multi_thread")]
async fn a_domain_search_finds_its_mail_in_every_folder() {
    use mazemail_types::{AccountId, Address, FolderId};
    use store::{NewLocation, NewMessage, ThreadAssignment};

    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let places: [(AccountId, FolderId); 6] = [
        (f.work, f.inbox),
        (f.work, f.archive),
        (f.work, f.projects),
        (f.work, f.sent),
        (f.gmail, f.g_label),
        (f.work, f.trash),
    ];
    const COUNT: i64 = 1_600;
    let stored = t
        .core
        .store()
        .write(move |w| {
            let mut out = Vec::new();
            for i in 0..COUNT {
                let (account, folder) = places[(i % 6) as usize];
                let mut m = NewMessage::new(account, 1_700_000_000 + i * 60);
                m.rfc_message_id = Some(format!("{i}.domain@example.com"));
                m.subject = Some(format!("Build {i} passed"));
                m.from = Some(Address::new(
                    Some("Builds"),
                    format!("builds{}@ci.northwind.example", i % 7),
                ));
                m.thread = ThreadAssignment::New;
                m.locations = vec![NewLocation::new(folder, Some(i as u32 + 1), Flags::SEEN)];
                let outcome = w.upsert_message(&m)?;
                out.push((outcome, m, folder));
            }
            Ok(out)
        })
        .await
        .unwrap();
    let sink = t.core.index_sink();
    let mut expected = Vec::new();
    for (outcome, m, folder) in stored {
        if folder != f.trash {
            expected.push((m.date, outcome.thread));
        }
        sink.add(mazemail_core::IndexDocument {
            message_id: outcome.message,
            thread_id: outcome.thread,
            account_id: m.account,
            date: m.date,
            size: 1024,
            from: m.from.into_iter().collect(),
            to: Vec::new(),
            cc: Vec::new(),
            subject: Sensitive::new(m.subject.unwrap_or_default()),
            body_text: Sensitive::default(),
            attachment_names: Vec::new(),
            list_id: None,
            has_attachment: false,
            language: None,
        })
        .unwrap();
    }
    t.core.commit_index().await.unwrap();
    expected.sort_by_key(|a| std::cmp::Reverse(a.0));
    let newest_first: Vec<ThreadId> = expected.iter().map(|(_, thread)| *thread).collect();

    for query in [
        "northwind.example",
        "from:northwind.example",
        "from:@northwind.example",
    ] {
        let results = t
            .core
            .search(
                query,
                SearchOpts {
                    limit: usize::MAX,
                    sort: SearchSort::NewestFirst,
                    snippets: false,
                    interactive: true,
                    ..SearchOpts::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(threads(&results.threads), newest_first, "{query}");
        assert_eq!(results.total_messages, newest_first.len(), "{query}");
        assert!(!results.truncated, "{query}");
    }
    // Trash holds a sixth of it.
    let trash = t
        .core
        .search("from:@northwind.example in:trash", SearchOpts::default())
        .await
        .unwrap();
    assert_eq!(trash.total_messages, (COUNT as usize).div_ceil(6) - 1);
}
