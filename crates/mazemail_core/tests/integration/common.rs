//! Shared helpers: a core over a temp data dir, a mail fixture, and a stand-in for
//! `ingest` (parse with `mazemail_mime`, write through the store, index through the
//! core's sink).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mazemail_core::{
    CoreConfig, CoreEventBatch, CoreHandle, FetchPriority, IndexDocument, RefreshScope, SyncBridge,
};
use mazemail_types::{AccountId, Flags, FolderId, FolderRole, IdentityId, MessageId, ThreadId};
use security::{Resolver, StaticDns, Verifier};
use store::{
    AccountKind, BodyState, IdentityInput, NewAccount, NewFolder, NewLocation, NewMessage,
    ThreadAssignment,
};
use tokio::sync::broadcast;
use vault::MemoryKeyStore;

/// A [`SyncBridge`] that records what core asked for.
#[derive(Debug, Default)]
pub(crate) struct RecordingSync {
    pub(crate) fetches: Mutex<Vec<(MessageId, FetchPriority)>>,
    pub(crate) refreshes: Mutex<Vec<RefreshScope>>,
    pub(crate) op_wakeups: Mutex<Vec<AccountId>>,
}

impl SyncBridge for RecordingSync {
    fn fetch_body(&self, message: MessageId, priority: FetchPriority) {
        self.fetches.lock().unwrap().push((message, priority));
    }

    fn refresh(&self, scope: RefreshScope) {
        self.refreshes.lock().unwrap().push(scope);
    }

    fn ops_enqueued(&self, accounts: &[AccountId]) {
        self.op_wakeups.lock().unwrap().extend_from_slice(accounts);
    }
}

/// A config for tests without a runtime or verifier choice: no blob GC.
pub(crate) fn config_without_runtime(dir: &Path, keys: &Arc<MemoryKeyStore>) -> CoreConfig {
    let mut config = CoreConfig::new(dir, Arc::clone(keys) as Arc<dyn vault::KeyStore>);
    config.app_version = "1.0.0-test".to_owned();
    config.blob_gc_interval = None;
    config
}

/// A config for tests: the current runtime, an offline verifier, no blob GC.
pub(crate) fn config(dir: &Path, keys: &Arc<MemoryKeyStore>) -> CoreConfig {
    config_without_runtime(dir, keys)
        .with_runtime(tokio::runtime::Handle::current())
        .with_verifier(Verifier::new(
            Resolver::from_static(StaticDns::new()).unwrap(),
        ))
}

pub(crate) struct TestCore {
    pub(crate) core: CoreHandle,
    pub(crate) keys: Arc<MemoryKeyStore>,
    pub(crate) sync: Arc<RecordingSync>,
    pub(crate) dir: tempfile::TempDir,
}

pub(crate) async fn open() -> TestCore {
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let sync = Arc::new(RecordingSync::default());
    let core = CoreHandle::open(config(dir.path(), &keys).with_sync(sync.clone()))
        .await
        .unwrap();
    TestCore {
        core,
        keys,
        sync,
        dir,
    }
}

/// Two accounts: "Work" (IMAP, identity with yearly archive) and "Gmail".
#[derive(Debug, Clone, Copy)]
#[allow(dead_code, reason = "each test uses a different part of the fixture")]
pub(crate) struct Fixture {
    pub(crate) work: AccountId,
    pub(crate) inbox: FolderId,
    pub(crate) archive: FolderId,
    pub(crate) sent: FolderId,
    pub(crate) trash: FolderId,
    pub(crate) projects: FolderId,
    pub(crate) identity: IdentityId,
    pub(crate) gmail: AccountId,
    pub(crate) g_inbox: FolderId,
    pub(crate) g_all: FolderId,
    pub(crate) g_trash: FolderId,
    pub(crate) g_label: FolderId,
}

pub(crate) async fn fixture(core: &CoreHandle) -> Fixture {
    core.store()
        .write(|w| {
            let work = w.create_account(&NewAccount {
                kind: AccountKind::Imap,
                name: "Work".to_owned(),
                config_json: r#"{"delimiter":"/"}"#.to_owned(),
                ordinal: 0,
            })?;
            let folder = |w: &mut store::WriteTx<'_>, account, remote: &str, role| {
                let mut new = NewFolder::new(account, remote, remote);
                new.role = role;
                w.create_folder(&new)
            };
            let inbox = folder(w, work, "INBOX", Some(FolderRole::Inbox))?;
            let archive = folder(w, work, "Archive", Some(FolderRole::Archive))?;
            let sent = folder(w, work, "Sent", Some(FolderRole::Sent))?;
            let trash = folder(w, work, "Trash", Some(FolderRole::Trash))?;
            let projects = folder(w, work, "Projects", None)?;
            let identity = w.create_identity(
                work,
                &IdentityInput {
                    email: "me@work.example".to_owned(),
                    archive_folder: Some(archive),
                    archive_granularity: 1,
                    is_default: true,
                    ..IdentityInput::default()
                },
            )?;

            let gmail = w.create_account(&NewAccount {
                kind: AccountKind::Gmail,
                name: "Gmail".to_owned(),
                config_json: "{}".to_owned(),
                ordinal: 1,
            })?;
            let g_inbox = folder(w, gmail, "INBOX", Some(FolderRole::Inbox))?;
            let g_all = folder(w, gmail, "[Gmail]/All Mail", Some(FolderRole::All))?;
            let g_trash = folder(w, gmail, "[Gmail]/Trash", Some(FolderRole::Trash))?;
            let g_label =
                w.create_folder(&NewFolder::new(gmail, "Receipts", "Receipts").label())?;
            Ok(Fixture {
                work,
                inbox,
                archive,
                sent,
                trash,
                projects,
                identity,
                gmail,
                g_inbox,
                g_all,
                g_trash,
                g_label,
            })
        })
        .await
        .unwrap()
}

