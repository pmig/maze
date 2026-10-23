//! End-to-end tests: a real server on a temporary socket, the real client, and fakes
//! for everything `mazemail_core` will provide.

#![allow(clippy::unwrap_used)] // Tests may unwrap (AGENTS.md).

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use local_api::registry::output::{MutationOutcome, MutationOutput};
use local_api::*;
use mazemail_types::AccountId;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use settings::Paths;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_util::codec::Framed;

// ---------------------------------------------------------------- fakes

/// What the fake core saw.
#[derive(Debug, Clone)]
struct Seen {
    name: &'static str,
    mode: ExecutionMode,
    approved: bool,
    accounts: BTreeSet<AccountId>,
    pid: Option<i32>,
}

#[derive(Default)]
struct FakeCore {
    seen: Mutex<Vec<Seen>>,
}

#[async_trait]
impl CommandHandler for FakeCore {
    async fn handle(
        &self,
        ctx: &RequestContext,
        command: Command,
    ) -> Result<CommandOutput, CommandError> {
        self.seen.lock().unwrap().push(Seen {
            name: command.name(),
            mode: ctx.mode,
            approved: ctx.approved,
            accounts: ctx.scope.accounts().clone(),
            pid: ctx.peer.process.pid,
        });
        match command {
            Command::Search(args) => {
                assert_eq!(args.query.expose(), "from:anna@example.com");
                CommandOutput::new(&json!([{ "id": "m_b", "thread": "t_c" }]))
                    .map(|out| out.with_affected(["m_b".to_owned()]))
            }
            Command::Archive(args) => {
                let outcome = match ctx.mode {
                    ExecutionMode::Execute => MutationOutcome::Applied,
                    ExecutionMode::Propose => MutationOutcome::Proposed,
                };
                CommandOutput::new(&MutationOutput {
                    outcome,
                    proposal: (outcome == MutationOutcome::Proposed).then(|| "p_b".to_owned()),
                    affected: args.ids.clone(),
                })
                .map(|out| out.with_affected(args.ids))
            }
            Command::Export(_) => {
                ctx.progress(Progress::working("exporting", Some(1), Some(2)))
                    .await;
                CommandOutput::new(&json!({ "path": "/x", "messages": 2 }))
            }
            Command::Show(_) => Err(CommandError::not_found("no such message")),
            Command::AccountAdd(args) => {
                // The handler gets the real password; dialogs and logs don't.
                assert_eq!(args.password.unwrap().expose(), "hunter2");
                CommandOutput::new(&json!({ "id": "a_b" }))
            }
            _ => CommandOutput::new(&Value::Null),
        }
    }

    async fn status(&self, ctx: &RequestContext) -> Value {
        json!({ "accounts": ctx.scope.accounts().len() })
    }
}

type Script = Box<dyn FnMut(&ApprovalRequest) -> Option<Decision> + Send>;

/// Answers approvals from a script; `None` never answers.
struct ScriptedApprover {
    script: Mutex<Script>,
    requests: Mutex<Vec<ApprovalRequest>>,
}

