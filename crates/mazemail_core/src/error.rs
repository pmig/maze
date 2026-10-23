//! The crate's error type.

use std::io;
use std::path::PathBuf;

use crate::lost_key::LostKey;

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors from [`CoreHandle`](crate::CoreHandle).
///
/// None of the variants carry message content (subjects, addresses, bodies, query
/// text), so every error is safe to log.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Another Maze process holds the data directory. Only one instance may open a data
    /// directory at a time; the UI brings the running instance to the front instead.
    #[error("Maze is already running with the data directory {}", data_dir.display())]
    AlreadyRunning {
        /// The data directory that is in use.
        data_dir: PathBuf,
        /// The process id the other instance wrote into the lock file, if readable.
        pid: Option<u32>,
    },

    /// Encrypted mail data exists but the key store has no key (PLAN §4.17, "Lost key
    /// and recovery"). The payload offers the two ways forward: enter the recovery key,
    /// or start fresh and re-sync. Nothing has been deleted.
    #[error("encrypted mail data exists but its key is missing")]
    MissingKeyForExistingData(Box<LostKey>),

    /// The recovery key is well-formed but doesn't open the existing data.
    #[error("the recovery key doesn't open the existing mail data")]
    WrongRecoveryKey,

    /// A search query couldn't be parsed.
    #[error("invalid search query: {0}")]
    Query(#[from] search::query::ParseError),

    /// An agent command was refused by the permission hook.
    #[error("the command was not allowed: {reason}")]
    PermissionDenied {
        /// Why, for the activity log (no message content).
        reason: String,
    },

    /// A command can't be applied as given (for example a move into a folder of another
    /// account).
    #[error("invalid command: {0}")]
    InvalidCommand(&'static str),

    /// A row the operation needs doesn't exist.
    #[error("{entity} {id} not found")]
    NotFound {
        /// The kind of row (`"message"`, `"folder"`, …).
        entity: &'static str,
        /// Its public id.
        id: String,
    },

    /// The store failed.
    #[error(transparent)]
    Store(#[from] store::Error),

    /// The search index failed.
    #[error(transparent)]
    Search(#[from] search::Error),

    /// The vault (key store, encryption) failed.
    #[error(transparent)]
    Vault(#[from] vault::Error),

    /// Watching the settings file failed.
    #[error(transparent)]
    Settings(#[from] settings::WatchError),

    /// A filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What core was doing.
        context: &'static str,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// Adding or removing an account failed. `message` is for people: what went
    /// wrong and what to do (no passwords or tokens).
    #[error("{message}")]
    Account {
        /// What kind of failure.
        kind: crate::accounts::AccountErrorKind,
        /// The explanation.
        message: String,
    },

    /// Accounts can't be added or synced: this core runs without a sync engine.
    #[error("accounts can't be added here: Maze runs without its sync engine")]
    NoSyncEngine,

    /// A background task panicked or was cancelled.
    #[error("a background task failed: {0}")]
    Task(&'static str),

    /// The core was shut down.
    #[error("the core has been shut down")]
    Closed,
}

impl Error {
    pub(crate) fn io(context: &'static str) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Io { context, source }
    }

    pub(crate) fn not_found(entity: &'static str, id: impl std::fmt::Display) -> Self {
        Self::NotFound {
            entity,
            id: id.to_string(),
        }
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(err: tokio::task::JoinError) -> Self {
        if err.is_cancelled() {
            Self::Task("cancelled")
        } else {
            Self::Task("panicked")
        }
    }
}
