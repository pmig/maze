//! The interface to `mazemail_core`: [`CommandHandler`] executes mail commands that
//! passed authentication, the permission matrix and any approval.

use std::sync::Arc;

use async_trait::async_trait;
use mazemail_types::Sensitive;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::peer::PeerInfo;
use crate::policy::EffectiveScope;
use crate::protocol::{ApiError, ErrorCode, Progress, Response};
use crate::registry::Command;
use crate::token::TokenRecord;

/// How a state-changing command must be carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionMode {
    /// Apply it (`core.apply(cmd, Origin::Agent { token })`).
    Execute,
    /// Record it as a proposal for the review UI instead (Triage profile).
    Propose,
}

/// Everything the handler knows about a request.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// The request id on its connection.
    pub request_id: u64,
    /// The connecting process.
    pub peer: Arc<PeerInfo>,
    /// The verified token. Always present for mail commands.
    pub token: Option<TokenRecord>,
    /// What the request may touch. The handler must check every account, folder,
    /// message and thread against it, and treat out-of-scope ids as `not_found`.
    pub scope: EffectiveScope,
    /// Execute or propose.
    pub mode: ExecutionMode,
    /// Whether the user approved this request in a dialog.
    pub approved: bool,
    progress: ProgressSender,
}

impl RequestContext {
    /// A context whose progress updates go nowhere (tests, in-process callers).
    pub fn new(
        request_id: u64,
        peer: Arc<PeerInfo>,
        token: Option<TokenRecord>,
        scope: EffectiveScope,
        mode: ExecutionMode,
    ) -> Self {
        Self {
            request_id,
            peer,
            token,
            scope,
            mode,
            approved: false,
            progress: ProgressSender::detached(request_id),
        }
    }

    pub(crate) fn with_progress(mut self, progress: ProgressSender, approved: bool) -> Self {
        self.progress = progress;
        self.approved = approved;
        self
    }

    /// Sends a progress frame to the client (for long-running commands). Returns
    /// `false` if the client is gone, which a handler may take as a hint to stop.
    pub async fn progress(&self, progress: Progress) -> bool {
        self.progress.send(progress).await
    }
}

/// Sends progress frames for one request.
#[derive(Debug, Clone)]
pub(crate) struct ProgressSender {
    id: u64,
    tx: Option<mpsc::Sender<Response>>,
}

impl ProgressSender {
    pub(crate) fn new(id: u64, tx: mpsc::Sender<Response>) -> Self {
        Self { id, tx: Some(tx) }
    }

    pub(crate) fn detached(id: u64) -> Self {
        Self { id, tx: None }
    }

    pub(crate) async fn send(&self, progress: Progress) -> bool {
        match &self.tx {
            Some(tx) => tx.send(Response::progress(self.id, progress)).await.is_ok(),
            None => false,
        }
    }
}

/// A successful command result.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOutput {
    /// The response `data`: one of the [output types](crate::registry::output).
    pub data: Sensitive<Value>,
    /// Public ids of what the command read or changed, for the audit log.
    pub affected: Vec<String>,
}

impl CommandOutput {
    /// Serializes `data`.
    pub fn new(data: &impl Serialize) -> Result<Self, CommandError> {
        let data = serde_json::to_value(data)
            .map_err(|error| CommandError::internal(format!("cannot encode output: {error}")))?;
        Ok(Self {
            data: Sensitive::new(data),
            affected: Vec::new(),
        })
    }

    /// Sets the affected ids.
    #[must_use]
    pub fn with_affected(mut self, affected: impl IntoIterator<Item = String>) -> Self {
        self.affected = affected.into_iter().collect();
        self
    }
}

/// A failed command. The message goes to the client, never to logs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct CommandError {
    /// Error code.
    pub code: ErrorCode,
    /// Human-readable message.
    pub message: String,
}

impl CommandError {
    /// An error with `code`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// `not_found`, also for ids outside the token's scope.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    /// `invalid_args`.
    pub fn invalid_args(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgs, message)
    }

    /// `internal`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
}

impl From<CommandError> for ApiError {
    fn from(error: CommandError) -> Self {
        Self::new(error.code, error.message)
    }
}

/// Executes mail commands. Implemented by `mazemail_core`.
///
/// By the time `handle` runs, the token is valid, the profile allows the command, the
/// user approved it if it needed approval, and [`RequestContext::mode`] says whether to
/// apply or propose. The handler still enforces [`RequestContext::scope`].
#[async_trait]
pub trait CommandHandler: Send + Sync {
    /// Runs `command`.
    async fn handle(
        &self,
        ctx: &RequestContext,
        command: Command,
    ) -> Result<CommandOutput, CommandError>;

    /// Extra details for `status` (accounts, sync state). Called with or without a
    /// token; return nothing about mail when `ctx.token` is `None`.
    async fn status(&self, ctx: &RequestContext) -> Value {
        let _ = ctx;
        Value::Null
    }
}
