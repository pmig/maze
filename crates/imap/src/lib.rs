//! IMAP protocol layer of Maze (layer L1).
//!
//! This crate talks IMAP and nothing else: no store, no sync decisions. The sync
//! engine (crate `sync`) is written against the [`ImapSession`] trait, which has two
//! implementations:
//!
//! - [`ImapClient`]: a real server over Tokio + rustls, built on the `async-imap`
//!   crate (ADR-003; upstream for now, our fork later).
//! - [`fake::FakeImap`] (feature `fake`): an in-memory server for deterministic tests.
//!
//! # Extension support in [`ImapClient`]
//!
//! | Extension | Status |
//! | --- | --- |
//! | CONDSTORE (RFC 7162) | Supported: `SELECT (CONDSTORE)`, `MODSEQ`, `CHANGEDSINCE`, `HIGHESTMODSEQ`, `NOMODSEQ` |
//! | QRESYNC (RFC 7162) | Supported: `ENABLE QRESYNC`, `SELECT (QRESYNC …)` with `VANISHED (EARLIER)` and changed flags, `[CLOSED]`, `CHANGEDSINCE … VANISHED` |
//! | MOVE (RFC 6851) | Supported, with COPYUID from the untagged `OK`; emulated with `COPY` + `\Deleted` + `UID EXPUNGE` otherwise |
//! | UIDPLUS (RFC 4315) | Supported: `APPENDUID`, `COPYUID`, `UID EXPUNGE` |
//! | IDLE (RFC 2177) | Supported, with cancellation and timeout |
//! | SPECIAL-USE (RFC 6154) | Supported (`LIST … RETURN (SPECIAL-USE)` with LIST-EXTENDED) |
//! | ENABLE (RFC 5161), ID (RFC 2971), LITERAL+ (RFC 7888) | Supported |
//! | COMPRESS=DEFLATE (RFC 4978) | Via async-imap, opt-in ([`ImapConfig::compress`]); not yet exercised against a real server |
//! | Gmail `X-GM-EXT-1` | Supported: `X-GM-MSGID`, `X-GM-THRID`, `X-GM-LABELS` fetch and store |
//! | ESEARCH (RFC 4731) | Not used: imap-proto cannot parse `* ESEARCH`, which would break the stream |
//! | NOTIFY (RFC 5465) | Not implemented: needs response routing for other mailboxes |
//!
//! Known gaps, to be closed in the async-imap fork (ADR-003): a `PREAUTH` greeting
//! is rejected (async-imap cannot build a session without logging in); any response
//! imap-proto cannot parse (`ESEARCH`, `NAMESPACE`, IMAP4rev2-only `CAPABILITY`,
//! Gmail labels sent as literals) breaks the session with
//! [`ImapError::Protocol`]; `AUTHENTICATE` does not use SASL-IR; OAUTHBEARER is not
//! offered (XOAUTH2 covers Gmail and Microsoft 365).
//!
//! # Testing
//!
//! - Unit and transcript tests run [`ImapClient`] against a scripted server over an
//!   in-memory pipe, including TLS and STARTTLS with a generated certificate.
//! - `tests/stalwart.rs` runs against a real Stalwart server, downloaded and started
//!   by `tests/support/`; run with
//!   `MAZEMAIL_TEST_IMAP=1 cargo nextest run -p imap --run-ignored only`.
//!
//! # Logging
//!
//! This crate logs command verbs and counts only. async-imap itself logs every
//! command line, including `LOGIN` credentials, through the `log` crate at `trace`
//! level; never forward `async_imap=trace` into logs.

#![warn(missing_docs)]

mod client;
mod config;
mod error;
#[cfg(feature = "fake")]
pub mod fake;
pub mod gmail;
mod session;
mod types;
mod uid_set;
pub mod utf7;

pub use client::ImapClient;
pub use config::{ImapAuth, ImapConfig, Security, TlsVerification};
pub use error::{ImapError, Result};
pub use gmail::{GmailLabel, LabelOp};
pub use session::ImapSession;
pub use types::{
    AppendUid, BodyPart, BodySummary, Caps, ChangeSet, CopyUid, FlagChange, FlagOp, FolderStatus,
    GmailMeta, HeaderFetch, IdleOutcome, QresyncArgs, RemoteFolder, SelectInfo, SessionEvent,
    StoreMode,
};
pub use uid_set::{ParseUidSetError, UidRange, UidSet};

/// Re-exported so callers can build [`TlsVerification::CustomRoots`].
pub use rustls::pki_types::CertificateDer;
/// Re-exported for [`ImapSession::idle`].
pub use tokio_util::sync::CancellationToken;
