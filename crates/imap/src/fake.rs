//! An in-memory IMAP server for deterministic tests of the sync engine.
//!
//! [`FakeServer`] holds folders and messages and is shared between any number of
//! [`FakeImap`] sessions (each one implements [`ImapSession`]). Tests mutate the
//! server directly to simulate other clients and the outside world: deliver mail,
//! change flags, expunge, reset UIDVALIDITY, inject failures. Sessions see those
//! changes like a real server reports them: as [`SessionEvent`]s at the next command
//! and as IDLE wake-ups.
//!
//! What is modelled:
//!
//! - UIDs, UIDNEXT, UIDVALIDITY, per-message mod-sequences and HIGHESTMODSEQ
//!   (CONDSTORE), an expunge log for `VANISHED` (QRESYNC),
//! - SPECIAL-USE roles, subscriptions, folder create/rename/delete,
//! - capability-dependent behavior: MOVE vs. COPY fallback, UIDPLUS results,
//!   CONDSTORE/QRESYNC/IDLE/Gmail availability,
//! - Gmail mode: `X-GM-MSGID`, `X-GM-THRID` and `X-GM-LABELS` per message.
//!
//! Not modelled: Gmail's label-to-folder views (a message lives in one folder; its
//! labels are metadata), MIME parsing (pass a [`BodySummary`] and parts explicitly
//! for multipart messages), sequence-number-only responses, `\Recent`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use mazemail_types::{Flags, FolderRole, Sensitive};
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::client::validate_keyword;
use crate::error::{ImapError, Result};
use crate::gmail::{GmailLabel, LabelOp};
use crate::session::ImapSession;
use crate::types::{
    AppendUid, BodyPart, BodySummary, Caps, ChangeSet, CopyUid, FlagChange, FlagOp, FolderStatus,
    GmailMeta, HeaderFetch, IdleOutcome, QresyncArgs, RemoteFolder, SelectInfo, SessionEvent,
    StoreMode,
};
use crate::uid_set::UidSet;
use crate::utf7;

/// Capabilities of a default [`FakeServer`]: a modern Dovecot-like server.
pub const DEFAULT_CAPS: &[&str] = &[
    "IMAP4rev1",
    "IDLE",
    "MOVE",
    "UIDPLUS",
    "CONDSTORE",
    "QRESYNC",
    "ENABLE",
    "SPECIAL-USE",
    "LITERAL+",
];

/// Capabilities of [`FakeServer::gmail`]: like Gmail, without QRESYNC.
pub const GMAIL_CAPS: &[&str] = &[
    "IMAP4rev1",
    "IDLE",
    "MOVE",
    "UIDPLUS",
    "CONDSTORE",
    "ENABLE",
    "SPECIAL-USE",
    "X-GM-EXT-1",
    "ID",
];

/// A message to deliver into a [`FakeServer`] folder.
#[derive(Debug, Clone)]
pub struct FakeMessage {
    raw: Bytes,
    flags: Flags,
    keywords: Vec<String>,
    internal_date: Option<OffsetDateTime>,
    labels: Vec<GmailLabel>,
    thread_id: Option<u64>,
    body: Option<BodySummary>,
    parts: HashMap<String, Bytes>,
}

impl FakeMessage {
    /// A message with the given raw RFC 5322 bytes and no flags.
    pub fn new(raw: impl Into<Bytes>) -> Self {
        Self {
            raw: raw.into(),
            flags: Flags::empty(),
            keywords: Vec::new(),
            internal_date: None,
            labels: Vec::new(),
            thread_id: None,
            body: None,
            parts: HashMap::new(),
        }
    }

    /// A simple text message with a `Subject` and `Message-ID`.
    pub fn simple(subject: &str, message_id: &str, text: &str) -> Self {
        Self::new(format!(
            "Message-ID: <{message_id}>\r\nSubject: {subject}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{text}"
        ))
    }

    /// Set the initial flags.
    #[must_use]
    pub fn flags(mut self, flags: Flags) -> Self {
        self.flags = flags;
        self
    }

    /// Set the initial keywords.
    #[must_use]
    pub fn keywords<I, S>(mut self, keywords: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.keywords = keywords.into_iter().map(Into::into).collect();
        self
    }

    /// Set the `INTERNALDATE`.
    #[must_use]
    pub fn internal_date(mut self, date: OffsetDateTime) -> Self {
        self.internal_date = Some(date);
        self
    }

    /// Set Gmail labels (Gmail mode).
    #[must_use]
    pub fn labels(mut self, labels: impl IntoIterator<Item = GmailLabel>) -> Self {
        self.labels = labels.into_iter().collect();
        self
    }

    /// Set the Gmail thread id (Gmail mode). Defaults to the message id.
    #[must_use]
    pub fn thread_id(mut self, thread_id: u64) -> Self {
        self.thread_id = Some(thread_id);
        self
    }

    /// Set the BODYSTRUCTURE summary. Defaults to one `text/plain` part.
    #[must_use]
    pub fn body(mut self, body: BodySummary) -> Self {
        self.body = Some(body);
        self
    }

    /// Provide the content of a body section for `fetch_part`.
    #[must_use]
    pub fn part(mut self, section: impl Into<String>, content: impl Into<Bytes>) -> Self {
        self.parts.insert(section.into(), content.into());
        self
    }
}

/// Snapshot of a stored message, for assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeMessageInfo {
    /// UID in its folder.
    pub uid: u32,
    /// System flags and well-known keywords.
    pub flags: Flags,
    /// Other keywords.
    pub keywords: Vec<String>,
    /// Mod-sequence.
    pub modseq: u64,
    /// Gmail labels.
    pub labels: Vec<GmailLabel>,
    /// Gmail message id.
    pub gm_msgid: u64,
}

