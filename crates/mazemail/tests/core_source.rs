//! `CoreMailSource` over a real core with mail stored like synced mail (`import`'s
//! mbox reader, then `ingest` with server UIDs).

#![allow(clippy::unwrap_used, reason = "tests")]

use std::sync::Arc;

use import::mbox::{Escaping, MboxReader};
use ingest::{Ingestor, MessageMeta, RawItem};
use mail_ui::source::{
    FolderKey, ListSort, MailSource, MessageBody, SortBy, ThreadFilter, ThreadScope,
};
use mazemail::source::{CoreMailSource, NO_ACCOUNTS};
use mazemail_core::{CoreConfig, CoreHandle};
use mazemail_types::{AccountId, Flags, FolderId, FolderRole};
use store::{AccountKind, NewAccount, NewFolder, NewLocation, Store};
use vault::MemoryKeyStore;

struct Seeded {
    source: CoreMailSource,
    core: CoreHandle,
    runtime: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

fn mbox() -> String {
    let mut text = String::new();
    let messages = [
        (
            "a1",
            None,
            "Anna <anna@example.com>",
            "Offsite plan",
            "0001",
            "Let's meet in *Lisbon*.",
        ),
        (
            "a2",
            Some("a1"),
            "Ben <ben@example.com>",
            "Re: Offsite plan",
            "0000",
            "Sounds good <b>to me</b>.",
        ),
        (
            "c1",
            None,
            "Clara <clara@example.com>",
            "Invoice 117",
            "0000",
            "Please find the invoice.",
        ),
    ];
    for (n, (id, parent, from, subject, status, body)) in messages.iter().enumerate() {
        let references = parent
            .map(|p| format!("In-Reply-To: <{p}@example.com>\nReferences: <{p}@example.com>\n"))
            .unwrap_or_default();
        text.push_str(&format!(
            "From - Mon Oct 13 09:0{n}:00 2025\nX-Mozilla-Status: {status}\n\
             Message-ID: <{id}@example.com>\n{references}Date: Mon, 13 Oct 2025 09:0{n}:00 +0000\n\
             From: {from}\nTo: me@example.com\nSubject: {subject}\n\n{body}\n\n"
        ));
    }
    text
}

fn open_core(runtime: &tokio::runtime::Runtime, dir: &std::path::Path) -> CoreHandle {
    let keys = Arc::new(MemoryKeyStore::new());
    let mut config = CoreConfig::new(dir, keys)
        .with_runtime(runtime.handle().clone())
        .with_settings_file(dir.join("settings.json"));
    config.blob_gc_interval = None;
    runtime.block_on(CoreHandle::open(config)).unwrap()
}

fn seeded() -> Seeded {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let core = open_core(&runtime, &dir.path().join("data"));
    let store = core.store().clone();
    let account = store
        .write_blocking(|w| {
            w.create_account(&NewAccount {
                kind: AccountKind::Imap,
                name: "Personal".to_owned(),
                config_json: "{}".to_owned(),
                ordinal: 0,
            })
        })
        .unwrap();
    let inbox = folder(
        &store,
        account,
        None,
        "Inbox",
        "INBOX",
        Some(FolderRole::Inbox),
    );
    folder(
        &store,
        account,
        None,
        "Archive",
        "Archive",
        Some(FolderRole::Archive),
    );
    let ingestor = Ingestor::with_sink(store, Arc::new(core.index_sink()));
    let text = mbox();
    let items: Vec<RawItem> = MboxReader::new(text.as_bytes())
        .with_escaping(Escaping::Mboxrd)
        .enumerate()
        .map(|(n, record)| {
            let record = record.unwrap();
            let r = import::reconstruct(&record.message);
            let location = NewLocation {
                folder: inbox,
                uid: Some(n as u32 + 1),
                modseq: None,
                flags: r.mozilla.flags(),
                keywords: r.mozilla.keywords(),
            };
            RawItem {
                meta: MessageMeta::new(account, location).with_internal_date(record.envelope.date),
                raw: r.bytes,
            }
        })
        .collect();
    let stored = ingestor.ingest_raw_batch_blocking(items);
    assert!(stored.iter().all(Result::is_ok));
    assert_eq!(stored.len(), 3);
    runtime.block_on(core.commit_index()).unwrap();
    Seeded {
        source: CoreMailSource::new(core.clone()),
        core,
        runtime,
        _dir: dir,
    }
}

/// Creates a server folder of `account`.
fn folder(
    store: &Store,
    account: AccountId,
    parent: Option<FolderId>,
    name: &str,
    remote: &str,
    role: Option<FolderRole>,
) -> FolderId {
    let (name, remote) = (name.to_owned(), remote.to_owned());
    store
        .write_blocking(move |w| {
            w.create_folder(&NewFolder {
                account,
                parent,
                remote_name: Some(remote),
                display_name: name,
                role,
                is_label: false,
                sync_policy: None,
                ordinal: 0,
            })
        })
        .unwrap()
}

const INBOX: FolderKey = FolderKey::Unified(FolderRole::Inbox);

#[test]
fn folders_lists_rows_and_messages() {
    let t = seeded();
    let tree = t.source.folder_tree();
    let unified: Vec<_> = tree.unified.iter().map(|e| e.key.clone()).collect();
    assert_eq!(unified, [INBOX, FolderKey::Unified(FolderRole::Archive)]);
    assert_eq!(tree.accounts.len(), 1);
    assert_eq!(tree.accounts[0].name.as_ref(), "Personal");
    assert_eq!(tree.accounts[0].kind, mail_ui::source::AccountKind::Imap);
    let inbox = tree.accounts[0]
        .folders
        .iter()
        .find(|f| f.role == Some(FolderRole::Inbox))
        .unwrap()
        .key
        .clone();
    // Two unread messages in two threads.
    assert_eq!(t.source.unread_count(&INBOX), 2);
    assert_eq!(t.source.unread_count(&inbox), 2);
    assert_eq!(t.source.placeholder(), None);

    let keys = t
        .source
        .thread_keys(
            &ThreadScope::Folder(INBOX),
            &ThreadFilter::default(),
            ListSort::NEWEST_FIRST,
        )
        .unwrap();
    assert_eq!(keys.len(), 2, "the reply joined its thread");
    let rows = t.source.thread_rows(&keys);
    assert_eq!(rows[0].subject.expose().as_ref(), "Invoice 117");
    assert_eq!(rows[0].sender.expose().as_ref(), "Clara");
    assert!(rows[0].is_unread());
    let offsite = &rows[1];
    assert_eq!(offsite.message_count, 2);
    assert_eq!(offsite.sender.expose().as_ref(), "Ben");

    // The newest message, rendered as escaped Markdown.
    let message = t.source.message(offsite.id).unwrap();
    assert_eq!(message.subject.expose().as_ref(), "Re: Offsite plan");
    let MessageBody::Markdown(body) = &message.body else {
        panic!("expected Markdown, got {:?}", message.body);
    };
    assert!(
        body.expose().contains(r"Sounds good \<b\>to me\</b\>\."),
        "{}",
        body.expose()
    );

    // Quick filter text (subject) and global search (body, through the index).
    let filtered = t
        .source
        .thread_keys(
            &ThreadScope::Folder(INBOX),
            &ThreadFilter {
                text: Some("invoice".into()),
                ..ThreadFilter::default()
            },
            ListSort::NEWEST_FIRST,
        )
        .unwrap();
    assert_eq!(filtered.len(), 1);
    let found = t
        .source
        .thread_keys(
            &ThreadScope::AllFolders,
            &ThreadFilter {
                text: Some("lisbon".into()),
                ..ThreadFilter::default()
            },
            ListSort::NEWEST_FIRST,
        )
        .unwrap();
    assert_eq!(&*found, &[offsite.id]);
    let unread_only = t
        .source
        .thread_keys(
            &ThreadScope::Folder(INBOX),
            &ThreadFilter {
                unread: true,
                ..ThreadFilter::default()
            },
            ListSort::NEWEST_FIRST,
        )
        .unwrap();
    assert_eq!(unread_only.len(), 2);
}

#[test]
fn favorites_are_saved_in_the_settings_and_shown_at_once() {
    let t = seeded();
    let tree = t.source.folder_tree();
    assert!(tree.favorites.is_empty());
    let archive = tree.accounts[0]
        .folders
        .iter()
        .find(|f| f.role == Some(FolderRole::Archive))
        .unwrap()
        .key
        .clone();
    let FolderKey::Folder(id) = archive else {
        panic!("a folder");
    };
    t.source.set_favorite(id, true).unwrap();
    let favorites: Vec<_> = t
        .source
        .folder_tree()
        .favorites
        .into_iter()
        .map(|f| f.key)
        .collect();
    assert_eq!(favorites, [archive]);
    let saved = t.core.settings().settings.folder_pane.favorites.clone();
    assert_eq!(saved.len(), 1);
    assert_eq!(
        (saved[0].account.as_str(), saved[0].folder.as_str()),
        ("Personal", "Archive")
    );
    t.source.set_favorite(id, false).unwrap();
    assert!(t.source.folder_tree().favorites.is_empty());
    assert_eq!(
        t.source.set_favorite(mazemail_types::FolderId(9_999), true),
        Err(mail_ui::source::SourceError::FolderNotFound)
    );
}

#[test]
fn flags_and_archive_apply_at_once_and_raise_events() {
    let t = seeded();
    // The window reads the tree when it opens.
    let _ = t.source.folder_tree();
    let mut events = t.core.events();
    let keys = t
        .source
        .thread_keys(
            &ThreadScope::Folder(INBOX),
            &ThreadFilter::default(),
            ListSort::NEWEST_FIRST,
        )
        .unwrap();
    let invoice = keys[0];

    t.source
        .set_flags(&[invoice], Flags::SEEN, Flags::empty())
        .unwrap();
    assert!(!t.source.thread_rows(&[invoice])[0].is_unread());
    t.source
        .set_flags(&[invoice], Flags::FLAGGED, Flags::empty())
        .unwrap();
    assert!(t.source.thread_rows(&[invoice])[0].is_starred());

    // The change event says what to refresh.
    let changes = t.runtime.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let batch = events.recv().await.unwrap();
                let changes = t.source.changes(&batch);
                if changes.counts {
                    return changes;
                }
            }
        })
        .await
        .unwrap()
    });
    assert!(!changes.everything);
    assert!(!changes.folders, "counts changed, not the folder structure");
    assert_eq!(t.source.unread_count(&INBOX), 1);

    // Archived threads leave the inbox for the yearly folder under Archive.
    t.source.archive(&[invoice]).unwrap();
    let keys = t
        .source
        .thread_keys(
            &ThreadScope::Folder(INBOX),
            &ThreadFilter::default(),
            ListSort::NEWEST_FIRST,
        )
        .unwrap();
    assert!(!keys.contains(&invoice));
    let tree = t.source.folder_tree();
    let archive = tree.accounts[0]
        .folders
        .iter()
        .find(|f| f.role == Some(FolderRole::Archive))
        .unwrap();
    assert_eq!(archive.children[0].name.as_ref(), "2025");
    let archived = t
        .source
        .thread_keys(
            &ThreadScope::Folder(archive.children[0].key.clone()),
            &ThreadFilter::default(),
            ListSort::NEWEST_FIRST,
        )
        .unwrap();
    assert_eq!(&*archived, &[invoice]);
}

