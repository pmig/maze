//! Message list keys (threads off) with keyset pagination, message rows and the
//! messages a thread's reply tree is built from.

use std::collections::HashSet;

use mazemail_types::{Address, Flags, FolderRole, MessageId};
use pretty_assertions::assert_eq;
use store::{
    AccountKind, ListFilter, ListScope, MessageKey, MessagePage, NewLocation, Queries, SortOrder,
    Store, ThreadAssignment,
};

use crate::common::{self, Fixture, message};

fn all_pages(
    store: &Store,
    scope: ListScope,
    sort: SortOrder,
    filter: &ListFilter,
    size: usize,
) -> Vec<MessageKey> {
    let reader = store.reader().unwrap();
    let mut out: Vec<MessageKey> = Vec::new();
    let mut page = MessagePage::first(size);
    loop {
        let keys = reader.message_list_keys(scope, sort, filter, page).unwrap();
        let done = keys.len() < size;
        if let Some(last) = keys.last() {
            page = MessagePage::after(*last, size);
        }
        out.extend(keys);
        if done {
            // Unified scopes may repeat a message on a later page.
            let mut seen = HashSet::new();
            out.retain(|k| seen.insert(k.message));
            return out;
        }
    }
}

/// `n` messages with colliding dates in groups of three threads; every third unread,
/// every fifth starred, every seventh with an attachment, every fourth also in the
/// archive (read there), every sixth tagged.
fn populate(store: &Store, f: &Fixture, n: u32) -> Vec<MessageId> {
    let (account, inbox, archive) = (f.account, f.inbox, f.archive);
    store
        .write_blocking(move |w| {
            let todo = w
                .tag_by_keyword("$label4")?
                .map(|t| t.id)
                .expect("default tags exist");
            let mut ids = Vec::new();
            let mut threads = Vec::new();
            for i in 0..n {
                let mut m = message(account, inbox, i + 1, i64::from(i / 2));
                let mut flags = Flags::SEEN;
                if i % 3 == 0 {
                    flags.remove(Flags::SEEN);
                }
                if i % 5 == 0 {
                    flags |= Flags::FLAGGED;
                }
                m.has_attachments = i % 7 == 0;
                m.locations[0].flags = flags;
                if i % 4 == 0 {
                    m.locations
                        .push(NewLocation::new(archive, Some(1000 + i), Flags::SEEN));
                }
                if i >= 3 {
                    m.thread = ThreadAssignment::Existing(threads[(i % 3) as usize]);
                }
                let outcome = w.upsert_message(&m)?;
                if i < 3 {
                    threads.push(outcome.thread);
                }
                if i % 6 == 0 {
                    w.set_tags(&[outcome.message], &[todo], &[])?;
                }
                ids.push(outcome.message);
            }
            Ok(ids)
        })
        .unwrap()
}

/// The keys a scope should list, computed from the message details.
fn naive(
    store: &Store,
    ids: &[MessageId],
    scope: ListScope,
    sort: SortOrder,
    filter: &ListFilter,
) -> Vec<MessageKey> {
    let reader = store.reader().unwrap();
    let folders = reader.folders(None).unwrap();
    let mut keys: Vec<MessageKey> = ids
        .iter()
        .filter_map(|id| reader.message(*id).unwrap())
        .filter(|d| !d.locations.is_empty())
        .filter(|d| {
            let copies: Vec<Flags> = d
                .locations
                .iter()
                .filter(|l| match scope {
                    ListScope::Folder(folder) => l.folder == folder,
                    ListScope::Unified(role) => folders
                        .iter()
                        .any(|f| f.id == l.folder && f.role == Some(role)),
                    ListScope::Account(account) => d.account == account,
                })
                .map(|l| l.flags)
                .collect();
            if copies.is_empty() {
                return false;
            }
            // Folder scopes filter each copy; others the effective flags.
            let passes = |flags: Flags| {
                (!filter.unread || !flags.contains(Flags::SEEN))
                    && (!filter.flagged || flags.contains(Flags::FLAGGED))
            };
            let flags_ok = match scope {
                ListScope::Account(_) => passes(d.effective_flags()),
                _ => copies.iter().any(|f| passes(*f)),
            };
            flags_ok
                && (!filter.has_attachment || d.has_attachments)
                && filter.tag.is_none_or(|t| d.tags.contains(&t))
        })
        .map(|d| MessageKey {
            message: d.id,
            key: d.date,
        })
        .collect();
    match sort {
        SortOrder::NewestFirst => keys.sort_by_key(|k| std::cmp::Reverse((k.key, k.message))),
        SortOrder::OldestFirst => keys.sort_by_key(|k| (k.key, k.message)),
    }
    keys
}