/// A raw RFC 5322 message.
pub(crate) fn raw_message(
    id: &str,
    from: &str,
    to: &str,
    subject: &str,
    date: &str,
    body: &str,
) -> Vec<u8> {
    format!(
        "Message-ID: <{id}>\r\nDate: {date}\r\nFrom: {from}\r\nTo: {to}\r\nSubject: {subject}\r\n\
         MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

/// An HTML newsletter with a link, a remote image and a tracking pixel.
pub(crate) fn html_message(id: &str, subject: &str, paragraphs: usize) -> Vec<u8> {
    let mut html = String::from("<html><body><table><tr><td>");
    for i in 0..paragraphs {
        html.push_str(&format!(
            "<p>Paragraph {i}: the quarterly report shows <b>steady</b> growth in every \
             region. <a href=\"https://example.com/article/{i}\">Read more</a></p>"
        ));
    }
    html.push_str(
        "<img src=\"https://cdn.example.com/hero.png\" width=\"600\" height=\"200\">\
         <img src=\"https://tracker.example.net/open.gif\" width=\"1\" height=\"1\">\
         </td></tr></table></body></html>",
    );
    format!(
        "Message-ID: <{id}>\r\nDate: Tue, 14 Oct 2025 09:30:00 +0000\r\n\
         From: Newsletter <news@example.com>\r\nTo: me@work.example\r\nSubject: {subject}\r\n\
         MIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{html}\r\n"
    )
    .into_bytes()
}

/// Where a simulated ingest puts a message.
pub(crate) struct Place {
    pub(crate) account: AccountId,
    pub(crate) folder: FolderId,
    pub(crate) uid: Option<u32>,
    pub(crate) flags: Flags,
    pub(crate) gm_msgid: Option<u64>,
    pub(crate) store_body: bool,
}

impl Place {
    pub(crate) fn new(account: AccountId, folder: FolderId, uid: u32) -> Self {
        Self {
            account,
            folder,
            uid: Some(uid),
            flags: Flags::empty(),
            gm_msgid: None,
            store_body: true,
        }
    }

    pub(crate) fn flags(mut self, flags: Flags) -> Self {
        self.flags = flags;
        self
    }

    pub(crate) fn headers_only(mut self) -> Self {
        self.store_body = false;
        self
    }

    pub(crate) fn gmail(mut self, gm_msgid: u64) -> Self {
        self.gm_msgid = Some(gm_msgid);
        self
    }

    pub(crate) fn no_uid(mut self) -> Self {
        self.uid = None;
        self
    }
}

/// What `ingest` will do: parse, thread, store headers and body, index.
pub(crate) async fn ingest(core: &CoreHandle, place: Place, raw: Vec<u8>) -> (MessageId, ThreadId) {
    let parsed = mazemail_mime::parse(&raw).unwrap();
    let date = parsed.headers.date.map_or(0, |d| d.unix_timestamp());
    let mut message = NewMessage::new(place.account, date);
    message.rfc_message_id = parsed.headers.message_id.clone();
    message.in_reply_to = parsed.headers.in_reply_to.first().cloned();
    message.references = parsed.headers.references.clone();
    message.from = parsed.headers.from.first().cloned();
    message.to = parsed.headers.to.clone();
    message.cc = parsed.headers.cc.clone();
    message.subject = parsed.headers.subject.clone();
    let subject = parsed
        .headers
        .subject
        .as_deref()
        .map(mazemail_mime::subject::parse_subject);
    message.subject_key = subject.as_ref().map(|s| s.key());
    message.snippet = Some(parsed.snippet.clone());
    message.size = Some(raw.len() as u64);
    message.has_attachments = parsed.has_attachments;
    message.list_id = parsed.headers.list_id.clone();
    message.gm_msgid = place.gm_msgid;
    message.thread = ThreadAssignment::Resolve {
        by_subject: subject.is_some_and(|s| s.is_reply),
    };
    message.locations = vec![NewLocation::new(place.folder, place.uid, place.flags)];

    let staged = if place.store_body {
        Some(core.store().blobs().put(&raw).unwrap())
    } else {
        None
    };
    let outcome = core
        .store()
        .write(move |w| {
            let outcome = w.upsert_message(&message)?;
            if let Some(staged) = &staged {
                w.set_message_body(outcome.message, staged, BodyState::Full)?;
            }
            Ok(outcome)
        })
        .await
        .unwrap();
    core.index_sink()
        .add(IndexDocument::from_parsed(
            outcome.message,
            outcome.thread,
            place.account,
            date,
            &parsed,
        ))
        .unwrap();
    (outcome.message, outcome.thread)
}

/// Waits for the next event batch that satisfies `pred`.
pub(crate) async fn wait_for(
    rx: &mut broadcast::Receiver<CoreEventBatch>,
    mut pred: impl FnMut(&CoreEventBatch) -> bool,
) -> CoreEventBatch {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(batch) if pred(&batch) => return batch,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("event stream closed"),
            }
        }
    })
    .await
    .expect("timed out waiting for an event batch")
}
