//! The Tantivy index: schema, opening, writing and searching.

mod compile;
mod doc;
mod execute;
mod schema;
mod searcher;
mod writer;

use std::path::Path;
use std::sync::Arc;

use tantivy::{Directory, Index, IndexReader, IndexSettings, ReloadPolicy};
use vault::BlockCipher;

pub use doc::{Attachment, IndexDoc, Language};
pub use execute::{ExecuteOptions, Executed, MetadataSource};
pub use searcher::{
    FacetOptions, Facets, Highlighter, Hit, IndexSearcher, RESULT_WINDOW, Scope, SearchOptions,
    Snippet, SortOrder, ThreadHit,
};
pub use writer::{CommitPolicy, Indexer};

use crate::directory::{DirectoryOptions, EncryptedDirectory};
use crate::error::{Error, Result};
use compile::Analyzers;
use schema::Fields;

/// Resources for the [`Indexer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexOptions {
    /// Indexing threads (default 1; use more for imports and rebuilds).
    pub threads: usize,
    /// Total indexing memory budget in bytes, split across threads (default 64 MiB;
    /// Tantivy needs at least 15 MB per thread).
    pub memory_budget: usize,
    /// When to commit.
    pub commit: CommitPolicy,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            threads: 1,
            memory_budget: 64 * 1024 * 1024,
            commit: CommitPolicy::default(),
        }
    }
}

/// An open search index.
#[derive(Clone)]
pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    fields: Arc<Fields>,
    analyzers: Analyzers,
}

impl std::fmt::Debug for SearchIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchIndex")
            .field("directory", self.index.directory())
            .finish_non_exhaustive()
    }
}

impl SearchIndex {
    /// Opens the index in `directory`, creating it when the directory holds none.
    ///
    /// Fails with [`Error::SchemaMismatch`] when the index was built with a different
    /// schema; the caller then deletes the directory and rebuilds from the store.
    pub fn open(directory: impl Into<Box<dyn Directory>>) -> Result<Self> {
        let (schema, fields) = schema::schema();
        let directory: Box<dyn Directory> = directory.into();
        let exists = Index::exists(&*directory).map_err(tantivy::TantivyError::from)?;
        let index = if exists {
            let index = Index::open(directory)?;
            if index.schema() != schema {
                return Err(Error::SchemaMismatch);
            }
            index
        } else {
            Index::create(directory, schema, IndexSettings::default())?
        };
        schema::register_tokenizers(&index);
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            fields: Arc::new(fields),
            analyzers: Analyzers::new(),
        })
    }

    /// Opens the encrypted index at `path` (`<data_dir>/mail/index/`).
    pub fn open_encrypted(
        path: impl AsRef<Path>,
        cipher: BlockCipher,
        options: DirectoryOptions,
    ) -> Result<Self> {
        Self::open(EncryptedDirectory::open(path, cipher, options)?)
    }

    /// An index in memory, for tests and previews.
    pub fn in_memory() -> Result<Self> {
        Self::open(tantivy::directory::RamDirectory::create())
    }

    /// Starts the (single) indexer.
    pub fn indexer(&self, options: &IndexOptions) -> Result<Indexer> {
        let threads = options.threads.max(1);
        let writer = self
            .index
            .writer_with_num_threads(threads, options.memory_budget)?;
        Ok(Indexer::new(
            writer,
            self.reader.clone(),
            Arc::clone(&self.fields),
            options.commit,
        ))
    }

    /// A searcher over the last commit seen by this handle.
    pub fn searcher(&self) -> IndexSearcher {
        IndexSearcher::new(
            self.reader.searcher(),
            Arc::clone(&self.fields),
            self.analyzers.clone(),
        )
    }

    /// Picks up commits made through another handle (the indexer reloads on commit).
    pub fn reload(&self) -> Result<()> {
        Ok(self.reader.reload()?)
    }

    /// Number of indexed messages as of the last reload.
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }
}

#[cfg(test)]
mod tests;
