//! Folder tree, thread lists, rows and the row cache.

use std::sync::Arc;

use mazemail_core::{
    CoreHandle, FolderNodeKind, ListOrder, ListQuery, ListScope, QuickFilter, SortField,
};
use mazemail_types::{Flags, FolderRole, Sensitive};
use store::NewFolder;
use vault::MemoryKeyStore;

use crate::common::{self, Place};

fn message(n: u32, from: &str, subject: &str) -> Vec<u8> {
    common::raw_message(
        &format!("q{n}@example.com"),
        from,
        "me@work.example",
        subject,
        &format!("Mon, {} Oct 2025 10:00:00 +0000", n.clamp(1, 28)),
        "Hello there",
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn the_folder_tree_has_unified_folders_favorites_and_accounts() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 1),
        message(1, "a@x.example", "One"),
    )
    .await;
    common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 2),
        message(2, "b@x.example", "Two"),
    )
    .await;
    common::ingest(
        &t.core,
        Place::new(f.gmail, f.g_inbox, 1)
            .flags(Flags::SEEN)
            .gmail(77),
        message(3, "c@x.example", "Three"),
    )
    .await;

    let tree = t.core.folders().unwrap();
    let kinds: Vec<FolderNodeKind> = tree.iter().map(|n| n.kind).collect();
    // No favorites yet: no Favorites section.
    assert_eq!(
        kinds,
        vec![
            FolderNodeKind::UnifiedSection,
            FolderNodeKind::Account(f.work),
            FolderNodeKind::Account(f.gmail),
        ]
    );
    let unified = &tree[0];
    assert_eq!(unified.name, "Unified Folders");
    let roles: Vec<FolderRole> = unified.children.iter().filter_map(|n| n.role).collect();
    // Only roles some account has, in display order (no Drafts, no Junk here).
    assert_eq!(
        roles,
        vec![
            FolderRole::Inbox,
            FolderRole::Sent,
            FolderRole::Archive,
            FolderRole::Trash
        ]
    );
    let inbox = &unified.children[0];
    assert_eq!(inbox.scope, Some(ListScope::Unified(FolderRole::Inbox)));
    assert_eq!((inbox.unread, inbox.total), (2, 3));

    let work = &tree[1];
    assert_eq!(work.name, "Work");
    assert_eq!(work.children.len(), 5);
    let mut work_inbox = None;
    work.walk(&mut |node| {
        if node.kind == FolderNodeKind::Folder(f.inbox) {
            work_inbox = Some((node.unread, node.total, node.scope));
        }
    });
    assert_eq!(work_inbox, Some((2, 2, Some(ListScope::Folder(f.inbox)))));
    let gmail = &tree[2];
    assert!(gmail.children.iter().any(|n| n.is_label));
}