#[test]
fn placeholders_cover_opening_failure_and_no_accounts() {
    let opening = CoreMailSource::opening();
    assert_eq!(opening.placeholder().unwrap().as_ref(), "Opening Maze…");
    assert!(opening.folder_tree().accounts.is_empty());
    assert!(
        opening
            .thread_keys(
                &ThreadScope::Folder(INBOX),
                &ThreadFilter::default(),
                ListSort::NEWEST_FIRST
            )
            .is_err()
    );
    opening.fail("Maze is already running");
    assert_eq!(
        opening.placeholder().unwrap().as_ref(),
        "Maze is already running"
    );

    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let core = open_core(&runtime, dir.path());
    opening.attach(core.clone());
    assert_eq!(opening.placeholder().unwrap().as_ref(), NO_ACCOUNTS);
    runtime.block_on(core.shutdown()).unwrap();
}

#[test]
fn the_status_bar_follows_the_accounts_sync_states() {
    use mail_ui::source::SyncState as Bar;
    use mazemail::source::status_of;
    use mazemail_core::{AccountInfo, AccountKind, SyncState, SyncStatus};
    use mazemail_types::AccountId;

    let account = |id: i64, state: Option<SyncState>, detail: Option<&str>| AccountInfo {
        id: AccountId(id),
        uuid: auth::Uuid::from_u128(id as u128),
        name: format!("Account {id}"),
        kind: AccountKind::Imap,
        email: None,
        server: None,
        auth: None,
        sync: state.map(|state| SyncStatus {
            account: AccountId(id),
            state,
            detail: detail.map(str::to_owned),
        }),
    };
    assert_eq!(status_of(&[]).sync, Bar::NoAccounts);
    assert_eq!(status_of(&[account(1, None, None)]).sync, Bar::NoAccounts);

    let idle = account(1, Some(SyncState::Idle), None);
    let status = status_of(std::slice::from_ref(&idle));
    assert_eq!(status.sync, Bar::Idle);
    assert!(status.online);

    let offline = account(
        2,
        Some(SyncState::Offline),
        Some("Offline: connection refused. Maze will try again in 30 seconds."),
    );
    let status = status_of(&[idle, offline.clone()]);
    assert_eq!(
        status.sync,
        Bar::Message {
            text: "Account 2: Offline: connection refused. Maze will try again in 30 seconds."
                .into(),
            problem: true,
        }
    );
    assert!(status.online, "one account is still online");

    let syncing = account(
        3,
        Some(SyncState::Syncing {
            folder: None,
            done: 2,
            total: Some(5),
        }),
        None,
    );
    assert_eq!(
        status_of(&[offline, syncing]).sync,
        Bar::Syncing {
            account: "Account 3".into(),
            done: 2,
            total: 5,
        }
    );
}

