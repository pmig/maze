//! [`CoreMailSource`]: the mail UI's [`MailSource`] over [`CoreHandle`].
//!
//! - **Folders**: [`CoreHandle::folders`] (unified folders, favorites, each account's
//!   tree) with their unread counts, cached for the folder pane's badges and refreshed
//!   on change events.
//! - **Lists**: [`CoreHandle::thread_list`] for folders and unified folders (keys only),
//!   [`CoreHandle::quick_filter_body`] when the Quick Filter has text, and
//!   [`CoreHandle::search`] for global search; with threads off
//!   [`CoreHandle::message_list`] and [`CoreHandle::message_quick_filter_body`]. Rows
//!   come from [`CoreHandle::rows`] and [`CoreHandle::message_rows`] in the context
//!   folder of the open list; a thread row's Correspondents, recipients, size and tags
//!   are those of its newest message. Expanded threads use [`CoreHandle::thread_tree`]
//!   and "expand all" [`CoreHandle::thread_message_counts`].
//! - **Messages**: [`CoreHandle::message`], with the reader document rendered as escaped
//!   Markdown ([`reader_text::display_markdown`]) that the view draws natively; remote
//!   content is never loaded.
//! - **Changes**: flags, archive and move go through [`CoreHandle::apply`] with
//!   [`Origin::User`] (undoable), applied before the call returns, so the list shows
//!   the new state at once.
//! - **Live updates**: [`CoreMailSource::changes`] turns a [`CoreEventBatch`] into the
//!   window's [`DataChanges`].
//!
//! The UI is synchronous and runs on GPUI's thread; the core's async calls are run on
//! the core's own runtime and waited for ([`WAIT_TIMEOUT`] at most), which is cheap
//! for local reads and writes.
//!
//! The source can exist before the core does ([`CoreMailSource::opening`]), so the
//! window opens while the key is read and the store opens (PLAN §4.17);
//! [`CoreMailSource::attach`] makes the mail appear.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Mutex, PoisonError, RwLock};
use std::time::Duration;

use gpui::SharedString;
use mail_ui::DataChanges;
use mail_ui::source::{
    AccountEntry, AccountKind, AttachmentInfo, FolderEntry, FolderKey, FolderTree, ListSort,
    MailSource, MessageBody, MessageDocument, MessageKeys, MessageRow, SenderClient, SourceError,
    SourceStatus, SyncState, ThreadFilter, ThreadKeys, ThreadRow, ThreadScope, ThreadTree,
    TreeNode, Verdict, Verification,
};
use mazemail_core::correspondents::display;
use mazemail_core::{
    AccountInfo, AuthStatus, Body, CoreEventBatch, CoreHandle, FolderNode, FolderNodeKind,
    ListQuery, ListScope, MailCommand, MessageListRow, MessageView, Origin, QuickFilter,
    SearchOpts, SearchSort, SortOrder, SyncState as CoreSyncState, reader_text,
};
use mazemail_types::{AccountId, Address, Flags, FolderId, MessageId, Sensitive, TagId, ThreadId};
use time::OffsetDateTime;

/// How long a UI call waits for the core.
pub const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Threads a global search shows at most.
pub const SEARCH_LIMIT: usize = 2_000;

/// What the list shows before any account exists.
pub const NO_ACCOUNTS: &str = "No accounts yet. Add your IMAP, Gmail or Microsoft account, or \
     import mail with `mazemail import <file.mbox>`.";

enum State {
    Opening,
    Ready(CoreHandle),
    Failed(SharedString),
}

/// The mail UI's data over the core. See the [module docs](self).
pub struct CoreMailSource {
    state: RwLock<State>,
    /// The context folder of the list opened last ([`CoreHandle::rows`] shows dates and
    /// counts of that folder).
    context: Mutex<Option<FolderId>>,
    /// Unread counts of the folder pane entries, from the last folder tree.
    counts: RwLock<HashMap<FolderKey, u64>>,
    /// The last folder tree, to tell structure changes from count changes.
    tree: Mutex<Option<FolderTree>>,
    /// Account and tag names for the table's columns, read with the folder tree.
    names: RwLock<Names>,
}

/// Account and tag names by id.
#[derive(Debug, Default)]
struct Names {
    accounts: HashMap<AccountId, SharedString>,
    tags: HashMap<TagId, SharedString>,
}

