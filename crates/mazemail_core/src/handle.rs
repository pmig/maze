//! [`CoreHandle`]: opening, the query and command API, and shutdown.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use mazemail_types::{AccountId, FolderId, MessageId, PublicId, ThreadId};
use search::{DirectoryOptions, SearchIndex};
use security::phishing::{DomainWatchlist, PhishingInput, analyze, links_from_reader_doc};
use security::{ProviderProfile, Resolver, Verifier, VerifyContext};
use settings::{LoadedSettings, SettingsWatcher, default_settings};
use store::{
    Account, AccountKind, BodyState, ChangeSet, DataLayout, MessageDetail, MessageScope,
    NewProposalItem, Queries, Store, StoreConfig, ThreadRow,
};
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::{broadcast, mpsc, watch};
use vault::Vault;

use crate::api_handler::{Authorization, CommandAuthorizer};
use crate::commands::{
    self, CommandOutcome, CommandReceipt, ExecContext, MailCommand, Origin, UndoAction,
};
use crate::config::CoreConfig;
use crate::crash_guard::{CrashGuard, CrashGuardStatus};
use crate::events::{self, CoreEvent, CoreEventBatch};
use crate::folders::{self, FolderNode};
use crate::global_search::{self as core_search, SearchOpts, SearchResults};
use crate::indexing::{
    self, CoreIndexSink, IndexCheck, IndexCommand, IndexDocument, IndexerTask, ReindexReport,
};
use crate::lock::DataDirLock;
use crate::lost_key::LostKey;
use crate::message::{
    AuthStatus, Body, MessageView, RemoteContent, StoreContacts, load_rendered, persist_auth,
    stored_auth,
};
use crate::row_cache::{RowCache, RowCacheStats};
use crate::runtime::RuntimeHolder;
use crate::sync_bridge::{FetchPriority, SyncBridge, SyncStatusSink};
use crate::sync_engine::SyncEngine;
use crate::sync_ingest::CoreIngest;
use crate::thread_list::{ListQuery, TextCache, ThreadListSnapshot, all_keys};
use crate::{Error, Result};

/// Name of the index directory inside `mail/`.
const INDEX_DIR: &str = "index";

/// Messages per progress report during a rebuild.
const REINDEX_PROGRESS_EVERY: u64 = 500;

