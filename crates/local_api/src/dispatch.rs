//! Turning a request frame into a response: version check, token, registry lookup,
//! permission matrix, approvals, the handler, and the audit event.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use mazemail_types::{PRODUCT_NAME, PublicId as _};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::{Semaphore, mpsc};

use crate::approval::{ApprovalKind, ApprovalRequest, Approver, Decision};
use crate::audit::{AuditEvent, AuditOutcome, AuditSink};
use crate::handler::{
    CommandHandler, CommandOutput, ExecutionMode, ProgressSender, RequestContext,
};
use crate::peer::PeerInfo;
use crate::permission::Access;
use crate::policy::{AccountPolicy, EffectiveScope, excluded_accounts};
use crate::protocol::{ApiError, ErrorCode, PROTOCOL_VERSION, Progress, Request, Response};
use crate::registry::args::{LoginArgs, TokenRevokeArgs};
use crate::registry::output::{HelpEntry, HelpOutput, LoginOutput, RevokeOutput, StatusOutput};
use crate::registry::{Command, CommandSpec, find, registry};
use crate::token::{
    CreatedBy, Grant, SESSION_ENV, TokenId, TokenRecord, TokenRejection, TokenScope, TokenStore,
    mint, verify,
};

/// The services the local API needs from the rest of Maze.
#[derive(Clone)]
pub struct Services {
    /// Executes mail commands (`mazemail_core`).
    pub handler: Arc<dyn CommandHandler>,
    /// Token persistence.
    pub tokens: Arc<dyn TokenStore>,
    /// Asks the user (the GUI, [`TerminalApprover`](crate::TerminalApprover) or
    /// [`DenyAllApprover`](crate::DenyAllApprover)).
    pub approver: Arc<dyn Approver>,
    /// Receives one event per request.
    pub audit: Arc<dyn AuditSink>,
    /// The per-account AI policy.
    pub policy: Arc<dyn AccountPolicy>,
}

impl fmt::Debug for Services {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Services").finish_non_exhaustive()
    }
}

/// Request-handling limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchConfig {
    /// Maze's version, reported by `status`.
    pub app_version: String,
    /// How long an approval may wait for the user.
    pub approval_timeout: Duration,
    /// How many approvals may wait at the same time; more fail with `busy`.
    pub max_pending_approvals: usize,
    /// The longest lifetime `login` can give a token.
    pub max_login_ttl: Duration,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            approval_timeout: Duration::from_secs(5 * 60),
            max_pending_approvals: 4,
            max_login_ttl: Duration::from_secs(30 * 86_400),
        }
    }
}

/// Handles request frames. Shared by all connections of a server.
pub struct Dispatcher {
    services: Services,
    config: DispatchConfig,
    approval_slots: Semaphore,
}

impl fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dispatcher")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// What the audit event needs to know, collected while handling.
#[derive(Debug, Default)]
struct Trace {
    cmd: Option<&'static str>,
    token: Option<TokenId>,
    approved: bool,
    proposed: bool,
    /// Whether the request got past authorization (errors then count as failures).
    ran: bool,
}

type Outcome = Result<CommandOutput, ApiError>;

const INVALID_CMD: &str = "<invalid>";

/// Arguments as an approval dialog may show them: secrets (`account add`'s password)
/// replaced, so they never reach a dialog, a prompt or a log.
fn redacted(mut args: Value) -> Value {
    if let Some(value) = args.as_object_mut().and_then(|o| o.get_mut("password"))
        && !value.is_null()
    {
        *value = Value::String("<redacted>".to_owned());
    }
    args
}

impl Dispatcher {
    /// A dispatcher over `services`.
    pub fn new(services: Services, config: DispatchConfig) -> Self {
        let slots = config.max_pending_approvals;
        Self {
            services,
            config,
            approval_slots: Semaphore::new(slots),
        }
    }

    /// Handles one request frame. Progress frames go to `progress_tx`; the final
    /// response is returned.
    pub async fn dispatch(
        &self,
        peer: &Arc<PeerInfo>,
        frame: &[u8],
        progress_tx: &mpsc::Sender<Response>,
    ) -> Response {
        let mut trace = Trace::default();
        let (id, outcome) = match self.decode(frame) {
            Err((id, error)) => (id, Err(error)),
            Ok(request) => {
                let id = request.id;
                let progress = ProgressSender::new(id, progress_tx.clone());
                (id, self.handle(peer, request, progress, &mut trace).await)
            }
        };
        self.finish(peer, id, outcome, &trace)
    }