/// A failure to inject into the next matching command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeFailure {
    /// The connection drops; the session is broken afterwards.
    ConnectionLost,
    /// The command times out; the session is broken afterwards.
    Timeout,
    /// The server answers `NO` with this text; the session stays usable.
    No(String),
}

#[derive(Debug, Clone)]
struct Stored {
    raw: Bytes,
    flags: Flags,
    keywords: Vec<String>,
    modseq: u64,
    internal_date: Option<OffsetDateTime>,
    gm_msgid: u64,
    gm_thrid: u64,
    labels: Vec<GmailLabel>,
    body: Option<BodySummary>,
    parts: HashMap<String, Bytes>,
}

impl Stored {
    fn header(&self) -> Bytes {
        let end = find(&self.raw, b"\r\n\r\n")
            .map(|i| i + 4)
            .or_else(|| find(&self.raw, b"\n\n").map(|i| i + 2))
            .unwrap_or(self.raw.len());
        self.raw.slice(..end)
    }

    fn text(&self) -> Bytes {
        self.raw.slice(self.header().len()..)
    }

    fn body_summary(&self) -> BodySummary {
        self.body.clone().unwrap_or_else(|| BodySummary {
            parts: vec![BodyPart {
                section: "1".into(),
                mime_type: "text".into(),
                subtype: "plain".into(),
                charset: Some("utf-8".into()),
                encoding: "7bit".into(),
                size: u32::try_from(self.text().len()).unwrap_or(u32::MAX),
                disposition: None,
                filename: None,
                content_id: None,
                is_attachment: false,
            }],
        })
    }