/// The headless Maze core: store, search, commands and events behind one cheap,
/// cloneable handle. See the [crate docs](crate).
#[derive(Clone)]
pub struct CoreHandle {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for CoreHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreHandle")
            .field("data_dir", &self.inner.data_dir)
            .field("closed", &self.inner.closed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

struct Inner {
    data_dir: PathBuf,
    runtime: Arc<RuntimeHolder>,
    store: Store,
    index: SearchIndex,
    indexer: IndexerTask,
    events_out: broadcast::Sender<CoreEventBatch>,
    events_in: mpsc::UnboundedSender<CoreEvent>,
    rows: Arc<RowCache>,
    text: Arc<TextCache>,
    settings: watch::Receiver<Arc<LoadedSettings>>,
    watcher: Mutex<Option<SettingsWatcher>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    guard: CrashGuard,
    lock: Mutex<Option<DataDirLock>>,
    undo: Mutex<Option<UndoAction>>,
    seq: AtomicU64,
    sync: Arc<dyn SyncBridge>,
    engine: Option<Arc<SyncEngine>>,
    authorizer: Arc<dyn CommandAuthorizer>,
    verifier: OnceLock<Option<Verifier>>,
    verifying: Mutex<HashSet<MessageId>>,
    watchlist: DomainWatchlist,
    utc_offset: UtcOffset,
    closed: AtomicBool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Without an explicit shutdown: stop the tasks; the indexer thread commits and
        // exits when its channel closes, the store's writer when the store is dropped.
        for task in lock(&self.tasks).drain(..) {
            task.abort();
        }
        drop(self.indexer.stop());
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn store_config(config: &CoreConfig) -> StoreConfig {
    let mut store = StoreConfig::new(&config.data_dir);
    store.app_version.clone_from(&config.app_version);
    store.readers = config.store_readers.max(1);
    store.gc_interval = config.blob_gc_interval;
    store
}

fn map_store(err: store::Error) -> Error {
    match err {
        store::Error::Invalid(reason) => Error::InvalidCommand(reason),
        store::Error::NotFound { entity, id } => Error::NotFound { entity, id },
        other => Error::Store(other),
    }
}

/// What the blocking part of startup found.
enum Startup {
    Ready {
        lock: DataDirLock,
        guard: CrashGuard,
        vault: Vault,
    },
    MissingKey {
        lock: DataDirLock,
        guard: CrashGuard,
    },
}

impl CoreHandle {
    /// Opens the core: locks the data directory (one instance per data directory),
    /// arms the crash guard, reads the master key, opens the store (running
    /// migrations), the encrypted search index and the settings, and starts the
    /// indexer, event batcher and change forwarding.
    ///
    /// The key is read on a blocking thread, so the caller (the UI creating its
    /// window, PLAN §4.17) keeps running meanwhile. Works from any executor.
    ///
    /// # Errors
    ///
    /// - [`Error::AlreadyRunning`] if another instance holds the data directory.
    /// - [`Error::MissingKeyForExistingData`] if encrypted data exists without its key;
    ///   it carries the [`LostKey`] recovery choices. Nothing is deleted.
    /// - [`Error::Store`] with [`store::Error::SchemaTooNew`] after a downgrade, or
    ///   [`store::Error::WrongKey`] if the key store holds a different key.
    pub async fn open(config: CoreConfig) -> Result<Self> {
        let runtime = Arc::new(RuntimeHolder::new(&config.runtime)?);
        let data_dir = config.data_dir.clone();
        let app_version = config.app_version.clone();
        let key_store = Arc::clone(&config.key_store);
        let startup = runtime
            .blocking(move || {
                let lock = DataDirLock::acquire(&data_dir)?;
                let guard = CrashGuard::arm(&data_dir, &app_version)?;
                let data_exists = DataLayout::new(&data_dir).data_exists();
                match Vault::open_or_create(&*key_store, data_exists) {
                    Ok(vault) => Ok(Startup::Ready { lock, guard, vault }),
                    Err(vault::Error::MissingKeyForExistingData) => {
                        Ok(Startup::MissingKey { lock, guard })
                    }
                    Err(err) => Err(err.into()),
                }
            })
            .await?;
        match startup {
            Startup::Ready { lock, guard, vault } => {
                finish(config, runtime, lock, guard, Arc::new(vault)).await
            }
            Startup::MissingKey { lock, guard } => {
                Err(Error::MissingKeyForExistingData(Box::new(LostKey {
                    config,
                    runtime,
                    lock,
                    guard,
                })))
            }
        }
    }

    fn ensure_open(&self) -> Result<()> {
        if self.inner.closed.load(Ordering::Acquire) {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }

    // ---- lifecycle ----------------------------------------------------------------

    /// The data directory.
    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    /// The core's Tokio runtime, for callers that spawn their own Tokio work (the UI
    /// bridge, `gpui_tokio`).
    pub fn runtime(&self) -> tokio::runtime::Handle {
        self.inner.runtime.handle().clone()
    }

    /// What the crash guard found at startup.
    pub fn crash_guard(&self) -> &CrashGuardStatus {
        self.inner.guard.status()
    }

    /// Whether to offer "Roll back to the previous version" (PLAN §4.15).
    pub fn should_offer_rollback(&self) -> bool {
        self.inner.guard.status().should_offer_rollback()
    }

    /// Call once the first window has rendered and the core works: removes the
    /// startup sentinel.
    pub fn mark_healthy(&self) -> Result<()> {
        self.inner.guard.disarm()
    }

    /// Stops the tasks, commits the index, closes the store, releases the data
    /// directory and stops an owned runtime. Other clones of the handle get
    /// [`Error::Closed`] afterwards. Idempotent.
    pub async fn shutdown(&self) -> Result<()> {
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let inner = &self.inner;
        // Sync first: actors finish their batch and log out while the store is open.
        if let Some(engine) = &inner.engine {
            engine.shutdown().await;
        }
        let tasks: Vec<_> = lock(&inner.tasks).drain(..).collect();
        for task in tasks {
            task.abort();
        }
        drop(lock(&inner.watcher).take());
        let indexer = inner.indexer.stop();
        let mut result = Ok(());
        if let Some(thread) = indexer {
            let joined = inner
                .runtime
                .blocking(move || thread.join().map_err(|_| Error::Task("indexer panicked"))?)
                .await;
            if let Err(err) = joined {
                tracing::error!(%err, "stopping the indexer");
                result = Err(err);
            }
        }
        if let Err(err) = inner.store.clone().close().await {
            tracing::error!(%err, "closing the store");
            result = result.and(Err(err.into()));
        }
        drop(lock(&inner.lock).take());
        inner.runtime.shutdown();
        tracing::info!("core shut down");
        result
    }

    // ---- events, settings, seams ----------------------------------------------------

    /// Batched change events (PLAN §4.2), at most one batch per event window.
    pub fn events(&self) -> broadcast::Receiver<CoreEventBatch> {
        self.inner.events_out.subscribe()
    }

    /// The current settings and their diagnostics.
    pub fn settings(&self) -> Arc<LoadedSettings> {
        Arc::clone(&self.inner.settings.borrow())
    }

    /// A receiver that is notified after every settings reload.
    pub fn settings_watch(&self) -> watch::Receiver<Arc<LoadedSettings>> {
        self.inner.settings.clone()
    }

    /// Where sync reports account status (see [`crate::sync_bridge`]).
    pub fn sync_status_sink(&self) -> SyncStatusSink {
        SyncStatusSink {
            tx: self.inner.events_in.clone(),
        }
    }

    /// Where `ingest` sends index documents (see [`crate::indexing`]).
    pub fn index_sink(&self) -> CoreIndexSink {
        CoreIndexSink {
            tx: self.inner.indexer.sender(),
        }
    }

    /// The store, for the L2 seams that write mail (`ingest`, `sync`, `import`). The
    /// UI never uses it (AGENTS.md); it goes through the methods of this handle.
    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// The sync engine, if this core runs one.
    pub(crate) fn engine(&self) -> Option<Arc<SyncEngine>> {
        self.inner.engine.clone()
    }

    /// Asks sync to run now for `scope`, without an audit entry (the local API audits
    /// its requests itself). [`MailCommand::Refresh`] is the audited form.
    pub fn request_refresh(&self, scope: crate::RefreshScope) {
        self.inner.sync.refresh(scope);
    }

    /// Accounts with a running sync actor.
    pub fn synced_accounts(&self) -> Vec<AccountId> {
        self.inner
            .engine
            .as_ref()
            .map(|engine| engine.running())
            .unwrap_or_default()
    }

    /// Runs `future` on the core runtime, from any executor.
    pub(crate) async fn run<F, T>(&self, future: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        self.ensure_open()?;
        self.inner.runtime.run(future).await
    }

    // ---- queries ------------------------------------------------------------------

    /// All accounts, by ordinal.
    pub fn accounts(&self) -> Result<Vec<Account>> {
        self.ensure_open()?;
        Ok(self.inner.store.reader()?.accounts()?)
    }

    /// The folder pane: unified folders, favorites and each account's folder tree, with
    /// counts (see [`crate::folders`]).
    pub fn folders(&self) -> Result<Vec<FolderNode>> {
        self.ensure_open()?;
        let mut reader = self.inner.store.reader()?;
        reader
            .snapshot(|reader| {
                let accounts = reader.accounts()?;
                let all = reader.folders(None)?;
                let trees = accounts
                    .iter()
                    .map(|a| Ok((a.id, reader.folder_tree(Some(a.id))?)))
                    .collect::<store::Result<Vec<_>>>()?;
                Ok(folders::build(&accounts, &all, &trees))
            })
            .map_err(Error::from)
    }

    /// The keys of a thread list (16 bytes per row). Metadata toggles of the Quick
    /// Filter run in SQL; typed text matches subjects and senders in memory, and
    /// [`CoreHandle::quick_filter_body`] adds body matches.
    pub fn thread_list(&self, query: &ListQuery) -> Result<ThreadListSnapshot> {
        self.ensure_open()?;
        let reader = self.inner.store.reader()?;
        let filter = query.quick_filter.list_filter();
        let mut keys = all_keys(&reader, query.scope, query.sort, &filter)?;
        let words = query.quick_filter.words();
        if let Some(words) = &words {
            let matching = self
                .inner
                .text
                .matching(&reader, query.scope, &keys, words)?;
            keys.retain(|k| matching.contains(&k.thread));
        }
        Ok(ThreadListSnapshot::new(
            query.clone(),
            keys,
            words.is_some(),
        ))
    }

    /// The Quick Filter with body matches: threads in the scope matching the metadata
    /// toggles whose subject or senders contain the text (as
    /// [`CoreHandle::thread_list`]) **or** with a message matching it in the index,
    /// scoped to the folder.
    pub async fn quick_filter_body(&self, query: &ListQuery) -> Result<ThreadListSnapshot> {
        self.ensure_open()?;
        let Some(text) = query.quick_filter.text.clone() else {
            return self.thread_list(query);
        };
        let Some(words) = query.quick_filter.words() else {
            return self.thread_list(query);
        };
        let inner = Arc::clone(&self.inner);
        let query = query.clone();
        self.inner
            .runtime
            .blocking(move || {
                let reader = inner.store.reader()?;
                let filter = query.quick_filter.list_filter();
                let mut keys = all_keys(&reader, query.scope, query.sort, &filter)?;
                let mut matching = inner.text.matching(&reader, query.scope, &keys, &words)?;
                matching.extend(core_search::body_matching_threads(
                    &inner.index,
                    &reader,
                    &core_search::message_scope(query.scope),
                    text.expose(),
                )?);
                keys.retain(|k| matching.contains(&k.thread));
                Ok(ThreadListSnapshot::new(query, keys, false))
            })
            .await
    }

    /// Row data for visible threads, in the given order (unknown threads are left
    /// out). `context` is [`ThreadListSnapshot::context`]. Served from an LRU of
    /// [`CoreConfig::row_cache_capacity`] rows that store changes invalidate.
    pub fn rows(&self, ids: &[ThreadId], context: Option<FolderId>) -> Result<Vec<ThreadRow>> {
        self.ensure_open()?;
        let (mut slots, epoch) = self.inner.rows.get(ids, context);
        let missing: Vec<ThreadId> = ids
            .iter()
            .zip(&slots)
            .filter(|(_, slot)| slot.is_none())
            .map(|(id, _)| *id)
            .collect();
        if !missing.is_empty() {
            let loaded = self.inner.store.reader()?.thread_rows(&missing, context)?;
            self.inner.rows.insert(&loaded, context, epoch);
            let mut loaded = loaded
                .into_iter()
                .map(|row| (row.id, row))
                .collect::<std::collections::HashMap<_, _>>();
            for (slot, id) in slots.iter_mut().zip(ids) {
                if slot.is_none() {
                    *slot = loaded.remove(id);
                }
            }
        }
        Ok(slots.into_iter().flatten().collect())
    }

    /// The messages of a thread, oldest first: what a thread-level command (mark read,
    /// archive) in the UI applies to.
    pub fn thread_message_ids(&self, thread: ThreadId) -> Result<Vec<MessageId>> {
        self.ensure_open()?;
        Ok(self.inner.store.reader()?.thread_message_ids(thread)?)
    }

    /// Row cache statistics.
    pub fn row_cache_stats(&self) -> RowCacheStats {
        self.inner.rows.stats()
    }

    /// Opens a message: headers, the rendered body (from the reader cache, or rendered
    /// and cached), attachments, sender verification and phishing warnings, remote
    /// content status. See [`crate::message`].
    ///
    /// A missing body gives [`Body::Pending`] and an interactive fetch request to
    /// sync; a missing verification is computed in the background (an event follows).
    pub async fn message(&self, id: MessageId) -> Result<MessageView> {
        self.ensure_open()?;
        let inner = Arc::clone(&self.inner);
        let (view, fetch, verify) = self
            .inner
            .runtime
            .blocking(move || build_view(&inner, id))
            .await?;
        if fetch {
            self.inner.sync.fetch_body(id, FetchPriority::Interactive);
        }
        if verify && lock(&self.inner.verifying).insert(id) {
            let core = self.clone();
            self.inner.runtime.handle().spawn(async move {
                if let Err(err) = core.verify_auth(id).await {
                    tracing::warn!(%err, message = %id, "sender verification failed");
                }
                lock(&core.inner.verifying).remove(&id);
            });
        }
        Ok(view)
    }

    fn verifier(&self) -> Option<Verifier> {
        self.inner
            .verifier
            .get_or_init(|| match Resolver::system() {
                Ok(resolver) => Some(Verifier::new(resolver)),
                Err(err) => {
                    tracing::warn!(%err, "no DNS resolver for sender verification");
                    None
                }
            })
            .clone()
    }

    /// Verifies the sender of a message now (DKIM, ARC, DMARC, trusted provider
    /// results) unless a current report is stored, and persists the result.
    /// [`AuthStatus::Pending`] if the body isn't stored yet.
    pub async fn verify_auth(&self, id: MessageId) -> Result<AuthStatus> {
        self.ensure_open()?;
        let Some(verifier) = self.verifier() else {
            return Ok(AuthStatus::Unavailable);
        };
        let core = self.clone();
        self.inner
            .runtime
            .run(async move {
                let inner = Arc::clone(&core.inner);
                let loaded = inner
                    .runtime
                    .blocking({
                        let inner = Arc::clone(&inner);
                        move || load_for_verification(&inner, id)
                    })
                    .await?;
                let (detail, kind, profile, raw) = match loaded {
                    Verification::Stored(report) => return Ok(AuthStatus::Ready(report)),
                    Verification::NoBody => return Ok(AuthStatus::Pending),
                    Verification::Needed {
                        detail,
                        kind,
                        profile,
                        raw,
                    } => (detail, kind, profile, raw),
                };
                let provider = profile.or(match kind {
                    AccountKind::Gmail => Some(ProviderProfile::gmail()),
                    AccountKind::M365 => Some(ProviderProfile::microsoft365()),
                    _ => None,
                });
                let mut context = VerifyContext::new();
                if let Some(provider) = &provider {
                    context = context.with_provider(provider);
                }
                if let Some(received) = detail
                    .received
                    .and_then(|r| OffsetDateTime::from_unix_timestamp(r).ok())
                {
                    context = context.with_received_at(received);
                }
                let report = verifier.verify(&raw, &context).await;
                persist_auth(&inner.store, detail.id, &report).await?;
                let _ = inner.events_in.send(CoreEvent::MessageUpdated(id));
                Ok(AuthStatus::Ready(Box::new(report)))
            })
            .await
    }

    /// Global search: query language, metadata filters in SQLite, thread grouping,
    /// facets and snippets (see [`crate::global_search`]).
    ///
    /// # Errors
    ///
    /// [`Error::Query`] for a query that doesn't parse (with the span to underline).
    pub async fn search(&self, query: &str, opts: SearchOpts) -> Result<SearchResults> {
        self.ensure_open()?;
        let inner = Arc::clone(&self.inner);
        let query = query.to_owned();
        self.inner
            .runtime
            .blocking(move || {
                core_search::run(&inner.store, &inner.index, &query, &opts, inner.utc_offset)
            })
            .await
    }

    // ---- commands -------------------------------------------------------------------

    /// Applies a command: permission check for agents, pending ops, optimistic local
    /// change and audit entry in one transaction (see [`crate::commands`]).
    ///
    /// # Errors
    ///
    /// - [`Error::PermissionDenied`] if the authorizer refused an agent command.
    /// - [`Error::InvalidCommand`] / [`Error::NotFound`] for commands that don't fit
    ///   the data; nothing is changed then.
    pub async fn apply(&self, command: MailCommand, origin: Origin) -> Result<CommandReceipt> {
        self.ensure_open()?;
        let seq = self.inner.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let origin_text = origin.as_text();
        if let Origin::Agent { token } = origin {
            match self.inner.authorizer.authorize(token, &command) {
                Authorization::Allow => {}
                Authorization::Deny { reason } => {
                    let detail = serde_json::json!({ "command": command.name(), "reason": reason })
                        .to_string();
                    self.inner
                        .store
                        .write(move |w| w.audit(&origin_text, "denied", Some(&detail)))
                        .await?;
                    return Err(Error::PermissionDenied { reason });
                }
                Authorization::Propose { title, reason } => {
                    let command_json = serde_json::to_string(&command)
                        .map_err(|err| Error::io("encoding a proposed command")(err.into()))?;
                    let name = command.name();
                    let proposal = self
                        .inner
                        .store
                        .write(move |w| {
                            let id = w.create_proposal(
                                Some(token),
                                &title,
                                &[NewProposalItem {
                                    command_json,
                                    reason,
                                }],
                            )?;
                            let detail = serde_json::json!({ "command": name, "proposal": id.0 })
                                .to_string();
                            w.audit(&origin_text, "proposed", Some(&detail))?;
                            Ok(id)
                        })
                        .await?;
                    return Ok(CommandReceipt {
                        seq,
                        outcome: CommandOutcome::Proposed(proposal),
                        messages: Vec::new(),
                        ops: Vec::new(),
                        created_folder: None,
                        undoable: false,
                    });
                }
            }
        }

        if let MailCommand::Refresh { scope } = command {
            self.inner.sync.refresh(scope);
            let detail = serde_json::json!({ "scope": scope }).to_string();
            self.inner
                .store
                .write(move |w| w.audit(&origin_text, "refresh", Some(&detail)))
                .await?;
            return Ok(CommandReceipt {
                seq,
                outcome: CommandOutcome::Requested,
                messages: Vec::new(),
                ops: Vec::new(),
                created_folder: None,
                undoable: false,
            });
        }

        let ctx = ExecContext {
            utc_offset: self.inner.utc_offset,
        };
        let weight = command_weight(&command);
        let applied = self
            .inner
            .store
            .write_weighted(weight, move |w| {
                commands::apply_in_tx(w, &command, &origin_text, ctx)
            })
            .await
            .map_err(map_store)?;
        self.after_write(&applied.threads, &applied.queued);
        let undoable = origin == Origin::User && applied.undo.is_some();
        if origin == Origin::User {
            *lock(&self.inner.undo) = applied.undo;
        }
        Ok(CommandReceipt {
            seq,
            outcome: CommandOutcome::Applied,
            messages: applied.messages,
            ops: applied.ops,
            created_folder: applied.created_folder,
            undoable,
        })
    }

    fn after_write(&self, threads: &BTreeSet<ThreadId>, queued: &[(AccountId, store::OpId)]) {
        // The change set invalidates too, asynchronously; doing it here as well means a
        // caller that reads rows right after `apply` never sees the old state.
        self.inner.rows.invalidate(threads.iter().copied());
        self.inner.text.invalidate(threads.iter().copied());
        if !queued.is_empty() {
            self.inner.sync.ops_queued(queued);
        }
    }

    /// Whether [`CoreHandle::undo`] has something to revert.
    pub fn can_undo(&self) -> bool {
        lock(&self.inner.undo).is_some()
    }

    /// Reverts the last undoable user command (moves, archives, deletes to Trash,
    /// flags, tags). Returns `None` if there is nothing to undo.
    pub async fn undo(&self) -> Result<Option<CommandReceipt>> {
        self.ensure_open()?;
        let Some(action) = lock(&self.inner.undo).take() else {
            return Ok(None);
        };
        let seq = self.inner.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let ctx = ExecContext {
            utc_offset: self.inner.utc_offset,
        };
        let origin = Origin::User.as_text();
        let applied = self
            .inner
            .store
            .write(move |w| commands::undo_in_tx(w, &action, &origin, ctx))
            .await
            .map_err(map_store)?;
        self.after_write(&applied.threads, &applied.queued);
        Ok(Some(CommandReceipt {
            seq,
            outcome: CommandOutcome::Applied,
            messages: applied.messages,
            ops: applied.ops,
            created_folder: None,
            undoable: false,
        }))
    }

    // ---- index maintenance ------------------------------------------------------------

    /// Commits pending index changes now and waits for them to be searchable.
    pub async fn commit_index(&self) -> Result<()> {
        self.ensure_open()?;
        self.inner.indexer.commit().await
    }

    /// Rebuilds the search index from the store and the blobs. Progress shows up as
    /// [`IndexProgress::rebuild`](crate::IndexProgress::rebuild) in the event batches;
    /// searches keep working on the old index until the final commit.
    pub async fn reindex_all(&self) -> Result<ReindexReport> {
        self.ensure_open()?;
        let inner = Arc::clone(&self.inner);
        let report = self.inner.runtime.blocking(move || reindex(&inner)).await?;
        self.inner.indexer.commit().await?;
        tracing::info!(indexed = report.indexed, "search index rebuilt");
        Ok(report)
    }

    /// Compares the index with the store: messages missing from the index, documents
    /// of messages that no longer exist, and duplicates. With `repair`, fixes them.
    pub async fn check_index(&self, repair: bool) -> Result<IndexCheck> {
        self.ensure_open()?;
        self.inner.indexer.commit().await?;
        let inner = Arc::clone(&self.inner);
        let check = self
            .inner
            .runtime
            .blocking(move || check(&inner, repair))
            .await?;
        if repair && !check.is_consistent() {
            self.inner.indexer.commit().await?;
        }
        Ok(check)
    }
}

fn command_weight(command: &MailCommand) -> usize {
    match command {
        MailCommand::SetFlags { ids, .. }
        | MailCommand::SetTags { ids, .. }
        | MailCommand::Move { ids, .. }
        | MailCommand::Copy { ids, .. }
        | MailCommand::Archive { ids }
        | MailCommand::Delete { ids, .. } => ids.len().max(1),
        _ => 1,
    }
}

/// Opens the store and index with an unlocked vault and starts the tasks.
pub(crate) async fn finish(
    config: CoreConfig,
    runtime: Arc<RuntimeHolder>,
    lock: DataDirLock,
    guard: CrashGuard,
    vault: Arc<Vault>,
) -> Result<CoreHandle> {
    let store_config = store_config(&config);
    let settings_file = config.settings_file.clone();
    let index_options = config.index;
    let cache_bytes = config.index_cache_bytes;
    let wants_engine = config.sync_engine.is_some();
    let opened = runtime
        .blocking(move || {
            let store = Store::open(&store_config, &vault)?;
            let reuse = if wants_engine {
                open_body_reuse(&store, &vault)
            } else {
                None
            };
            let (index, rebuilt) = open_index(store.layout(), &vault, cache_bytes)?;
            let indexer = index.indexer(&index_options)?;
            let (watcher, settings) = match settings_file {
                Some(path) => {
                    let (watcher, rx) = SettingsWatcher::start(path, settings::DEFAULT_DEBOUNCE)?;
                    (Some(watcher), rx)
                }
                None => {
                    let (_tx, rx) = watch::channel(Arc::new(LoadedSettings {
                        settings: default_settings(),
                        diagnostics: Vec::new(),
                    }));
                    (None, rx)
                }
            };
            Ok((store, index, rebuilt, indexer, watcher, settings, reuse))
        })
        .await?;
    let (store, index, rebuilt, indexer, watcher, settings, reuse) = opened;

    let (events_in, events_rx) = mpsc::unbounded_channel();
    let events_out = events::channel();
    let indexer = IndexerTask::spawn(index.clone(), indexer, events_in.clone())?;
    let rows = Arc::new(RowCache::new(config.row_cache_capacity));
    let text = Arc::new(TextCache::default());

    let handle = runtime.handle();
    let tasks = vec![
        handle.spawn(events::run_batcher(
            events_rx,
            events_out.clone(),
            store.clone(),
            config.event_window,
        )),
        handle.spawn(forward_changes(
            store.subscribe(),
            Arc::clone(&rows),
            Arc::clone(&text),
            indexer.sender(),
            events_in.clone(),
        )),
        handle.spawn(forward_settings(settings.clone(), events_in.clone())),
    ];

    let verifier = OnceLock::new();
    if let Some(configured) = config.verifier.clone() {
        let _ = verifier.set(Some(configured));
    }

    let engine = match config.sync_engine.clone() {
        Some(engine_config) => {
            let sink = Arc::new(CoreIndexSink {
                tx: indexer.sender(),
            });
            let ingestor = ingest::Ingestor::with_sink(store.clone(), sink);
            Some(Arc::new(SyncEngine::new(
                store.clone(),
                CoreIngest::new(ingestor, reuse),
                engine_config,
                settings.clone(),
                events_in.clone(),
                handle.clone(),
            )))
        }
        None => None,
    };
    let sync: Arc<dyn SyncBridge> = match &engine {
        Some(engine) => Arc::clone(engine) as Arc<dyn SyncBridge>,
        None => Arc::clone(&config.sync),
    };
    let core = CoreHandle {
        inner: Arc::new(Inner {
            data_dir: config.data_dir.clone(),
            runtime,
            store,
            index,
            indexer,
            events_out,
            events_in,
            rows,
            text,
            settings,
            watcher: Mutex::new(watcher),
            tasks: Mutex::new(tasks),
            guard,
            lock: Mutex::new(Some(lock)),
            undo: Mutex::new(None),
            seq: AtomicU64::new(0),
            sync,
            engine,
            authorizer: Arc::clone(&config.authorizer),
            verifier,
            verifying: Mutex::new(HashSet::new()),
            watchlist: DomainWatchlist::builtin(),
            utc_offset: config.utc_offset,
            closed: AtomicBool::new(false),
        }),
    };
    if let Some(engine) = &core.inner.engine
        && let Err(err) = engine.start_all()
    {
        tracing::error!(%err, "starting sync");
    }
    if rebuilt {
        let background = core.clone();
        core.inner.runtime.handle().spawn(async move {
            if let Err(err) = background.reindex_all().await {
                tracing::error!(%err, "rebuilding the search index");
            }
        });
    }
    tracing::info!(
        unhealthy_launches = core.crash_guard().unhealthy_launches,
        "core opened"
    );
    Ok(core)
}

/// The Thunderbird body reuse index an import left behind, if any (PLAN §4.14).
fn open_body_reuse(store: &Store, vault: &Vault) -> Option<Arc<dyn ingest::BodyLookup>> {
    let path = import::BodyReuse::default_path(store.layout());
    if !path.exists() {
        return None;
    }
    match import::BodyReuse::open(&path, vault) {
        Ok(reuse) => {
            tracing::info!("using the Thunderbird body reuse index for the first sync");
            Some(Arc::new(reuse))
        }
        Err(err) => {
            tracing::warn!(%err, "the Thunderbird body reuse index can't be opened");
            None
        }
    }
}

/// Opens `mail/index/`; an index with another schema is deleted and recreated (it is
/// a cache of the store). Returns whether that happened.
fn open_index(layout: &DataLayout, vault: &Vault, cache_bytes: u64) -> Result<(SearchIndex, bool)> {
    let path = layout.mail_dir().join(INDEX_DIR);
    let options = || DirectoryOptions {
        cache: Arc::new(search::BlockCache::new(cache_bytes)),
        ..DirectoryOptions::default()
    };
    match SearchIndex::open_encrypted(&path, vault.block_cipher(), options()) {
        Ok(index) => Ok((index, false)),
        Err(search::Error::SchemaMismatch) => {
            tracing::warn!("search index schema changed; rebuilding the index");
            std::fs::remove_dir_all(&path).map_err(Error::io("removing the old search index"))?;
            let index = SearchIndex::open_encrypted(&path, vault.block_cipher(), options())?;
            Ok((index, true))
        }
        Err(err) => Err(err.into()),
    }
}

async fn forward_changes(
    mut rx: broadcast::Receiver<Arc<ChangeSet>>,
    rows: Arc<RowCache>,
    text: Arc<TextCache>,
    indexer: Option<std::sync::mpsc::Sender<IndexCommand>>,
    events: mpsc::UnboundedSender<CoreEvent>,
) {
    loop {
        match rx.recv().await {
            Ok(changes) => {
                let touched = || {
                    changes
                        .threads
                        .iter()
                        .chain(&changes.threads_removed)
                        .chain(changes.folder_threads.values().flatten())
                        .copied()
                };
                rows.invalidate(touched());
                text.invalidate(touched());
                if let Some(indexer) = &indexer {
                    for message in &changes.messages_removed {
                        let _ = indexer.send(IndexCommand::Delete(*message));
                    }
                }
                if events.send(CoreEvent::Store(changes)).is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "store change sets lagged; dropping caches");
                rows.clear();
                text.clear();
                if events.send(CoreEvent::StoreLagged).is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn forward_settings(
    mut rx: watch::Receiver<Arc<LoadedSettings>>,
    events: mpsc::UnboundedSender<CoreEvent>,
) {
    while rx.changed().await.is_ok() {
        if events.send(CoreEvent::Settings).is_err() {
            return;
        }
    }
}

/// Builds a message view. Returns the view, whether to request the body, and whether
/// to verify the sender.
fn build_view(inner: &Inner, id: MessageId) -> Result<(MessageView, bool, bool)> {
    let reader = inner.store.reader()?;
    let detail = reader
        .message(id)?
        .ok_or_else(|| Error::not_found("message", id.to_public()))?;
    let settings = Arc::clone(&inner.settings.borrow());
    let policy = match reader.account(detail.account)? {
        Some(account) => settings.settings.account(&account.name).remote_content,
        None => settings.settings.reader.remote_content,
    };

    let mut fetch = false;
    let body = match detail.blob {
        Some(blob) if detail.body_state != BodyState::None => {
            match load_rendered(inner.store.blobs(), &blob) {
                Ok(Some((body, _))) => Body::Ready(Arc::new(body)),
                Ok(None) => Body::Unreadable,
                Err(Error::Store(store::Error::BlobMissing(missing))) => {
                    // After a rollback: forget the body and fetch it again (PLAN §4.15).
                    inner
                        .store
                        .write_blocking(move |w| w.blob_missing(missing))?;
                    fetch = true;
                    Body::Pending
                }
                Err(err) => return Err(err),
            }
        }
        _ => {
            fetch = true;
            Body::Pending
        }
    };

    let stored = stored_auth(&detail);
    let verifier_known_missing = matches!(inner.verifier.get(), Some(None));
    let mut verify = false;
    let auth = match &stored {
        Some(report) => AuthStatus::Ready(Box::new(report.clone())),
        None if verifier_known_missing => AuthStatus::Unavailable,
        None => {
            verify = matches!(body, Body::Ready(_));
            AuthStatus::Pending
        }
    };

    let rendered = match &body {
        Body::Ready(body) => Some(Arc::clone(body)),
        Body::Pending | Body::Unreadable => None,
    };
    let links = rendered
        .as_ref()
        .map(|r| links_from_reader_doc(&r.reader))
        .unwrap_or_default();
    let contacts = StoreContacts { reader: &reader };
    let warnings = analyze(&PhishingInput {
        from: detail.from.as_ref(),
        links: &links,
        auth: stored.as_ref(),
        contacts: Some(&contacts),
        watchlist: &inner.watchlist,
    });
    let sanitized = rendered.as_ref().and_then(|r| r.sanitized_html.as_ref());
    let has_remote_resources = sanitized.is_some_and(|s| s.has_remote_resources);
    let remote_content = RemoteContent {
        policy,
        has_remote_resources,
        trackers_removed: sanitized.map_or(0, |s| s.trackers_removed),
        blocked: has_remote_resources && policy != settings::RemoteContentPolicy::Always,
    };

    let flags = detail.effective_flags();
    let view = MessageView {
        id: detail.id,
        thread: detail.thread,
        account: detail.account,
        folders: detail.locations.iter().map(|l| l.folder).collect(),
        tags: detail.tags.clone(),
        has_attachments: detail.has_attachments,
        subject: detail.subject,
        from: detail.from,
        to: detail.to,
        cc: detail.cc,
        date: detail.date,
        received: detail.received,
        rfc_message_id: detail.rfc_message_id,
        list_id: detail.list_id,
        flags,
        body,
        auth,
        warnings,
        remote_content,
    };
    Ok((view, fetch, verify))
}

enum Verification {
    Stored(Box<security::AuthReport>),
    NoBody,
    Needed {
        detail: Box<MessageDetail>,
        kind: AccountKind,
        /// The account's stored provider profile.
        profile: Option<ProviderProfile>,
        raw: Vec<u8>,
    },
}

fn load_for_verification(inner: &Inner, id: MessageId) -> Result<Verification> {
    let reader = inner.store.reader()?;
    let detail = reader
        .message(id)?
        .ok_or_else(|| Error::not_found("message", id.to_public()))?;
    if let Some(report) = stored_auth(&detail) {
        return Ok(Verification::Stored(Box::new(report)));
    }
    let Some(blob) = detail.blob else {
        return Ok(Verification::NoBody);
    };
    let raw = match inner.store.blobs().read(&blob) {
        Ok(raw) => raw,
        Err(store::Error::BlobMissing(_)) => return Ok(Verification::NoBody),
        Err(err) => return Err(err.into()),
    };
    let account = reader.account(detail.account)?;
    let kind = account.as_ref().map_or(AccountKind::Imap, |a| a.kind);
    let profile = account
        .as_ref()
        .and_then(|a| crate::accounts::AccountConfig::parse(&a.config_json))
        .and_then(|c| c.provider_profile);
    Ok(Verification::Needed {
        detail: Box::new(detail),
        kind,
        profile,
        raw,
    })
}

/// Every message id the store has (with a location).
fn all_message_ids(reader: &store::Reader) -> Result<BTreeSet<MessageId>> {
    let mut ids = BTreeSet::new();
    for account in reader.accounts()? {
        ids.extend(reader.message_ids_in_scope(&MessageScope::Account(account.id))?);
    }
    Ok(ids)
}

/// The index document for a stored message: from its body if stored, else from its
/// headers. The second value says which: `Some(true)` body, `Some(false)` unreadable
/// body, `None` no body.
fn document_for(store: &Store, detail: &MessageDetail) -> (IndexDocument, Option<bool>) {
    if let Some(blob) = detail.blob {
        match store
            .blobs()
            .read(&blob)
            .ok()
            .map(|raw| mazemail_mime::parse(&raw))
        {
            Some(Ok(parsed)) => {
                let doc = IndexDocument::from_parsed(
                    detail.id,
                    detail.thread,
                    detail.account,
                    detail.date,
                    &parsed,
                );
                return (doc, Some(true));
            }
            Some(Err(_)) | None => return (headers_document(detail), Some(false)),
        }
    }
    (headers_document(detail), None)
}

fn headers_document(detail: &MessageDetail) -> IndexDocument {
    IndexDocument {
        message_id: detail.id,
        thread_id: detail.thread,
        account_id: detail.account,
        date: detail.date,
        size: detail.size.unwrap_or_default(),
        from: detail.from.iter().cloned().collect(),
        to: detail.to.clone(),
        cc: detail.cc.clone(),
        subject: detail.subject.clone().unwrap_or_default().into(),
        body_text: String::new().into(),
        attachment_names: Vec::new(),
        list_id: detail.list_id.clone(),
        has_attachment: detail.has_attachments,
        language: None,
    }
}

fn reindex(inner: &Inner) -> Result<ReindexReport> {
    let sender = inner.indexer.sender().ok_or(Error::Closed)?;
    let send = |command| sender.send(command).map_err(|_| Error::Closed);
    let reader = inner.store.reader()?;
    let ids = all_message_ids(&reader)?;
    let total = ids.len() as u64;
    send(IndexCommand::DeleteAll)?;
    let mut report = ReindexReport::default();
    for (done, id) in ids.into_iter().enumerate() {
        let Some(detail) = reader.message(id)? else {
            continue;
        };
        let (doc, body) = document_for(&inner.store, &detail);
        match body {
            Some(true) => {}
            Some(false) => report.unreadable += 1,
            None => report.headers_only += 1,
        }
        send(IndexCommand::Add(Box::new(doc.into_search_doc())))?;
        report.indexed += 1;
        let done = done as u64 + 1;
        if done.is_multiple_of(REINDEX_PROGRESS_EVERY) || done == total {
            send(IndexCommand::Progress { done, total })?;
        }
    }
    Ok(report)
}

fn check(inner: &Inner, repair: bool) -> Result<IndexCheck> {
    let reader = inner.store.reader()?;
    let in_store = all_message_ids(&reader)?;
    let (indexed, duplicated) = indexing::indexed_ids(&inner.index)?;
    let mut result = IndexCheck {
        store_messages: in_store.len() as u64,
        indexed_documents: inner.index.num_docs(),
        missing: in_store.difference(&indexed).copied().collect(),
        stale: indexed.difference(&in_store).copied().collect(),
        duplicated,
        repaired: false,
    };
    if repair && !result.is_consistent() {
        let sink = CoreIndexSink {
            tx: inner.indexer.sender(),
        };
        for id in &result.stale {
            sink.delete(*id)?;
        }
        // `add` replaces, which also collapses duplicates.
        for id in result.missing.iter().chain(&result.duplicated) {
            if let Some(detail) = reader.message(*id)? {
                sink.add(document_for(&inner.store, &detail).0)?;
            }
        }
        result.repaired = true;
    }
    Ok(result)
}
