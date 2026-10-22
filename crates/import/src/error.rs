use std::io;
use std::path::PathBuf;

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors from importing and exporting.
///
/// Paths are included (they name files and folders, not message content); message
/// content, subjects and addresses never are.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Reading or writing a file failed.
    #[error("{context} {path}: {source}")]
    Io {
        /// What the importer was doing.
        context: &'static str,
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// The store rejected a read or write.
    #[error(transparent)]
    Store(#[from] store::Error),

    /// Ingesting a message failed as a whole (individual failures are counted in the
    /// statistics instead).
    #[error(transparent)]
    Ingest(#[from] ingest::Error),

    /// A Thunderbird address book or the body reuse index couldn't be read or written.
    #[error("database {path}: {source}")]
    Sqlite {
        /// The database file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: rusqlite::Error,
    },

    /// No Thunderbird profile was found where one was expected.
    #[error("no Thunderbird profile found in {0}")]
    NoProfile(PathBuf),

    /// The import target doesn't exist or is of the wrong kind.
    #[error("invalid import target: {0}")]
    InvalidTarget(String),

    /// The body reuse index was built with another key or is damaged.
    #[error("the body reuse index at {0} can't be opened with this key")]
    ReuseIndexKey(PathBuf),
}

impl Error {
    /// A closure that wraps an IO error with context and path, for `map_err`.
    pub(crate) fn io(
        context: &'static str,
        path: impl Into<PathBuf>,
    ) -> impl FnOnce(io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io {
            context,
            path,
            source,
        }
    }

    /// A closure that wraps an SQLite error with the database path, for `map_err`.
    pub(crate) fn sqlite(path: impl Into<PathBuf>) -> impl FnOnce(rusqlite::Error) -> Self {
        let path = path.into();
        move |source| Self::Sqlite { path, source }
    }
}
