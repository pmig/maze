//! The library half of the `mazemail` binary (layer L4): what the app, `--headless`,
//! the examples and the tests share.
//!
//! - [`startup`]: the core configuration for the standard directories and opening it
//!   with the local API's token authorizer.
//! - [`keys`]: which key store holds the master key (the OS keychain; a file only for
//!   development, behind `MAZEMAIL_INSECURE_DEV_KEYSTORE=1`).
//! - [`source`]: [`CoreMailSource`](source::CoreMailSource), the `mail_ui::MailSource`
//!   over [`CoreHandle`](mazemail_core::CoreHandle), and the mapping from the core's
//!   change events to the window's [`DataChanges`](mail_ui::DataChanges).
//! - [`approve`]: approvers that need no UI (the debug-only auto-approver for tests).
//! - [`agent_host`]: the `agent` crate's host over the core (conversations, thread
//!   exports, session tokens, proposals).

pub mod agent_host;
pub mod approve;
pub mod keys;
pub mod source;
pub mod startup;