    /// The response (and audit event) for a frame over the size limit.
    pub fn oversized(&self, peer: &Arc<PeerInfo>, len: usize, max: usize) -> Response {
        let error = ApiError::new(
            ErrorCode::FrameTooLarge,
            format!("request of {len} bytes exceeds the limit of {max} bytes"),
        );
        self.finish(peer, 0, Err(error), &Trace::default())
    }

    /// The audit event for a connection from another uid (no response is sent).
    pub fn reject_peer(&self, peer: &PeerInfo) {
        self.services.audit.record(AuditEvent {
            at: OffsetDateTime::now_utc(),
            request_id: 0,
            token: None,
            pid: peer.process.pid,
            exe: peer.process.exe.clone(),
            cmd: "<connect>".to_owned(),
            outcome: AuditOutcome::Rejected {
                code: ErrorCode::PermissionDenied,
            },
            approved: false,
            affected: Vec::new(),
        });
    }

    fn decode(&self, frame: &[u8]) -> Result<Request, (u64, ApiError)> {
        let value: Value = serde_json::from_slice(frame).map_err(|_| {
            (
                0,
                ApiError::new(ErrorCode::BadRequest, "the request is not valid JSON"),
            )
        })?;
        // `v` and `id` are the only fields every protocol version must keep readable.
        let id = value.get("id").and_then(Value::as_u64).unwrap_or(0);
        match value.get("v").map(Value::as_u64) {
            None => {
                return Err((
                    id,
                    ApiError::new(
                        ErrorCode::BadRequest,
                        "the request has no protocol version `v`",
                    ),
                ));
            }
            Some(Some(v)) if v == u64::from(PROTOCOL_VERSION) => {}
            Some(_) => return Err((id, ApiError::restart_required())),
        }
        serde_json::from_value(value).map_err(|error| {
            (
                id,
                ApiError::new(ErrorCode::BadRequest, format!("invalid request: {error}")),
            )
        })
    }

    fn finish(&self, peer: &PeerInfo, id: u64, outcome: Outcome, trace: &Trace) -> Response {
        let (audit_outcome, affected, response) = match outcome {
            Ok(output) => {
                let audit_outcome = if trace.proposed {
                    AuditOutcome::Proposed
                } else {
                    AuditOutcome::Executed
                };
                (
                    audit_outcome,
                    output.affected,
                    Response::data(id, output.data.into_inner()),
                )
            }
            Err(error) => {
                let audit_outcome = if trace.ran {
                    AuditOutcome::Failed { code: error.code }
                } else {
                    AuditOutcome::Rejected { code: error.code }
                };
                (audit_outcome, Vec::new(), Response::error(id, error))
            }
        };
        let cmd = trace.cmd.unwrap_or(INVALID_CMD);
        tracing::debug!(
            request = id,
            cmd,
            token = trace.token.as_ref().map(TokenId::as_str),
            pid = peer.process.pid,
            outcome = ?audit_outcome,
            "local api request"
        );
        self.services.audit.record(AuditEvent {
            at: OffsetDateTime::now_utc(),
            request_id: id,
            token: trace.token.clone(),
            pid: peer.process.pid,
            exe: peer.process.exe.clone(),
            cmd: cmd.to_owned(),
            outcome: audit_outcome,
            approved: trace.approved,
            affected,
        });
        response
    }

