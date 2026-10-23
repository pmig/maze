//! Lists sorted by any column ([`Queries::sorted_thread_keys`],
//! [`Queries::sorted_message_keys`]): every field in both directions, threads by what
//! their rows show, messages by their own fields, ties broken by date and id.

use mazemail_types::{AccountId, Address, Flags, FolderId, FolderRole, MessageId, ThreadId};
use pretty_assertions::assert_eq;
use store::{
    AccountKind, IdentityInput, ListOrder, NewAccount, NewFolder, NewLocation, Queries, SortField,
    Store, ThreadAssignment,
};

use crate::common::{self, Fixture, message};

/// The mail of the tests, in the Inbox of "Work" unless noted:
///
/// | thread | message | date | subject             | from                  | to          | size | flags / tags        |
/// |--------|---------|------|---------------------|-----------------------|-------------|------|---------------------|
/// | a      | a1      | 100  | Invoice March       | Zed                   | Me          | 700  | starred in Archive  |
/// | a      | a2      | 300  | Re: invoice march   | anna                  | Me          | 500  | received 310        |
/// | b      | b1      | 200  | AW: Budget          | Bob                   | Me          | 9000 | unread, starred, attachment, Work |
/// | c      | c1      | 250  | (none)              | carl@example.com      | Yvonne      | 100  | Important, received 260 |
/// | d      | d1      | 150  | Lunch               | Philip (an identity)  | Aaron       | 2000 | received 1000       |
struct Mail {
    f: Fixture,
    threads: [ThreadId; 4],
    a1: MessageId,
    a2: MessageId,
    b1: MessageId,
    c1: MessageId,
    d1: MessageId,
}

fn person(name: Option<&str>, email: &str) -> Address {
    Address::new(name, email)
}

fn populate(store: &Store) -> Mail {
    let f = common::fixture(store, AccountKind::Imap);
    let (account, inbox, archive) = (f.account, f.inbox, f.archive);
    let ids = store
        .write_blocking(move |w| {
            w.create_identity(
                account,
                &IdentityInput {
                    label: None,
                    display_name: Some("Philip".to_owned()),
                    email: "ME@example.com".to_owned(),
                    reply_to: None,
                    signature: None,
                    smtp: None,
                    sent_folder: None,
                    drafts_folder: None,
                    archive_folder: None,
                    archive_granularity: 1,
                    color: None,
                    match_patterns_json: None,
                    is_default: true,
                    ordinal: 0,
                },
            )?;
            let work = w.tag_by_keyword("$label2")?.map(|t| t.id).expect("Work");
            let important = w
                .tag_by_keyword("$label1")?
                .map(|t| t.id)
                .expect("Important");
            let me = person(Some("Me"), "me@example.com");
            let new = |uid: u32, date: i64, subject: Option<&str>, from: Address, size: u64| {
                let mut m = message(account, inbox, uid, date);
                m.subject = subject.map(str::to_owned);
                m.from = Some(from);
                m.to = vec![me.clone()];
                m.size = Some(size);
                m.locations[0].flags = Flags::SEEN;
                m
            };

            let mut a1 = new(
                1,
                100,
                Some("Invoice March"),
                person(Some("Zed"), "z@example.com"),
                700,
            );
            a1.locations.push(NewLocation::new(
                archive,
                Some(101),
                Flags::SEEN | Flags::FLAGGED,
            ));
            let a1 = w.upsert_message(&a1)?;
            let mut a2 = new(
                2,
                300,
                Some("Re: invoice march"),
                person(Some("anna"), "a@example.com"),
                500,
            );
            a2.received = Some(310);
            a2.thread = ThreadAssignment::Existing(a1.thread);
            let a2 = w.upsert_message(&a2)?;

            let mut b1 = new(
                3,
                200,
                Some("AW: Budget"),
                person(Some("Bob"), "b@example.com"),
                9000,
            );
            b1.locations[0].flags = Flags::FLAGGED;
            b1.has_attachments = true;
            let b1 = w.upsert_message(&b1)?;
            w.set_tags(&[b1.message], &[work], &[])?;

            let mut c1 = new(4, 250, None, person(None, "carl@example.com"), 100);
            c1.to = vec![person(Some("Yvonne"), "y@example.com")];
            c1.received = Some(260);
            let c1 = w.upsert_message(&c1)?;
            w.set_tags(&[c1.message], &[important], &[])?;

            let mut d1 = new(
                5,
                150,
                Some("Lunch"),
                person(Some("Philip"), "me+lists@example.com"),
                2000,
            );
            d1.to = vec![person(Some("Aaron"), "aaron@example.com")];
            d1.received = Some(1000);
            let d1 = w.upsert_message(&d1)?;
            Ok([a1, a2, b1, c1, d1])
        })
        .unwrap();
    let [a1, a2, b1, c1, d1] = ids;
    Mail {
        f,
        threads: [a1.thread, b1.thread, c1.thread, d1.thread],
        a1: a1.message,
        a2: a2.message,
        b1: b1.message,
        c1: c1.message,
        d1: d1.message,
    }
}

