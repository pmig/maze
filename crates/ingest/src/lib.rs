//! The one pipeline that turns a message into stored state (PLAN §4.3, §4.5, §4.14).
//!
//! IMAP sync and the importers both hand messages to an [`Ingestor`]:
//!
//! 1. **Prepare** (caller's thread, CPU and file IO): parse with `mazemail_mime`, build
//!    the [`NewMessage`](store::NewMessage) summary (snippet, attachments, List-Id,
//!    Delivered-To, attachment structure), compute the [`IndexDoc`] for search, and
//!    write the encrypted body blob ([`BlobStore::put`](store::BlobStore::put)).
//!    [`Ingestor::prepare_raw`] and [`Ingestor::prepare_headers`] are `&self` and
//!    thread-safe, so importers run them on many cores.
//! 2. **Commit** (the store's writer thread): one write job per batch decides the
//!    thread (Gmail `X-GM-THRID`, else the store's Message-ID lookups fed to
//!    [`join_thread`](mazemail_mime::thread::join_thread), merging threads a message
//!    connects), upserts the message with its locations and points it at its body.
//! 3. **Index**: after the commit, every [`IndexDoc`] goes to the [`IndexSink`].
//!
//! Header-first sync uses [`Ingestor::ingest_headers`] and later
//! [`Ingestor::attach_body`] once the body arrives.
//!
//! # Batches
//!
//! [`Ingestor::commit`] runs a whole batch in one store write job, which the store
//! commits as one transaction (a job heavier than
//! [`MAX_BATCH_WEIGHT`](store::writer::MAX_BATCH_WEIGHT) runs alone). If the job fails,
//! the batch is retried message by message so one bad message only fails itself.
//! [`DEFAULT_BATCH`] is the size the importers use.
//!
//! Measured by the ignored `perf` test (release, Apple Silicon, 4,000 messages of
//! 2.5 KB, a quarter of them thread roots): the commit costs 1.0–1.3 ms per message one
//! by one, 0.42 ms in batches of 50, 0.30 ms at 200 and 0.22 ms at 500. Preparing is
//! dominated by [`BlobStore::put`](store::BlobStore::put), which fsyncs every blob
//! (`F_FULLFSYNC` on macOS): about 8 ms per blob, and about 190 blobs/s from 8 threads,
//! against a few µs for parsing. Bulk ingestion with bodies is therefore bound at
//! roughly 120–190 messages/s by blob durability, not by the database.
//!
//! # Search coupling
//!
//! This crate doesn't depend on `search`: [`IndexDoc`] and [`IndexSink`] are the
//! contract, `mazemail_core` adapts them to the index. [`NoopIndexSink`] and
//! [`MemoryIndexSink`] are for tools and tests.
//!
//! # Body reuse
//!
//! [`HeaderFingerprint`] and [`BodyLookup`] let the first IMAP sync take bodies from an
//! imported Thunderbird offline store instead of the network (PLAN §4.14): sync
//! computes the fingerprint of the server's header and asks the lookup (implemented by
//! the `import` crate, wired up by `mazemail_core`).
//!
//! # Privacy
//!
//! Prepared messages and index documents print only ids and counts in `Debug`.

mod doc;
mod error;
mod fingerprint;
mod ingestor;

pub use doc::{IndexAddress, IndexDoc, IndexLanguage, IndexSink, MemoryIndexSink, NoopIndexSink};
pub use error::{Error, Result};
pub use fingerprint::{BodyLookup, HeaderFingerprint};
pub use ingestor::{
    BodySummary, DEFAULT_BATCH, GmailMeta, Ingested, Ingestor, MAX_INDEX_BODY_BYTES, MessageMeta,
    PreparedMessage, RawItem,
};