#[test]
fn threads_off_trees_and_message_commands() {
    let t = seeded();
    t.source.folder_tree();
    let scope = ThreadScope::Folder(INBOX);
    let threads = t
        .source
        .thread_keys(&scope, &ThreadFilter::default(), ListSort::NEWEST_FIRST)
        .unwrap();
    let rows = t.source.thread_rows(&threads);
    let offsite = rows
        .iter()
        .find(|r| r.message_count == 2)
        .expect("the offsite thread");
    assert_eq!(offsite.account.as_ref(), "Personal");
    assert_eq!(offsite.sender.expose().as_ref(), "Ben");
    assert_eq!(offsite.recipients.expose().as_ref(), "me@example.com");
    assert!(!offsite.outgoing, "received mail shows its sender");
    assert_eq!(offsite.correspondents(), &offsite.sender);

    // The reply tree: Anna's message, Ben's reply below it.
    let tree = t.source.thread_tree(offsite.id).unwrap();
    assert_eq!(tree.len(), 2);
    assert_eq!((tree[0].depth, tree[1].depth), (0, 1));
    assert_eq!(tree[1].parent, Some(tree[0].message));
    assert_eq!(t.source.message_counts(&[offsite.id]), vec![2]);
    let reply = t.source.message_document(tree[1].message).unwrap();
    assert_eq!(reply.subject.expose().as_ref(), "Re: Offsite plan");
    assert_eq!(reply.thread, offsite.id);

    // Threads off: one row per message, newest first (or oldest first).
    let messages = t
        .source
        .message_keys(&scope, &ThreadFilter::default(), ListSort::NEWEST_FIRST)
        .unwrap();
    assert_eq!(messages.len(), 3);
    let message_rows = t.source.message_rows(&messages);
    assert!(message_rows.windows(2).all(|w| w[0].date >= w[1].date));
    let oldest = t
        .source
        .message_keys(&scope, &ThreadFilter::default(), ListSort::OLDEST_FIRST)
        .unwrap();
    assert_eq!(oldest.first(), messages.last());
    let filtered = t
        .source
        .message_keys(
            &scope,
            &ThreadFilter {
                text: Some("offsite".to_owned()),
                ..ThreadFilter::default()
            },
            ListSort::NEWEST_FIRST,
        )
        .unwrap();
    assert_eq!(filtered.len(), 2, "the messages of the matching thread");

    // Flags and archive act on one message.
    let anna = tree[0].message;
    t.source
        .set_message_flags(&[anna], Flags::FLAGGED, Flags::empty())
        .unwrap();
    let anna_row = &t.source.message_rows(&[anna])[0];
    assert!(anna_row.flags.contains(Flags::FLAGGED));
    assert!(t.source.thread_rows(&[offsite.id])[0].is_starred());
    t.source.archive_messages(&[anna]).unwrap();
    let after = t
        .source
        .message_keys(&scope, &ThreadFilter::default(), ListSort::NEWEST_FIRST)
        .unwrap();
    assert_eq!(after.len(), 2);
    assert!(!after.contains(&anna));
    drop(t.source);
    t.runtime.block_on(t.core.shutdown()).unwrap();
}

