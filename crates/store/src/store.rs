//! [`Store`]: opening, the writer handle, readers, and shutdown.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::{broadcast, oneshot};
use vault::Vault;

use crate::blob::{BlobStore, GcOptions, GcReport};
use crate::changes::ChangeSet;
use crate::conn::{self, Access};
use crate::layout::DataLayout;
use crate::migrate::{self, MigrationReport};
use crate::reader::{Reader, ReaderPool};
use crate::sendlog::SendLog;
use crate::writer::{self, Command, WriteTx, WriterThread};
use crate::{Error, Result};

/// How to open the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreConfig {
    /// The data dir (from the `settings` crate); the store uses `<data_dir>/mail/`.
    pub data_dir: PathBuf,
    /// The running Maze version, used in pre-migration backup names.
    pub app_version: String,
    /// Read-only connections in the pool (PLAN §4.3: 4).
    pub readers: usize,
    /// Page cache of the writer connection, KiB.
    pub writer_cache_kib: u32,
    /// Page cache of each reader connection, KiB. Bounded so the pool fits the idle
    /// memory budget; tuned in spike S7.
    pub reader_cache_kib: u32,
    /// Collect unreferenced blobs when the writer has been idle this long (`None`:
    /// only on [`Store::collect_garbage`]).
    pub gc_interval: Option<Duration>,
}

impl StoreConfig {
    /// Defaults for `data_dir`.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            readers: 4,
            writer_cache_kib: 32 * 1024,
            reader_cache_kib: 16 * 1024,
            gc_interval: Some(Duration::from_secs(10 * 60)),
        }
    }
}

