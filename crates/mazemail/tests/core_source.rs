//! `CoreMailSource` over a real core with mail imported through the `import` crate.

#![allow(clippy::unwrap_used, reason = "tests")]

use std::sync::Arc;

use import::ImportTarget;
use import::local::{ensure_folder_path, local_account};
use ingest::Ingestor;
use mail_ui::source::{FolderKey, MailSource, MessageBody, ThreadFilter, ThreadScope};
use mazemail::source::{CoreMailSource, NO_ACCOUNTS};
use mazemail_core::{CoreConfig, CoreHandle};
use mazemail_types::{Flags, FolderRole};
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
    let mut config = CoreConfig::new(dir, keys).with_runtime(runtime.handle().clone());
    config.blob_gc_interval = None;
    runtime.block_on(CoreHandle::open(config)).unwrap()
}

fn seeded() -> Seeded {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let core = open_core(&runtime, &dir.path().join("data"));
    let file = dir.path().join("inbox.mbox");
    std::fs::write(&file, mbox()).unwrap();
    let store = core.store().clone();
    let account = local_account(&store, "Personal").unwrap();
    let path = vec!["Inbox".to_owned()];
    ensure_folder_path(&store, account, &path, Some(FolderRole::Inbox)).unwrap();
    ensure_folder_path(
        &store,
        account,
        &["Archive".to_owned()],
        Some(FolderRole::Archive),
    )
    .unwrap();
    let ingestor = Ingestor::with_sink(store, Arc::new(core.index_sink()));
    let stats = import::import_mbox(
        &ingestor,
        &file,
        &ImportTarget {
            account: Some(account),
            folder: Some(path),
        },
        &import::NoProgress,
    )
    .unwrap();
    assert_eq!(stats.imported, 3);
    runtime.block_on(core.commit_index()).unwrap();
    Seeded {
        source: CoreMailSource::new(core.clone()),
        core,
        runtime,
        _dir: dir,
    }
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
    assert_eq!(tree.accounts[0].kind, mail_ui::source::AccountKind::Local);
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
        .thread_keys(&ThreadScope::Folder(INBOX), &ThreadFilter::default())
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
        )
        .unwrap();
    assert_eq!(unread_only.len(), 2);
}

#[test]
fn flags_and_archive_apply_at_once_and_raise_events() {
    let t = seeded();
    // The window reads the tree when it opens.
    let _ = t.source.folder_tree();
    let mut events = t.core.events();
    let keys = t
        .source
        .thread_keys(&ThreadScope::Folder(INBOX), &ThreadFilter::default())
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
        .thread_keys(&ThreadScope::Folder(INBOX), &ThreadFilter::default())
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
            .thread_keys(&ThreadScope::Folder(INBOX), &ThreadFilter::default())
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
