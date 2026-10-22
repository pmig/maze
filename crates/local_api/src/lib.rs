//! Maze's local API: the socket that the `mazemail` CLI, scripts and agents use to
//! reach mail (PLAN §4.11, CONCEPT §10.2–10.5).
//!
//! The app (or `mazemail --headless`) runs a [`LocalApiServer`] on
//! `<data_dir>/mazemail.sock`; the CLI is a thin [`LocalApiClient`] and never opens
//! the encrypted store. Because the socket hands out decrypted mail, it is part of
//! encryption at rest (§4.17):
//!
//! - **Peer check.** Only processes running as our uid may connect
//!   ([`check_peer_uid`]); their pid and executable go into approvals and the audit log.
//! - **Tokens.** Every request that touches mail needs a session token
//!   (`MAZEMAIL_SESSION`, `mazemail_s_…`). Without one only `help`, `status` and
//!   `login` work. Maze stores only BLAKE3 hashes ([`TokenStore`]).
//! - **Profiles.** A token's [`Profile`] decides per command whether it runs, becomes
//!   a proposal, waits for approval or is refused ([`access`], [`Permission`]).
//! - **Approvals.** `login` and `ask` commands wait for the user through an
//!   [`Approver`]. Tokens can't mint tokens: a token holder's `login` asks as well.
//! - **AI policy.** Accounts whose policy excludes agents are never in a token's
//!   effective scope ([`AccountPolicy`]).
//! - **Audit.** Every request, allowed or not, produces an [`AuditEvent`] with ids
//!   only, never mail content.
//!
//! The [`registry`] defines every command once (CLI arguments, wire arguments,
//! permission, schemas, examples) and generates AGENTS.md ([`registry::agents_md`]).
//! See [`protocol`] for the wire format, including streaming progress frames and
//! version negotiation.
//!
//! `mazemail_core` plugs in through [`CommandHandler`], [`TokenStore`],
//! [`AccountPolicy`] and [`AuditSink`]; in-memory implementations are provided for
//! tests and development.
//!
//! This crate is layer L2 and depends only on `mazemail_types` and `settings`.

mod approval;
mod audit;
mod client;
mod codec;
mod dispatch;
mod handler;
mod peer;
mod permission;
mod policy;
pub mod protocol;
mod pubid;
pub mod registry;
mod server;
mod token;

pub use approval::{
    ApprovalKind, ApprovalRequest, Approver, Decision, DenyAllApprover, TerminalApprover,
};
pub use audit::{AuditEvent, AuditOutcome, AuditSink, MemoryAuditSink, TracingAuditSink};
pub use client::{API_ADDR_ENV, ClientError, Endpoint, LocalApiClient};
pub use codec::{Frame, FrameCodec};
pub use dispatch::{DispatchConfig, Dispatcher, Services};
pub use handler::{CommandError, CommandHandler, CommandOutput, ExecutionMode, RequestContext};
pub use peer::{ParentProcess, PeerInfo, PeerRejected, ProcessInfo, check_peer_uid, current_uid};
pub use permission::{Access, Permission, access};
pub use policy::{AccountPolicy, EffectiveScope, StaticAccountPolicy, excluded_accounts};
pub use protocol::{
    ApiError, ErrorCode, ExitCode, PROTOCOL_VERSION, Progress, ProgressCode, Request, Response,
    ResponseBody,
};
pub use registry::Command;
pub use server::{LocalApiServer, ServerConfig, ServerError};
pub use token::{
    CreatedBy, Grant, MemoryTokenStore, Profile, SESSION_ENV, SessionToken, TOKEN_PREFIX,
    TokenError, TokenHash, TokenId, TokenIdError, TokenInfo, TokenRecord, TokenRejection,
    TokenScope, TokenStore, TokenStoreError, Ttl, TtlError, mint, verify,
};