/// The store: one writer thread, a pool of readers, the blob store and the send log.
///
/// Cheap to clone (all clones share the same connections). See the crate docs for the
/// threading model.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    layout: DataLayout,
    commands: Sender<Command>,
    writer: Mutex<Option<JoinHandle<()>>>,
    readers: Arc<ReaderPool>,
    blobs: BlobStore,
    sendlog: SendLog,
    events: broadcast::Sender<Arc<ChangeSet>>,
    migration: MigrationReport,
    cipher_provider: String,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("mail_dir", &self.inner.layout.mail_dir())
            .field("migration", &self.inner.migration)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens (or creates) the store under `config.data_dir`, keyed by `vault`.
    ///
    /// Blocking: it opens files, verifies the key, may back up and migrate the
    /// database. Call it off the UI thread.
    ///
    /// # Errors
    ///
    /// - [`Error::WrongKey`] / [`Error::PlaintextDatabase`] if the database exists but
    ///   doesn't open with this key. Nothing is created or overwritten.
    /// - [`Error::SchemaTooNew`] if a newer Maze wrote the database (downgrade guard).
    pub fn open(config: &StoreConfig, vault: &Vault) -> Result<Self> {
        let layout = DataLayout::new(&config.data_dir);
        layout.create_dirs()?;
        let key = vault.sqlcipher_pragma_key();

        let db_path = layout.database();
        let mut writer_conn =
            conn::open(&db_path, &key, Access::ReadWrite, config.writer_cache_kib)?;
        let cipher_provider = conn::cipher_provider(&writer_conn)?;
        let migration = migrate::migrate_main(
            &mut writer_conn,
            &key,
            &layout.backups_dir(),
            &config.app_version,
        )?;

        let readers = (0..config.readers.max(1))
            .map(|_| conn::open(&db_path, &key, Access::ReadOnly, config.reader_cache_kib))
            .collect::<Result<Vec<_>>>()?;
        let sendlog = SendLog::open(&layout.sendlog(), &key)?;
        drop(key);

        let blobs = BlobStore::new(layout.clone(), vault.blob_cipher(), vault.blob_id_hasher());
        let (events, _) = broadcast::channel(1024);
        let (commands, rx) = writer::channel();
        let handle = WriterThread::spawn(
            writer_conn,
            blobs.clone(),
            rx,
            events.clone(),
            config.gc_interval,
        )?;
        tracing::info!(
            schema = migration.to,
            from = migration.from,
            provider = %cipher_provider,
            "store opened"
        );
        Ok(Self {
            inner: Arc::new(Inner {
                layout,
                commands,
                writer: Mutex::new(Some(handle)),
                readers: ReaderPool::new(readers),
                blobs,
                sendlog,
                events,
                migration,
                cipher_provider,
            }),
        })
    }

    /// The paths the store uses.
    pub fn layout(&self) -> &DataLayout {
        &self.inner.layout
    }

    /// What opening did to the schema (and the backup it made, if any).
    pub fn migration(&self) -> &MigrationReport {
        &self.inner.migration
    }

    /// The SQLCipher crypto backend in use (`commoncrypto` on macOS, `openssl`
    /// elsewhere).
    pub fn cipher_provider(&self) -> &str {
        &self.inner.cipher_provider
    }

    /// The blob store.
    pub fn blobs(&self) -> &BlobStore {
        &self.inner.blobs
    }

    /// The send log.
    pub fn sendlog(&self) -> &SendLog {
        &self.inner.sendlog
    }

    /// A read-only connection from the pool (waits while all are busy).
    pub fn reader(&self) -> Result<Reader> {
        self.inner.readers.get()
    }

    /// Runs `f` on a pooled reader.
    pub fn read<R>(&self, f: impl FnOnce(&Reader) -> Result<R>) -> Result<R> {
        f(&self.reader()?)
    }

    /// Runs `f` on the writer thread, batched with other writes; resolves after the
    /// batch committed. See [`writer`](crate::writer) for the batching rules.
    ///
    /// `f` runs in its own savepoint: if it returns an error, its changes are rolled
    /// back and the error is returned; other writes in the batch are unaffected.
    pub async fn write<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut WriteTx<'_>) -> Result<R> + Send + 'static,
    {
        self.write_weighted(1, f).await
    }

    /// Like [`Store::write`], for a job that does about `weight` statements' worth of
    /// work (for example inserting 200 messages), so batches stay bounded.
    pub async fn write_weighted<R, F>(&self, weight: usize, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut WriteTx<'_>) -> Result<R> + Send + 'static,
    {
        let rx = self.submit(weight, f)?;
        rx.await.map_err(|_| Error::Closed)?
    }

    /// Blocking variant of [`Store::write`] for synchronous callers (importers, tests).
    /// Must not be called from inside an async runtime's worker thread.
    pub fn write_blocking<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut WriteTx<'_>) -> Result<R> + Send + 'static,
    {
        let rx = self.submit(1, f)?;
        rx.blocking_recv().map_err(|_| Error::Closed)?
    }

    fn submit<R, F>(&self, weight: usize, f: F) -> Result<oneshot::Receiver<Result<R>>>
    where
        R: Send + 'static,
        F: FnOnce(&mut WriteTx<'_>) -> Result<R> + Send + 'static,
    {
        let (run, rx) = writer::job(f);
        self.inner
            .commands
            .send(Command::Job { weight, run })
            .map_err(|_| Error::Closed)?;
        Ok(rx)
    }

    /// Runs blob garbage collection on the writer thread (between transactions).
    pub async fn collect_garbage(&self, options: GcOptions) -> Result<GcReport> {
        let (reply, rx) = oneshot::channel();
        self.inner
            .commands
            .send(Command::Gc { options, reply })
            .map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }

    /// Receives a [`ChangeSet`] after every commit that changed something. A slow
    /// receiver lags (see `tokio::sync::broadcast`) and should then reload.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ChangeSet>> {
        self.inner.events.subscribe()
    }

    /// Records a successful SMTP transaction: the send log entry first, then the outbox
    /// row is marked sent (PLAN §4.5). If Maze dies in between, the outbox finds the
    /// receipt on its next check and doesn't send again.
    pub async fn record_sent(
        &self,
        outbox: crate::model::OutboxId,
        rfc_message_id: &str,
        identity: Option<mazemail_types::IdentityId>,
    ) -> Result<crate::model::SentReceipt> {
        let receipt = self
            .inner
            .sendlog
            .record(rfc_message_id, identity, Some(outbox))?;
        self.write(move |w| w.outbox_mark_sent(outbox)).await?;
        Ok(receipt)
    }

    /// After a rollback restored an older database (PLAN §4.15): marks outbox items
    /// found in the send log as sent and freezes the rest until you press Send again.
    /// Returns `(marked_sent, frozen)`.
    pub async fn reconcile_outbox_after_rollback(&self) -> Result<(usize, usize)> {
        let store = self.clone();
        self.write(move |w| w.outbox_reconcile_after_rollback(store.sendlog()))
            .await
    }

    /// Stops the writer (after the queued writes), checkpoints the WAL and closes the
    /// connections. Other clones of this store get [`Error::Closed`] afterwards.
    pub async fn close(self) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        let sent = self
            .inner
            .commands
            .send(Command::Shutdown { reply })
            .is_ok();
        let result = if sent {
            rx.await.map_err(|_| Error::Closed)?
        } else {
            Ok(())
        };
        let handle = self
            .inner
            .writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            // The writer has replied, so it is about to return.
            let _ = handle.join();
        }
        self.inner.readers.close();
        self.inner.sendlog.close()?;
        result
    }
}
