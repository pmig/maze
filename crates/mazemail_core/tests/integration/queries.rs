//! Folder tree, thread lists, rows and the row cache.

use mazemail_core::{FolderNodeKind, ListQuery, ListScope, QuickFilter, SortOrder};
use mazemail_types::{Flags, FolderRole, Sensitive};
use store::FolderPatch;

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
    let projects = f.projects;
    t.core
        .store()
        .write(move |w| {
            w.update_folder(
                projects,
                &FolderPatch {
                    favorite: Some(true),
                    ..FolderPatch::default()
                },
            )
        })
        .await
        .unwrap();
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
    assert_eq!(
        kinds,
        vec![
            FolderNodeKind::UnifiedSection,
            FolderNodeKind::FavoritesSection,
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

    let favorites = &tree[1];
    assert_eq!(favorites.children.len(), 1);
    assert_eq!(
        favorites.children[0].kind,
        FolderNodeKind::Folder(f.projects)
    );
    assert!(favorites.children[0].favorite);

    let work = &tree[2];
    assert_eq!(work.name, "Work");
    assert_eq!(work.children.len(), 5);
    let mut work_inbox = None;
    work.walk(&mut |node| {
        if node.kind == FolderNodeKind::Folder(f.inbox) {
            work_inbox = Some((node.unread, node.total, node.scope));
        }
    });
    assert_eq!(work_inbox, Some((2, 2, Some(ListScope::Folder(f.inbox)))));
    let gmail = &tree[3];
    assert!(gmail.children.iter().any(|n| n.is_label));
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_lists_are_key_snapshots_and_rows_come_from_the_cache() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
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
            sort: SortOrder::OldestFirst,
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

    // A change from elsewhere (sync marking a message read) invalidates the row.
    let mut events = t.core.events();
    // Let batches from the ingest above go by, so the next one is the change below.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    while events.try_recv().is_ok() {}
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
