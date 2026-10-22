//! The one pipeline that turns a message into stored state (PLAN §4.3, §4.5, §4.14).
//!
//! IMAP sync and the importers both hand messages to an [`Ingestor`]:
//!
//! 1. **Prepare** (caller's thread, CPU and file IO): parse with `mazemail_mime`, build
//!    the [`NewMessage`](store::NewMessage) summary (snippet, attachments, List-Id,
//!    Delivered-To, attachment structure), compute the [`IndexDoc`] for search, and
//!    stage the encrypted body blob ([`BlobStore::stage`](store::BlobStore::stage):
//!    written, not yet durable).
//!    [`Ingestor::prepare_raw`] and [`Ingestor::prepare_headers`] are `&self` and
//!    thread-safe, so importers run them on many cores.
//! 2. **Commit** (the store's writer thread): the batch's blobs are made durable
//!    together ([`BlobStore::persist`](store::BlobStore::persist); on the caller's
//!    thread for [`Ingestor::commit_blocking`], else by the writer right before its
//!    commit), then one write job per batch decides the
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
//! 2.5 KB, a quarter of them thread roots, one thread): about 109 messages/s one by
//! one, 1,840/s in batches of 50, 3,000/s at 200 and 3,400/s at 500. Blob durability
//! costs two device flushes (`F_FULLFSYNC`, a few ms each) per batch, plus about
//! 0.25 ms per blob for the staging write, rename and directory syncs; parsing is a
//! few µs. One message per batch is therefore bound by the flushes (as it was when
//! every blob was flushed on its own: 108–140 messages/s at any batch size). The
//! ignored `perf_scaling` test checks that commit cost stays flat as the account grows
//! (header-only, 16 µs per message from the first to the 50,000th).
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
