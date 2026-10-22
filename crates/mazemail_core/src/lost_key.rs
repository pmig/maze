//! The lost-key flow (PLAN §4.17, "Lost key and recovery").
//!
//! When `mail/` holds encrypted data but the key store has no key,
//! [`CoreHandle::open`](crate::CoreHandle::open) fails with
//! [`Error::MissingKeyForExistingData`] carrying a [`LostKey`]. It keeps the data
//! directory locked and offers exactly the two ways out the UI shows:
//!
//! - **Enter recovery key** ([`LostKey::enter_recovery_key`]): the key is checked
//!   against the existing database first and only then stored in the key store. A
//!   wrong key gives the [`LostKey`] back for another try.
//! - **Start fresh and re-sync** ([`LostKey::start_fresh`]): the old `mail/` directory
//!   is moved aside to `mail-unreadable-<unix time>/` (never deleted; the user can
//!   delete it, or restore it if the key turns up), and a new key is created.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use store::{DataLayout, Store};
use vault::Vault;

use crate::config::CoreConfig;
use crate::crash_guard::CrashGuard;
use crate::handle::{self, CoreHandle};
use crate::lock::DataDirLock;
use crate::runtime::RuntimeHolder;
use crate::{Error, Result};

/// A data directory whose key is missing. See the [module docs](self).
pub struct LostKey {
    pub(crate) config: CoreConfig,
    pub(crate) runtime: Arc<RuntimeHolder>,
    pub(crate) lock: DataDirLock,
    pub(crate) guard: CrashGuard,
}

impl fmt::Debug for LostKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LostKey")
            .field("data_dir", &self.config.data_dir)
            .finish_non_exhaustive()
    }
}

/// A recovery attempt that failed.
#[derive(Debug, thiserror::Error)]
#[error("recovery failed: {error}")]
pub struct RecoveryFailed {
    /// Try again with this. `None` if the key was accepted and stored but opening the
    /// core failed afterwards: then retry [`CoreHandle::open`].
    pub lost_key: Option<Box<LostKey>>,
    /// Why it failed ([`Error::WrongRecoveryKey`] for a key that doesn't fit).
    #[source]
    pub error: Error,
}

impl LostKey {
    /// The data directory in question.
    pub fn data_dir(&self) -> &Path {
        &self.config.data_dir
    }

    /// Unlocks with the recovery key the user typed (grouped base32, as shown in
    /// Settings → Privacy), stores it in the key store and opens the core.
    ///
    /// # Errors
    ///
    /// [`RecoveryFailed`] with [`Error::WrongRecoveryKey`] if the key is well-formed
    /// but doesn't open the database, or [`Error::Vault`] if it isn't a recovery key.
    pub async fn enter_recovery_key(
        self,
        recovery_key: &str,
    ) -> Result<CoreHandle, RecoveryFailed> {
        let vault = match Vault::from_recovery_key(recovery_key) {
            Ok(vault) => vault,
            Err(err) => {
                return Err(RecoveryFailed {
                    lost_key: Some(Box::new(self)),
                    error: err.into(),
                });
            }
        };
        let store_config = handle::store_config(&self.config);
        let checked = self
            .runtime
            .blocking(move || match Store::open(&store_config, &vault) {
                Ok(store) => Ok(Ok((store, vault))),
                Err(store::Error::WrongKey { .. } | store::Error::PlaintextDatabase { .. }) => {
                    Ok(Err(Error::WrongRecoveryKey))
                }
                Err(err) => Ok(Err(err.into())),
            })
            .await;
        let (store, vault) = match checked {
            Ok(Ok(opened)) => opened,
            Ok(Err(error)) | Err(error) => {
                return Err(RecoveryFailed {
                    lost_key: Some(Box::new(self)),
                    error,
                });
            }
        };
        if let Err(error) = store.close().await {
            return Err(RecoveryFailed {
                lost_key: Some(Box::new(self)),
                error: error.into(),
            });
        }
        let Self {
            config,
            runtime,
            lock,
            guard,
        } = self;
        let key_store = Arc::clone(&config.key_store);
        let vault = Arc::new(vault);
        let persisted = Arc::clone(&vault);
        // Only now, with the key proven, does it go into the key store.
        if let Err(error) = runtime
            .blocking(move || persisted.persist(&*key_store).map_err(Error::from))
            .await
        {
            return Err(RecoveryFailed {
                lost_key: Some(Box::new(Self {
                    config,
                    runtime,
                    lock,
                    guard,
                })),
                error,
            });
        }
        tracing::info!("data directory unlocked with the recovery key");
        handle::finish(config, runtime, lock, guard, vault)
            .await
            .map_err(|error| RecoveryFailed {
                lost_key: None,
                error,
            })
    }

    /// Moves the unreadable data aside and starts over with a new key. Mail is
    /// re-synced from the servers; local-only data stays in the moved directory.
    ///
    /// Returns the core and the directory the old data was moved to.
    pub async fn start_fresh(self) -> Result<(CoreHandle, PathBuf)> {
        let Self {
            config,
            runtime,
            lock,
            guard,
        } = self;
        let data_dir = config.data_dir.clone();
        let key_store = Arc::clone(&config.key_store);
        let (vault, moved_to) = runtime
            .blocking(move || {
                let mail = DataLayout::new(&data_dir).mail_dir().to_path_buf();
                let moved_to = aside_path(&data_dir);
                std::fs::rename(&mail, &moved_to)
                    .map_err(Error::io("moving the unreadable mail data aside"))?;
                tracing::warn!(to = %moved_to.display(), "moved unreadable mail data aside");
                Ok((Vault::start_fresh(&*key_store)?, moved_to))
            })
            .await?;
        let core = handle::finish(config, runtime, lock, guard, Arc::new(vault)).await?;
        Ok((core, moved_to))
    }
}

/// `<data_dir>/mail-unreadable-<unix time>[-n]/`, not yet existing.
fn aside_path(data_dir: &Path) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let base = data_dir.join(format!("mail-unreadable-{now}"));
    let mut candidate = base.clone();
    let mut n = 1;
    while candidate.exists() {
        candidate = PathBuf::from(format!("{}-{n}", base.display()));
        n += 1;
    }
    candidate
}