impl ScriptedApprover {
    fn new(script: impl FnMut(&ApprovalRequest) -> Option<Decision> + Send + 'static) -> Self {
        Self {
            script: Mutex::new(Box::new(script)),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn approve_all() -> Self {
        Self::new(|request| Some(Decision::Approve(request.requested_grant())))
    }

    fn requests(&self) -> Vec<ApprovalRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Approver for ScriptedApprover {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        let decision = (self.script.lock().unwrap())(&request);
        self.requests.lock().unwrap().push(request);
        match decision {
            Some(decision) => decision,
            None => std::future::pending().await,
        }
    }
}

// ---------------------------------------------------------------- harness

struct Harness {
    _dir: tempfile::TempDir,
    paths: Paths,
    socket: PathBuf,
    core: Arc<FakeCore>,
    tokens: Arc<MemoryTokenStore>,
    audit: Arc<MemoryAuditSink>,
    approver: Arc<ScriptedApprover>,
    policy: Arc<StaticAccountPolicy>,
    shutdown: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn start(approver: ScriptedApprover) -> Self {
        Self::start_with(approver, |_| {}).await
    }

    async fn start_with(
        approver: ScriptedApprover,
        configure: impl FnOnce(&mut ServerConfig),
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_dirs(dir.path().join("data"), dir.path().join("config"));
        let core = Arc::new(FakeCore::default());
        let tokens = Arc::new(MemoryTokenStore::new());
        let audit = Arc::new(MemoryAuditSink::new());
        let approver = Arc::new(approver);
        let policy = Arc::new(StaticAccountPolicy::new([AccountId(1), AccountId(2)]));
        let services = Services {
            handler: core.clone(),
            tokens: tokens.clone(),
            approver: approver.clone(),
            audit: audit.clone(),
            policy: policy.clone(),
        };
        let mut config = ServerConfig::default();
        config.dispatch.approval_timeout = Duration::from_secs(5);
        configure(&mut config);
        let server = LocalApiServer::bind(&paths, services, config)
            .await
            .unwrap();
        let socket = server.path().to_owned();
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(server.serve(async move {
            let _ = rx.await;
        }));
        Self {
            _dir: dir,
            paths,
            socket,
            core,
            tokens,
            audit,
            approver,
            policy,
            shutdown: Some(tx),
            server: Some(task),
        }
    }

    async fn token(&self, profile: Profile, allow_send: bool) -> (SessionToken, TokenRecord) {
        self.token_with(Grant {
            profile,
            scope: TokenScope::all(),
            lifetime: Some(Duration::from_secs(3600)),
            allow_send,
        })
        .await
    }

    async fn token_with(&self, grant: Grant) -> (SessionToken, TokenRecord) {
        let (token, record) = mint(
            &grant,
            Some("test".into()),
            CreatedBy::User,
            OffsetDateTime::now_utc(),
        )
        .unwrap();
        self.tokens.insert(record.clone()).await.unwrap();
        (token, record)
    }

    async fn client(&self, token: Option<&SessionToken>) -> LocalApiClient {
        LocalApiClient::connect(&Endpoint::discover_with(&self.paths, None))
            .await
            .unwrap()
            .with_token(token.cloned())
    }

    async fn raw(&self) -> Framed<UnixStream, FrameCodec> {
        let stream = UnixStream::connect(&self.socket).await.unwrap();
        Framed::new(stream, FrameCodec::new(protocol::MAX_RESPONSE_FRAME_LEN))
    }

    async fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(server) = self.server.take() {
            tokio::time::timeout(Duration::from_secs(10), server)
                .await
                .unwrap()
                .unwrap();
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.core.seen.lock().unwrap().clone()
    }
}

fn api_error(error: ClientError) -> ApiError {
    match error {
        ClientError::Api(error) => error,
        other => panic!("expected an API error, got {other:?}"),
    }
}

async fn read_response(framed: &mut Framed<UnixStream, FrameCodec>) -> Response {
    match framed.next().await.unwrap().unwrap() {
        Frame::Data(bytes) => serde_json::from_slice(&bytes).unwrap(),
        Frame::Oversized { len } => panic!("oversized response {len}"),
    }
}

fn error_code(response: &Response) -> ErrorCode {
    match &response.body {
        ResponseBody::Error(error) => error.code,
        other => panic!("expected an error, got {other:?}"),
    }
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn search_end_to_end() {
    let mut h = Harness::start(ScriptedApprover::approve_all()).await;
    let (token, record) = h.token(Profile::ReadOnly, false).await;
    let mut client = h.client(Some(&token)).await;
    let data = client
        .call("search", json!({ "query": "from:anna@example.com" }))
        .await
        .unwrap();
    assert_eq!(data, json!([{ "id": "m_b", "thread": "t_c" }]));

    let seen = h.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].mode, ExecutionMode::Execute);
    assert_eq!(seen[0].accounts, [AccountId(1), AccountId(2)].into());
    assert_eq!(
        seen[0].pid,
        Some(i32::try_from(std::process::id()).unwrap())
    );

    let events = h.audit.events();
    let event = events.last().unwrap();
    assert_eq!(event.cmd, "search");
    assert_eq!(event.token, Some(record.id.clone()));
    assert_eq!(event.outcome, AuditOutcome::Executed);
    assert_eq!(event.affected, ["m_b"]);
    assert_eq!(event.pid, Some(i32::try_from(std::process::id()).unwrap()));
    assert!(event.exe.is_some());
    // The audit log never contains the query.
    let audit_text = format!("{events:?}{}", serde_json::to_string(&events).unwrap());
    assert!(!audit_text.contains("anna"), "{audit_text}");

    // Several requests on one connection.
    client.call("folders", json!({})).await.unwrap();
    h.stop().await;
}

#[tokio::test]
async fn socket_file_is_private_and_removed_on_shutdown() {
    let mut h = Harness::start(ScriptedApprover::approve_all()).await;
    let mode = std::fs::metadata(&h.socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(h.socket, h.paths.data_dir().join("mazemail.sock"));
    h.stop().await;
    assert!(!h.socket.exists());
}

#[tokio::test]
async fn second_server_refuses_and_stale_socket_is_replaced() {
    let mut h = Harness::start(ScriptedApprover::approve_all()).await;
    let services = || Services {
        handler: Arc::new(FakeCore::default()),
        tokens: Arc::new(MemoryTokenStore::new()),
        approver: Arc::new(DenyAllApprover),
        audit: Arc::new(MemoryAuditSink::new()),
        policy: Arc::new(StaticAccountPolicy::default()),
    };
    let error = LocalApiServer::bind(&h.paths, services(), ServerConfig::default())
        .await
        .unwrap_err();
    assert!(matches!(error, ServerError::AlreadyRunning(_)), "{error}");
    h.stop().await;

    // A socket left behind by a crashed server is replaced.
    let stale = std::os::unix::net::UnixListener::bind(&h.socket).unwrap();
    drop(stale);
    assert!(h.socket.exists());
    let server = LocalApiServer::bind(&h.paths, services(), ServerConfig::default())
        .await
        .unwrap();
    assert_eq!(server.path(), h.socket);
}

#[tokio::test]
async fn fallback_socket_for_long_data_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("d".repeat(110));
    let paths = Paths::from_dirs(&data, dir.path().join("config"));
    let services = Services {
        handler: Arc::new(FakeCore::default()),
        tokens: Arc::new(MemoryTokenStore::new()),
        approver: Arc::new(DenyAllApprover),
        audit: Arc::new(MemoryAuditSink::new()),
        policy: Arc::new(StaticAccountPolicy::default()),
    };
    let server = LocalApiServer::bind(&paths, services, ServerConfig::default())
        .await
        .unwrap();
    let socket = server.path().to_owned();
    assert!(!socket.starts_with(&data));
    let dir_mode = std::fs::metadata(socket.parent().unwrap())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o700);
    let record = std::fs::read_to_string(paths.socket_path_file()).unwrap();
    assert_eq!(Path::new(record.trim()), socket);

    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(async move {
        let _ = rx.await;
    }));
    // The client finds the server through the record.
    let mut client = LocalApiClient::connect(&Endpoint::discover_with(&paths, None))
        .await
        .unwrap();
    let status = client.call("status", json!({})).await.unwrap();
    assert_eq!(status["running"], true);
    drop(client);
    tx.send(()).unwrap();
    task.await.unwrap();
    assert!(!socket.exists());
    assert!(!paths.socket_path_file().exists());
}

