//! Message parsing, threading, HTML sanitizing and the reader model for Maze.
//!
//! This crate is layer L0: pure functions over bytes and strings, no IO, no async.
//!
//! - [`parse`] / [`parse_headers_only`] turn raw RFC 5322 bytes into a typed
//!   [`ParsedMessage`] / [`MessageHeaders`] (on top of `mail-parser`).
//! - [`thread`] groups messages into conversations (JWZ with subject fallback), as a
//!   batch or incrementally.
//! - [`html`] sanitizes message HTML (`ammonia` with a mail allowlist), removes
//!   tracking pixels, rewrites resources to a resource origin and builds the CSP
//!   (PLAN §4.8).
//! - [`reader`] converts HTML or plain text into a [`ReaderDoc`] for native
//!   rendering.
//! - [`client`] detects the sender's mail client from `User-Agent`, `X-Mailer` and
//!   other header traces.
//! - [`lang`] gives the search index a language hint.
//! - [`compose`] builds outgoing mail: Markdown to HTML, `multipart/alternative` via
//!   `mail-builder`, reply/forward subjects, `References` and quoting (HTML quotes
//!   become Markdown with `htmd`).
//!
//! # Privacy
//!
//! Types holding message content implement `Debug` with the content redacted, so
//! `tracing::debug!(?msg)` can't leak subjects, addresses or bodies. Log them through
//! [`mazemail_types::Sensitive`] anyway.
//!
//! # Performance
//!
//! Everything is linear in the input apart from bounded factors (reference cap,
//! cycle-check walk, table nesting, all documented where they apply). Opening a
//! cached message is budgeted at 30 ms (CONCEPT §3); the `timing` integration test
//! runs the full pipeline on a large synthetic newsletter and fails far above that
//! budget, so regressions to quadratic behavior show up in CI.

pub mod client;
pub mod compose;
pub mod html;
pub mod lang;
mod parse;
pub mod reader;
pub mod subject;
pub mod thread;

pub use client::{MailClient, detect_client};
pub use lang::{Lang, detect_language};
pub use parse::{
    AttachmentMeta, Disposition, MessageHeaders, ParseError, ParsedMessage, SNIPPET_CHARS,
    TextBody, parse, parse_headers_only,
};
pub use reader::ReaderDoc;
