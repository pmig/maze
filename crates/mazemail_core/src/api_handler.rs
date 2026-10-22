//! The seam between core and the `local_api` crate (PLAN §4.11, CONCEPT §10).
//!
//! Every mutation goes through [`CoreHandle::apply`](crate::CoreHandle::apply) with an
//! [`Origin`](crate::Origin). For [`Origin::Agent`](crate::Origin::Agent), `apply` asks
//! the [`CommandAuthorizer`] **before anything changes**:
//!
//! - [`Authorization::Allow`]: the command runs like any other and is audited with
//!   origin `agent:<token id>`.
//! - [`Authorization::Deny`]: `apply` fails with
//!   [`Error::PermissionDenied`](crate::Error::PermissionDenied); the refusal is audited.
//! - [`Authorization::Propose`]: nothing is applied; the command is stored as a pending
//!   proposal for review (CONCEPT §10.3), and the receipt says so.
//!
//! # Wiring local_api in
//!
//! `local_api` resolves a presented token to its profile and scope (`TokenStore`),
//! decides per command (read-only, triage, writer; per-account access from settings),
//! and may ask the user (`Approver`). It implements [`CommandAuthorizer`] with that
//! logic and is passed in [`CoreConfig::authorizer`](crate::CoreConfig::authorizer).
//! Its `CommandHandler` implementation for the socket server calls the `CoreHandle`
//! query methods and `apply(…, Origin::Agent { token })` / `apply(…, Origin::Cli)`
//! directly; its `AuditSink` can read [`store::Queries::audit_log`] entries that `apply`
//! writes. Until then core runs with [`DenyAgents`], which refuses every agent command.

use std::fmt;

use store::TokenId;

use crate::commands::MailCommand;

/// The decision for one agent command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authorization {
    /// Apply it.
    Allow,
    /// Refuse it.
    Deny {
        /// Why, for the agent and the activity log (no message content).
        reason: String,
    },
    /// Don't apply it; store it as a proposal for the user to review.
    Propose {
        /// The proposal title shown in the review panel.
        title: String,
        /// The agent's stated reason, if any.
        reason: Option<String>,
    },
}

/// Decides whether an agent may run a command. Must be fast and must not block on the
/// user: anything that needs a human answer is [`Authorization::Propose`].
pub trait CommandAuthorizer: Send + Sync + fmt::Debug {
    /// Decide for `command` issued with `token`.
    fn authorize(&self, token: TokenId, command: &MailCommand) -> Authorization;
}

/// Refuses every agent command. The default until `local_api` provides the real policy.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAgents;

impl CommandAuthorizer for DenyAgents {
    fn authorize(&self, _token: TokenId, _command: &MailCommand) -> Authorization {
        Authorization::Deny {
            reason: "no agent access is configured".to_owned(),
        }
    }
}