#[tokio::test]
async fn tokenless_requests_are_limited_to_help_status_and_login() {
    let mut h = Harness::start(ScriptedApprover::new(|_| Some(Decision::Deny))).await;
    let mut client = h.client(None).await;

    let status = client.call("status", json!({})).await.unwrap();
    assert_eq!(status["signed_in"], false);
    assert_eq!(status["protocol"], 1);
    assert_eq!(status["token"], Value::Null);
    let help = client.call("help", json!({})).await.unwrap();
    assert!(help["commands"].as_array().unwrap().len() > 20);
    // `login` without a token reaches the approval (denied here).
    let login = api_error(client.call("login", json!({})).await.unwrap_err());
    assert_eq!(login.code, ErrorCode::ApprovalDenied);

    for name in registry::registry()
        .iter()
        .filter(|spec| spec.requires_token)
        .map(|spec| spec.name)
    {
        let error = client.call(name, json!({})).await.unwrap_err();
        assert_eq!(error.exit_code(), ExitCode::NotSignedIn, "{name}");
    }
    assert!(h.seen().is_empty());
    h.stop().await;
}

#[tokio::test]
async fn invalid_expired_and_revoked_tokens() {
    let mut h = Harness::start(ScriptedApprover::approve_all()).await;
    let args = json!({ "query": "from:anna@example.com" });

    let malformed = SessionToken::from_string("mazemail_s_nope".into());
    let error = api_error(
        h.client(Some(&malformed))
            .await
            .call("search", args.clone())
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code, ErrorCode::NotSignedIn);
    assert!(error.message.contains("malformed"), "{}", error.message);

    let unknown = SessionToken::generate().unwrap();
    let error = api_error(
        h.client(Some(&unknown))
            .await
            .call("search", args.clone())
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code, ErrorCode::NotSignedIn);

    let (expired, _) = h
        .token_with(Grant {
            profile: Profile::Writer,
            scope: TokenScope::all(),
            lifetime: Some(Duration::from_secs(1)),
            allow_send: false,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let mut client = h.client(Some(&expired)).await;
    let error = api_error(client.call("search", args.clone()).await.unwrap_err());
    assert!(error.message.contains("expired"), "{}", error.message);
    let status = client.call("status", json!({})).await.unwrap();
    assert_eq!(status["signed_in"], false);
    assert_eq!(status["token_error"], "the session token has expired");

    let (token, record) = h.token(Profile::Writer, false).await;
    h.tokens
        .revoke(&record.id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    let error = api_error(
        h.client(Some(&token))
            .await
            .call("search", args)
            .await
            .unwrap_err(),
    );
    assert!(error.message.contains("revoked"), "{}", error.message);
    h.stop().await;
}

#[tokio::test]
async fn profiles_decide_execute_propose_or_deny() {
    let mut h = Harness::start(ScriptedApprover::approve_all()).await;
    let ids = json!({ "ids": ["m_b", "t_c"] });

    let (triage, _) = h.token(Profile::Triage, false).await;
    let data = h
        .client(Some(&triage))
        .await
        .call("archive", ids.clone())
        .await
        .unwrap();
    assert_eq!(data["outcome"], "proposed");
    let (writer, _) = h.token(Profile::Writer, false).await;
    let data = h
        .client(Some(&writer))
        .await
        .call("archive", ids.clone())
        .await
        .unwrap();
    assert_eq!(data["outcome"], "applied");
    let (read_only, _) = h.token(Profile::ReadOnly, false).await;
    let error = h
        .client(Some(&read_only))
        .await
        .call("archive", ids)
        .await
        .unwrap_err();
    assert_eq!(error.exit_code(), ExitCode::PermissionDenied);

    let seen = h.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].mode, ExecutionMode::Propose);
    assert_eq!(seen[1].mode, ExecutionMode::Execute);

    let outcomes: Vec<_> = h.audit.events().iter().map(|event| event.outcome).collect();
    assert_eq!(
        outcomes,
        [
            AuditOutcome::Proposed,
            AuditOutcome::Executed,
            AuditOutcome::Rejected {
                code: ErrorCode::PermissionDenied
            }
        ]
    );
    assert_eq!(h.audit.events()[0].affected, ["m_b", "t_c"]);
    // Nothing needed approval.
    assert!(h.approver.requests().is_empty());
    h.stop().await;
}

#[tokio::test]
async fn send_is_disabled_unless_enabled_and_then_always_asks() {
    let mut h = Harness::start(ScriptedApprover::approve_all()).await;
    let args = json!({ "draft": "d_b" });

    let (plain, _) = h.token(Profile::Writer, false).await;
    let error = api_error(
        h.client(Some(&plain))
            .await
            .call("send", args.clone())
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    assert!(error.message.contains("--allow-send"), "{}", error.message);

    let (enabled, record) = h.token(Profile::Writer, true).await;
    let mut progress = Vec::new();
    h.client(Some(&enabled))
        .await
        .call_with_progress("send", args, |p| progress.push(p.clone()))
        .await
        .unwrap();
    assert_eq!(progress, [Progress::approval_pending()]);

    let requests = h.approver.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].command, "send");
    assert_eq!(requests[0].requester_token, Some(record.id));
    assert!(matches!(
        requests[0].kind,
        ApprovalKind::Command {
            rememberable: false,
            ..
        }
    ));
    let seen = h.seen();
    assert!(seen[0].approved);
    assert_eq!(h.audit.events().last().unwrap().approved, true);
    h.stop().await;
}