#[test]
fn message_keyset_pages_match_a_naive_list_in_every_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let a = common::fixture(&store, AccountKind::Imap);
    let b = common::fixture(&store, AccountKind::Imap);
    let mut ids = populate(&store, &a, 90);
    ids.extend(populate(&store, &b, 40));
    let todo = store
        .reader()
        .unwrap()
        .tag_by_keyword("$label4")
        .unwrap()
        .unwrap()
        .id;

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
            ..ListFilter::default()
        },
        ListFilter {
            tag: Some(todo),
            ..ListFilter::default()
        },
        ListFilter {
            unread: true,
            flagged: true,
            has_attachment: true,
            tag: None,
        },
    ];
    for scope in [
        ListScope::Folder(a.inbox),
        ListScope::Folder(a.archive),
        ListScope::Account(a.account),
        ListScope::Unified(FolderRole::Inbox),
        ListScope::Unified(FolderRole::Archive),
    ] {
        for sort in [SortOrder::NewestFirst, SortOrder::OldestFirst] {
            for filter in &filters {
                let want = naive(&store, &ids, scope, sort, filter);
                for size in [1, 7, 1000] {
                    let got = all_pages(&store, scope, sort, filter, size);
                    assert_eq!(got, want, "{scope:?} {sort:?} {filter:?} page {size}");
                }
            }
        }
    }
    assert_eq!(
        all_pages(
            &store,
            ListScope::Folder(a.inbox),
            SortOrder::NewestFirst,
            &ListFilter::default(),
            10
        )
        .len(),
        90
    );
    assert_eq!(
        all_pages(
            &store,
            ListScope::Unified(FolderRole::Inbox),
            SortOrder::NewestFirst,
            &ListFilter::default(),
            10
        )
        .len(),
        130
    );
}

#[test]
fn moved_and_detached_messages_leave_the_list() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let ids = populate(&store, &f, 10);
    let (first, inbox, sent) = (ids[1], f.inbox, f.sent);
    store
        .write_blocking(move |w| w.move_location(first, inbox, sent, Some(77)))
        .unwrap();
    let keys = |scope| {
        all_pages(
            &store,
            scope,
            SortOrder::NewestFirst,
            &ListFilter::default(),
            4,
        )
    };
    assert!(
        !keys(ListScope::Folder(f.inbox))
            .iter()
            .any(|k| k.message == first)
    );
    let in_sent = keys(ListScope::Folder(f.sent));
    assert_eq!(in_sent.len(), 1);
    // The moved copy carries the message's date.
    assert_eq!(in_sent[0].key, 0);
    let (second, archive) = (ids[0], f.archive);
    store
        .write_blocking(move |w| {
            w.remove_location(second, inbox)?;
            w.remove_location(second, archive)
        })
        .unwrap();
    assert!(
        !keys(ListScope::Account(f.account))
            .iter()
            .any(|k| k.message == second),
        "a detached message is in no list"
    );
}