impl std::fmt::Debug for CoreMailSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match &*read(&self.state) {
            State::Opening => "opening",
            State::Ready(_) => "ready",
            State::Failed(_) => "failed",
        };
        f.debug_struct("CoreMailSource")
            .field("state", &state)
            .finish_non_exhaustive()
    }
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unavailable(error: impl std::fmt::Display) -> SourceError {
    SourceError::Unavailable(error.to_string())
}

/// Runs `future` on the core's runtime and waits for it from a non-Tokio thread.
fn wait<T: Send + 'static>(
    core: &CoreHandle,
    future: impl Future<Output = T> + Send + 'static,
) -> Result<T, SourceError> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    core.runtime().spawn(async move {
        let _ = tx.send(future.await);
    });
    rx.recv_timeout(WAIT_TIMEOUT).map_err(|_| {
        unavailable(format!(
            "{} didn't answer in time",
            mazemail_types::PRODUCT_NAME
        ))
    })
}

impl CoreMailSource {
    /// A source whose core is still opening: the window shows "Opening Maze…".
    pub fn opening() -> Self {
        Self::with_state(State::Opening)
    }

    /// A source over an open core.
    pub fn new(core: CoreHandle) -> Self {
        Self::with_state(State::Ready(core))
    }

    fn with_state(state: State) -> Self {
        Self {
            state: RwLock::new(state),
            context: Mutex::new(None),
            counts: RwLock::new(HashMap::new()),
            tree: Mutex::new(None),
            names: RwLock::new(Names::default()),
        }
    }

    /// The core opened: show its mail. Follow with
    /// [`MailWindow::apply_changes`](mail_ui::MailWindow::apply_changes) with
    /// [`DataChanges::everything`].
    pub fn attach(&self, core: CoreHandle) {
        *write(&self.state) = State::Ready(core);
    }

    /// The core couldn't be opened: show `message` instead of mail.
    pub fn fail(&self, message: impl Into<SharedString>) {
        *write(&self.state) = State::Failed(message.into());
    }

    /// The core, once it is open.
    pub fn core(&self) -> Option<CoreHandle> {
        match &*read(&self.state) {
            State::Ready(core) => Some(core.clone()),
            State::Opening | State::Failed(_) => None,
        }
    }

    fn ready(&self) -> Result<CoreHandle, SourceError> {
        self.core().ok_or_else(|| match &*read(&self.state) {
            State::Failed(message) => unavailable(message),
            _ => unavailable(format!("{} is still opening", mazemail_types::PRODUCT_NAME)),
        })
    }

    /// What `batch` means for the window. Folder changes re-read the folder tree here
    /// (refreshing the cached counts) to tell a structure change, which rebuilds the
    /// pane, from a count change, which only re-renders its badges.
    pub fn changes(&self, batch: &CoreEventBatch) -> DataChanges {
        let folders_touched = !batch.folders_changed.is_empty()
            || !batch.counts.is_empty()
            || !batch.accounts_changed.is_empty()
            || batch.settings;
        let mut folders = false;
        if folders_touched || batch.reload {
            let before = lock(&self.tree).clone();
            let after = self.folder_tree();
            folders = before.as_ref() != Some(&after);
        }
        DataChanges {
            folders,
            counts: folders_touched,
            threads: !batch.threads_changed.is_empty() || !batch.removed.is_empty(),
            threads_updated: batch.threads_updated.clone(),
            messages_updated: batch.messages_changed.clone(),
            status: !batch.sync_status.is_empty() || !batch.accounts_changed.is_empty(),
            everything: batch.reload,
        }
    }

    /// The message ids of `threads`.
    fn messages_of(core: &CoreHandle, threads: &[ThreadId]) -> Result<Vec<MessageId>, SourceError> {
        let mut ids = Vec::new();
        for thread in threads {
            ids.extend(core.thread_message_ids(*thread).map_err(unavailable)?);
        }
        Ok(ids)
    }

    fn apply(
        &self,
        command: impl FnOnce(Vec<MessageId>) -> MailCommand,
        threads: &[ThreadId],
    ) -> Result<(), SourceError> {
        let core = self.ready()?;
        let ids = Self::messages_of(&core, threads)?;
        if ids.is_empty() {
            return Ok(());
        }
        let command = command(ids);
        let runner = core.clone();
        wait(
            &core,
            async move { runner.apply(command, Origin::User).await },
        )?
        .map(drop)
        .map_err(unavailable)
    }
}

