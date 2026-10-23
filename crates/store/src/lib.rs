//! The Maze store (PLAN §4.3, §4.15, §4.17): the SQLCipher database with its schema
//! and migrations, the encrypted blob store, the send log, and typed repositories.
//!
//! # Files
//!
//! Everything lives in `<data_dir>/mail/` (mode `0700`, see [`DataLayout`]):
//! `mazemail.db` (SQLCipher, WAL), `sendlog.db` (SQLCipher), `blobs/` and `cache/`
//! (`MZB1`-sealed, zstd inside), `backups/` (pre-migration copies, same key) and `tmp/`.
//! Nothing with mail content is ever written in plaintext.
//!
//! # Threading model
//!
//! - **One writer.** [`Store::write`] sends a closure to a dedicated thread that owns
//!   the read-write connection. Jobs are batched into transactions of up to
//!   [`MAX_BATCH_WEIGHT`](writer::MAX_BATCH_WEIGHT) statements or
//!   [`MAX_BATCH_TIME`](writer::MAX_BATCH_TIME); each job runs in its own savepoint
//!   and its future resolves after the commit. Derived data (thread and folder
//!   aggregates, tags) is maintained inside the job, so readers never see it stale.
//!   The futures are runtime-agnostic.
//! - **Readers.** [`Store::reader`] lends one of a small pool of read-only connections
//!   for synchronous, cheap reads (all on the [`Queries`] trait). WAL lets them read
//!   while the writer writes.
//! - **Changes.** After every commit, a [`ChangeSet`] goes out on
//!   [`Store::subscribe`], for `mazemail_core`'s event batches.
//! - **Blobs** are staged by the caller's thread ([`BlobStore::stage`]) and referenced
//!   in a write job. They become durable in batches ([`BlobStore::persist`], two device
//!   flushes per batch): by the caller, or at the latest by the writer right before the
//!   commit, so a committed row never points at a blob that isn't on stable storage.
//!   Garbage collection runs on the writer thread (see [`blob`]).
//!
//! # Data model notes
//!
//! - **Schema** v1 is the base schema (`migrations/main/0001_schema_v1.sql`); v2 adds
//!   `account.uuid`; v3 adds `message_location.date` and the message list indexes.
//!   Migrations only ever add files; opening an older database backs it up first (see
//!   [`MigrationReport`]).
//! - **Message lists** (threads off): [`Queries::message_list_keys`] pages through the
//!   messages of a scope by `(date, message id)` like thread lists do, on
//!   `location_list` (folders, with partial indexes for unread and starred) and
//!   `message_list` (accounts). Each location carries its message's date for that.
//!   [`Queries::message_rows`] gives the visible rows, [`Queries::thread_tree_messages`]
//!   what the reply tree of a thread is built from.
//! - **Accounts** have a stable [`Account::uuid`] (random v4, unique, immutable, never
//!   reused) besides the numeric id; keychain secrets are keyed by it (`auth` crate).
//!   [`Queries::account_by_uuid`] looks it up.
//! - **Bodies**: header-first sync stores a message without a body; when the body
//!   arrives, [`WriteTx::update_body_summary`] sets the body and replaces what the
//!   headers could only guess (snippet, size, attachment flag, body structure), keeping
//!   the thread and folder attachment counts right. [`WriteTx::set_message_body`] only
//!   points the message at a blob.
//! - **Sender verification** reports live in `message.auth_json`
//!   ([`WriteTx::set_auth_json`]); an upsert only fills it if empty.
//! - **Blobs** referenced by a committed row are always durable (see [`blob`]).
//! - **Flags are per location** (IMAP flags belong to the copy in each mailbox). A
//!   message's effective flags ([`effective_flags`]) are `\Seen` only if every location
//!   is seen, any other flag if some location has it. Folder counts and
//!   `folder_thread` use each location's own flags; `thread` uses effective flags.
//! - **Gmail**: one message per `(account, X-GM-MSGID)`, labels are locations in label
//!   folders (`is_label`), threads follow `X-GM-THRID`.
//! - **Detached messages**: an expunge or UIDVALIDITY reset removes locations but keeps
//!   the message (and its body) until [`WriteTx::purge_detached`]; a re-sync finds it
//!   again by Message-ID. Detached messages count nowhere and never match a filter.
//! - **Tags** are lowercase IMAP keywords on the locations; `message_tag` is derived.
//! - **Threads**: [`ThreadAssignment`] lets the caller decide (JWZ in `mazemail_mime`)
//!   or use the built-in resolver; [`Queries::threads_for_rfc_ids`] is the lookup.
//! - **Search**: the index holds only immutable fields; [`Queries::filter_message_ids`]
//!   applies folder, flag, tag and role conditions to candidate ids, and
//!   [`Queries::message_ids_in_scope`] turns narrow scopes into id sets.
//! - **Message-ID lookups** (threading, detached messages) go through the
//!   `message_rfcid` index and filter by account afterwards (`+account_id` in the SQL):
//!   left to itself, SQLite without statistics scans the whole account through
//!   `message_account`, which made ingest quadratic. A unit test pins the query plans.
//!
//! # SQLCipher crypto backend
//!
//! `bundled-sqlcipher` uses CommonCrypto on Apple targets (no OpenSSL in the binary);
//! other targets use `bundled-sqlcipher-vendored-openssl`. Measured on an Apple
//! Silicon Mac (release, 1.1M messages): runtime is the same for both backends
//! within noise, while vendored OpenSSL adds about 23 s to a clean build (51 s vs 29 s
//! for this crate and its dependencies) and 3.2 MB to the binary.
//!
//! # Performance (1.1M messages, encryption on)
//!
//! Measured by the ignored `perf` integration test on an Apple Silicon Mac (release,
//! warm page cache, median of several runs): first keyset page of 200k thread keys from
//! a 1M-thread folder 6 ms, all 1M keys 36 ms, unread quick filter 6 ms, account scope
//! 6 ms, rows for 60 visible threads 0.05 ms, folder switch to a 100k folder (all keys
//! plus visible rows) 3 ms, metadata filter over 10k search candidates 5 ms, marking 10k
//! messages read 82 ms (commit included). Bulk insert runs at 20k–60k messages/s; the
//! database is about 600 bytes per message.
//!
//! # Example
//!
//! ```no_run
//! # async fn demo(vault: &vault::Vault) -> store::Result<()> {
//! use store::{ListFilter, ListScope, NewAccount, NewFolder, AccountKind, Page, Queries, SortOrder, Store, StoreConfig};
//! use mazemail_types::FolderRole;
//!
//! let store = Store::open(&StoreConfig::new("/tmp/maze-data"), vault)?;
//! let inbox = store
//!     .write(|w| {
//!         let account = w.create_account(&NewAccount {
//!             kind: AccountKind::Imap,
//!             name: "Work".into(),
//!             config_json: "{}".into(),
//!             ordinal: 0,
//!         })?;
//!         w.create_folder(&NewFolder::new(account, "INBOX", "Inbox").with_role(FolderRole::Inbox))
//!     })
//!     .await?;
//! let keys = store.reader()?.thread_list_keys(
//!     ListScope::Folder(inbox),
//!     SortOrder::NewestFirst,
//!     &ListFilter::default(),
//!     Page::first(200_000),
//! )?;
//! # let _ = keys;
//! # Ok(())
//! # }
//! ```

pub mod blob;
mod changes;
mod conn;
mod error;
// The only module with `unsafe` code: `fsync` and `fcntl` calls into libc on Apple
// platforms.
#[allow(unsafe_code)]
mod fsync;
mod layout;
mod migrate;
mod model;
mod reader;
mod repo;
mod sendlog;
mod store;
pub mod writer;

pub use blob::{BlobReader, BlobStore, GcOptions, GcReport, StagedBlob};
pub use changes::ChangeSet;
pub use error::{Error, Result};
pub use layout::{DATABASE_FILE, DataLayout, SENDLOG_FILE};
pub use migrate::{BACKUPS_KEPT, MigrationReport, SCHEMA_VERSION, SENDLOG_SCHEMA_VERSION};
pub use model::*;
pub use reader::Reader;
pub use repo::Queries;
pub use repo::agents::token_hash;
pub use repo::ops::{MAX_OP_ATTEMPTS, retry_backoff_secs};
pub use sendlog::SendLog;
pub use store::{Store, StoreConfig};
pub use writer::WriteTx;

/// The type of [`Account::uuid`], re-exported.
pub use uuid::Uuid;