#[tokio::test]
async fn ask_commands_wait_for_approval() {
    let answers = Arc::new(Mutex::new(vec![
        Some(Decision::Deny),
        None,
        Some(Decision::Approve(Grant {
            profile: Profile::ReadOnly,
            scope: TokenScope::all(),
            lifetime: None,
            allow_send: false,
        })),
    ]));
    let script = answers.clone();
    let mut h = Harness::start_with(
        ScriptedApprover::new(move |_| script.lock().unwrap().remove(0)),
        |config| config.dispatch.approval_timeout = Duration::from_millis(200),
    )
    .await;
    let (token, _) = h.token(Profile::Triage, false).await;
    let args = json!({ "name": "r", "when": "from:x", "actions": [{ "action": "archive" }] });
    let mut client = h.client(Some(&token)).await;

    let denied = client.call("rule.add", args.clone()).await.unwrap_err();
    assert_eq!(denied.exit_code(), ExitCode::ApprovalDenied);
    let timed_out = api_error(client.call("rule.add", args.clone()).await.unwrap_err());
    assert_eq!(timed_out.code, ErrorCode::ApprovalTimeout);
    assert_eq!(timed_out.exit_code(), ExitCode::ApprovalDenied);
    client.call("rule.add", args).await.unwrap();

    let seen = h.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].name, "rule.add");
    assert!(seen[0].approved);
    h.stop().await;
}