impl CoreMailSource {
    fn apply_to_messages(
        &self,
        command: MailCommand,
        ids: &[MessageId],
    ) -> Result<(), SourceError> {
        if ids.is_empty() {
            return Ok(());
        }
        let core = self.ready()?;
        let runner = core.clone();
        wait(
            &core,
            async move { runner.apply(command, Origin::User).await },
        )?
        .map(drop)
        .map_err(unavailable)
    }

    /// The threads a global search finds for the UI's filter.
    fn search_threads(
        core: &CoreHandle,
        filter: &ThreadFilter,
        sort: ListSort,
    ) -> Result<Vec<ThreadId>, SourceError> {
        let query = search_query(filter);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let opts = SearchOpts {
            limit: SEARCH_LIMIT,
            sort: match sort {
                ListSort::NewestFirst => SearchSort::NewestFirst,
                ListSort::OldestFirst => SearchSort::OldestFirst,
            },
            snippets: false,
            ..SearchOpts::default()
        };
        let runner = core.clone();
        let results =
            wait(core, async move { runner.search(&query, opts).await })?.map_err(unavailable)?;
        Ok(results.threads.iter().map(|t| t.thread).collect())
    }
}

/// The core's list query for a folder and the UI's filter.
fn list_query(key: &FolderKey, filter: &ThreadFilter, sort: ListSort) -> ListQuery {
    let scope = match key {
        FolderKey::Folder(id) => ListScope::Folder(*id),
        FolderKey::Unified(role) => ListScope::Unified(*role),
    };
    ListQuery {
        scope,
        sort: sort_order(sort),
        quick_filter: QuickFilter {
            unread: filter.unread,
            flagged: filter.starred,
            has_attachment: filter.attachments,
            tag: None,
            text: filter
                .text
                .clone()
                .filter(|t| !t.trim().is_empty())
                .map(Sensitive::new),
        },
    }
}

fn account_kind(kind: mazemail_core::AccountKind) -> AccountKind {
    match kind {
        mazemail_core::AccountKind::Gmail => AccountKind::Gmail,
        mazemail_core::AccountKind::M365 => AccountKind::Microsoft,
        mazemail_core::AccountKind::Local => AccountKind::Local,
        _ => AccountKind::Imap,
    }
}

/// A folder pane entry for a unified folder or folder node, recording its count.
fn entry(node: &FolderNode, counts: &mut HashMap<FolderKey, u64>) -> Option<FolderEntry> {
    let key = match node.kind {
        FolderNodeKind::Unified(role) => FolderKey::Unified(role),
        FolderNodeKind::Folder(id) => FolderKey::Folder(id),
        FolderNodeKind::UnifiedSection
        | FolderNodeKind::FavoritesSection
        | FolderNodeKind::Account(_) => return None,
    };
    counts.insert(key.clone(), node.unread);
    Some(FolderEntry {
        key,
        name: node.name.clone().into(),
        role: node.role,
        children: node
            .children
            .iter()
            .filter_map(|child| entry(child, counts))
            .collect(),
    })
}

fn timestamp(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn subject(subject: Option<String>) -> Sensitive<SharedString> {
    Sensitive::new(
        subject
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "(no subject)".to_owned())
            .into(),
    )
}

fn sort_order(sort: ListSort) -> SortOrder {
    match sort {
        ListSort::NewestFirst => SortOrder::NewestFirst,
        ListSort::OldestFirst => SortOrder::OldestFirst,
    }
}

fn recipients(addresses: &[Address]) -> String {
    addresses.iter().map(display).collect::<Vec<_>>().join(", ")
}

impl Names {
    fn account(&self, id: AccountId) -> SharedString {
        self.accounts.get(&id).cloned().unwrap_or_default()
    }

    fn tags(&self, ids: &[TagId]) -> Vec<SharedString> {
        ids.iter()
            .filter_map(|id| self.tags.get(id).cloned())
            .collect()
    }
}