#[test]
fn message_rows_carry_addresses_flags_and_sent_copies() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let (account, inbox, sent) = (f.account, f.inbox, f.sent);
    let (received, outgoing) = store
        .write_blocking(move |w| {
            let mut a = message(account, inbox, 1, 100);
            a.to = vec![Address::new(Some("Me"), "me@example.com")];
            a.size = Some(1234);
            a.locations
                .push(NewLocation::new(sent, Some(5), Flags::SEEN));
            let mut b = message(account, sent, 2, 200);
            b.from = Some(Address::new(Some("Me"), "me@example.com"));
            b.to = vec![
                Address::new(Some("Anna"), "anna@example.com"),
                Address::new(None::<String>, "bob@example.com"),
            ];
            b.locations[0].flags = Flags::SEEN | Flags::FLAGGED;
            let a = w.upsert_message(&a)?.message;
            let b = w.upsert_message(&b)?.message;
            Ok((a, b))
        })
        .unwrap();
    let reader = store.reader().unwrap();
    let rows = reader
        .message_rows(&[outgoing, MessageId(999_999), received], Some(f.inbox))
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, outgoing);
    assert_eq!(rows[0].to.len(), 2);
    assert!(rows[0].in_sent);
    assert!(rows[0].flags.contains(Flags::FLAGGED));
    assert_eq!(rows[1].id, received);
    assert_eq!(rows[1].size, Some(1234));
    assert_eq!(rows[1].from.as_ref().unwrap().email, "sender1@example.com");
    // In the inbox the copy is unread; effectively it's unread too (one copy unseen).
    assert!(!rows[1].flags.contains(Flags::SEEN));
    let in_sent = reader.message_rows(&[received], Some(f.sent)).unwrap();
    assert!(
        in_sent[0].flags.contains(Flags::SEEN),
        "the context folder's copy decides"
    );
    // Debug output never shows addresses.
    assert!(!format!("{:?}", rows[0]).contains("anna"));
}

#[test]
fn tree_messages_and_counts_follow_the_context_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let (account, inbox, sent) = (f.account, f.inbox, f.sent);
    let (thread, ids) = store
        .write_blocking(move |w| {
            let mut root = message(account, inbox, 1, 10);
            root.rfc_message_id = Some("root@x".to_owned());
            let root_out = w.upsert_message(&root)?;
            let mut reply = message(account, sent, 2, 20);
            reply.rfc_message_id = Some("reply@x".to_owned());
            reply.in_reply_to = Some("root@x".to_owned());
            reply.references = vec!["root@x".to_owned()];
            reply.thread = ThreadAssignment::Existing(root_out.thread);
            let reply = w.upsert_message(&reply)?.message;
            let mut answer = message(account, inbox, 3, 30);
            answer.in_reply_to = Some("reply@x".to_owned());
            answer.references = vec!["root@x".to_owned(), "reply@x".to_owned()];
            answer.thread = ThreadAssignment::Existing(root_out.thread);
            let answer = w.upsert_message(&answer)?.message;
            Ok((root_out.thread, vec![root_out.message, reply, answer]))
        })
        .unwrap();
    let reader = store.reader().unwrap();
    let all = reader.thread_tree_messages(thread, None).unwrap();
    assert_eq!(all.iter().map(|m| m.id).collect::<Vec<_>>(), ids);
    assert_eq!(all[2].references, vec!["root@x", "reply@x"]);
    assert_eq!(all[1].in_reply_to.as_deref(), Some("root@x"));
    let in_inbox = reader.thread_tree_messages(thread, Some(f.inbox)).unwrap();
    assert_eq!(
        in_inbox.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![ids[0], ids[2]]
    );
    assert_eq!(
        reader
            .thread_message_counts(&[thread, mazemail_types::ThreadId(424_242)], Some(f.inbox))
            .unwrap(),
        vec![2, 0]
    );
    assert_eq!(
        reader.thread_message_counts(&[thread], None).unwrap(),
        vec![3]
    );
}

#[test]
fn the_attachment_filter_follows_the_body() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let (account, inbox) = (f.account, f.inbox);
    let id = store
        .write_blocking(move |w| Ok(w.upsert_message(&message(account, inbox, 1, 5))?.message))
        .unwrap();
    let with_attachments = ListFilter {
        has_attachment: true,
        ..ListFilter::default()
    };
    let listed = |scope| {
        all_pages(&store, scope, SortOrder::NewestFirst, &with_attachments, 10)
            .iter()
            .map(|k| k.message)
            .collect::<Vec<_>>()
    };
    assert!(listed(ListScope::Folder(f.inbox)).is_empty());
    store
        .write_blocking(move |w| {
            w.update_body_summary(
                id,
                &store::BodySummaryUpdate {
                    has_attachments: Some(true),
                    ..store::BodySummaryUpdate::default()
                },
            )
        })
        .unwrap();
    assert_eq!(listed(ListScope::Folder(f.inbox)), vec![id]);
    assert_eq!(listed(ListScope::Account(f.account)), vec![id]);
}