fn threads(
    store: &Store,
    mail: &Mail,
    context: Option<FolderId>,
    field: SortField,
    descending: bool,
) -> Vec<&'static str> {
    let keys = store
        .reader()
        .unwrap()
        .sorted_thread_keys(&mail.threads, context, ListOrder::new(field, descending))
        .unwrap();
    keys.iter()
        .map(|k| {
            let ix = mail.threads.iter().position(|t| *t == k.thread).unwrap();
            ["a", "b", "c", "d"][ix]
        })
        .collect()
}

fn messages(
    store: &Store,
    mail: &Mail,
    context: Option<FolderId>,
    field: SortField,
    descending: bool,
) -> Vec<&'static str> {
    let ids = [mail.a1, mail.a2, mail.b1, mail.c1, mail.d1];
    let keys = store
        .reader()
        .unwrap()
        .sorted_message_keys(&ids, context, ListOrder::new(field, descending))
        .unwrap();
    keys.iter()
        .map(|k| {
            let ix = ids.iter().position(|m| *m == k.message).unwrap();
            ["a1", "a2", "b1", "c1", "d1"][ix]
        })
        .collect()
}

#[test]
fn threads_sort_by_every_column_both_ways() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let mail = populate(&store);
    let inbox = Some(mail.f.inbox);
    use SortField::*;
    let cases: &[(SortField, [&str; 4], [&str; 4])] = &[
        // field, descending, ascending
        (Date, ["a", "c", "b", "d"], ["d", "b", "c", "a"]),
        // The newest message's received time, else its date.
        (Received, ["d", "a", "c", "b"], ["b", "c", "a", "d"]),
        // Ties (and the other way round) go newest first.
        (Starred, ["b", "a", "c", "d"], ["a", "c", "d", "b"]),
        (Unread, ["b", "a", "c", "d"], ["a", "c", "d", "b"]),
        (Attachments, ["b", "a", "c", "d"], ["a", "c", "d", "b"]),
        // "AW: Budget" is "budget"; a thread without a subject is last both ways.
        (Subject, ["d", "a", "b", "c"], ["b", "a", "d", "c"]),
        // The newest message's sender: anna (not Zed) for a.
        (Sender, ["d", "c", "b", "a"], ["a", "b", "c", "d"]),
        // Mail you sent shows its recipients: Aaron for d.
        (Correspondents, ["c", "b", "a", "d"], ["d", "a", "b", "c"]),
        // Equal recipients ("me") go newest first both ways.
        (Recipients, ["c", "a", "b", "d"], ["d", "a", "b", "c"]),
        // Untagged threads are last both ways.
        (Tags, ["b", "c", "a", "d"], ["c", "b", "a", "d"]),
        // One account: by date.
        (Account, ["a", "c", "b", "d"], ["a", "c", "b", "d"]),
        // The newest message's size: 500 for a.
        (Size, ["b", "d", "a", "c"], ["c", "a", "d", "b"]),
    ];
    for (field, descending, ascending) in cases {
        assert_eq!(
            threads(&store, &mail, inbox, *field, true),
            descending,
            "{field:?} descending"
        );
        assert_eq!(
            threads(&store, &mail, inbox, *field, false),
            ascending,
            "{field:?} ascending"
        );
    }
}