/// A thread row, with what its newest message adds (Correspondents, recipients, size).
fn row(row: mazemail_core::ThreadRow, newest: Option<&MessageListRow>, names: &Names) -> ThreadRow {
    // Participants are the distinct senders, oldest first: the last is the newest.
    let participant = row
        .participants
        .last()
        .map(|p| display(&Address::new(p.name.clone(), p.email.clone())))
        .unwrap_or_default();
    let sender = newest
        .and_then(|m| m.row.from.as_ref())
        .map_or(participant, display);
    let mut flags = Flags::empty();
    if row.unread_count == 0 {
        flags |= Flags::SEEN;
    }
    if row.flagged {
        flags |= Flags::FLAGGED;
    }
    ThreadRow {
        id: row.id,
        subject: subject(row.subject),
        sender: Sensitive::new(sender.into()),
        recipients: Sensitive::new(
            newest
                .map(|m| m.correspondents_or_recipients())
                .unwrap_or_default()
                .into(),
        ),
        outgoing: newest.is_some_and(|m| m.outgoing),
        date: timestamp(row.date),
        flags,
        has_attachments: row.has_attachments,
        message_count: row.message_count,
        size: newest.and_then(|m| m.row.size).unwrap_or(0),
        account: names.account(row.account),
        tags: names.tags(&row.tags),
    }
}

/// The recipients text of a message row.
trait RecipientsText {
    fn correspondents_or_recipients(&self) -> String;
}

impl RecipientsText for MessageListRow {
    fn correspondents_or_recipients(&self) -> String {
        let to = if self.row.to.is_empty() {
            &self.row.cc
        } else {
            &self.row.to
        };
        recipients(to)
    }
}

fn message_row(row: MessageListRow, names: &Names) -> MessageRow {
    let recipients_text = row.correspondents_or_recipients();
    let outgoing = row.outgoing;
    let m = row.row;
    MessageRow {
        id: m.id,
        thread: m.thread,
        subject: subject(m.subject),
        sender: Sensitive::new(m.from.as_ref().map(display).unwrap_or_default().into()),
        recipients: Sensitive::new(recipients_text.into()),
        outgoing,
        date: timestamp(m.date),
        flags: m.flags,
        has_attachments: m.has_attachments,
        size: m.size.unwrap_or(0),
        account: names.account(m.account),
        tags: names.tags(&m.tags),
    }
}

/// A message view as the message pane shows it.
fn document(view: &MessageView) -> MessageDocument {
    let (body, attachments) = match &view.body {
        Body::Ready(rendered) => (
            MessageBody::Markdown(Sensitive::new(
                reader_text::display_markdown(&rendered.reader).into(),
            )),
            rendered
                .attachments
                .iter()
                .filter(|a| a.counts_as_attachment())
                .map(|a| AttachmentInfo {
                    name: Sensitive::new(
                        a.filename
                            .clone()
                            .unwrap_or_else(|| "Unnamed attachment".to_owned())
                            .into(),
                    ),
                    size: u64::try_from(a.size).unwrap_or(u64::MAX),
                })
                .collect(),
        ),
        Body::Pending => (
            MessageBody::PlainText(Sensitive::new(
                "This message hasn't been downloaded yet.".into(),
            )),
            Vec::new(),
        ),
        Body::Unreadable => (
            MessageBody::PlainText(Sensitive::new("This message can't be displayed.".into())),
            Vec::new(),
        ),
    };
    MessageDocument {
        thread: view.thread,
        message: view.id,
        subject: subject(view.subject.clone()),
        from: Sensitive::new(
            view.from
                .clone()
                .unwrap_or_else(|| Address::new(None::<String>, "")),
        ),
        to: Sensitive::new(view.to.clone()),
        cc: Sensitive::new(view.cc.clone()),
        date: timestamp(view.date),
        body,
        attachments,
        verification: verification(&view.auth),
        client: Sensitive::new(view.rendered().and_then(|rendered| {
            let client = rendered.mail_client.as_ref()?;
            Some(SenderClient {
                name: client.name.clone().into(),
                header: format!("{}: {}", client.header, client.raw).into(),
            })
        })),
    }
}

/// The verification row's data from the core's sender verification.
fn verification(auth: &AuthStatus) -> Verification {
    match auth {
        AuthStatus::Pending => Verification::Pending,
        AuthStatus::Unavailable => Verification::Unavailable,
        AuthStatus::Ready(report) => Verification::Checked {
            verdict: match report.verdict {
                security::Verdict::Verified => Verdict::Verified,
                security::Verdict::Unverified => Verdict::Unverified,
                security::Verdict::Suspicious => Verdict::Suspicious,
            },
            dkim: report.dkim.result.label().into(),
            spf: report.spf.result.label().into(),
            dmarc: report.dmarc.result.label().into(),
            explanation: report.explanation.clone().into(),
        },
    }
}