#[test]
fn every_order_of_the_list_is_the_cores() {
    let t = seeded();
    let scope = ThreadScope::Folder(INBOX);
    let subjects = |sort: ListSort| -> Vec<String> {
        let keys = t
            .source
            .thread_keys(&scope, &ThreadFilter::default(), sort)
            .unwrap();
        t.source
            .thread_rows(&keys)
            .iter()
            .map(|r| r.subject.expose().to_string())
            .collect()
    };
    assert_eq!(
        subjects(ListSort::NEWEST_FIRST),
        ["Invoice 117", "Offsite plan"]
    );
    assert_eq!(
        subjects(ListSort::by(SortBy::Subject).toggled()),
        ["Offsite plan", "Invoice 117"]
    );
    // The newest message's sender: Ben (not Anna) for the offsite thread.
    assert_eq!(
        subjects(ListSort::by(SortBy::Sender)),
        ["Offsite plan", "Invoice 117"]
    );
    // Unread first: the offsite thread's reply is unread, the invoice too; ties go
    // newest first.
    assert_eq!(
        subjects(ListSort::by(SortBy::Unread)),
        ["Invoice 117", "Offsite plan"]
    );

    // Threads off: messages by subject without "Re:", equal subjects newest first.
    let keys = t
        .source
        .message_keys(
            &scope,
            &ThreadFilter::default(),
            ListSort::by(SortBy::Subject),
        )
        .unwrap();
    let rows: Vec<String> = t
        .source
        .message_rows(&keys)
        .iter()
        .map(|r| r.subject.expose().to_string())
        .collect();
    assert_eq!(rows, ["Invoice 117", "Re: Offsite plan", "Offsite plan"]);

    // Global search results sort the same way (threads off: their messages).
    let lisbon = ThreadFilter {
        text: Some("lisbon".into()),
        ..ThreadFilter::default()
    };
    let found = t
        .source
        .message_keys(&ThreadScope::AllFolders, &lisbon, ListSort::OLDEST_FIRST)
        .unwrap();
    let rows: Vec<String> = t
        .source
        .message_rows(&found)
        .iter()
        .map(|r| r.sender.expose().to_string())
        .collect();
    assert_eq!(rows, ["Anna", "Ben"], "oldest first");
    let found = t
        .source
        .thread_keys(
            &ThreadScope::AllFolders,
            &lisbon,
            ListSort::by(SortBy::Subject),
        )
        .unwrap();
    assert_eq!(found.len(), 1);
    drop(t.source);
    t.runtime.block_on(t.core.shutdown()).unwrap();
}