#[tokio::test]
async fn too_many_pending_approvals_are_busy() {
    let mut h = Harness::start_with(ScriptedApprover::new(|_| None), |config| {
        config.dispatch.max_pending_approvals = 1;
    })
    .await;
    let (token, _) = h.token(Profile::Triage, false).await;
    let args = json!({ "name": "r", "when": "from:x", "actions": [{ "action": "archive" }] });
    let mut first = h.client(Some(&token)).await;
    let pending = tokio::spawn(async move { first.call("rule.add", args).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let error = api_error(
        h.client(Some(&token))
            .await
            .call("login", json!({}))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code, ErrorCode::Busy);
    pending.abort();
    h.stop().await;
}

#[tokio::test]
async fn login_returns_an_export_line_and_a_working_token() {
    let mut h = Harness::start(ScriptedApprover::approve_all()).await;
    let mut client = h.client(None).await;
    let mut progress = Vec::new();
    let data = client
        .call_with_progress(
            "login",
            json!({ "profile": "triage", "ttl": "2h", "label": "claude", "accounts": ["a_b"] }),
            |p| progress.push(p.code),
        )
        .await
        .unwrap();
    assert_eq!(progress, [ProgressCode::ApprovalPending]);
    let token = data["token"].as_str().unwrap();
    assert!(token.starts_with("mazemail_s_"));
    assert_eq!(data["export"], format!("export MAZEMAIL_SESSION={token}"));
    assert_eq!(data["profile"], "triage");
    assert_eq!(
        data["scope"],
        json!({ "accounts": ["a_b"], "folders": null })
    );

    let request = &h.approver.requests()[0];
    assert_eq!(request.command, "login");
    assert_eq!(request.lifetime, Some(Duration::from_secs(7200)));
    assert_eq!(request.requester_token, None);
    assert_eq!(
        request.process.pid,
        Some(i32::try_from(std::process::id()).unwrap())
    );

    // Only the hash is stored.
    let stored = h.tokens.list().await.unwrap();
    assert_eq!(stored.len(), 1);
    assert!(
        !serde_json::to_string(&stored)
            .unwrap()
            .contains(&token["mazemail_s_".len()..])
    );

    // The token works and is scoped to the approved account.
    let token = SessionToken::from_string(token.to_owned());
    let mut client = h.client(Some(&token)).await;
    client
        .call("search", json!({ "query": "from:anna@example.com" }))
        .await
        .unwrap();
    assert_eq!(h.seen()[0].accounts, [AccountId(1)].into());
    let status = client.call("status", json!({})).await.unwrap();
    assert_eq!(status["signed_in"], true);
    assert_eq!(status["token"]["label"], "claude");
    assert_eq!(status["app"]["accounts"], 1);

    // The audit event names the new token, not its secret.
    let login_event = &h.audit.events()[0];
    assert_eq!(login_event.cmd, "login");
    assert!(login_event.approved);
    assert_eq!(login_event.affected, [stored[0].id.to_string()]);
    h.stop().await;
}

#[tokio::test]
async fn tokens_cant_mint_tokens() {
    // The approver only grants read-only, whatever is asked.
    let mut h = Harness::start(ScriptedApprover::new(|request| {
        let mut grant = request.requested_grant();
        grant.profile = Profile::ReadOnly;
        grant.allow_send = false;
        Some(Decision::Approve(grant))
    }))
    .await;
    let (writer, writer_record) = h.token(Profile::Writer, true).await;
    let mut client = h.client(Some(&writer)).await;

    // Holding a writer token doesn't skip the approval, and the new token gets only
    // what was approved.
    let data = client
        .call(
            "token.create",
            json!({ "profile": "writer", "allow_send": true }),
        )
        .await
        .unwrap();
    assert_eq!(data["profile"], "read-only");
    assert_eq!(data["allow_send"], false);
    let requests = h.approver.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].requester_token, Some(writer_record.id.clone()));
    assert_eq!(requests[0].profile, Profile::Writer);

    let minted = SessionToken::from_string(data["token"].as_str().unwrap().to_owned());
    let error = h
        .client(Some(&minted))
        .await
        .call("archive", json!({ "ids": ["m_b"] }))
        .await
        .unwrap_err();
    assert_eq!(error.exit_code(), ExitCode::PermissionDenied);
    h.stop().await;
}