/// The status bar's sync state over all synced accounts: progress of a syncing account
/// first, then problems (sign-in, errors, offline), then "Connecting…", else up to date.
pub fn status_of(accounts: &[AccountInfo]) -> SourceStatus {
    let synced: Vec<(&AccountInfo, &mazemail_core::SyncStatus)> = accounts
        .iter()
        .filter_map(|a| a.sync.as_ref().map(|s| (a, s)))
        .collect();
    let online = synced
        .iter()
        .any(|(_, s)| matches!(s.state, CoreSyncState::Idle | CoreSyncState::Syncing { .. }));
    let problem = |state: &CoreSyncState| {
        matches!(
            state,
            CoreSyncState::AuthRequired | CoreSyncState::Error { .. } | CoreSyncState::Offline
        )
    };
    let sync = if synced.is_empty() {
        SyncState::NoAccounts
    } else if let Some((account, done, total)) = synced.iter().find_map(|(a, s)| match s.state {
        CoreSyncState::Syncing { done, total, .. } => Some((a, done, total.unwrap_or(done))),
        _ => None,
    }) {
        SyncState::Syncing {
            account: account.name.clone().into(),
            done,
            total,
        }
    } else if let Some((account, status)) = synced.iter().find(|(_, s)| problem(&s.state)) {
        SyncState::Message {
            text: format!("{}: {}", account.name, status.text()).into(),
            problem: true,
        }
    } else if let Some((_, status)) = synced
        .iter()
        .find(|(_, s)| s.state == CoreSyncState::Connecting)
    {
        SyncState::Message {
            text: status.text().into(),
            problem: false,
        }
    } else {
        SyncState::Idle
    };
    SourceStatus {
        sync,
        outbox: 0,
        online,
    }
}

/// The global search query for the UI's filter: its text plus the toggles as
/// operators.
fn search_query(filter: &ThreadFilter) -> String {
    let mut query = filter.text.clone().unwrap_or_default();
    for (on, operator) in [
        (filter.unread, "is:unread"),
        (filter.starred, "is:flagged"),
        (filter.attachments, "has:attachment"),
    ] {
        if on {
            query.push(' ');
            query.push_str(operator);
        }
    }
    query.trim().to_owned()
}

impl MailSource for CoreMailSource {
    fn folder_tree(&self) -> FolderTree {
        let Ok(core) = self.ready() else {
            return FolderTree::default();
        };
        let (nodes, accounts) = match (core.folders(), core.accounts()) {
            (Ok(nodes), Ok(accounts)) => (nodes, accounts),
            (Err(error), _) | (_, Err(error)) => {
                tracing::warn!(%error, "cannot read the folder tree");
                return FolderTree::default();
            }
        };
        let kinds: HashMap<AccountId, AccountKind> = accounts
            .iter()
            .map(|a| (a.id, account_kind(a.kind)))
            .collect();
        let mut counts = HashMap::new();
        let mut tree = FolderTree::default();
        for node in &nodes {
            match node.kind {
                FolderNodeKind::UnifiedSection => {
                    tree.unified = node
                        .children
                        .iter()
                        .filter_map(|c| entry(c, &mut counts))
                        .collect();
                }
                FolderNodeKind::FavoritesSection => {
                    tree.favorites = node
                        .children
                        .iter()
                        .filter_map(|c| entry(c, &mut counts))
                        .collect();
                }
                FolderNodeKind::Account(id) => tree.accounts.push(AccountEntry {
                    id,
                    name: node.name.clone().into(),
                    kind: kinds.get(&id).copied().unwrap_or(AccountKind::Imap),
                    folders: node
                        .children
                        .iter()
                        .filter_map(|c| entry(c, &mut counts))
                        .collect(),
                }),
                FolderNodeKind::Unified(_) | FolderNodeKind::Folder(_) => {}
            }
        }
        *write(&self.counts) = counts;
        *lock(&self.tree) = Some(tree.clone());
        let tags = core.tags().unwrap_or_default();
        *write(&self.names) = Names {
            accounts: accounts
                .iter()
                .map(|a| (a.id, SharedString::from(a.name.clone())))
                .collect(),
            tags: tags
                .into_iter()
                .map(|t| (t.id, SharedString::from(t.name)))
                .collect(),
        };
        tree
    }