    async fn handle(
        &self,
        peer: &Arc<PeerInfo>,
        request: Request,
        progress: ProgressSender,
        trace: &mut Trace,
    ) -> Outcome {
        let now = OffsetDateTime::now_utc();
        let (token, rejection) = match &request.token {
            None => (None, None),
            Some(presented) => match verify(self.services.tokens.as_ref(), presented, now).await {
                Ok(record) => (Some(record), None),
                Err(TokenRejection::Store(error)) => {
                    tracing::error!(%error, "token store failed");
                    return Err(ApiError::new(ErrorCode::Internal, "cannot check the token"));
                }
                Err(rejection) => (None, Some(rejection)),
            },
        };
        trace.token = token.as_ref().map(|record| record.id.clone());

        let spec = find(&request.cmd).ok_or_else(|| {
            ApiError::new(
                ErrorCode::UnknownCommand,
                "unknown command (see `mazemail help`)",
            )
        })?;
        trace.cmd = Some(spec.name);

        if spec.requires_token && token.is_none() {
            let reason = rejection.map_or_else(|| "not signed in".to_owned(), |r| r.to_string());
            return Err(ApiError::new(
                ErrorCode::NotSignedIn,
                format!("{reason}: run `mazemail login` and set {SESSION_ENV}"),
            ));
        }

        let args = request.args.into_inner();
        match spec.name {
            "status" => {
                self.status(peer, request.id, token, rejection.as_ref())
                    .await
            }
            "help" => help(token.as_ref()),
            "login" | "token.create" => {
                self.login(peer, spec, args, token.as_ref(), &progress, trace)
                    .await
            }
            "token.list" => self.list_tokens().await,
            "token.revoke" => {
                // Checked above: the command requires a token.
                let token = token.ok_or_else(not_signed_in)?;
                self.revoke(peer, spec, args, &token, &progress, trace)
                    .await
            }
            _ => {
                let token = token.ok_or_else(not_signed_in)?;
                self.mail_command(peer, spec, request.id, args, token, progress, trace)
                    .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn mail_command(
        &self,
        peer: &Arc<PeerInfo>,
        spec: &'static CommandSpec,
        request_id: u64,
        args: Value,
        token: TokenRecord,
        progress: ProgressSender,
        trace: &mut Trace,
    ) -> Outcome {
        let command = match Command::from_wire(spec.name, args.clone()) {
            Ok(Some(command)) => command,
            Ok(None) => {
                return Err(ApiError::new(
                    ErrorCode::Internal,
                    "the command has no handler",
                ));
            }
            Err(error) => return Err(ApiError::new(ErrorCode::InvalidArgs, error.0)),
        };

        let scope = EffectiveScope::resolve(&token.scope, &self.services.policy.agent_accounts());
        if spec.touches_mail && scope.is_empty() {
            return Err(ApiError::new(
                ErrorCode::PermissionDenied,
                "no account in this token's scope allows external agents (AI policy)",
            ));
        }

        let access = spec.access(token.profile, token.allow_send);
        match access {
            Access::Deny => return Err(denied(spec, &token)),
            Access::Ask => {
                let request = ApprovalRequest {
                    process: peer.process.clone(),
                    requester_token: Some(token.id.clone()),
                    profile: token.profile,
                    scope: token.scope.clone(),
                    lifetime: token.grant().lifetime,
                    command: spec.name.to_owned(),
                    kind: ApprovalKind::Command {
                        args: redacted(args).into(),
                        // `send` is never auto-approved.
                        rememberable: spec.permission != crate::Permission::Disabled,
                    },
                };
                self.ask(request, &progress).await?;
                trace.approved = true;
            }
            Access::Allow | Access::Propose => {}
        }
        let mode = if access == Access::Propose {
            trace.proposed = true;
            ExecutionMode::Propose
        } else {
            ExecutionMode::Execute
        };

        trace.ran = true;
        let ctx = RequestContext::new(request_id, peer.clone(), Some(token), scope, mode)
            .with_progress(progress, trace.approved);
        self.services
            .handler
            .handle(&ctx, command)
            .await
            .map_err(ApiError::from)
    }

    /// Waits for the user, with the progress frame, the slot limit and the timeout.
    async fn ask(
        &self,
        request: ApprovalRequest,
        progress: &ProgressSender,
    ) -> Result<Grant, ApiError> {
        let _slot = self.approval_slots.try_acquire().map_err(|_| {
            ApiError::new(
                ErrorCode::Busy,
                format!("too many requests are waiting for approval in {PRODUCT_NAME}"),
            )
        })?;
        progress.send(Progress::approval_pending()).await;
        match tokio::time::timeout(
            self.config.approval_timeout,
            self.services.approver.approve(request),
        )
        .await
        {
            Ok(Decision::Approve(grant)) => Ok(grant),
            Ok(Decision::Deny) => Err(ApiError::new(
                ErrorCode::ApprovalDenied,
                format!("denied in {PRODUCT_NAME}"),
            )),
            Err(_) => Err(ApiError::new(
                ErrorCode::ApprovalTimeout,
                format!("no answer in {PRODUCT_NAME} in time"),
            )),
        }
    }

    async fn status(
        &self,
        peer: &Arc<PeerInfo>,
        request_id: u64,
        token: Option<TokenRecord>,
        rejection: Option<&TokenRejection>,
    ) -> Outcome {
        let scope = token.as_ref().map_or_else(EffectiveScope::none, |token| {
            EffectiveScope::resolve(&token.scope, &self.services.policy.agent_accounts())
        });
        let info = token.as_ref().map(TokenRecord::info);
        let ctx = RequestContext::new(
            request_id,
            peer.clone(),
            token,
            scope,
            ExecutionMode::Execute,
        );
        let app = self.services.handler.status(&ctx).await;
        output(&StatusOutput {
            running: true,
            version: self.config.app_version.clone(),
            protocol: PROTOCOL_VERSION,
            signed_in: info.is_some(),
            token: info,
            token_error: rejection.map(ToString::to_string),
            app,
        })
    }

    async fn login(
        &self,
        peer: &Arc<PeerInfo>,
        spec: &'static CommandSpec,
        args: Value,
        requester: Option<&TokenRecord>,
        progress: &ProgressSender,
        trace: &mut Trace,
    ) -> Outcome {
        let args: LoginArgs = parse(args)?;
        let lifetime = args.ttl.duration();
        if lifetime > self.config.max_login_ttl {
            return Err(ApiError::new(
                ErrorCode::InvalidArgs,
                format!(
                    "ttl: at most {}",
                    crate::token::Ttl::new(self.config.max_login_ttl)
                ),
            ));
        }
        if args.allow_send && args.profile == crate::Profile::ReadOnly {
            return Err(ApiError::new(
                ErrorCode::InvalidArgs,
                "allow_send needs the triage or writer profile",
            ));
        }
        let scope = TokenScope {
            accounts: (!args.accounts.is_empty()).then(|| args.accounts.iter().copied().collect()),
            folders: (!args.folders.is_empty()).then(|| args.folders.iter().copied().collect()),
        };
        let allowed = self.services.policy.agent_accounts();
        check_policy(args.profile, &scope, &allowed)?;

        let request = ApprovalRequest {
            process: peer.process.clone(),
            requester_token: requester.map(|token| token.id.clone()),
            profile: args.profile,
            scope,
            lifetime: Some(lifetime),
            command: spec.name.to_owned(),
            kind: ApprovalKind::Login {
                label: args.label.clone(),
                allow_send: args.allow_send,
            },
        };
        // Always ask, whether or not the caller holds a token: tokens can't mint tokens.
        let mut grant = self.ask(request, progress).await?;
        trace.approved = true;
        trace.ran = true;

        // The token gets exactly what was approved, within what `login` may hand out.
        if grant
            .lifetime
            .is_none_or(|lifetime| lifetime > self.config.max_login_ttl)
        {
            grant.lifetime = Some(self.config.max_login_ttl);
        }
        if let Some(accounts) = &mut grant.scope.accounts {
            accounts.retain(|account| allowed.contains(account));
        }
        check_policy(grant.profile, &grant.scope, &allowed)?;
        if grant.profile == crate::Profile::ReadOnly {
            grant.allow_send = false;
        }

        let now = OffsetDateTime::now_utc();
        let (secret, record) = mint(&grant, args.label, CreatedBy::User, now)
            .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
        let output = LoginOutput {
            export: format!("export {SESSION_ENV}={}", secret.expose()),
            token: secret.expose().to_owned(),
            id: record.id.clone(),
            profile: record.profile,
            scope: record.scope.clone(),
            allow_send: record.allow_send,
            expires_at: record.expires_at,
        };
        let id = record.id.clone();
        self.services.tokens.insert(record).await.map_err(|error| {
            tracing::error!(%error, "cannot store a new token");
            ApiError::new(ErrorCode::Internal, "cannot store the token")
        })?;
        output_with(&output, [id.to_string()])
    }

    async fn list_tokens(&self) -> Outcome {
        let records = self.services.tokens.list().await.map_err(|error| {
            tracing::error!(%error, "cannot list tokens");
            ApiError::new(ErrorCode::Internal, "cannot list tokens")
        })?;
        let infos: Vec<_> = records.iter().map(TokenRecord::info).collect();
        output(&infos)
    }

    async fn revoke(
        &self,
        peer: &Arc<PeerInfo>,
        spec: &'static CommandSpec,
        args: Value,
        token: &TokenRecord,
        progress: &ProgressSender,
        trace: &mut Trace,
    ) -> Outcome {
        let parsed: TokenRevokeArgs = parse(args.clone())?;
        // Signing yourself out never needs approval.
        if parsed.id != token.id {
            match spec.access(token.profile, token.allow_send) {
                Access::Deny => return Err(denied(spec, token)),
                Access::Allow | Access::Propose => {}
                Access::Ask => {
                    let request = ApprovalRequest {
                        process: peer.process.clone(),
                        requester_token: Some(token.id.clone()),
                        profile: token.profile,
                        scope: token.scope.clone(),
                        lifetime: token.grant().lifetime,
                        command: spec.name.to_owned(),
                        kind: ApprovalKind::Command {
                            args: args.into(),
                            rememberable: false,
                        },
                    };
                    self.ask(request, progress).await?;
                    trace.approved = true;
                }
            }
        }
        trace.ran = true;
        let now = OffsetDateTime::now_utc();
        let revoked = self
            .services
            .tokens
            .revoke(&parsed.id, now)
            .await
            .map_err(|_| ApiError::new(ErrorCode::Internal, "cannot revoke the token"))?;
        if !revoked {
            let known = self
                .services
                .tokens
                .list()
                .await
                .map_err(|_| ApiError::new(ErrorCode::Internal, "cannot read tokens"))?
                .iter()
                .any(|record| record.id == parsed.id);
            if !known {
                return Err(ApiError::new(ErrorCode::NotFound, "no such token"));
            }
        }
        output_with(
            &RevokeOutput {
                id: parsed.id.clone(),
                revoked,
            },
            [parsed.id.to_string()],
        )
    }
}

fn not_signed_in() -> ApiError {
    ApiError::new(ErrorCode::NotSignedIn, "not signed in")
}

fn denied(spec: &CommandSpec, token: &TokenRecord) -> ApiError {
    let message = if spec.permission == crate::Permission::Disabled {
        format!(
            "`mazemail {}` is disabled for this token (approve a token with --allow-send)",
            spec.cli_name()
        )
    } else {
        format!(
            "the {} profile can't run `mazemail {}`",
            token.profile,
            spec.cli_name()
        )
    };
    ApiError::new(ErrorCode::PermissionDenied, message)
}

/// A token's scope can never include accounts whose AI policy excludes agents.
///
/// One exception keeps the CLI usable before any account is shared: a writer token
/// for all accounts may be approved while no account allows agents. It sees no mail
/// (its effective scope is empty) and can only run `account add` (which asks again
/// each time), `account list` and `sync`.
fn check_policy(
    profile: crate::Profile,
    scope: &TokenScope,
    allowed: &BTreeSet<mazemail_types::AccountId>,
) -> Result<(), ApiError> {
    if allowed.is_empty() && profile == crate::Profile::Writer && *scope == TokenScope::all() {
        return Ok(());
    }
    if allowed.is_empty() {
        return Err(ApiError::new(
            ErrorCode::PermissionDenied,
            format!(
                "no account allows external agents; enable AI for an account in {PRODUCT_NAME}'s settings"
            ),
        ));
    }
    let excluded = excluded_accounts(scope, allowed);
    if !excluded.is_empty() {
        let ids: Vec<String> = excluded.iter().map(|id| id.to_public()).collect();
        return Err(ApiError::new(
            ErrorCode::PermissionDenied,
            format!(
                "the AI policy of {} doesn't allow external agents",
                ids.join(", ")
            ),
        ));
    }
    if scope.accounts.as_ref().is_some_and(BTreeSet::is_empty) {
        return Err(ApiError::new(
            ErrorCode::PermissionDenied,
            "the approved scope contains no account that allows external agents",
        ));
    }
    Ok(())
}

fn help(token: Option<&TokenRecord>) -> Outcome {
    let commands = registry()
        .iter()
        .map(|spec| HelpEntry {
            command: spec.cli_name(),
            summary: spec.summary.to_owned(),
            permission: spec.permission,
            requires_token: spec.requires_token,
            access: token
                .map(|token| spec.access(token.profile, token.allow_send))
                .or_else(|| {
                    (!spec.requires_token).then(|| spec.access(crate::Profile::ReadOnly, false))
                }),
        })
        .collect();
    output(&HelpOutput { commands })
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, ApiError> {
    crate::registry::parse_wire_args(args)
        .map_err(|error| ApiError::new(ErrorCode::InvalidArgs, error.0))
}

fn output(data: &impl Serialize) -> Outcome {
    CommandOutput::new(data).map_err(ApiError::from)
}

fn output_with(data: &impl Serialize, affected: impl IntoIterator<Item = String>) -> Outcome {
    Ok(CommandOutput::new(data)
        .map_err(ApiError::from)?
        .with_affected(affected))
}
