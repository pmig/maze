use std::io;

use crate::plan::PlanError;

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors from the search index.
///
/// None of the variants carry message content or query text.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Tantivy failed (I/O, corrupted or undecryptable index files, bad request).
    #[error("search index: {0}")]
    Index(#[from] tantivy::TantivyError),

    /// The index directory couldn't be opened.
    #[error("search index directory: {0}")]
    OpenDirectory(#[from] tantivy::directory::error::OpenDirectoryError),

    /// The index on disk was built with another schema and has to be rebuilt.
    #[error("the search index was built by another version of Maze and must be rebuilt")]
    SchemaMismatch,

    /// The query couldn't be planned.
    #[error(transparent)]
    Plan(#[from] PlanError),

    /// The metadata source (SQLite) failed while executing a plan.
    #[error("metadata filter: {0}")]
    Metadata(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// Plain I/O.
    #[error(transparent)]
    Io(#[from] io::Error),
}
