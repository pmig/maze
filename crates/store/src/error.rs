use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use mazemail_types::BlobId;

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors from the store.
///
/// None of the variants carry message content (subjects, addresses, bodies), so every
/// error is safe to log.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The database exists but doesn't open with this key. Maze never creates a new
    /// database over existing data; recover the key or move the data aside.
    #[error("the database at {path} can't be opened with this key")]
    WrongKey {
        /// The database file.
        path: PathBuf,
    },

    /// The file is a plaintext SQLite database. Maze only ever writes encrypted
    /// databases, so this is someone else's file or a tampered copy.
    #[error("the database at {path} is not encrypted")]
    PlaintextDatabase {
        /// The database file.
        path: PathBuf,
    },

    /// The database was written by a newer Maze (downgrade guard, PLAN §4.15). Opening
    /// it with this build could corrupt data.
    #[error(
        "the database schema version {found} is newer than this version of Maze understands \
         (up to {supported}); update Maze to open it"
    )]
    SchemaTooNew {
        /// Schema version in the file.
        found: u32,
        /// Newest schema version this build knows.
        supported: u32,
    },

    /// Applying a schema migration failed. The pre-migration backup is untouched.
    #[error("database migration failed: {0}")]
    Migration(#[source] rusqlite_migration::Error),

    /// An SQLite error.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The batch transaction this write was part of failed to commit; nothing of it was
    /// stored.
    #[error("database commit failed: {0}")]
    Commit(#[source] Arc<rusqlite::Error>),

    /// The blobs this write references couldn't be made durable (see
    /// [`BlobStore::persist`](crate::BlobStore::persist)), so the batch transaction it
    /// was part of was rolled back; nothing of it was stored.
    #[error("making blobs durable failed: {0}")]
    BlobsNotDurable(#[source] Arc<Error>),

    /// A filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What the store was doing.
        context: &'static str,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// Encrypting or decrypting a blob failed (wrong key, tampered or truncated file).
    #[error(transparent)]
    Vault(#[from] vault::Error),

    /// A blob that the database references has no file on disk (for example after a
    /// rollback restored an older database). Reset the message's body state and fetch
    /// it again.
    #[error("blob {0:?} is missing on disk")]
    BlobMissing(BlobId),

    /// A row that the operation needs doesn't exist.
    #[error("{entity} {id} not found")]
    NotFound {
        /// The kind of row (`"message"`, `"folder"`, …).
        entity: &'static str,
        /// Its public or raw id.
        id: String,
    },

    /// The input violates an invariant of the store (for example a location in a folder
    /// of another account).
    #[error("invalid input: {0}")]
    Invalid(&'static str),

    /// A JSON column couldn't be encoded or decoded.
    #[error("JSON column: {0}")]
    Json(#[from] serde_json::Error),

    /// The store was closed, or its writer thread stopped.
    #[error("the store is closed")]
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

impl From<rusqlite_migration::Error> for Error {
    fn from(err: rusqlite_migration::Error) -> Self {
        match err {
            rusqlite_migration::Error::RusqliteError { err, .. } => Self::Sqlite(err),
            other => Self::Migration(other),
        }
    }
}
