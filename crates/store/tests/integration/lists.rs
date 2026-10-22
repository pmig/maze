//! Thread list keys with keyset pagination, rows, and search id-set filters.

use std::collections::HashSet;

use mazemail_types::{Flags, FolderRole, MessageId};
use pretty_assertions::assert_eq;
use proptest::prelude::*;
use store::{
    AccountKind, ListFilter, ListKey, ListScope, MessageScope, MetadataFilter, NewLocation, Page,
    Queries, SortOrder, Store,
};

use crate::common::{self, Fixture, message};

/// Pages through a scope and returns every key.
fn all_pages(
    store: &Store,
    scope: ListScope,
    sort: SortOrder,
    filter: &ListFilter,
    size: usize,
) -> Vec<ListKey> {
    let reader = store.reader().unwrap();
    let mut out: Vec<ListKey> = Vec::new();
    let mut page = Page::first(size);
    loop {
        let keys = reader.thread_list_keys(scope, sort, filter, page).unwrap();
        let done = keys.len() < size;
        if let Some(last) = keys.last() {
            page = Page::after(*last, size);
        }
        out.extend(keys);
        if done {
            return out;
        }
    }
}

/// Inserts `n` single-message threads with colliding dates; every third is unread,
/// every fifth flagged, every seventh has an attachment.
fn populate(store: &Store, f: &Fixture, n: u32) {
    let (account, inbox) = (f.account, f.inbox);
    store
        .write_blocking(move |w| {
            for i in 0..n {
                let mut m = message(account, inbox, i + 1, i64::from(i / 3));
                let mut flags = Flags::SEEN;
                if i % 3 == 0 {
                    flags.remove(Flags::SEEN);
                }
                if i % 5 == 0 {
                    flags |= Flags::FLAGGED;
                }
                m.has_attachments = i % 7 == 0;
                m.locations[0].flags = flags;
                w.upsert_message(&m)?;
            }
            Ok(())
        })
        .unwrap();
}

fn expected(store: &Store, scope: ListScope, sort: SortOrder, filter: &ListFilter) -> Vec<ListKey> {
    let mut all = all_pages(store, scope, sort, filter, 1_000_000);
    // Sanity: the single page is sorted.
    let mut sorted = all.clone();
    match sort {
        SortOrder::NewestFirst => sorted.sort_by_key(|k| std::cmp::Reverse((k.key, k.thread))),
        SortOrder::OldestFirst => sorted.sort_by_key(|k| (k.key, k.thread)),
    }
    assert_eq!(all, sorted);
    all.dedup();
    all
}

#[test]
fn keyset_pages_concatenate_to_the_full_list() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    populate(&store, &f, 200);

    let filters = [
        ListFilter::default(),
        ListFilter {
            unread: true,
            ..ListFilter::default()
        },
        ListFilter {
            flagged: true,
            ..ListFilter::default()
        },
        ListFilter {
            has_attachment: true,
            unread: true,
            ..ListFilter::default()
        },
    ];
    for scope in [
        ListScope::Folder(f.inbox),
        ListScope::Account(f.account),
        ListScope::Unified(FolderRole::Inbox),
    ] {
        for sort in [SortOrder::NewestFirst, SortOrder::OldestFirst] {
            for filter in &filters {
                let full = expected(&store, scope, sort, filter);
                for size in [1, 7, 64] {
                    let paged = all_pages(&store, scope, sort, filter, size);
                    assert_eq!(paged, full, "{scope:?} {sort:?} {filter:?} page {size}");
                }
            }
        }
    }
    let full = expected(
        &store,
        ListScope::Folder(f.inbox),
        SortOrder::NewestFirst,
        &ListFilter::default(),
    );
    assert_eq!(full.len(), 200);
    let unread = expected(
        &store,
        ListScope::Folder(f.inbox),
        SortOrder::NewestFirst,
        &ListFilter {
            unread: true,
            ..ListFilter::default()
        },
    );
    assert_eq!(unread.len(), 67);
}

#[test]
fn unified_merges_accounts_and_tag_filter_works() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let a = common::fixture(&store, AccountKind::Imap);
    let b = common::fixture(&store, AccountKind::Imap);
    populate(&store, &a, 30);
    populate(&store, &b, 20);
    let unified = expected(
        &store,
        ListScope::Unified(FolderRole::Inbox),
        SortOrder::NewestFirst,
        &ListFilter::default(),
    );
    assert_eq!(unified.len(), 50);
    for size in [3, 10] {
        assert_eq!(
            all_pages(
                &store,
                ListScope::Unified(FolderRole::Inbox),
                SortOrder::NewestFirst,
                &ListFilter::default(),
                size
            ),
            unified
        );
    }

    // Tag two messages in account a.
    let reader = store.reader().unwrap();
    let todo = reader.tag_by_keyword("$label4").unwrap().unwrap().id;
    let ids: Vec<MessageId> = reader
        .message_ids_in_scope(&MessageScope::Folder(a.inbox))
        .unwrap()
        .into_iter()
        .take(2)
        .collect();
    drop(reader);
    let tagged = ids;
    store
        .write_blocking(move |w| w.set_tags(&tagged, &[todo], &[]))
        .unwrap();
    let filter = ListFilter {
        tag: Some(todo),
        ..ListFilter::default()
    };
    for scope in [
        ListScope::Folder(a.inbox),
        ListScope::Account(a.account),
        ListScope::Unified(FolderRole::Inbox),
    ] {
        assert_eq!(
            all_pages(&store, scope, SortOrder::NewestFirst, &filter, 1).len(),
            2,
            "{scope:?}"
        );
    }
    assert!(
        all_pages(
            &store,
            ListScope::Folder(b.inbox),
            SortOrder::NewestFirst,
            &filter,
            10
        )
        .is_empty()
    );
}

