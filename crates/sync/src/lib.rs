//! The Maze sync engine (layer L1, PLAN §4.5, CONCEPT §7).
//!
//! One actor per account ([`spawn_account`]) keeps the local store in step with the
//! IMAP server:
//!
//! - **Folder list**: `LIST` → local folders with SPECIAL-USE roles (name-based
//!   guesses for servers without it), subscriptions, renames (detected by
//!   UIDVALIDITY) and deletions. On Gmail only `[Gmail]/All Mail`, Trash and Spam
//!   are mailboxes; every other folder is a label folder whose contents come from
//!   All Mail's `X-GM-LABELS` (ADR-007).
//! - **Folder passes** (see [`pass`](crate) docs in the source): QRESYNC, else
//!   CONDSTORE with a `UID SEARCH ALL` expunge diff, else a chunked flag diff; new
//!   mail newest-first in batches of 200 through the shared ingest pipeline
//!   ([`MessageIngest`]); UIDVALIDITY changes reset the folder (bodies stay).
//!   UIDNEXT/HIGHESTMODSEQ are stored only after a pass committed, so an interrupted
//!   pass is repeated idempotently.
//! - **Bodies**: the offline policy ([`OfflinePolicy`]) feeds a background queue
//!   (≤ 2 downloads at a time); [`AccountHandle::fetch_body`] for an opened message
//!   jumps the queue and fetches only the text parts of messages over 256 KB. The
//!   Thunderbird body-reuse hook ([`MessageIngest::lookup_reused_body`]) is asked first.
//! - **Change detection**: IDLE on INBOX (Gmail: All Mail) re-issued every 25 min;
//!   `STATUS` polling of the other folders every 2 min and of the open folder every
//!   30 s, with a pass only when UIDNEXT/HIGHESTMODSEQ changed; reconnects with
//!   exponential backoff and jitter; [`SyncCommand::NetworkChanged`] and
//!   [`SyncCommand::Wake`] reconnect and run one round. No timer fires while there
//!   is nothing to do.
//! - **Pending ops**: the store's queue is replayed in order and translated to IMAP or
//!   Gmail semantics. The payload format is [`ops::OpPayload`] (documented there).
//! - **Activity**: [`AccountHandle::activity`] snapshots what the engine is doing
//!   (folder passes with their step and header progress, op replay, the body queue,
//!   the last finished round) from in-memory state the actor keeps; no events.
//! - **Events**: [`SyncEventBatch`]es on a broadcast channel, coalesced per ~16 ms,
//!   with folder, message and count changes, body arrivals, op progress and the
//!   [`SyncStatus`] (with human text).
//!
//! # Dependencies
//!
//! Same-layer dependencies (allowed by AGENTS.md when noted): `imap` (the protocol),
//! `store` (state) and `settings` (only for [`OfflinePolicy`], so the setting and the
//! engine share one type). Credentials come through [`CredentialProvider`], which
//! `auth` implements; the real ingest pipeline is plugged in by `mazemail_core`.
//!
//! # Privacy
//!
//! The engine logs ids, counts and server error texts only; subjects, addresses and
//! folder names never reach the log.
//!
//! # Performance
//!
//! Measured by `tests/perf.rs` on an Apple Silicon Mac (release, encrypted store, the
//! in-memory fake server): the initial header sync of a 50,000-message folder takes
//! 1.8 s (≈ 27,000 messages/s) when ingest assigns threads itself. With the store's
//! built-in References thread resolution (`ThreadAssignment::Resolve`, what
//! [`StoreIngest`] uses) it degrades to 1.1 s for 5,000 and 16 s for 20,000
//! messages: `store`'s `threads_for_rfc_ids` scans the account per lookup (see the
//! crate's follow-ups).

#![warn(missing_docs)]

mod activity;
mod actor;
mod backoff;
mod bodies;
mod config;
mod connect;
mod error;
mod events;
mod folders;
mod gmail;
mod handle;
mod ingest;
pub mod ops;
mod pass;
mod replay;
mod shared;

pub use activity::{FolderActivity, FolderPhase, SyncActivity};
pub use config::{AccountSyncConfig, Clock, OfflinePolicy, SyncTiming};
pub use connect::{
    BoxSession, CredentialError, CredentialProvider, ImapSessionFactory, SessionFactory,
};
pub use error::{Result, SyncError};
pub use events::{FolderCounts, OpEvent, SyncEvent, SyncEventBatch, SyncStatus};
pub use handle::{AccountHandle, BodyPriority, SyncCommand, SyncDeps, SyncScope, spawn_account};
pub use ingest::{
    HeaderInput, IngestError, Ingested, MessageIngest, ReuseQuery, StoreIngest, new_message,
};
