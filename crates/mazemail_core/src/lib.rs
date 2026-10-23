//! The Maze core (PLAN §4.1, §4.2): one headless facade over the store, the search
//! index, sender verification and settings, used by the GUI, the CLI, headless mode
//! and tests. No GPUI; everything runs on Tokio.
//!
//! # Lifecycle
//!
//! [`CoreHandle::open`] takes the single-instance lock on the data directory, arms the
//! [crash guard](crash_guard), reads the master key (on a blocking thread, so the window
//! can open meanwhile), handles a [lost key](lost_key), opens the store (running
//! migrations with their backup), the encrypted search index and the settings (with a
//! watcher), and starts three tasks: the [indexer](indexing), the [event
//! batcher](events) and the change forwarder that keeps the caches in step with the
//! store. [`CoreHandle::mark_healthy`] disarms the crash guard once the first window has
//! rendered; [`CoreHandle::shutdown`] stops everything cleanly.
//!
//! # API
//!
//! | Area     | Methods |
//! | -------- | ------- |
//! | Queries  | [`accounts`](CoreHandle::accounts), [`folders`](CoreHandle::folders), [`thread_list`](CoreHandle::thread_list), [`rows`](CoreHandle::rows): synchronous, cheap, no network |
//! | Lists    | [`message_list`](CoreHandle::message_list), [`message_rows`](CoreHandle::message_rows) (threads off), [`thread_tree`](CoreHandle::thread_tree), [`thread_message_counts`](CoreHandle::thread_message_counts) (expanded threads) |
//! | Messages | [`message`](CoreHandle::message), [`verify_auth`](CoreHandle::verify_auth) |
//! | Search   | [`search`](CoreHandle::search), [`quick_filter_body`](CoreHandle::quick_filter_body), [`sort_threads`](CoreHandle::sort_threads) / [`sort_messages`](CoreHandle::sort_messages) (search results by any column) |
//! | Commands | [`apply`](CoreHandle::apply), [`undo`](CoreHandle::undo) |
//! | Compose  | [`identities`](CoreHandle::identities), [`new_draft`](CoreHandle::new_draft), [`reply_draft`](CoreHandle::reply_draft), [`forward_draft`](CoreHandle::forward_draft), [`create_draft`](CoreHandle::create_draft)/[`update_draft`](CoreHandle::update_draft)/[`delete_draft`](CoreHandle::delete_draft), [`send_draft`](CoreHandle::send_draft) (see [`compose`], [`outgoing`]) |
//! | Events   | [`events`](CoreHandle::events), [`settings_watch`](CoreHandle::settings_watch) |
//! | Settings | [`settings`](CoreHandle::settings), [`set_favorite_folder`](CoreHandle::set_favorite_folder) (writes `settings.json`) |
//! | Index    | [`index_sink`](CoreHandle::index_sink), [`reindex_all`](CoreHandle::reindex_all), [`check_index`](CoreHandle::check_index) |
//!
//! # Seams for crates built later
//!
//! Each integration sits behind one small module, so wiring it is mechanical:
//!
//! - **sync**: [`sync_bridge`] ([`SyncBridge`] for body fetches, refresh and op
//!   wake-ups; [`SyncStatusSink`] for status) and [`ops`] (the pending-op payload,
//!   `sync::ops::OpPayload`, and how commands follow its enqueue contract).
//! - **ingest**: [`CoreIndexSink`] implements `ingest::IndexSink` (see [`indexing`]);
//!   ingest writes through [`CoreHandle::store`].
//! - **local_api**: [`api`] implements `local_api`'s `CommandHandler`, `TokenStore`,
//!   `AccountPolicy` and `AuditSink` over this handle, plus the [`CommandAuthorizer`]
//!   of [`api_handler`] that holds agent commands to their token's profile.
//! - **import**: [`api`]'s `import` command runs the `import` crate (same layer; it
//!   writes through [`CoreHandle::store`] and indexes through [`CoreIndexSink`]).
//! - **auth**: nothing in core; its token provider and secret store are handed to the
//!   sync engine, which core only sees through [`SyncBridge`].
//!
//! # Privacy
//!
//! Types with message content implement `Debug` without it, and nothing here logs
//! subjects, addresses, bodies or queries.

mod config;
mod error;
mod handle;
mod lock;
mod metadata;
mod row_cache;
mod runtime;

pub mod accounts;
pub mod api;
pub mod api_handler;
pub mod commands;
pub mod compose;
pub mod correspondents;
pub mod crash_guard;
mod credentials;
pub mod events;
pub mod folders;
pub mod global_search;
pub mod indexing;
pub mod lost_key;
pub mod message;
pub mod message_list;
pub mod ops;
pub mod outgoing;
pub mod reader_text;
pub mod sync_bridge;
pub mod sync_engine;
pub mod sync_ingest;
pub mod thread_list;
pub mod thread_tree;

pub use accounts::{
    AccountAuth, AccountConfig, AccountErrorKind, AccountInfo, AddImapAccount, AddOAuthAccount,
    ConnectionTest, ImapSecurity, ImapServer, ServerSuggestion, suggest_imap_server,
};
pub use api_handler::{Authorization, CommandAuthorizer, DenyAgents};
pub use commands::{CommandOutcome, CommandReceipt, MailCommand, Origin};
pub use compose::{ComposeErrorKind, IdentityInfo, ReplyKind, SendResult};
pub use config::{CoreConfig, RuntimeConfig};
pub use crash_guard::{CrashGuardStatus, ROLLBACK_THRESHOLD, SENTINEL_FILE};
pub use error::{Error, Result};
pub use events::{CoreEventBatch, DEFAULT_EVENT_WINDOW, FolderCounts};
pub use folders::{FolderNode, FolderNodeKind, UNIFIED_ROLES, folder_path, unified_name};
pub use global_search::{SearchFacets, SearchOpts, SearchResults, SearchSort, SearchThread};
pub use handle::CoreHandle;
pub use indexing::{CoreIndexSink, IndexCheck, IndexDocument, IndexProgress, ReindexReport};
pub use lock::LOCK_FILE;
pub use lost_key::{LostKey, RecoveryFailed};
pub use message::{AuthStatus, Body, MessageView, RENDERER_VERSION, RemoteContent, RenderedBody};
pub use outgoing::{SmtpServerSettings, SmtpSuggestion, smtp_default_port, suggest_smtp_server};
pub use row_cache::{DEFAULT_ROW_CACHE_CAPACITY, RowCacheStats};
pub use sync_bridge::{
    FetchPriority, NoSync, RefreshScope, SyncBridge, SyncState, SyncStatus, SyncStatusSink,
};
pub use sync_engine::{ImapConnector, NetworkImapConnector, SyncEngineConfig};
pub use sync_ingest::CoreIngest;

pub use mazemail_types::DraftId;
/// Compose types that are part of this API.
pub use mazemail_types::compose::{Draft, DraftAttachment, DraftOrigin};
/// SMTP types that are part of this API.
pub use smtp::{Mailer, SmtpError};

/// `auth` types that are part of this API.
pub use auth::{ProviderKind, SecretStore};
pub use message_list::{MessageListRow, MessageListSnapshot};
pub use thread_list::{ListQuery, QuickFilter, ThreadListSnapshot};
pub use thread_tree::TreeNode;

/// Store types that are part of this API.
pub use store::{
    Account, AccountKind, ListKey, ListOrder, ListScope, MessageKey, MessageRow, OpId, ProposalId,
    RuleId, SortField, SortOrder, Tag, ThreadRow, TokenId,
};

/// Search types that are part of this API.
pub use search::Snippet;
