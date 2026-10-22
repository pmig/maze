//! The audit trail: one [`AuditEvent`] per request, allowed or not (PLAN §4.11).
//!
//! Events carry ids, never mail content: no queries, subjects, addresses or bodies.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::protocol::ErrorCode;
use crate::token::TokenId;

/// One audited request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// When the request finished.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// The request id on its connection.
    pub request_id: u64,
    /// The token used, if any (valid or not, as long as it was recognized).
    pub token: Option<TokenId>,
    /// Peer process id.
    pub pid: Option<i32>,
    /// Peer executable path.
    pub exe: Option<PathBuf>,
    /// The command's wire name, or `<invalid>` for frames that named no known command.
    pub cmd: String,
    /// What happened.
    pub outcome: AuditOutcome,
    /// Whether the user approved the request in a dialog.
    pub approved: bool,
    /// Public ids of the messages, threads, drafts, … the command read or changed.
    pub affected: Vec<String>,
}

/// The result of an audited request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditOutcome {
    /// The command ran.
    Executed,
    /// The command was turned into a proposal.
    Proposed,
    /// Refused before running (not signed in, permission or approval denied, bad
    /// request).
    Rejected {
        /// Why.
        code: ErrorCode,
    },
    /// The command ran and failed.
    Failed {
        /// How.
        code: ErrorCode,
    },
}

/// Receives audit events. `mazemail_core` writes them to the encrypted audit log and
/// the bottom dock.
///
/// `record` is called on the request path, so implementations must not block: queue
/// the event and return.
pub trait AuditSink: Send + Sync {
    /// Records one event.
    fn record(&self, event: AuditEvent);
}

/// Logs events with `tracing` (they contain no mail content).
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingAuditSink;

impl AuditSink for TracingAuditSink {
    fn record(&self, event: AuditEvent) {
        tracing::info!(
            target: "mazemail::audit",
            request = event.request_id,
            token = event.token.as_ref().map(TokenId::as_str),
            pid = event.pid,
            exe = ?event.exe,
            cmd = %event.cmd,
            outcome = ?event.outcome,
            approved = event.approved,
            affected = event.affected.len(),
            "local api request"
        );
    }
}

/// Keeps events in memory, for tests.
#[derive(Debug, Default)]
pub struct MemoryAuditSink {
    events: Mutex<Vec<AuditEvent>>,
}

impl MemoryAuditSink {
    /// An empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything recorded so far.
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl AuditSink for MemoryAuditSink {
    fn record(&self, event: AuditEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}
