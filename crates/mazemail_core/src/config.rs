//! How to open the core.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use search::IndexOptions;
use security::Verifier;
use settings::Paths;
use time::UtcOffset;
use vault::KeyStore;

use crate::api_handler::{CommandAuthorizer, DenyAgents};
use crate::events::DEFAULT_EVENT_WINDOW;
use crate::row_cache::DEFAULT_ROW_CACHE_CAPACITY;
use crate::sync_bridge::{NoSync, SyncBridge};
use crate::sync_engine::SyncEngineConfig;

/// Which Tokio runtime the core's tasks run on.
#[derive(Debug, Clone)]
pub enum RuntimeConfig {
    /// The core builds and owns a multi-thread runtime, and shuts it down in
    /// [`CoreHandle::shutdown`](crate::CoreHandle::shutdown). This is what the GUI
    /// uses: GPUI isn't a Tokio runtime.
    Owned {
        /// Worker threads; `None`: one per core.
        worker_threads: Option<usize>,
    },
    /// Spawn onto an existing multi-thread runtime (CLI, headless mode, tests).
    Handle(tokio::runtime::Handle),
}

/// Everything [`CoreHandle::open`](crate::CoreHandle::open) needs.
#[derive(Clone)]
pub struct CoreConfig {
    /// The data directory (`<data_dir>/mail/` holds the encrypted data).
    pub data_dir: PathBuf,
    /// The user's `settings.json`; `None` runs on the defaults without a watcher.
    pub settings_file: Option<PathBuf>,
    /// The running Maze version (crash guard, pre-migration backup names).
    pub app_version: String,
    /// Where the master key lives (the OS keychain in production).
    pub key_store: Arc<dyn KeyStore>,
    /// The runtime for the core's tasks.
    pub runtime: RuntimeConfig,
    /// The user's time zone, for query dates and archive folders. The binary
    /// determines it at startup (before other threads exist); defaults to UTC.
    pub utc_offset: UtcOffset,
    /// Sender verification with its DNS resolver; `None`: the system resolver, created
    /// on first use.
    pub verifier: Option<Verifier>,
    /// Read-only store connections.
    pub store_readers: usize,
    /// Idle time after which the store collects unreferenced blobs (`None`: never).
    pub blob_gc_interval: Option<Duration>,
    /// Indexer threads, memory and commit policy.
    pub index: IndexOptions,
    /// Size of the decrypted index block cache, bytes.
    pub index_cache_bytes: u64,
    /// Event batching window (PLAN §4.2: one frame).
    pub event_window: Duration,
    /// Thread rows kept in the LRU (PLAN §4.3: 2,000).
    pub row_cache_capacity: usize,
    /// The sync engine as a bridge, used when [`CoreConfig::sync_engine`] is `None`
    /// (see [`crate::sync_bridge`]).
    pub sync: Arc<dyn SyncBridge>,
    /// Run the real sync engine: an account actor per IMAP account, with secrets,
    /// network and browser from this config (see [`crate::sync_engine`]). `None`
    /// (the default): no actors; adding accounts fails with
    /// [`Error::NoSyncEngine`](crate::Error::NoSyncEngine).
    pub sync_engine: Option<SyncEngineConfig>,
    /// The permission hook for agent commands (see [`crate::api_handler`]).
    pub authorizer: Arc<dyn CommandAuthorizer>,
}

impl std::fmt::Debug for CoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreConfig")
            .field("data_dir", &self.data_dir)
            .field("settings_file", &self.settings_file)
            .field("app_version", &self.app_version)
            .field("key_store", &self.key_store)
            .field("runtime", &self.runtime)
            .field("utc_offset", &self.utc_offset)
            .field("store_readers", &self.store_readers)
            .field("index", &self.index)
            .field("event_window", &self.event_window)
            .field("row_cache_capacity", &self.row_cache_capacity)
            .field("sync", &self.sync)
            .field("sync_engine", &self.sync_engine)
            .field("authorizer", &self.authorizer)
            .finish_non_exhaustive()
    }
}

impl CoreConfig {
    /// Defaults for `data_dir` and `key_store`: an owned runtime, no settings file,
    /// UTC, no sync engine, no agent access.
    pub fn new(data_dir: impl Into<PathBuf>, key_store: Arc<dyn KeyStore>) -> Self {
        Self {
            data_dir: data_dir.into(),
            settings_file: None,
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            key_store,
            runtime: RuntimeConfig::Owned {
                worker_threads: None,
            },
            utc_offset: UtcOffset::UTC,
            verifier: None,
            store_readers: 4,
            blob_gc_interval: Some(Duration::from_secs(10 * 60)),
            index: IndexOptions::default(),
            index_cache_bytes: search::DEFAULT_CACHE_BYTES,
            event_window: DEFAULT_EVENT_WINDOW,
            row_cache_capacity: DEFAULT_ROW_CACHE_CAPACITY,
            sync: Arc::new(NoSync),
            sync_engine: None,
            authorizer: Arc::new(DenyAgents),
        }
    }

    /// Defaults for the standard directories (data dir and `settings.json`).
    pub fn from_paths(paths: &Paths, key_store: Arc<dyn KeyStore>) -> Self {
        Self {
            settings_file: Some(paths.settings_file()),
            ..Self::new(paths.data_dir(), key_store)
        }
    }

    /// Runs the core's tasks on an existing runtime.
    #[must_use]
    pub fn with_runtime(mut self, handle: tokio::runtime::Handle) -> Self {
        self.runtime = RuntimeConfig::Handle(handle);
        self
    }

    /// Plugs in the sync engine.
    #[must_use]
    pub fn with_sync(mut self, sync: Arc<dyn SyncBridge>) -> Self {
        self.sync = sync;
        self
    }

    /// Runs the real sync engine (see [`CoreConfig::sync_engine`]).
    #[must_use]
    pub fn with_sync_engine(mut self, engine: SyncEngineConfig) -> Self {
        self.sync_engine = Some(engine);
        self
    }

    /// Plugs in the agent permission hook.
    #[must_use]
    pub fn with_authorizer(mut self, authorizer: Arc<dyn CommandAuthorizer>) -> Self {
        self.authorizer = authorizer;
        self
    }

    /// Uses this verifier (and its resolver) for sender verification.
    #[must_use]
    pub fn with_verifier(mut self, verifier: Verifier) -> Self {
        self.verifier = Some(verifier);
        self
    }

    /// Sets the user's time zone.
    #[must_use]
    pub fn with_utc_offset(mut self, offset: UtcOffset) -> Self {
        self.utc_offset = offset;
        self
    }

    /// Reads (and watches) this settings file.
    #[must_use]
    pub fn with_settings_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.settings_file = Some(path.into());
        self
    }
}