    fn flag_change(&self, uid: u32, gmail: bool) -> FlagChange {
        FlagChange {
            uid,
            flags: self.flags,
            keywords: self.keywords.clone(),
            modseq: Some(self.modseq),
            gmail_labels: gmail.then(|| self.labels.clone()),
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[derive(Debug, Clone)]
struct Folder {
    delimiter: Option<char>,
    attributes: Vec<String>,
    role: Option<FolderRole>,
    selectable: bool,
    subscribed: bool,
    uidvalidity: u32,
    uidnext: u32,
    highest_modseq: u64,
    messages: BTreeMap<u32, Stored>,
    /// `(uid, modseq)` of expunged messages, for `VANISHED`.
    expunged: Vec<(u32, u64)>,
}

#[derive(Debug)]
struct ServerState {
    caps: Caps,
    folders: BTreeMap<String, Folder>,
    modseq: u64,
    next_uidvalidity: u32,
    next_gm_msgid: u64,
    failures: VecDeque<(Option<&'static str>, FakeFailure)>,
    log: Vec<String>,
}

impl ServerState {
    fn bump(&mut self) -> u64 {
        self.modseq += 1;
        self.modseq
    }

    fn new_folder(&mut self, role: Option<FolderRole>, attributes: Vec<String>) -> Folder {
        self.next_uidvalidity += 1;
        Folder {
            delimiter: Some('/'),
            attributes,
            role,
            selectable: true,
            subscribed: true,
            uidvalidity: self.next_uidvalidity,
            uidnext: 1,
            highest_modseq: 1,
            messages: BTreeMap::new(),
            expunged: Vec::new(),
        }
    }

    fn folder(&self, name: &str) -> Result<&Folder> {
        self.folders
            .get(&canonical(name))
            .ok_or_else(|| no("NONEXISTENT", "no such folder"))
    }

    fn folder_mut(&mut self, name: &str) -> Result<&mut Folder> {
        self.folders
            .get_mut(&canonical(name))
            .ok_or_else(|| no("NONEXISTENT", "no such folder"))
    }

    fn insert(&mut self, folder: &str, message: FakeMessage) -> Result<(u32, u32)> {
        let modseq = self.bump();
        self.next_gm_msgid += 1;
        let gm_msgid = self.next_gm_msgid;
        let target = self
            .folders
            .get_mut(&canonical(folder))
            .ok_or_else(|| no("TRYCREATE", "no such folder"))?;
        let uid = target.uidnext;
        target.uidnext += 1;
        target.highest_modseq = modseq;
        target.messages.insert(
            uid,
            Stored {
                raw: message.raw,
                flags: message.flags,
                keywords: message.keywords,
                modseq,
                internal_date: message.internal_date,
                gm_msgid,
                gm_thrid: message.thread_id.unwrap_or(gm_msgid),
                labels: message.labels,
                body: message.body,
                parts: message.parts,
            },
        );
        Ok((target.uidvalidity, uid))
    }

    fn remove(&mut self, folder: &str, uid: u32) -> Result<Option<Stored>> {
        let modseq = self.bump();
        let f = self.folder_mut(folder)?;
        let removed = f.messages.remove(&uid);
        if removed.is_some() {
            f.expunged.push((uid, modseq));
            f.highest_modseq = modseq;
        }
        Ok(removed)
    }

    fn copy_into(&mut self, to: &str, stored: Stored) -> Result<(u32, u32)> {
        let modseq = self.bump();
        let target = self
            .folders
            .get_mut(&canonical(to))
            .ok_or_else(|| no("TRYCREATE", "no such folder"))?;
        let uid = target.uidnext;
        target.uidnext += 1;
        target.highest_modseq = modseq;
        target.messages.insert(uid, Stored { modseq, ..stored });
        Ok((target.uidvalidity, uid))
    }
}

fn canonical(name: &str) -> String {
    if name.eq_ignore_ascii_case("INBOX") {
        "INBOX".to_owned()
    } else {
        name.to_owned()
    }
}

fn no(code: &str, text: &str) -> ImapError {
    ImapError::No {
        command: "command",
        code: Some(code.to_owned()),
        text: text.to_owned(),
    }
}

fn apply_flags(stored: &mut Stored, mode: StoreMode, flags: Flags, keywords: &[String]) -> bool {
    let before = (stored.flags, stored.keywords.clone());
    match mode {
        StoreMode::Set => {
            stored.flags = flags;
            stored.keywords = keywords.to_vec();
        }
        StoreMode::Add => {
            stored.flags |= flags;
            for k in keywords {
                if !stored.keywords.iter().any(|e| e.eq_ignore_ascii_case(k)) {
                    stored.keywords.push(k.clone());
                }
            }
        }
        StoreMode::Remove => {
            stored.flags &= !flags;
            stored
                .keywords
                .retain(|e| !keywords.iter().any(|k| k.eq_ignore_ascii_case(e)));
        }
    }
    before != (stored.flags, stored.keywords.clone())
}

/// The shared in-memory server. Cloning gives another handle to the same server.
#[derive(Debug, Clone)]
pub struct FakeServer {
    state: Arc<Mutex<ServerState>>,
    changes: Arc<watch::Sender<u64>>,
}

impl Default for FakeServer {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeServer {
    /// A server with [`DEFAULT_CAPS`] and the folders INBOX, Sent, Drafts, Trash,
    /// Junk and Archive (with SPECIAL-USE roles).
    pub fn new() -> Self {
        let server = Self::empty(DEFAULT_CAPS);
        server.add_folder("INBOX", Some(FolderRole::Inbox));
        for (name, role) in [
            ("Sent", FolderRole::Sent),
            ("Drafts", FolderRole::Drafts),
            ("Trash", FolderRole::Trash),
            ("Junk", FolderRole::Junk),
            ("Archive", FolderRole::Archive),
        ] {
            server.add_folder(name, Some(role));
        }
        server
    }

    /// A Gmail-like server with [`GMAIL_CAPS`] and the `[Gmail]` folder tree.
    pub fn gmail() -> Self {
        let server = Self::empty(GMAIL_CAPS);
        server.add_folder("INBOX", Some(FolderRole::Inbox));
        {
            let mut state = server.lock();
            let mut parent =
                state.new_folder(None, vec!["\\HasChildren".into(), "\\Noselect".into()]);
            parent.selectable = false;
            state.folders.insert("[Gmail]".into(), parent);
        }
        for (name, role) in [
            ("[Gmail]/All Mail", FolderRole::All),
            ("[Gmail]/Sent Mail", FolderRole::Sent),
            ("[Gmail]/Drafts", FolderRole::Drafts),
            ("[Gmail]/Trash", FolderRole::Trash),
            ("[Gmail]/Spam", FolderRole::Junk),
            ("[Gmail]/Starred", FolderRole::Flagged),
            ("[Gmail]/Important", FolderRole::Important),
        ] {
            server.add_folder(name, Some(role));
        }
        server
    }

    /// A server without folders and with the given capabilities.
    pub fn empty(caps: &[&str]) -> Self {
        Self {
            state: Arc::new(Mutex::new(ServerState {
                caps: Caps::from_names(caps),
                folders: BTreeMap::new(),
                modseq: 1,
                next_uidvalidity: 1000,
                next_gm_msgid: 1_000_000,
                failures: VecDeque::new(),
                log: Vec::new(),
            })),
            changes: Arc::new(watch::channel(0).0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ServerState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notify(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    /// Replace the capability list (affects new and existing sessions).
    pub fn set_caps(&self, caps: &[&str]) {
        self.lock().caps = Caps::from_names(caps);
    }

    /// Add a folder (and nothing else; parents are not created).
    pub fn add_folder(&self, name: &str, role: Option<FolderRole>) {
        let mut state = self.lock();
        let attributes = match role {
            Some(FolderRole::Sent) => vec!["\\Sent".to_owned()],
            Some(FolderRole::Drafts) => vec!["\\Drafts".to_owned()],
            Some(FolderRole::Trash) => vec!["\\Trash".to_owned()],
            Some(FolderRole::Junk) => vec!["\\Junk".to_owned()],
            Some(FolderRole::Archive) => vec!["\\Archive".to_owned()],
            Some(FolderRole::All) => vec!["\\All".to_owned()],
            Some(FolderRole::Flagged) => vec!["\\Flagged".to_owned()],
            Some(FolderRole::Important) => vec!["\\Important".to_owned()],
            Some(FolderRole::Inbox) | None => Vec::new(),
        };
        let folder = state.new_folder(role, attributes);
        state.folders.insert(canonical(name), folder);
    }

    /// Deliver a message as if another client or the MTA added it. Returns its UID.
    ///
    /// # Panics
    ///
    /// If the folder does not exist (a test setup error).
    pub fn deliver(&self, folder: &str, message: FakeMessage) -> u32 {
        let (_, uid) = self
            .lock()
            .insert(folder, message)
            .unwrap_or_else(|e| panic!("deliver to {folder:?}: {e}"));
        self.notify();
        uid
    }

    /// Change a message's flags as another client would.
    pub fn set_flags(&self, folder: &str, uid: u32, flags: Flags, keywords: &[&str]) {
        let mut state = self.lock();
        let modseq = state.bump();
        if let Ok(f) = state.folder_mut(folder)
            && let Some(m) = f.messages.get_mut(&uid)
        {
            m.flags = flags;
            m.keywords = keywords.iter().map(|k| (*k).to_owned()).collect();
            m.modseq = modseq;
            f.highest_modseq = modseq;
        }
        drop(state);
        self.notify();
    }

    /// Change a message's Gmail labels as the Gmail web UI would.
    pub fn set_labels(&self, folder: &str, uid: u32, labels: Vec<GmailLabel>) {
        let mut state = self.lock();
        let modseq = state.bump();
        if let Ok(f) = state.folder_mut(folder)
            && let Some(m) = f.messages.get_mut(&uid)
        {
            m.labels = labels;
            m.modseq = modseq;
            f.highest_modseq = modseq;
        }
        drop(state);
        self.notify();
    }

    /// Expunge a message as another client would.
    pub fn expunge(&self, folder: &str, uid: u32) {
        // A missing folder or message is simply a no-op for the test.
        let _removed = self.lock().remove(folder, uid);
        self.notify();
    }

    /// Give a folder a new UIDVALIDITY and renumber its messages from 1, as after a
    /// server-side mailbox rebuild. Returns the new UIDVALIDITY.
    ///
    /// # Panics
    ///
    /// If the folder does not exist.
    pub fn reset_uidvalidity(&self, folder: &str) -> u32 {
        let mut state = self.lock();
        state.next_uidvalidity += 1;
        let validity = state.next_uidvalidity;
        let modseq = state.bump();
        let f = state
            .folder_mut(folder)
            .unwrap_or_else(|e| panic!("reset {folder:?}: {e}"));
        let messages = std::mem::take(&mut f.messages);
        f.messages = messages
            .into_values()
            .zip(1u32..)
            .map(|(m, uid)| (uid, m))
            .collect();
        f.uidnext = u32::try_from(f.messages.len()).unwrap_or(u32::MAX - 1) + 1;
        f.uidvalidity = validity;
        f.expunged.clear();
        f.highest_modseq = modseq;
        drop(state);
        self.notify();
        validity
    }

    /// The messages of a folder, by UID.
    pub fn messages(&self, folder: &str) -> Vec<FakeMessageInfo> {
        let state = self.lock();
        state
            .folder(folder)
            .map(|f| {
                f.messages
                    .iter()
                    .map(|(uid, m)| FakeMessageInfo {
                        uid: *uid,
                        flags: m.flags,
                        keywords: m.keywords.clone(),
                        modseq: m.modseq,
                        labels: m.labels.clone(),
                        gm_msgid: m.gm_msgid,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The raw bytes of a message.
    pub fn raw(&self, folder: &str, uid: u32) -> Option<Bytes> {
        let state = self.lock();
        state
            .folder(folder)
            .ok()
            .and_then(|f| f.messages.get(&uid))
            .map(|m| m.raw.clone())
    }

    /// Folder names, sorted.
    pub fn folder_names(&self) -> Vec<String> {
        self.lock().folders.keys().cloned().collect()
    }

    /// UIDVALIDITY of a folder.
    pub fn uidvalidity(&self, folder: &str) -> Option<u32> {
        self.lock().folder(folder).ok().map(|f| f.uidvalidity)
    }

    /// Make the next command of any session fail.
    pub fn fail_next(&self, failure: FakeFailure) {
        self.lock().failures.push_back((None, failure));
    }

    /// Make the next command with this verb (`SELECT`, `UID MOVE`, `APPEND`, …) fail.
    pub fn fail_command(&self, verb: &'static str, failure: FakeFailure) {
        self.lock().failures.push_back((Some(verb), failure));
    }

    /// Commands executed so far, e.g. `SELECT INBOX`, `UID MOVE 1:2 Archive`.
    pub fn command_log(&self) -> Vec<String> {
        self.lock().log.clone()
    }

    /// Open a session (already authenticated). QRESYNC is enabled if advertised.
    pub fn connect(&self) -> FakeImap {
        FakeImap {
            server: self.clone(),
            changes: self.changes.subscribe(),
            selected: None,
            snapshot: Snapshot::default(),
            events: Vec::new(),
            broken: false,
        }
    }
}

/// What a session last reported about its selected folder.
#[derive(Debug, Default, Clone)]
struct Snapshot {
    uids: Vec<u32>,
    modseq: u64,
}

/// One session on a [`FakeServer`].
#[derive(Debug)]
pub struct FakeImap {
    server: FakeServer,
    changes: watch::Receiver<u64>,
    selected: Option<String>,
    snapshot: Snapshot,
    events: Vec<SessionEvent>,
    broken: bool,
}

impl FakeImap {
    /// The server this session talks to.
    pub fn server(&self) -> &FakeServer {
        &self.server
    }

    /// Start a command: check the session, consume an injected failure, log it.
    ///
    /// The guard borrows `server` (a clone of `self.server`), not `self`, so the
    /// session can update its own state while the server is locked.
    fn begin<'a>(
        &mut self,
        server: &'a FakeServer,
        verb: &'static str,
        detail: &str,
    ) -> Result<MutexGuard<'a, ServerState>> {
        if self.broken {
            return Err(ImapError::Broken);
        }
        let mut state = server.lock();
        let pos = state
            .failures
            .iter()
            .position(|(v, _)| v.is_none_or(|v| v == verb));
        if let Some(pos) = pos
            && let Some((_, failure)) = state.failures.remove(pos)
        {
            return Err(match failure {
                FakeFailure::ConnectionLost => {
                    self.broken = true;
                    ImapError::ConnectionLost(None)
                }
                FakeFailure::Timeout => {
                    self.broken = true;
                    ImapError::Timeout {
                        operation: verb,
                        after: Duration::from_secs(60),
                    }
                }
                FakeFailure::No(text) => ImapError::No {
                    command: verb,
                    code: None,
                    text,
                },
            });
        }
        let entry = if detail.is_empty() {
            verb.to_owned()
        } else {
            format!("{verb} {detail}")
        };
        state.log.push(entry);
        Ok(state)
    }

    fn selected(&self) -> Result<String> {
        self.selected.clone().ok_or(ImapError::NoFolderSelected)
    }

    /// Report changes to the selected folder since the last snapshot as events,
    /// like the unsolicited responses a real server sends.
    fn sync_events(&mut self, state: &ServerState) {
        let Some(name) = &self.selected else { return };
        let Ok(folder) = state.folder(name) else {
            return;
        };
        let qresync = state.caps.qresync();
        let removed: Vec<u32> = self
            .snapshot
            .uids
            .iter()
            .copied()
            .filter(|uid| !folder.messages.contains_key(uid))
            .collect();
        if !removed.is_empty() {
            if qresync {
                self.events
                    .push(SessionEvent::Vanished(removed.iter().copied().collect()));
            } else {
                // Highest sequence numbers first so each one stays valid.
                for uid in removed.iter().rev() {
                    if let Some(pos) = self.snapshot.uids.iter().position(|u| u == uid) {
                        let seq = u32::try_from(pos + 1).unwrap_or(u32::MAX);
                        self.events.push(SessionEvent::Expunge(seq));
                    }
                }
            }
        }
        let current: Vec<u32> = folder.messages.keys().copied().collect();
        for (pos, (uid, m)) in folder.messages.iter().enumerate() {
            if m.modseq > self.snapshot.modseq && self.snapshot.uids.contains(uid) {
                self.events.push(SessionEvent::Flags {
                    seq: u32::try_from(pos + 1).unwrap_or(u32::MAX),
                    uid: Some(*uid),
                    flags: m.flags,
                    keywords: m.keywords.clone(),
                    modseq: Some(m.modseq),
                });
            }
        }
        if current.iter().any(|u| !self.snapshot.uids.contains(u)) {
            self.events.push(SessionEvent::Exists(
                u32::try_from(current.len()).unwrap_or(u32::MAX),
            ));
        }
        self.snapshot = Snapshot {
            uids: current,
            modseq: folder.highest_modseq,
        };
    }

    /// Accept the selected folder's current state without reporting it (after the
    /// session's own silent changes).
    fn refresh_snapshot(&mut self, state: &ServerState) {
        if let Some(name) = &self.selected
            && let Ok(folder) = state.folder(name)
        {
            self.snapshot = Snapshot {
                uids: folder.messages.keys().copied().collect(),
                modseq: folder.highest_modseq,
            };
        }
    }

    fn has_change_events(&self) -> bool {
        self.events
            .iter()
            .any(|e| !matches!(e, SessionEvent::Alert(_) | SessionEvent::Recent(_)))
    }

    fn fetch_body(&mut self, uid: u32, section: &str) -> Result<Bytes> {
        let server = self.server.clone();
        let folder = self.selected()?;
        let state = self.begin(&server, "UID FETCH", &format!("{uid} BODY.PEEK[{section}]"))?;
        let f = state.folder(&folder)?;
        let m = f
            .messages
            .get(&uid)
            .ok_or(ImapError::MessageNotFound { uid })?;
        let found = match section {
            "" => Some(m.raw.clone()),
            "HEADER" => Some(m.header()),
            "TEXT" => Some(m.text()),
            other => m
                .parts
                .get(other)
                .cloned()
                .or_else(|| (other == "1" && m.body.is_none()).then(|| m.text())),
        };
        found.ok_or_else(|| ImapError::PartNotFound {
            uid,
            section: section.to_owned(),
        })
    }

    fn remove_uids(&self, folder: &str, uids: &UidSet) -> Result<Vec<(u32, Stored)>> {
        let mut state = self.server.lock();
        let present: Vec<u32> = state
            .folder(folder)?
            .messages
            .keys()
            .copied()
            .filter(|u| uids.contains(*u))
            .collect();
        let mut out = Vec::new();
        for uid in present {
            if let Some(m) = state.remove(folder, uid)? {
                out.push((uid, m));
            }
        }
        Ok(out)
    }

    fn copy_uids(
        &self,
        from: &str,
        uids: &UidSet,
        to: &str,
        remove: bool,
    ) -> Result<Option<CopyUid>> {
        let mut state = self.server.lock();
        state
            .folder(to)
            .map_err(|_| no("TRYCREATE", "no such folder"))?;
        let source: Vec<(u32, Stored)> = state
            .folder(from)?
            .messages
            .iter()
            .filter(|(u, _)| uids.contains(**u))
            .map(|(u, m)| (*u, m.clone()))
            .collect();
        let mut copy = CopyUid {
            uidvalidity: state.folder(to)?.uidvalidity,
            source: Vec::new(),
            destination: Vec::new(),
        };
        for (uid, m) in source {
            let (_, new_uid) = state.copy_into(to, m)?;
            if remove {
                state.remove(from, uid)?;
            }
            copy.source.push(uid);
            copy.destination.push(new_uid);
        }
        let uidplus = state.caps.uidplus();
        drop(state);
        self.server.notify();
        Ok((uidplus && !copy.source.is_empty()).then_some(copy))
    }
}

#[async_trait]
impl ImapSession for FakeImap {
    async fn capabilities(&mut self) -> Result<Caps> {
        if self.broken {
            return Err(ImapError::Broken);
        }
        Ok(self.server.lock().caps.clone())
    }

    async fn list(&mut self) -> Result<Vec<RemoteFolder>> {
        let server = self.server.clone();
        let state = self.begin(&server, "LIST", "")?;
        let mut folders: Vec<RemoteFolder> = state
            .folders
            .iter()
            .map(|(name, f)| {
                let mut attributes = f.attributes.clone();
                if !f.selectable && !attributes.iter().any(|a| a == "\\Noselect") {
                    attributes.push("\\Noselect".into());
                }
                RemoteFolder {
                    name: name.clone(),
                    raw_name: utf7::encode(name),
                    delimiter: f.delimiter,
                    attributes,
                    role: f.role,
                    selectable: f.selectable,
                    subscribed: f.subscribed,
                }
            })
            .collect();
        // INBOX first, like most servers.
        folders.sort_by_key(|f| (f.name != "INBOX", f.name.clone()));
        Ok(folders)
    }

    async fn select(&mut self, folder: &str, qresync: Option<QresyncArgs>) -> Result<SelectInfo> {
        let server = self.server.clone();
        let state = self.begin(&server, "SELECT", folder)?;
        self.selected = None;
        let caps = state.caps.clone();
        let f = state.folder(folder)?;
        if !f.selectable {
            return Err(no("CANNOT", "folder is not selectable"));
        }
        let mut info = SelectInfo {
            uidvalidity: f.uidvalidity,
            uidnext: Some(f.uidnext),
            highest_modseq: caps.condstore().then_some(f.highest_modseq),
            exists: u32::try_from(f.messages.len()).unwrap_or(u32::MAX),
            flags: vec![
                "\\Answered".into(),
                "\\Flagged".into(),
                "\\Deleted".into(),
                "\\Seen".into(),
                "\\Draft".into(),
            ],
            permanent_flags: vec![
                "\\Answered".into(),
                "\\Flagged".into(),
                "\\Deleted".into(),
                "\\Seen".into(),
                "\\Draft".into(),
            ],
            allows_new_keywords: true,
            ..SelectInfo::default()
        };
        if let Some(q) = qresync.filter(|q| caps.qresync() && q.highest_modseq > 0)
            && q.uidvalidity == f.uidvalidity
        {
            info.qresync_applied = true;
            for (uid, modseq) in &f.expunged {
                let known = q.known_uids.as_ref().is_none_or(|k| k.contains(*uid));
                if *modseq > q.highest_modseq && known {
                    info.vanished.insert(*uid);
                }
            }
            info.changed = f
                .messages
                .iter()
                .filter(|(_, m)| m.modseq > q.highest_modseq)
                .map(|(uid, m)| m.flag_change(*uid, caps.gmail()))
                .collect();
        }
        let name = canonical(folder);
        let snapshot = Snapshot {
            uids: f.messages.keys().copied().collect(),
            modseq: f.highest_modseq,
        };
        drop(state);
        self.selected = Some(name);
        self.snapshot = snapshot;
        Ok(info)
    }

    async fn fetch_changes(&mut self, since_modseq: u64) -> Result<ChangeSet> {
        let server = self.server.clone();
        let folder = self.selected()?;
        let state = self.begin(
            &server,
            "UID FETCH",
            &format!("1:* CHANGEDSINCE {since_modseq}"),
        )?;
        if !state.caps.condstore() {
            return Err(ImapError::Unsupported("CONDSTORE"));
        }
        let gmail = state.caps.gmail();
        let qresync = state.caps.qresync();
        let f = state.folder(&folder)?;
        let mut changes = ChangeSet {
            changed: f
                .messages
                .iter()
                .filter(|(_, m)| m.modseq > since_modseq)
                .map(|(uid, m)| m.flag_change(*uid, gmail))
                .collect(),
            vanished: UidSet::new(),
        };
        if qresync {
            for (uid, modseq) in &f.expunged {
                if *modseq > since_modseq {
                    changes.vanished.insert(*uid);
                }
            }
        }
        Ok(changes)
    }

    async fn fetch_flags(&mut self, uids: &UidSet) -> Result<Vec<FlagChange>> {
        let server = self.server.clone();
        let folder = self.selected()?;
        let state = self.begin(&server, "UID FETCH", &format!("{uids} FLAGS"))?;
        let gmail = state.caps.gmail();
        let f = state.folder(&folder)?;
        Ok(f.messages
            .iter()
            .filter(|(uid, _)| uids.contains(**uid))
            .map(|(uid, m)| m.flag_change(*uid, gmail))
            .collect())
    }

    async fn fetch_headers(&mut self, uids: &UidSet) -> Result<Vec<HeaderFetch>> {
        let server = self.server.clone();
        let folder = self.selected()?;
        let state = self.begin(&server, "UID FETCH", &format!("{uids} HEADERS"))?;
        let gmail = state.caps.gmail();
        let condstore = state.caps.condstore();
        let f = state.folder(&folder)?;
        Ok(f.messages
            .iter()
            .filter(|(uid, _)| uids.contains(**uid))
            .map(|(uid, m)| HeaderFetch {
                uid: *uid,
                flags: m.flags,
                keywords: m.keywords.clone(),
                size: u32::try_from(m.raw.len()).unwrap_or(u32::MAX),
                internal_date: m.internal_date,
                modseq: condstore.then_some(m.modseq),
                header: Sensitive::new(m.header()),
                body: Some(m.body_summary()),
                gmail: gmail.then(|| GmailMeta {
                    msg_id: m.gm_msgid,
                    thread_id: m.gm_thrid,
                    labels: m.labels.clone(),
                }),
            })
            .collect())
    }

    async fn fetch_part(&mut self, uid: u32, section: &str) -> Result<Bytes> {
        if section.is_empty() {
            return Err(ImapError::InvalidInput(
                "empty section; use fetch_raw for the whole message".into(),
            ));
        }
        self.fetch_body(uid, section)
    }

    async fn fetch_raw(&mut self, uid: u32) -> Result<Bytes> {
        self.fetch_body(uid, "")
    }

    async fn uid_search_all(&mut self) -> Result<UidSet> {
        let server = self.server.clone();
        let folder = self.selected()?;
        let state = self.begin(&server, "UID SEARCH", "ALL")?;
        Ok(state.folder(&folder)?.messages.keys().copied().collect())
    }

    async fn store_flags(&mut self, uids: &UidSet, op: &FlagOp) -> Result<()> {
        let server = self.server.clone();
        let folder = self.selected()?;
        for k in &op.keywords {
            validate_keyword(k)?;
        }
        let mut state = self.begin(&server, "UID STORE", &format!("{uids} {:?}", op.mode))?;
        self.sync_events(&state);
        let modseq = state.modseq + 1;
        let f = state.folder_mut(&folder)?;
        let mut changed = false;
        for (uid, m) in f.messages.iter_mut() {
            if uids.contains(*uid) && apply_flags(m, op.mode, op.flags, &op.keywords) {
                m.modseq = modseq;
                changed = true;
            }
        }
        if changed {
            f.highest_modseq = modseq;
            state.modseq = modseq;
        }
        self.refresh_snapshot(&state);
        drop(state);
        if changed {
            self.server.notify();
        }
        Ok(())
    }

    async fn store_gmail_labels(&mut self, uids: &UidSet, op: &LabelOp) -> Result<()> {
        let server = self.server.clone();
        let folder = self.selected()?;
        let mut state = self.begin(
            &server,
            "UID STORE",
            &format!("{uids} X-GM-LABELS {:?}", op.mode),
        )?;
        if !state.caps.gmail() {
            return Err(ImapError::Unsupported("X-GM-EXT-1"));
        }
        self.sync_events(&state);
        let modseq = state.modseq + 1;
        let f = state.folder_mut(&folder)?;
        for (uid, m) in f.messages.iter_mut().filter(|(u, _)| uids.contains(**u)) {
            let before = m.labels.clone();
            match op.mode {
                StoreMode::Set => m.labels = op.labels.clone(),
                StoreMode::Add => {
                    for l in &op.labels {
                        if !m.labels.contains(l) {
                            m.labels.push(l.clone());
                        }
                    }
                }
                StoreMode::Remove => m.labels.retain(|l| !op.labels.contains(l)),
            }
            if m.labels != before {
                m.modseq = modseq;
                f.highest_modseq = modseq;
                tracing::trace!(uid, "fake label change");
            }
        }
        state.modseq = modseq;
        self.refresh_snapshot(&state);
        drop(state);
        self.server.notify();
        Ok(())
    }

    async fn r#move(&mut self, uids: &UidSet, to: &str) -> Result<Option<CopyUid>> {
        let server = self.server.clone();
        let folder = self.selected()?;
        let has_move = {
            let state = self.begin(&server, "UID MOVE", &format!("{uids} {to}"))?;
            self.sync_events(&state);
            state.caps.has_move()
        };
        let copy = if has_move {
            self.copy_uids(&folder, uids, to, true)?
        } else {
            // Emulate the client's fallback: COPY, \Deleted, UID EXPUNGE (UIDPLUS).
            let copy = self.copy_uids(&folder, uids, to, false)?;
            let uidplus = self.server.lock().caps.uidplus();
            if uidplus {
                self.remove_uids(&folder, uids)?;
            } else {
                let mut state = self.server.lock();
                let f = state.folder_mut(&folder)?;
                for (_, m) in f.messages.iter_mut().filter(|(u, _)| uids.contains(**u)) {
                    m.flags |= Flags::DELETED;
                }
            }
            copy
        };
        let state = server.lock();
        let snapshot_before = self.snapshot.modseq;
        self.sync_events(&state);
        // Flag changes caused by this move are not news.
        self.events.retain(
            |e| !matches!(e, SessionEvent::Flags { modseq: Some(m), .. } if *m > snapshot_before),
        );
        drop(state);
        Ok(copy)
    }

    async fn copy(&mut self, uids: &UidSet, to: &str) -> Result<Option<CopyUid>> {
        let server = self.server.clone();
        let folder = self.selected()?;
        {
            let state = self.begin(&server, "UID COPY", &format!("{uids} {to}"))?;
            self.sync_events(&state);
        }
        self.copy_uids(&folder, uids, to, false)
    }

    async fn expunge_uids(&mut self, uids: &UidSet) -> Result<()> {
        let server = self.server.clone();
        let folder = self.selected()?;
        let deleted: UidSet = {
            let state = self.begin(&server, "UID EXPUNGE", &uids.to_string())?;
            if !state.caps.uidplus() {
                return Err(ImapError::Unsupported("UIDPLUS"));
            }
            state
                .folder(&folder)?
                .messages
                .iter()
                .filter(|(u, m)| uids.contains(**u) && m.flags.contains(Flags::DELETED))
                .map(|(u, _)| *u)
                .collect()
        };
        self.remove_uids(&folder, &deleted)?;
        let state = server.lock();
        self.sync_events(&state);
        drop(state);
        self.server.notify();
        Ok(())
    }

    async fn append(
        &mut self,
        folder: &str,
        raw: &[u8],
        flags: Flags,
        keywords: &[String],
    ) -> Result<Option<AppendUid>> {
        let server = self.server.clone();
        let mut state = self.begin(&server, "APPEND", folder)?;
        let uidplus = state.caps.uidplus();
        let (uidvalidity, uid) = state.insert(
            folder,
            FakeMessage::new(Bytes::copy_from_slice(raw))
                .flags(flags)
                .keywords(keywords.iter().cloned()),
        )?;
        drop(state);
        self.server.notify();
        Ok(uidplus.then_some(AppendUid { uidvalidity, uid }))
    }

    async fn create(&mut self, folder: &str) -> Result<()> {
        let server = self.server.clone();
        let mut state = self.begin(&server, "CREATE", folder)?;
        if state.folders.contains_key(&canonical(folder)) {
            return Err(no("ALREADYEXISTS", "folder exists"));
        }
        let mut new = state.new_folder(None, Vec::new());
        new.subscribed = false;
        state.folders.insert(canonical(folder), new);
        Ok(())
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let server = self.server.clone();
        let mut state = self.begin(&server, "RENAME", &format!("{from} {to}"))?;
        if state.folders.contains_key(&canonical(to)) {
            return Err(no("ALREADYEXISTS", "target exists"));
        }
        let folder = state
            .folders
            .remove(&canonical(from))
            .ok_or_else(|| no("NONEXISTENT", "no such folder"))?;
        // Children move along with their parent.
        let prefix = format!("{from}/");
        let children: Vec<String> = state
            .folders
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        for child in children {
            if let Some(f) = state.folders.remove(&child) {
                state
                    .folders
                    .insert(format!("{to}/{}", &child[prefix.len()..]), f);
            }
        }
        state.folders.insert(canonical(to), folder);
        drop(state);
        if self.selected.as_deref() == Some(from) {
            self.selected = None;
        }
        Ok(())
    }

    async fn delete(&mut self, folder: &str) -> Result<()> {
        let server = self.server.clone();
        let mut state = self.begin(&server, "DELETE", folder)?;
        if canonical(folder) == "INBOX" {
            return Err(no("CANNOT", "INBOX cannot be deleted"));
        }
        state
            .folders
            .remove(&canonical(folder))
            .ok_or_else(|| no("NONEXISTENT", "no such folder"))?;
        drop(state);
        if self.selected.as_deref() == Some(folder) {
            self.selected = None;
        }
        Ok(())
    }

    async fn subscribe(&mut self, folder: &str, on: bool) -> Result<()> {
        let server = self.server.clone();
        let verb = if on { "SUBSCRIBE" } else { "UNSUBSCRIBE" };
        let mut state = self.begin(&server, verb, folder)?;
        state.folder_mut(folder)?.subscribed = on;
        Ok(())
    }

    async fn status(&mut self, folder: &str) -> Result<FolderStatus> {
        let server = self.server.clone();
        let state = self.begin(&server, "STATUS", folder)?;
        let condstore = state.caps.condstore();
        let f = state.folder(folder)?;
        let count = |pred: &dyn Fn(&Stored) -> bool| {
            u32::try_from(f.messages.values().filter(|m| pred(m)).count()).unwrap_or(u32::MAX)
        };
        Ok(FolderStatus {
            messages: Some(count(&|_| true)),
            unseen: Some(count(&|m| !m.flags.contains(Flags::SEEN))),
            uidnext: Some(f.uidnext),
            uidvalidity: Some(f.uidvalidity),
            highest_modseq: condstore.then_some(f.highest_modseq),
        })
    }

    async fn idle(&mut self, cancel: CancellationToken, timeout: Duration) -> Result<IdleOutcome> {
        let server = self.server.clone();
        self.selected()?;
        {
            let state = self.begin(&server, "IDLE", "")?;
            if !state.caps.idle() {
                return Err(ImapError::Unsupported("IDLE"));
            }
        }
        if cancel.is_cancelled() {
            return Ok(IdleOutcome::Cancelled);
        }
        self.changes.mark_unchanged();
        {
            let state = server.lock();
            self.sync_events(&state);
        }
        if self.has_change_events() {
            return Ok(IdleOutcome::NewData);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(IdleOutcome::Cancelled),
                () = tokio::time::sleep_until(deadline) => return Ok(IdleOutcome::Timeout),
                changed = self.changes.changed() => {
                    if changed.is_err() {
                        self.broken = true;
                        return Err(ImapError::ConnectionLost(None));
                    }
                    {
                        let state = server.lock();
                        self.sync_events(&state);
                    }
                    if self.has_change_events() {
                        return Ok(IdleOutcome::NewData);
                    }
                }
            }
        }
    }

    async fn noop(&mut self) -> Result<()> {
        let server = self.server.clone();
        let state = self.begin(&server, "NOOP", "")?;
        self.sync_events(&state);
        Ok(())
    }

    fn take_events(&mut self) -> Vec<SessionEvent> {
        std::mem::take(&mut self.events)
    }

    async fn logout(&mut self) -> Result<()> {
        let server = self.server.clone();
        if !self.broken {
            let _state = self.begin(&server, "LOGOUT", "")?;
        }
        self.broken = true;
        self.selected = None;
        Ok(())
    }
}