    fn unread_count(&self, folder: &FolderKey) -> u64 {
        read(&self.counts).get(folder).copied().unwrap_or(0)
    }

    fn thread_keys(
        &self,
        scope: &ThreadScope,
        filter: &ThreadFilter,
        sort: ListSort,
    ) -> Result<ThreadKeys, SourceError> {
        let core = self.ready()?;
        match scope {
            ThreadScope::Folder(key) => {
                let query = list_query(key, filter, sort);
                let snapshot = if query.quick_filter.text.is_some() {
                    let runner = core.clone();
                    wait(&core, async move { runner.quick_filter_body(&query).await })?
                } else {
                    core.thread_list(&query)
                }
                .map_err(unavailable)?;
                *lock(&self.context) = snapshot.context();
                Ok(snapshot.keys().iter().map(|k| k.thread).collect())
            }
            ThreadScope::AllFolders => {
                *lock(&self.context) = None;
                Ok(Self::search_threads(&core, filter, sort)?.into())
            }
        }
    }

    fn thread_rows(&self, keys: &[ThreadId]) -> Vec<ThreadRow> {
        let Ok(core) = self.ready() else {
            return Vec::new();
        };
        let context = *lock(&self.context);
        let rows = match core.rows(keys, context) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "cannot read thread rows");
                return Vec::new();
            }
        };
        // The newest messages give Correspondents, recipients and sizes.
        let newest_ids: Vec<MessageId> = rows.iter().filter_map(|r| r.last_message).collect();
        let newest: HashMap<MessageId, MessageListRow> = core
            .message_rows(&newest_ids, context)
            .unwrap_or_default()
            .into_iter()
            .map(|m| (m.row.id, m))
            .collect();
        let names = read(&self.names);
        rows.into_iter()
            .map(|r| {
                let last = r.last_message.and_then(|id| newest.get(&id));
                row(r, last, &names)
            })
            .collect()
    }

    fn message_keys(
        &self,
        scope: &ThreadScope,
        filter: &ThreadFilter,
        sort: ListSort,
    ) -> Result<MessageKeys, SourceError> {
        let core = self.ready()?;
        match scope {
            ThreadScope::Folder(key) => {
                let query = list_query(key, filter, sort);
                let snapshot = if query.quick_filter.text.is_some() {
                    let runner = core.clone();
                    wait(&core, async move {
                        runner.message_quick_filter_body(&query).await
                    })?
                } else {
                    core.message_list(&query)
                }
                .map_err(unavailable)?;
                *lock(&self.context) = snapshot.context();
                Ok(snapshot.keys().iter().map(|k| k.message).collect())
            }
            ThreadScope::AllFolders => {
                // Search finds threads; with threads off, list their messages by date.
                *lock(&self.context) = None;
                let threads = Self::search_threads(&core, filter, sort)?;
                let mut ids = Vec::new();
                for thread in threads {
                    ids.extend(core.thread_message_ids(thread).map_err(unavailable)?);
                }
                let mut dated: Vec<(i64, MessageId)> = Vec::with_capacity(ids.len());
                for chunk in ids.chunks(1_000) {
                    dated.extend(
                        core.message_rows(chunk, None)
                            .map_err(unavailable)?
                            .into_iter()
                            .map(|m| (m.row.date, m.row.id)),
                    );
                }
                match sort {
                    ListSort::NewestFirst => dated.sort_by_key(|k| std::cmp::Reverse(*k)),
                    ListSort::OldestFirst => dated.sort_unstable(),
                }
                Ok(dated.into_iter().map(|(_, id)| id).collect())
            }
        }
    }

    fn message_rows(&self, keys: &[MessageId]) -> Vec<MessageRow> {
        let Ok(core) = self.ready() else {
            return Vec::new();
        };
        let context = *lock(&self.context);
        match core.message_rows(keys, context) {
            Ok(rows) => {
                let names = read(&self.names);
                rows.into_iter().map(|r| message_row(r, &names)).collect()
            }
            Err(error) => {
                tracing::warn!(%error, "cannot read message rows");
                Vec::new()
            }
        }
    }

    fn thread_tree(&self, thread: ThreadId) -> Result<ThreadTree, SourceError> {
        let core = self.ready()?;
        let context = *lock(&self.context);
        Ok(core
            .thread_tree(thread, context)
            .map_err(unavailable)?
            .into_iter()
            .map(|n| TreeNode {
                message: n.message,
                parent: n.parent,
                depth: n.depth,
            })
            .collect())
    }

    fn message_counts(&self, threads: &[ThreadId]) -> Vec<u32> {
        let Ok(core) = self.ready() else {
            return vec![0; threads.len()];
        };
        let context = *lock(&self.context);
        core.thread_message_counts(threads, context)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot read message counts");
                vec![0; threads.len()]
            })
    }

    fn message(&self, thread: ThreadId) -> Result<MessageDocument, SourceError> {
        let core = self.ready()?;
        let context = *lock(&self.context);
        let newest = core
            .rows(&[thread], context)
            .map_err(unavailable)?
            .into_iter()
            .next()
            .and_then(|row| row.last_message)
            .ok_or(SourceError::ThreadNotFound(thread))?;
        let runner = core.clone();
        let view =
            wait(&core, async move { runner.message(newest).await })?.map_err(unavailable)?;
        Ok(document(&view))
    }

    fn message_document(&self, message: MessageId) -> Result<MessageDocument, SourceError> {
        let core = self.ready()?;
        let runner = core.clone();
        let view = wait(&core, async move { runner.message(message).await })?
            .map_err(|_| SourceError::MessageNotFound(message))?;
        Ok(document(&view))
    }

    fn set_flags(
        &self,
        threads: &[ThreadId],
        add: Flags,
        remove: Flags,
    ) -> Result<(), SourceError> {
        self.apply(|ids| MailCommand::SetFlags { ids, add, remove }, threads)
    }

    fn set_message_flags(
        &self,
        messages: &[MessageId],
        add: Flags,
        remove: Flags,
    ) -> Result<(), SourceError> {
        self.apply_to_messages(
            MailCommand::SetFlags {
                ids: messages.to_vec(),
                add,
                remove,
            },
            messages,
        )
    }

    fn archive(&self, threads: &[ThreadId]) -> Result<(), SourceError> {
        self.apply(|ids| MailCommand::Archive { ids }, threads)
    }

    fn archive_messages(&self, messages: &[MessageId]) -> Result<(), SourceError> {
        self.apply_to_messages(
            MailCommand::Archive {
                ids: messages.to_vec(),
            },
            messages,
        )
    }

    fn undo(&self) -> Result<bool, SourceError> {
        let core = self.ready()?;
        let runner = core.clone();
        let receipt = wait(&core, async move { runner.undo().await })?.map_err(unavailable)?;
        Ok(receipt.is_some())
    }

    fn thread_account(&self, thread: ThreadId) -> Option<AccountId> {
        let core = self.ready().ok()?;
        let row = core.rows(&[thread], None).ok()?.into_iter().next()?;
        Some(row.account)
    }

    fn message_account(&self, message: MessageId) -> Option<AccountId> {
        let core = self.ready().ok()?;
        let row = core
            .message_rows(&[message], None)
            .ok()?
            .into_iter()
            .next()?;
        Some(row.row.account)
    }

    fn move_threads(
        &self,
        threads: &[ThreadId],
        to: FolderId,
        from: Option<FolderId>,
    ) -> Result<(), SourceError> {
        self.apply(|ids| MailCommand::Move { ids, to, from }, threads)
    }

    fn move_messages(
        &self,
        messages: &[MessageId],
        to: FolderId,
        from: Option<FolderId>,
    ) -> Result<(), SourceError> {
        self.apply_to_messages(
            MailCommand::Move {
                ids: messages.to_vec(),
                to,
                from,
            },
            messages,
        )
    }

    fn status(&self) -> SourceStatus {
        let accounts = self
            .core()
            .and_then(|core| core.accounts_with_status().ok())
            .unwrap_or_default();
        status_of(&accounts)
    }

    fn offers_add_account(&self) -> bool {
        self.core()
            .is_some_and(|core| core.accounts().is_ok_and(|accounts| accounts.is_empty()))
    }

    fn placeholder(&self) -> Option<SharedString> {
        match &*read(&self.state) {
            State::Opening => Some(format!("Opening {}…", mazemail_types::PRODUCT_NAME).into()),
            State::Failed(message) => Some(message.clone()),
            State::Ready(core) => match core.accounts() {
                Ok(accounts) if accounts.is_empty() => Some(NO_ACCOUNTS.into()),
                Ok(_) => None,
                Err(error) => Some(format!("Mail can't be read: {error}").into()),
            },
        }
    }
}