#[test]
fn thread_rows_follow_the_requested_order() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    populate(&store, &f, 10);
    let reader = store.reader().unwrap();
    let keys = reader
        .thread_list_keys(
            ListScope::Folder(f.inbox),
            SortOrder::NewestFirst,
            &ListFilter::default(),
            Page::first(10),
        )
        .unwrap();
    let mut ids: Vec<_> = keys.iter().map(|k| k.thread).collect();
    ids.reverse();
    ids.push(mazemail_types::ThreadId(999_999));
    let rows = reader.thread_rows(&ids, Some(f.inbox)).unwrap();
    assert_eq!(rows.len(), 10);
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), ids[..10]);
    let row = rows.iter().find(|r| r.id == keys[0].thread).unwrap();
    assert_eq!(row.date, keys[0].key);
    assert_eq!(row.message_count, 1);
    assert_eq!(row.participants.len(), 1);
    assert!(row.subject.as_deref().unwrap().starts_with("Subject"));
}

fn arb_flags() -> impl Strategy<Value = Flags> {
    (0u32..16).prop_map(Flags::from_bits_truncate)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// `filter_message_ids` agrees with filtering the message details in Rust.
    #[test]
    fn filter_matches_a_naive_filter(
        messages in proptest::collection::vec((proptest::collection::vec((0..3usize, arb_flags()), 0..3), any::<bool>()), 1..30),
        set in arb_flags(),
        unset in arb_flags(),
        folder_pick in proptest::option::of(0..3usize),
        role in proptest::option::of(prop_oneof![Just(FolderRole::Inbox), Just(FolderRole::Sent)]),
        with_tag in any::<bool>(),
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let store = common::open(tmp.path());
        let f = common::fixture(&store, AccountKind::Imap);
        let folders = [f.inbox, f.archive, f.sent];
        let account = f.account;
        let ids: Vec<MessageId> = store.write_blocking(move |w| {
            let mut ids = Vec::new();
            let mut uid = 0;
            for (i, (locations, tagged)) in messages.iter().enumerate() {
                let mut m = message(account, folders[0], 10_000 + i as u32, i as i64);
                m.locations.clear();
                let mut used = HashSet::new();
                for (folder, flags) in locations {
                    if used.insert(*folder) {
                        uid += 1;
                        let mut loc = NewLocation::new(folders[*folder], Some(uid), *flags);
                        if *tagged {
                            loc.keywords = vec!["$label1".to_owned()];
                        }
                        m.locations.push(loc);
                    }
                }
                ids.push(w.upsert_message(&m)?.message);
            }
            Ok(ids)
        }).unwrap();

        let reader = store.reader().unwrap();
        let important = reader.tag_by_keyword("$label1").unwrap().unwrap().id;
        let filter = MetadataFilter {
            folders: folder_pick.map(|i| vec![folders[i]]),
            account: Some(account),
            in_role: role,
            flags_set: set,
            flags_unset: unset,
            tags: if with_tag { vec![important] } else { Vec::new() },
        };
        // Candidates in a scrambled order, with an unknown id.
        let mut candidates: Vec<MessageId> = ids.iter().rev().copied().collect();
        candidates.push(MessageId(424_242));
        let got = reader.filter_message_ids(&candidates, &filter).unwrap();

        let roles = |folder| [FolderRole::Inbox, FolderRole::Archive, FolderRole::Sent][folders.iter().position(|f| *f == folder).unwrap()];
        let want: Vec<MessageId> = candidates.iter().copied().filter(|id| {
            let Some(d) = reader.message(*id).unwrap() else { return false };
            if d.locations.is_empty() { return false; }
            let eff = d.effective_flags();
            filter.folders.as_ref().is_none_or(|fs| d.locations.iter().any(|l| fs.contains(&l.folder)))
                && filter.in_role.is_none_or(|r| d.locations.iter().any(|l| roles(l.folder) == r))
                && eff.contains(set)
                && (eff & unset).is_empty()
                && (!with_tag || d.tags.contains(&important))
        }).collect();
        prop_assert_eq!(got, want);
    }
}

#[test]
fn scope_id_sets_and_facets() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    populate(&store, &f, 12);
    let (account, sent) = (f.account, f.sent);
    let extra = store
        .write_blocking(move |w| w.upsert_message(&message(account, sent, 500, 99)))
        .unwrap();
    let reader = store.reader().unwrap();
    let inbox = reader
        .message_ids_in_scope(&MessageScope::Folder(f.inbox))
        .unwrap();
    assert_eq!(inbox.len(), 12);
    assert!(inbox.windows(2).all(|w| w[0] < w[1]));
    let both = reader
        .message_ids_in_scope(&MessageScope::Folders(vec![f.inbox, f.sent]))
        .unwrap();
    assert_eq!(both.len(), 13);
    assert_eq!(
        reader
            .message_ids_in_scope(&MessageScope::Role(FolderRole::Sent))
            .unwrap(),
        vec![extra.message]
    );
    assert_eq!(
        reader
            .message_ids_in_scope(&MessageScope::Account(f.account))
            .unwrap()
            .len(),
        13
    );
    assert_eq!(
        reader
            .message_ids_in_scope(&MessageScope::Thread(extra.thread))
            .unwrap(),
        vec![extra.message]
    );
    let counts = reader.folder_counts(&both).unwrap();
    assert_eq!(counts, vec![(f.inbox, 12), (f.sent, 1)]);
}
