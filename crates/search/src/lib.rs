//! Full-text search for Maze (PLAN §4.4, CONCEPT §13).
//!
//! - [`query`]: the Gmail-like query language, parsed with `winnow` into a typed
//!   [`Query`](query::Query) AST, printed back in canonical form, and translated to IMAP
//!   `SEARCH` and Gmail `X-GM-RAW` for server fallback.
//! - [`plan`]: splits a query into the part the index answers ([`TantivyPart`]) and the
//!   part SQLite answers ([`MetadataFilter`]: folders, tags, flags, account), with exact
//!   semantics for ORs across both.
//! - [`SearchIndex`], [`Indexer`] and [`IndexSearcher`]: the Tantivy index holding only
//!   immutable message data, its commit policy, ranked and thread-grouped search, facets
//!   and snippets.
//! - [`EncryptedDirectory`]: a Tantivy `Directory` that keeps every index file encrypted
//!   with the vault's `MZI1` block format, reading through a shared, bounded
//!   [`BlockCache`] (PLAN §4.17).
//!
//! Queries, indexed text and snippets are message content: none of it is logged.

mod directory;
mod error;
mod index;

pub mod plan;
pub mod query;

pub use directory::{
    BlockCache, BlockCacheStats, DEFAULT_CACHE_BYTES, DirectoryOptions, EncryptedDirectory,
};
pub use error::{Error, Result};
pub use index::{
    Attachment, CommitPolicy, ExecuteOptions, Executed, FacetOptions, Facets, Highlighter, Hit,
    IndexDoc, IndexOptions, IndexSearcher, Indexer, Language, MetadataSource, RESULT_WINDOW, Scope,
    SearchIndex, SearchOptions, Snippet, SortOrder, ThreadHit,
};
pub use plan::{MetadataFilter, PlanContext, QueryPlan, TantivyPart};