#[tokio::test]
async fn login_with_a_token_is_denied_without_approval() {
    let mut h = Harness::start(ScriptedApprover::new(|_| Some(Decision::Deny))).await;
    let (writer, _) = h.token(Profile::Writer, false).await;
    let error = h
        .client(Some(&writer))
        .await
        .call("login", json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.exit_code(), ExitCode::ApprovalDenied);
    assert_eq!(h.tokens.list().await.unwrap().len(), 1);
    h.stop().await;
}

#[tokio::test]
async fn ai_policy_limits_token_scopes() {
    let mut h = Harness::start(ScriptedApprover::approve_all()).await;
    let mut client = h.client(None).await;

    // Asking for an excluded account fails before any dialog.
    let error = api_error(
        client
            .call("login", json!({ "accounts": ["a_h"] }))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    assert!(error.message.contains("a_h"), "{}", error.message);
    assert!(h.approver.requests().is_empty());

    // An approver widening the scope to an excluded account is clipped.
    let mut h2 = Harness::start(ScriptedApprover::new(|request| {
        let mut grant = request.requested_grant();
        grant.scope.accounts = Some([AccountId(1), AccountId(7)].into());
        Some(Decision::Approve(grant))
    }))
    .await;
    let data = h2
        .client(None)
        .await
        .call("login", json!({}))
        .await
        .unwrap();
    assert_eq!(data["scope"]["accounts"], json!(["a_b"]));
    h2.stop().await;

    // Turning the policy off later takes effect for existing tokens.
    let (token, _) = h.token(Profile::Writer, false).await;
    h.policy.set([]);
    let error = api_error(
        h.client(Some(&token))
            .await
            .call("search", json!({ "query": "from:anna@example.com" }))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    // And no new tokens can be made…
    let error = api_error(client.call("login", json!({})).await.unwrap_err());
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    // …except a writer token for all accounts, so accounts can be added from the CLI
    // before any is shared. It sees no mail.
    let data = client
        .call("login", json!({ "profile": "writer" }))
        .await
        .unwrap();
    let bootstrap = SessionToken::from_string(data["token"].as_str().unwrap().to_owned());
    let mut bootstrap = h.client(Some(&bootstrap)).await;
    let error = api_error(
        bootstrap
            .call("search", json!({ "query": "x" }))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    bootstrap.call("account.list", json!({})).await.unwrap();
    let error = api_error(
        client
            .call("login", json!({ "profile": "writer", "accounts": ["a_b"] }))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    h.stop().await;
}

#[tokio::test]
async fn token_list_and_revoke() {
    let mut h = Harness::start(ScriptedApprover::new(|_| Some(Decision::Deny))).await;
    let (token, record) = h.token(Profile::ReadOnly, false).await;
    let (_, other) = h.token(Profile::Writer, false).await;
    let mut client = h.client(Some(&token)).await;

    let list = client.call("token.list", json!({})).await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert!(!list.to_string().contains("hash"));

    // Revoking another token: read-only can't.
    let error = client
        .call("token.revoke", json!({ "id": other.id }))
        .await
        .unwrap_err();
    assert_eq!(error.exit_code(), ExitCode::PermissionDenied);
    // Revoking your own token always works, without approval.
    let data = client
        .call("token.revoke", json!({ "id": record.id }))
        .await
        .unwrap();
    assert_eq!(data["revoked"], true);
    assert!(h.approver.requests().is_empty());
    let error = client.call("token.list", json!({})).await.unwrap_err();
    assert_eq!(error.exit_code(), ExitCode::NotSignedIn);

    // A writer revoking another token is asked (denied here), unknown ids are not found.
    let (writer, _) = h.token(Profile::Writer, false).await;
    let mut client = h.client(Some(&writer)).await;
    let error = client
        .call("token.revoke", json!({ "id": other.id }))
        .await
        .unwrap_err();
    assert_eq!(error.exit_code(), ExitCode::ApprovalDenied);
    h.stop().await;
}

#[tokio::test]
async fn handler_errors_and_bad_arguments() {
    let mut h = Harness::start(ScriptedApprover::approve_all()).await;
    let (token, _) = h.token(Profile::Writer, false).await;
    let mut client = h.client(Some(&token)).await;

    let error = api_error(
        client
            .call("show", json!({ "id": "m_zz" }))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code, ErrorCode::NotFound);
    let error = api_error(
        client
            .call("search", json!({ "query": 5 }))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code, ErrorCode::InvalidArgs);
    assert!(error.message.starts_with("query:"), "{}", error.message);
    let error = api_error(client.call("delete", json!({})).await.unwrap_err());
    assert_eq!(error.code, ErrorCode::UnknownCommand);
    assert_eq!(error.exit_code(), ExitCode::Usage);

    let events = h.audit.events();
    assert_eq!(
        events[0].outcome,
        AuditOutcome::Failed {
            code: ErrorCode::NotFound
        }
    );
    assert_eq!(
        events[1].outcome,
        AuditOutcome::Rejected {
            code: ErrorCode::InvalidArgs
        }
    );
    // Unknown command names are not copied into the audit log.
    assert_eq!(events[2].cmd, "<invalid>");
    h.stop().await;
}

#[tokio::test]
async fn long_running_commands_stream_progress() {
    let mut h = Harness::start(ScriptedApprover::approve_all()).await;
    let (token, _) = h.token(Profile::Writer, false).await;
    let mut progress = Vec::new();
    let data = h
        .client(Some(&token))
        .await
        .call_with_progress(
            "export",
            json!({ "query": "folder:Inbox", "to": "/tmp/out.mbox" }),
            |p| progress.push(p.clone()),
        )
        .await
        .unwrap();
    assert_eq!(data["messages"], 2);
    assert_eq!(
        progress,
        [
            Progress::approval_pending(),
            Progress::working("exporting", Some(1), Some(2))
        ]
    );
    h.stop().await;
}

#[tokio::test]
async fn garbage_and_oversized_frames_keep_the_connection() {
    let mut h = Harness::start_with(ScriptedApprover::approve_all(), |config| {
        config.max_frame_len = 1024;
    })
    .await;
    let mut framed = h.raw().await;

    framed.send(Bytes::from_static(b"not json")).await.unwrap();
    let response = read_response(&mut framed).await;
    assert_eq!(
        (response.id, error_code(&response)),
        (0, ErrorCode::BadRequest)
    );

    framed
        .send(Bytes::from_static(br#"{"v":1,"id":4,"cmd":7}"#))
        .await
        .unwrap();
    let response = read_response(&mut framed).await;
    assert_eq!(
        (response.id, error_code(&response)),
        (4, ErrorCode::BadRequest)
    );

    framed
        .send(Bytes::from_static(br#"{"id":5,"cmd":"status"}"#))
        .await
        .unwrap();
    let response = read_response(&mut framed).await;
    assert_eq!(
        (response.id, error_code(&response)),
        (5, ErrorCode::BadRequest)
    );

    // Over the server's 1 KiB limit: answered without reading it into memory.
    framed.send(Bytes::from(vec![b'x'; 5000])).await.unwrap();
    let response = read_response(&mut framed).await;
    assert_eq!(
        (response.id, error_code(&response)),
        (0, ErrorCode::FrameTooLarge)
    );

    // Still usable.
    framed
        .send(Bytes::from_static(br#"{"v":1,"id":9,"cmd":"status"}"#))
        .await
        .unwrap();
    let response = read_response(&mut framed).await;
    assert_eq!(response.id, 9);
    assert!(matches!(response.body, ResponseBody::Data(_)));
    assert!(h.audit.events().iter().any(|event| event.outcome
        == AuditOutcome::Rejected {
            code: ErrorCode::FrameTooLarge
        }));
    h.stop().await;
}

#[tokio::test]
async fn version_negotiation() {
    let mut h = Harness::start(ScriptedApprover::approve_all()).await;
    let mut framed = h.raw().await;
    // A newer client: the server answers restart_required with the request id, even
    // if the rest of the frame has an unknown shape.
    framed
        .send(Bytes::from_static(
            br#"{"v":2,"id":3,"command":{"name":"status"}}"#,
        ))
        .await
        .unwrap();
    let response = read_response(&mut framed).await;
    assert_eq!(response.v, PROTOCOL_VERSION);
    assert_eq!(response.id, 3);
    let ResponseBody::Error(error) = &response.body else {
        panic!("{response:?}");
    };
    assert_eq!(error.code, ErrorCode::RestartRequired);
    assert_eq!(error.message, "Maze was updated, restart it");
    assert_eq!(error.exit_code(), ExitCode::NotRunning);
    h.stop().await;

    // An older client talking to a newer server: the client reports it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut len = [0u8; 4];
        stream.read_exact(&mut len).await.unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        stream.read_exact(&mut body).await.unwrap();
        let reply = br#"{"v":2,"id":1,"result":{}}"#;
        stream
            .write_all(&(reply.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stream.write_all(reply).await.unwrap();
    });
    let mut client = LocalApiClient::connect(&Endpoint::Unix(path))
        .await
        .unwrap();
    let error = client.call("status", json!({})).await.unwrap_err();
    assert!(matches!(error, ClientError::RestartRequired), "{error:?}");
    assert_eq!(error.exit_code(), ExitCode::NotRunning);
}

#[tokio::test]
async fn other_uids_are_rejected() {
    let mut h = Harness::start_with(ScriptedApprover::approve_all(), |config| {
        // Pretend the server runs as someone else, so our own uid is "foreign".
        config.expected_uid = current_uid().wrapping_add(1);
    })
    .await;
    let error = h
        .client(None)
        .await
        .call("status", json!({}))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ClientError::Closed | ClientError::Io(_)),
        "{error:?}"
    );
    let events = h.audit.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cmd, "<connect>");
    assert_eq!(
        events[0].outcome,
        AuditOutcome::Rejected {
            code: ErrorCode::PermissionDenied
        }
    );
    assert_eq!(
        events[0].pid,
        Some(i32::try_from(std::process::id()).unwrap())
    );
    h.stop().await;
}

#[tokio::test]
async fn shutdown_aborts_requests_waiting_too_long() {
    let mut h = Harness::start_with(ScriptedApprover::new(|_| None), |config| {
        config.drain_timeout = Duration::from_millis(100);
        config.dispatch.approval_timeout = Duration::from_secs(60);
    })
    .await;
    let mut client = h.client(None).await;
    let pending = tokio::spawn(async move { client.call("login", json!({})).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = std::time::Instant::now();
    h.stop().await;
    assert!(started.elapsed() < Duration::from_secs(5));
    let error = pending.await.unwrap().unwrap_err();
    assert_eq!(error.exit_code(), ExitCode::NotRunning);
    assert!(!h.socket.exists());
}

#[tokio::test]
async fn account_management_needs_a_writer_token_and_approval() {
    let mut h = Harness::start(ScriptedApprover::approve_all()).await;
    let approver = h.approver.clone();
    let args = json!({ "email": "alice@example.org", "password": "hunter2" });

    let (triage, _) = h.token(Profile::Triage, false).await;
    let denied = api_error(
        h.client(Some(&triage))
            .await
            .call("account.add", args.clone())
            .await
            .unwrap_err(),
    );
    assert_eq!(denied.code, ErrorCode::PermissionDenied);
    assert!(approver.requests().is_empty());
    // Listing and syncing are reads; they work without any account in scope.
    let mut reader = h.client(Some(&triage)).await;
    reader.call("account.list", json!({})).await.unwrap();
    reader.call("sync", json!({})).await.unwrap();

    let (writer, _) = h.token(Profile::Writer, false).await;
    h.client(Some(&writer))
        .await
        .call("account.add", args)
        .await
        .unwrap();
    let requests = approver.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].command, "account.add");
    let ApprovalKind::Command { args, .. } = &requests[0].kind else {
        panic!("not a command approval");
    };
    assert_eq!(args.expose()["password"], "<redacted>");
    assert!(!format!("{:?}", requests[0]).contains("hunter2"));
    let seen = h.seen();
    assert_eq!(seen.last().unwrap().name, "account.add");
    assert!(seen.last().unwrap().approved);
    h.stop().await;
}