/// The favorite folders of `tree`, in order.
fn favorites(tree: &[mazemail_core::FolderNode]) -> Vec<FolderNodeKind> {
    tree.iter()
        .find(|n| n.kind == FolderNodeKind::FavoritesSection)
        .map(|section| section.children.iter().map(|n| n.kind).collect())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn favorite_folders_live_in_the_settings_file() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    std::fs::write(
        &settings,
        "{\n  // Mine.\n  \"theme\": { \"mode\": \"dark\" },\n  \"folder_pane\": {\n    \"favorites\": [\n      { \"account\": \"Work\", \"folder\": \"Gone\" }\n    ]\n  }\n}\n",
    )
    .unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let core = CoreHandle::open(
        common::config(&dir.path().join("data"), &keys).with_settings_file(&settings),
    )
    .await
    .unwrap();
    let f = common::fixture(&core).await;
    let acme = core
        .store()
        .write(move |w| {
            let mut acme = NewFolder::new(f.work, "Projects/Acme", "Acme");
            acme.parent = Some(f.projects);
            w.create_folder(&acme)
        })
        .await
        .unwrap();
    // The favorite whose folder doesn't exist is hidden.
    assert_eq!(favorites(&core.folders().unwrap()), vec![]);

    // Added out of order, shown in account order, then tree order.
    core.set_favorite_folder(f.g_label, true).unwrap();
    core.set_favorite_folder(acme, true).unwrap();
    core.set_favorite_folder(f.inbox, true).unwrap();
    core.set_favorite_folder(f.inbox, true).unwrap();
    let tree = core.folders().unwrap();
    assert_eq!(
        favorites(&tree),
        vec![
            FolderNodeKind::Folder(f.inbox),
            FolderNodeKind::Folder(acme),
            FolderNodeKind::Folder(f.g_label),
        ]
    );
    // Marked in the account trees too.
    let mut marked = Vec::new();
    for node in &tree {
        node.walk(&mut |n| {
            if n.favorite {
                marked.push(n.kind);
            }
        });
    }
    assert_eq!(marked.len(), 6, "three favorites, each twice");

    // In the file: label and path, the comment and the stale entry kept.
    let text = std::fs::read_to_string(&settings).unwrap();
    assert!(text.contains("// Mine."), "{text}");
    let loaded = settings::load_settings(&settings);
    assert_eq!(loaded.diagnostics, vec![]);
    let saved: Vec<(String, String)> = loaded
        .settings
        .folder_pane
        .favorites
        .into_iter()
        .map(|f| (f.account, f.folder))
        .collect();
    let entry = |a: &str, f: &str| (a.to_owned(), f.to_owned());
    assert_eq!(
        saved,
        vec![
            entry("Work", "Gone"),
            entry("Gmail", "Receipts"),
            entry("Work", "Projects/Acme"),
            entry("Work", "INBOX"),
        ]
    );
    assert_eq!(loaded.settings.theme.mode, settings::ThemeMode::Dark);

    // Removing one keeps the others.
    core.set_favorite_folder(acme, false).unwrap();
    assert_eq!(
        favorites(&core.folders().unwrap()),
        vec![
            FolderNodeKind::Folder(f.inbox),
            FolderNodeKind::Folder(f.g_label)
        ]
    );
    assert_eq!(
        settings::load_settings(&settings)
            .settings
            .folder_pane
            .favorites
            .len(),
        3
    );

    // A hand edit is picked up by the watcher.
    let mut events = core.events();
    std::fs::write(
        &settings,
        r#"{ "folder_pane": { "favorites": [{ "account": "Work", "folder": "Sent" }] } }"#,
    )
    .unwrap();
    // (The batch for the last change by Maze itself may still arrive first.)
    for _ in 0..3 {
        if favorites(&core.folders().unwrap()) == vec![FolderNodeKind::Folder(f.sent)] {
            break;
        }
        common::wait_for(&mut events, |batch| batch.settings).await;
    }
    assert_eq!(
        favorites(&core.folders().unwrap()),
        vec![FolderNodeKind::Folder(f.sent)]
    );

    // A broken file is never overwritten.
    std::fs::write(&settings, r#"{ "folder_pane": "#).unwrap();
    assert!(core.set_favorite_folder(f.inbox, true).is_err());
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        r#"{ "folder_pane": "#
    );
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn favorites_need_a_settings_file() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    assert!(matches!(
        t.core.set_favorite_folder(f.inbox, true),
        Err(mazemail_core::Error::SettingsWrite(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_lists_are_key_snapshots_and_rows_come_from_the_cache() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let mut events = t.core.events();
    let mut threads = Vec::new();
    for n in 1..=5 {
        let (_, thread) = common::ingest(
            &t.core,
            Place::new(f.work, f.inbox, n),
            message(n, &format!("sender{n}@x.example"), &format!("Subject {n}")),
        )
        .await;
        threads.push(thread);
    }
    // A write resolves before its change set reaches the row cache, which drops the
    // touched rows when it arrives. Wait for the last ingest's batch (change sets are
    // applied in commit order), so no ingest evicts the rows read below.
    let newest = threads[4];
    common::wait_for(&mut events, |b| b.threads_updated.contains(&newest)).await;
    let query = ListQuery::new(ListScope::Folder(f.inbox));
    let snapshot = t.core.thread_list(&query).unwrap();
    assert_eq!(snapshot.len(), 5);
    assert_eq!(snapshot.context(), Some(f.inbox));
    // Newest first.
    let ids = snapshot.thread_ids(0..10);
    assert_eq!(ids, threads.iter().rev().copied().collect::<Vec<_>>());
    assert_eq!(snapshot.position(threads[0]), Some(4));
    assert!(!snapshot.body_matches_pending());

    let oldest = t
        .core
        .thread_list(&ListQuery {
            sort: ListOrder::OLDEST_FIRST,
            ..query.clone()
        })
        .unwrap();
    assert_eq!(oldest.thread_ids(0..1), vec![threads[0]]);

    let rows = t.core.rows(&ids[..3], snapshot.context()).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].subject.as_deref(), Some("Subject 5"));
    assert_eq!(rows[0].unread_count, 1);
    let stats = t.core.row_cache_stats();
    assert_eq!((stats.len, stats.misses, stats.hits), (3, 3, 0));
    t.core.rows(&ids[..3], snapshot.context()).unwrap();
    assert_eq!(t.core.row_cache_stats().hits, 3);

    // A change from elsewhere (sync marking a message read) invalidates the row. The
    // ingest's batch for this thread was consumed above, so the next one is this change.
    let newest_message = rows[0].last_message.unwrap();
    t.core
        .store()
        .write(move |w| w.set_flags(&[newest_message], Flags::SEEN, Flags::empty()))
        .await
        .unwrap();
    let target = ids[0];
    common::wait_for(&mut events, |b| b.threads_updated.contains(&target)).await;
    let rows = t.core.rows(&ids[..1], snapshot.context()).unwrap();
    assert_eq!(rows[0].unread_count, 0);

    // Quick Filter toggles go to SQL.
    let unread = t
        .core
        .thread_list(&ListQuery {
            quick_filter: QuickFilter {
                unread: true,
                ..QuickFilter::default()
            },
            ..query.clone()
        })
        .unwrap();
    assert_eq!(unread.len(), 4);
    assert_eq!(unread.position(target), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn unified_lists_merge_accounts_and_text_filters_match_in_memory() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let (_, work_thread) = common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 1),
        message(1, "Anna Example <anna@x.example>", "Quarterly numbers"),
    )
    .await;
    let (_, gmail_thread) = common::ingest(
        &t.core,
        Place::new(f.gmail, f.g_inbox, 1).gmail(5),
        message(2, "bob@y.example", "Lunch?"),
    )
    .await;
    let query = ListQuery::new(ListScope::Unified(FolderRole::Inbox));
    let all = t.core.thread_list(&query).unwrap();
    assert_eq!(all.thread_ids(0..10), vec![gmail_thread, work_thread]);
    assert_eq!(all.context(), None);
    let rows = t.core.rows(&all.thread_ids(0..10), None).unwrap();
    assert_eq!(rows.len(), 2);

    let by_text = |text: &str| {
        t.core
            .thread_list(&ListQuery {
                quick_filter: QuickFilter {
                    text: Some(Sensitive::new(text.to_owned())),
                    ..QuickFilter::default()
                },
                ..query.clone()
            })
            .unwrap()
    };
    let anna = by_text("ANNA quarterly");
    assert_eq!(anna.thread_ids(0..10), vec![work_thread]);
    assert!(anna.body_matches_pending());
    assert_eq!(by_text("y.example").thread_ids(0..10), vec![gmail_thread]);
    assert!(by_text("nobody").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_sort_by_any_column_after_the_quick_filter() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let mut threads = Vec::new();
    let mut messages = Vec::new();
    for (n, from, subject, flags) in [
        (1, "Zed <zed@x.example>", "Re: Apples", Flags::empty()),
        (2, "anna@x.example", "Bananas", Flags::SEEN),
        (3, "Bob <bob@x.example>", "AW: cherries", Flags::empty()),
    ] {
        let (message_id, thread) = common::ingest(
            &t.core,
            Place::new(f.work, f.inbox, n).flags(flags),
            message(n, from, subject),
        )
        .await;
        threads.push(thread);
        messages.push(message_id);
    }
    let sorted = |field: SortField, descending: bool, unread: bool| ListQuery {
        sort: ListOrder::new(field, descending),
        quick_filter: QuickFilter {
            unread,
            ..QuickFilter::default()
        },
        ..ListQuery::new(ListScope::Folder(f.inbox))
    };
    let thread_order = |query: &ListQuery| t.core.thread_list(query).unwrap().thread_ids(0..10);
    let (apples, bananas, cherries) = (threads[0], threads[1], threads[2]);
    // Subjects without "Re:" and "AW:", ignoring case.
    assert_eq!(
        thread_order(&sorted(SortField::Subject, false, false)),
        [apples, bananas, cherries]
    );
    assert_eq!(
        thread_order(&sorted(SortField::Subject, true, false)),
        [cherries, bananas, apples]
    );
    // Display name, else address.
    assert_eq!(
        thread_order(&sorted(SortField::Sender, false, false)),
        [bananas, cherries, apples]
    );
    // The Quick Filter first, then the order.
    assert_eq!(
        thread_order(&sorted(SortField::Subject, true, true)),
        [cherries, apples]
    );
    let messages_by_subject = t
        .core
        .message_list(&sorted(SortField::Subject, false, false))
        .unwrap();
    assert_eq!(messages_by_subject.message_ids(0..10), messages);
    // Search results (any folder) sort the same way.
    assert_eq!(
        t.core
            .sort_threads(
                &[cherries, apples, bananas],
                ListOrder::new(SortField::Subject, false)
            )
            .unwrap(),
        threads
    );
    let mut reversed = messages.clone();
    reversed.reverse();
    assert_eq!(
        t.core
            .sort_messages(&messages, ListOrder::new(SortField::Subject, true))
            .unwrap(),
        reversed
    );
}

/// A reply with `In-Reply-To` and `References`.
fn reply(n: u32, from: &str, to: &str, parents: &[u32]) -> Vec<u8> {
    let refs: Vec<String> = parents
        .iter()
        .map(|p| format!("<q{p}@example.com>"))
        .collect();
    format!(
        "Message-ID: <q{n}@example.com>\r\nDate: Mon, {} Oct 2025 10:00:00 +0000\r\n\
         From: {from}\r\nTo: {to}\r\nSubject: Re: Plan\r\nIn-Reply-To: {}\r\n\
         References: {}\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nok\r\n",
        n.clamp(1, 28),
        refs.last().cloned().unwrap_or_default(),
        refs.join(" "),
    )
    .into_bytes()
}

#[tokio::test(flavor = "multi_thread")]
async fn message_lists_rows_and_thread_trees() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    // Plan (1) ─┬─ my reply in Sent (2) ── answer (4)
    //           └─ second reply (3)
    // plus an unrelated message (5).
    common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 1),
        message(1, "Anna <anna@x.example>", "Plan"),
    )
    .await;
    common::ingest(
        &t.core,
        Place::new(f.work, f.sent, 1).flags(Flags::SEEN),
        reply(
            2,
            "Me <Me+maze@Work.Example>",
            "Anna <anna@x.example>, bob@x.example",
            &[1],
        ),
    )
    .await;
    common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 2).flags(Flags::SEEN),
        reply(3, "Bob <bob@x.example>", "me@work.example", &[1]),
    )
    .await;
    common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 3),
        reply(4, "Anna <anna@x.example>", "me@work.example", &[1, 2]),
    )
    .await;
    common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 4).flags(Flags::SEEN),
        message(5, "Shop <shop@x.example>", "Receipt"),
    )
    .await;

    // Threads off: one row per message, newest first.
    let inbox = ListQuery::new(ListScope::Folder(f.inbox));
    let list = t.core.message_list(&inbox).unwrap();
    assert_eq!(list.len(), 4);
    let ids = list.message_ids(0..4);
    let rows = t.core.message_rows(&ids, list.context()).unwrap();
    let dates: Vec<i64> = rows.iter().map(|r| r.row.date).collect();
    assert!(dates.windows(2).all(|w| w[0] >= w[1]));
    let oldest = t
        .core
        .message_list(&ListQuery {
            sort: ListOrder::OLDEST_FIRST,
            ..inbox
        })
        .unwrap();
    let mut reversed = oldest.message_ids(0..4);
    reversed.reverse();
    assert_eq!(reversed, ids);

    // Quick filter: toggles in SQL, text keeps the matching conversations' messages.
    let unread = t
        .core
        .message_list(&ListQuery {
            quick_filter: QuickFilter {
                unread: true,
                ..QuickFilter::default()
            },
            ..ListQuery::new(ListScope::Folder(f.inbox))
        })
        .unwrap();
    assert_eq!(unread.len(), 2);
    let plan = t
        .core
        .message_list(&ListQuery {
            quick_filter: QuickFilter {
                text: Some(Sensitive::new("plan".to_owned())),
                ..QuickFilter::default()
            },
            ..ListQuery::new(ListScope::Folder(f.inbox))
        })
        .unwrap();
    assert_eq!(plan.len(), 3, "the receipt is filtered out");

    // Correspondents: my reply (identity match despite case and +tag) lists recipients.
    let sent = t
        .core
        .message_list(&ListQuery::new(ListScope::Folder(f.sent)))
        .unwrap();
    let mine = t
        .core
        .message_rows(&sent.message_ids(0..1), sent.context())
        .unwrap();
    assert!(mine[0].outgoing);
    assert_eq!(mine[0].correspondents().text(), "Anna, bob@x.example");
    let received = rows
        .iter()
        .find(|r| r.row.subject.as_deref() == Some("Receipt"));
    let received = received.unwrap();
    assert!(!received.outgoing);
    assert_eq!(received.correspondents().text(), "Shop");

    // The reply tree: in the account all four messages, in the inbox my reply is
    // missing and its answer hangs below the root instead.
    let thread = rows
        .iter()
        .find(|r| r.row.subject.as_deref() == Some("Plan"))
        .unwrap()
        .row
        .thread;
    let tree = t.core.thread_tree(thread, None).unwrap();
    let shape: Vec<u16> = tree.iter().map(|n| n.depth).collect();
    assert_eq!(shape, vec![0, 1, 2, 1]);
    let in_inbox = t.core.thread_tree(thread, Some(f.inbox)).unwrap();
    assert_eq!(in_inbox.len(), 3);
    assert!(in_inbox[1..].iter().all(|n| n.depth == 1));
    assert_eq!(
        t.core
            .thread_message_counts(&[thread], Some(f.inbox))
            .unwrap(),
        vec![3]
    );
    assert_eq!(
        t.core.thread_message_counts(&[thread], None).unwrap(),
        vec![4]
    );
}
