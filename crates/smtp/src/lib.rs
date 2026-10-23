//! Maze mail submission (PLAN §4.5), layer L1.
//!
//! - [`LettreMailer`] submits a stored message with lettre's `AsyncSmtpTransport`
//!   (Tokio, rustls with the platform certificate verifier) and `send_raw`: TLS on 465,
//!   STARTTLS on 587 (never falling back to plain text), `AUTH PLAIN`/`LOGIN` with the
//!   account password or `AUTH XOAUTH2` with an OAuth access token (the string checked
//!   by `auth::sasl::xoauth2`).
//! - [`SmtpError`] classifies failures; Exchange's `554 5.2.252 SendAsDenied` becomes
//!   [`SmtpError::SendAsDenied`] with the fix-it text of CONCEPT §8.6.
//! - [`Outbox`] sends queued items: it claims an item, checks the send log for its
//!   Message-ID, submits it through the identity's [`Route`] and records the acceptance
//!   in the send log **before** the outbox row (`store::Store::record_sent_with`). A
//!   crash between the server's `250` and the outbox update is found by
//!   [`Outbox::recover`] at the next start, so nothing is sent twice. Temporary failures
//!   retry with backoff, permanent ones mark the item failed.
//!
//! Depends on `store` (same layer, for the outbox and the send log) and `auth` (same
//! layer, for the XOAUTH2 string). Messages are built elsewhere (`mazemail_mime`).
//! Passwords and tokens are `Sensitive` and never logged; neither are addresses.

mod config;
mod error;
mod mailer;
mod outbox;

pub use config::{Credentials, DEFAULT_TIMEOUT, Security, SmtpConfig};
pub use error::{SEND_AS_DENIED_EXPLANATION, SmtpError};
pub use mailer::{LettreMailer, Mailer};
pub use outbox::{
    Delivery, MAX_SEND_ATTEMPTS, NoHook, Outbox, OutboxError, Recovery, Route, Routes, SentHook,
};
pub use store::OutboxEnvelope;