#[test]
fn messages_sort_by_their_own_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let mail = populate(&store);
    let inbox = Some(mail.f.inbox);
    use SortField::*;
    let cases: &[(SortField, bool, [&str; 5])] = &[
        (Date, true, ["a2", "c1", "b1", "d1", "a1"]),
        (Date, false, ["a1", "d1", "b1", "c1", "a2"]),
        (Received, true, ["d1", "a2", "c1", "b1", "a1"]),
        // "Invoice March" and "Re: invoice march" are the same subject: newest first.
        (Subject, false, ["b1", "a2", "a1", "d1", "c1"]),
        (Subject, true, ["d1", "a2", "a1", "b1", "c1"]),
        (Sender, false, ["a2", "b1", "c1", "d1", "a1"]),
        (Correspondents, false, ["d1", "a2", "b1", "c1", "a1"]),
        (Size, true, ["b1", "d1", "a1", "a2", "c1"]),
        (Tags, false, ["c1", "b1", "a2", "d1", "a1"]),
        (Starred, true, ["b1", "a2", "c1", "d1", "a1"]),
    ];
    for (field, descending, expected) in cases {
        assert_eq!(
            messages(&store, &mail, inbox, *field, *descending),
            expected,
            "{field:?} descending: {descending}"
        );
    }
}

#[test]
fn flags_are_those_of_the_context_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let mail = populate(&store);
    // a1 is starred in the Archive only: not in the Inbox, but in a unified list.
    assert_eq!(
        messages(&store, &mail, Some(mail.f.inbox), SortField::Starred, true)[..2],
        ["b1", "a2"]
    );
    assert_eq!(
        messages(&store, &mail, None, SortField::Starred, true)[..2],
        ["b1", "a1"]
    );
    // Thread a: unstarred in the Inbox, starred as a whole.
    assert_eq!(
        threads(&store, &mail, Some(mail.f.inbox), SortField::Starred, true),
        ["b", "a", "c", "d"]
    );
    assert_eq!(
        threads(&store, &mail, None, SortField::Starred, true),
        ["a", "b", "c", "d"]
    );
}

#[test]
fn accounts_sort_by_name_and_keys_keep_their_dates() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let mail = populate(&store);
    let other: (AccountId, ThreadId) = store
        .write_blocking(|w| {
            let account = w.create_account(&NewAccount {
                kind: AccountKind::Imap,
                name: "alpha".to_owned(),
                config_json: "{}".to_owned(),
                ordinal: 1,
            })?;
            let inbox = w.create_folder(
                &NewFolder::new(account, "INBOX", "Inbox").with_role(FolderRole::Inbox),
            )?;
            let outcome = w.upsert_message(&message(account, inbox, 1, 50))?;
            Ok((account, outcome.thread))
        })
        .unwrap();
    let mut ids = mail.threads.to_vec();
    ids.push(other.1);
    let reader = store.reader().unwrap();
    let keys = reader
        .sorted_thread_keys(&ids, None, ListOrder::new(SortField::Account, false))
        .unwrap();
    // "alpha" before "Work" (case is ignored), then Work's threads newest first.
    let order: Vec<ThreadId> = keys.iter().map(|k| k.thread).collect();
    let [a, b, c, d] = mail.threads;
    assert_eq!(order, [other.1, a, c, b, d]);
    assert_eq!(keys[0].key, 50, "keys carry the thread's date");
    let keys = reader
        .sorted_thread_keys(&ids, None, ListOrder::new(SortField::Account, true))
        .unwrap();
    assert_eq!(keys[0].thread, a);
}

#[test]
fn ties_are_broken_by_date_then_id_and_unknown_ids_are_left_out() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let (account, inbox) = (f.account, f.inbox);
    // Four messages with the same subject, two of them at the same time.
    let ids: Vec<MessageId> = store
        .write_blocking(move |w| {
            let mut ids = Vec::new();
            for (uid, date) in [(1, 10), (2, 20), (3, 20), (4, 5)] {
                let mut m = message(account, inbox, uid, date);
                m.subject = Some("Same".to_owned());
                ids.push(w.upsert_message(&m)?.message);
            }
            Ok(ids)
        })
        .unwrap();
    let reader = store.reader().unwrap();
    let mut asked = ids.clone();
    asked.push(MessageId(9_999));
    for descending in [false, true] {
        let keys = reader
            .sorted_message_keys(
                &asked,
                Some(inbox),
                ListOrder::new(SortField::Subject, descending),
            )
            .unwrap();
        let order: Vec<MessageId> = keys.iter().map(|k| k.message).collect();
        assert_eq!(
            order,
            [ids[2], ids[1], ids[0], ids[3]],
            "descending: {descending}"
        );
    }
    assert!(
        reader
            .sorted_thread_keys(&[], None, ListOrder::NEWEST_FIRST)
            .unwrap()
            .is_empty()
    );
}