#[test]
fn quick_move_moves_threads_and_messages_undoably() {
    let t = seeded();
    let tree = t.source.folder_tree();
    let account = tree.accounts[0].id;
    let inbox = tree.accounts[0]
        .folders
        .iter()
        .find(|f| f.role == Some(FolderRole::Inbox))
        .unwrap()
        .key
        .clone();
    let FolderKey::Folder(inbox_id) = inbox else {
        panic!("a folder");
    };
    let parent = folder(t.core.store(), account, None, "Projects", "Projects", None);
    let projects = folder(
        t.core.store(),
        account,
        Some(parent),
        "Maze",
        "Projects/Maze",
        None,
    );
    let keys = |key: FolderKey| {
        t.source
            .thread_keys(
                &ThreadScope::Folder(key),
                &ThreadFilter::default(),
                ListSort::NEWEST_FIRST,
            )
            .unwrap()
    };
    let threads = keys(INBOX);
    let (invoice, offsite) = (threads[0], threads[1]);
    assert_eq!(t.source.thread_account(invoice), Some(account));
    let tree = t.source.thread_tree(offsite).unwrap();
    assert_eq!(t.source.message_account(tree[1].message), Some(account));

    // Both threads at once, out of the Inbox shown: one command, one undo.
    t.source
        .move_threads(&[invoice, offsite], projects, Some(inbox_id))
        .unwrap();
    assert!(keys(INBOX).is_empty());
    assert_eq!(keys(FolderKey::Folder(projects)).len(), 2);
    assert!(t.core.can_undo());
    // `cmd-z` in the window goes through the source.
    assert!(t.source.undo().unwrap());
    assert_eq!(keys(INBOX).len(), 2, "undo brings both back");
    assert!(keys(FolderKey::Folder(projects)).is_empty());
    assert!(!t.source.undo().unwrap(), "nothing left to undo");

    // A single message (threads off, or a message of an expanded thread).
    t.source
        .move_messages(&[tree[1].message], projects, Some(inbox_id))
        .unwrap();
    assert_eq!(&*keys(FolderKey::Folder(projects)), &[offsite]);
    // The thread tree reads the folder listed last: only the moved reply is here.
    assert_eq!(t.source.thread_tree(offsite).unwrap().len(), 1);
    assert_eq!(
        keys(INBOX).len(),
        2,
        "Anna's message keeps the thread in the Inbox"
    );
    drop(t.source);
    t.runtime.block_on(t.core.shutdown()).unwrap();
}
