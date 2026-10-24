//! One agent session: an ACP connection with one ACP session, linked to an agent
//! thread (and through it to a mail thread), PLAN §4.12.
//!
//! ```text
//! initialize (V1, terminal only) → [authenticate] → session/new | session/load
//!   → [session/set_config_option | session/set_mode: remembered choices]
//!   → session/prompt … (session/update stream, request_permission, terminal/*)
//!   → session/cancel → teardown
//! ```
//!
//! [`start_with_transport`] runs the session on Tokio and returns a [`SessionHandle`]:
//! the UI reads [`SessionEvent`]s and sends commands through it. The transport is the
//! sandboxed agent process in production ([`crate::AgentProcess`]) and an in-process
//! agent in tests.
//!
//! ACP callbacks run inside the SDK's dispatch loop, so the ones that wait for the user
//! (permission prompts, unapproved commands) only hand their request to the session
//! loop and return; the loop answers when the user decides.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AuthCapabilities, AuthMethod, AuthenticateRequest, BooleanConfigOptionCapabilities,
    CancelNotification, ClientCapabilities, ClientSessionCapabilities, ContentBlock,
    CreateTerminalRequest, CreateTerminalResponse, ErrorCode, Implementation, InitializeRequest,
    KillTerminalRequest, KillTerminalResponse, LoadSessionRequest, NewSessionRequest,
    PermissionOptionKind, PlanEntryStatus, PromptRequest, PromptResponse, ReleaseTerminalRequest,
    ReleaseTerminalResponse, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionConfigOptionsCapabilities,
    SessionId, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionModeRequest, StopReason, TerminalExitStatus, TerminalOutputRequest,
    TerminalOutputResponse, TextContent, ToolCallContent, ToolCallStatus,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Responder};
use local_api::{TokenId, TokenStore};
use sandbox::SandboxPolicy;

use crate::auth::{UrlInbox, links_in};
use crate::controls::{self, Control, ControlValue};
use crate::host::{AgentHost, AgentThreadKey};
use crate::process::LaunchSpec;
use crate::scratch::{Scratch, THREAD_FILE};
use crate::terminal::{CommandClass, ExitInfo, TerminalHost, classify};
use crate::transcript::{
    EntryUpdate, NoticeLevel, PermissionChoice, PermissionEntry, PermissionKind, PlanItem,
    PlanStatus, TerminalEntry, TerminalStatus, ToolCallEntry, ToolStatus, Transcript,
};

/// Output shown per terminal in the agent column.
const TERMINAL_TAIL: usize = 4096;
/// Agent stderr lines kept for error messages.
const STDERR_LINES: usize = 20;

/// The mail thread attached to the session as context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedThread {
    /// Public id (`t_…`).
    pub public_id: String,
}

/// How one session runs.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Maze's id for the session (its scratch dir's name).
    pub session_id: String,
    /// The agent's id.
    pub agent_id: String,
    /// The agent's display name.
    pub agent_name: String,
    /// The stored conversation.
    pub agent_thread: AgentThreadKey,
    /// The agent's earlier session id to load (`session/load`), when resuming.
    pub resume_acp_session: Option<String>,
    /// The working directory (the scratch dir).
    pub cwd: PathBuf,
    /// Environment for terminal commands (the agent's environment).
    pub terminal_env: BTreeMap<String, String>,
    /// The `mazemail` CLI that plain `mazemail …` commands run.
    pub cli_program: Option<PathBuf>,
    /// The sandbox for terminal commands; `None` only when the user turned it off.
    pub terminal_policy: Option<SandboxPolicy>,
    /// The attached mail thread.
    pub attached: Option<AttachedThread>,
    /// The conversation so far (when resuming).
    pub transcript: Transcript,
    /// The file the agent's `open` shim writes links to ([`crate::auth::OPEN_SHIM`]).
    pub open_urls: Option<PathBuf>,
    /// The environment variable carrying the user's stored API key, if any: an
    /// `api-key` sign-in method is then used without asking.
    pub key_env: Option<String>,
    /// Remembered choices (control id → value) applied when the session starts.
    pub preferred: BTreeMap<String, ControlValue>,
}

/// The session's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// Starting the agent and initializing.
    Starting,
    /// Waiting for the user to sign in.
    SignInRequired,
    /// Ready for a prompt.
    Ready,
    /// Working on a prompt.
    Working,
    /// Ended.
    Ended,
}

/// A way to sign in that the agent offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthMethodInfo {
    /// Method id for [`SessionHandle::authenticate`].
    pub id: String,
    /// Label.
    pub name: String,
    /// More text.
    pub description: Option<String>,
    /// A login the client runs in a terminal instead of calling `authenticate`.
    pub terminal: Option<TerminalAuth>,
    /// The provider of an API-key method (`_meta["api-key"].provider`, e.g. `openai`).
    pub api_key_provider: Option<String>,
}

/// A terminal login: the agent's own command plus `args` (ACP `terminal` methods), or
/// another `command` (the older `_meta["terminal-auth"]`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalAuth {
    /// A program to run instead of the agent's.
    pub command: Option<String>,
    /// Arguments (appended to the agent's own when `command` is `None`).
    pub args: Vec<String>,
    /// Extra environment.
    pub env: BTreeMap<String, String>,
}

/// The agent wants a sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInRequest {
    /// The ways it offers.
    pub methods: Vec<AuthMethodInfo>,
    /// What the agent said (its auth error).
    pub message: Option<String>,
}

/// How a sign-in goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInProgress {
    /// `authenticate` was sent for this method; the agent may be waiting for a browser.
    Started(String),
    /// It failed; the agent's error.
    Failed(String),
    /// It worked.
    Succeeded,
}

/// What the UI hears from a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// The state changed.
    Status(SessionStatus),
    /// The agent introduced itself.
    AgentInfo {
        /// Name.
        name: String,
        /// Version.
        version: String,
    },
    /// The agent needs a sign-in first.
    SignInRequired(SignInRequest),
    /// A sign-in started, failed or worked.
    SignIn(SignInProgress),
    /// The agent wants a link opened (through its `open`, or printed while signing
    /// in). `open` is set when Maze should open it right away: during a sign-in, for
    /// links the agent handed to `open`.
    Link {
        /// An `https://` link.
        url: String,
        /// Open it now.
        open: bool,
    },
    /// What the agent lets the user choose (model, mode, thinking, …) changed.
    Controls(Vec<Control>),
    /// The conversation changed.
    Update(EntryUpdate),
    /// The session ended; with an error if it didn't end by request.
    Ended(Option<String>),
}

/// What the UI asks of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommand {
    /// Send a prompt.
    Prompt(String),
    /// Stop the current turn.
    Cancel,
    /// Answer a permission prompt.
    Permission {
        /// Prompt id.
        id: u64,
        /// The decision.
        choice: PermissionChoice,
    },
    /// Sign in with one of the agent's methods.
    Authenticate(String),
    /// The attached mail thread changed (the file is already written).
    SetAttached(Option<AttachedThread>),
    /// Change a control (model, mode, thinking, …).
    SetControl {
        /// The control's id.
        id: String,
        /// The new value.
        value: ControlValue,
    },
    /// End the session.
    Shutdown,
}

/// The UI's end of a running session. Dropping it ends the session.
#[derive(Debug)]
pub struct SessionHandle {
    id: String,
    agent_id: String,
    agent_thread: AgentThreadKey,
    events: async_channel::Receiver<SessionEvent>,
    commands: async_channel::Sender<SessionCommand>,
}

impl SessionHandle {
    /// Maze's session id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The agent's id.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The stored conversation.
    pub fn agent_thread(&self) -> AgentThreadKey {
        self.agent_thread
    }

    /// The event stream (runtime-independent, so GPUI can await it).
    pub fn events(&self) -> async_channel::Receiver<SessionEvent> {
        self.events.clone()
    }

    fn send(&self, command: SessionCommand) {
        if self.commands.try_send(command).is_err() {
            tracing::debug!("the agent session has ended");
        }
    }

    /// Sends a prompt.
    pub fn prompt(&self, text: impl Into<String>) {
        self.send(SessionCommand::Prompt(text.into()));
    }

    /// Stops the current turn.
    pub fn cancel(&self) {
        self.send(SessionCommand::Cancel);
    }

    /// Answers a permission prompt.
    pub fn respond(&self, id: u64, choice: PermissionChoice) {
        self.send(SessionCommand::Permission { id, choice });
    }

    /// Signs in with `method`.
    pub fn authenticate(&self, method: impl Into<String>) {
        self.send(SessionCommand::Authenticate(method.into()));
    }

    /// Changes a control (model, mode, thinking, …).
    pub fn set_control(&self, id: impl Into<String>, value: ControlValue) {
        self.send(SessionCommand::SetControl {
            id: id.into(),
            value,
        });
    }

    /// Tells the session the attached thread changed.
    pub fn set_attached(&self, attached: Option<AttachedThread>) {
        self.send(SessionCommand::SetAttached(attached));
    }

    /// Ends the session.
    pub fn shutdown(&self) {
        self.send(SessionCommand::Shutdown);
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        let _ = self.commands.try_send(SessionCommand::Shutdown);
        self.commands.close();
    }
}

/// What a session owns and gives back when it ends.
#[derive(Default)]
pub struct SessionResources {
    /// The scratch dir, deleted at the end.
    pub scratch: Option<Scratch>,
    /// The session token, revoked at the end.
    pub token: Option<(Arc<dyn TokenStore>, TokenId)>,
    /// The agent's last stderr lines.
    pub stderr: Option<StderrTail>,
    /// Released when the session ended (the agent version it runs on).
    pub lease: Option<Box<dyn std::any::Any + Send + Sync>>,
}

impl std::fmt::Debug for SessionResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionResources")
            .field("scratch", &self.scratch)
            .field("token", &self.token.as_ref().map(|(_, id)| id))
            .finish_non_exhaustive()
    }
}

/// The last lines an agent wrote to stderr, and new ones as they come.
#[derive(Debug, Clone, Default)]
pub struct StderrTail {
    lines: Arc<Mutex<VecDeque<String>>>,
    live: Option<async_channel::Sender<String>>,
}

impl StderrTail {
    /// A tail that also forwards lines to `live` (sign-in links).
    pub fn new(live: async_channel::Sender<String>) -> Self {
        Self {
            lines: Arc::default(),
            live: Some(live),
        }
    }

    /// Records a line.
    pub fn push(&self, line: String) {
        if let Some(live) = &self.live {
            let _ = live.try_send(line.clone());
        }
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() == STDERR_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    fn text(&self) -> String {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Runs a session over `transport` on the current Tokio runtime.
pub fn start_with_transport<T>(
    transport: T,
    config: SessionConfig,
    host: Arc<dyn AgentHost>,
    resources: SessionResources,
    stderr_live: Option<async_channel::Receiver<String>>,
) -> SessionHandle
where
    T: ConnectTo<Client> + 'static,
{
    let (events_tx, events_rx) = async_channel::unbounded();
    let (commands_tx, commands_rx) = async_channel::unbounded();
    let handle = SessionHandle {
        id: config.session_id.clone(),
        agent_id: config.agent_id.clone(),
        agent_thread: config.agent_thread,
        events: events_rx,
        commands: commands_tx,
    };
    tokio::spawn(run(
        transport,
        config,
        host,
        resources,
        events_tx,
        commands_rx,
        stderr_live,
    ));
    handle
}

enum HostRequest {
    Permission(
        Box<(
            RequestPermissionRequest,
            Responder<RequestPermissionResponse>,
        )>,
    ),
    CreateTerminal(Box<(CreateTerminalRequest, Responder<CreateTerminalResponse>)>),
    TerminalExited(String),
}

/// Terminal display state shared with the callbacks.
type TerminalViews = Arc<Mutex<HashMap<String, TerminalEntry>>>;

/// The session's controls, shared with the update callback.
type SharedControls = Arc<Mutex<Vec<Control>>>;

/// Applies a mode or config option update to the controls; `false` for other updates.
fn update_controls(
    update: &SessionUpdate,
    controls: &Mutex<Vec<Control>>,
    events: &async_channel::Sender<SessionEvent>,
) -> bool {
    let mut controls = controls.lock().unwrap_or_else(PoisonError::into_inner);
    match update {
        SessionUpdate::CurrentModeUpdate(update) => {
            if let Some(mode) = controls.iter_mut().find(|c| c.legacy_mode) {
                mode.set(&ControlValue::Choice(update.current_mode_id.0.to_string()));
            }
        }
        SessionUpdate::ConfigOptionUpdate(update) => {
            *controls = controls::from_config_options(&update.config_options);
        }
        _ => return false,
    }
    let _ = events.try_send(SessionEvent::Controls(controls.clone()));
    true
}

#[allow(clippy::too_many_arguments)]
async fn run<T>(
    transport: T,
    config: SessionConfig,
    host: Arc<dyn AgentHost>,
    resources: SessionResources,
    events: async_channel::Sender<SessionEvent>,
    commands: async_channel::Receiver<SessionCommand>,
    stderr_live: Option<async_channel::Receiver<String>>,
) where
    T: ConnectTo<Client> + 'static,
{
    let transcript = Arc::new(Mutex::new(config.transcript.clone()));
    let terminals = Arc::new(TerminalHost::new());
    let agent_thread = config.agent_thread;
    let result = connect(
        transport,
        config,
        Arc::clone(&host),
        Arc::clone(&transcript),
        Arc::clone(&terminals),
        events.clone(),
        commands,
        stderr_live,
    )
    .await;
    terminals.kill_all();

    let snapshot = transcript
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    if !snapshot.entries.is_empty()
        && let Err(error) = host.save_transcript(agent_thread, snapshot.to_json()).await
    {
        tracing::warn!(%error, "could not save the agent transcript");
    }
    if let Some((store, id)) = &resources.token {
        crate::tokens::revoke(store.as_ref(), id).await;
    }
    let error = result.err().map(|error| {
        let stderr = resources
            .stderr
            .as_ref()
            .map(StderrTail::text)
            .unwrap_or_default();
        if stderr.is_empty() {
            error
        } else {
            format!("{error}\n{stderr}")
        }
    });
    drop(resources.scratch);
    drop(resources.lease);
    let _ = events.try_send(SessionEvent::Status(SessionStatus::Ended));
    let _ = events.try_send(SessionEvent::Ended(error));
}

/// Session state owned by the main loop.
struct Loop {
    session: SessionId,
    config: SessionConfig,
    cx: ConnectionTo<Agent>,
    host: Arc<dyn AgentHost>,
    transcript: Arc<Mutex<Transcript>>,
    terminals: Arc<TerminalHost>,
    views: TerminalViews,
    events: async_channel::Sender<SessionEvent>,
    requests: async_channel::Sender<HostRequest>,
    next_prompt: u64,
    pending_tools: HashMap<
        u64,
        (
            RequestPermissionRequest,
            Responder<RequestPermissionResponse>,
        ),
    >,
    pending_commands: HashMap<u64, PendingCommand>,
    allowed_programs: HashSet<String>,
    context_sent: Option<String>,
    sign_in: SignIn,
    controls: SharedControls,
}

struct PendingCommand {
    request: CreateTerminalRequest,
    responder: Responder<CreateTerminalResponse>,
    program: String,
    terminal_id: String,
}

type PromptFuture =
    Pin<Box<dyn Future<Output = Result<PromptResponse, agent_client_protocol::Error>> + Send>>;

fn emit_to(
    transcript: &Mutex<Transcript>,
    events: &async_channel::Sender<SessionEvent>,
    update: EntryUpdate,
) {
    transcript
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .apply(&update);
    let _ = events.try_send(SessionEvent::Update(update));
}

#[allow(clippy::too_many_arguments)]
async fn connect<T>(
    transport: T,
    config: SessionConfig,
    host: Arc<dyn AgentHost>,
    transcript: Arc<Mutex<Transcript>>,
    terminals: Arc<TerminalHost>,
    events: async_channel::Sender<SessionEvent>,
    commands: async_channel::Receiver<SessionCommand>,
    stderr_live: Option<async_channel::Receiver<String>>,
) -> Result<(), String>
where
    T: ConnectTo<Client> + 'static,
{
    let (requests_tx, requests_rx) = async_channel::unbounded::<HostRequest>();
    let replaying = Arc::new(AtomicBool::new(false));
    let views: TerminalViews = Arc::default();
    let controls: SharedControls = Arc::default();

    let on_update = {
        let transcript = Arc::clone(&transcript);
        let events = events.clone();
        let replaying = Arc::clone(&replaying);
        let controls = Arc::clone(&controls);
        async move |notification: SessionNotification, _cx: ConnectionTo<Agent>| {
            if update_controls(&notification.update, &controls, &events) {
                return Ok(());
            }
            // `session/load` replays the old conversation; Maze has it stored already.
            if !replaying.load(Ordering::SeqCst) {
                for update in map_update(notification.update) {
                    emit_to(&transcript, &events, update);
                }
            }
            Ok(())
        }
    };
    let on_permission = {
        let requests = requests_tx.clone();
        async move |request: RequestPermissionRequest,
                    responder: Responder<RequestPermissionResponse>,
                    _cx: ConnectionTo<Agent>| {
            if let Err(error) =
                requests.try_send(HostRequest::Permission(Box::new((request, responder))))
                && let HostRequest::Permission(pending) = error.into_inner()
            {
                pending.1.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))?;
            }
            Ok(())
        }
    };
    let on_create_terminal = {
        let requests = requests_tx.clone();
        async move |request: CreateTerminalRequest,
                    responder: Responder<CreateTerminalResponse>,
                    _cx: ConnectionTo<Agent>| {
            if let Err(error) =
                requests.try_send(HostRequest::CreateTerminal(Box::new((request, responder))))
                && let HostRequest::CreateTerminal(pending) = error.into_inner()
            {
                pending.1.respond_with_error(session_ended())?;
            }
            Ok(())
        }
    };
    let on_output = {
        let terminals = Arc::clone(&terminals);
        let views = Arc::clone(&views);
        let transcript = Arc::clone(&transcript);
        let events = events.clone();
        async move |request: TerminalOutputRequest,
                    responder: Responder<TerminalOutputResponse>,
                    _cx: ConnectionTo<Agent>| {
            let id = request.terminal_id.0.to_string();
            match terminals.output(&id) {
                Some(output) => {
                    update_view(
                        &views,
                        &transcript,
                        &events,
                        &id,
                        &output.output,
                        output.exit.as_ref(),
                    );
                    responder.respond(
                        TerminalOutputResponse::new(output.output, output.truncated)
                            .exit_status(output.exit.map(exit_status)),
                    )
                }
                None => responder.respond_with_error(unknown_terminal()),
            }
        }
    };
    let on_wait = {
        let terminals = Arc::clone(&terminals);
        async move |request: WaitForTerminalExitRequest,
                    responder: Responder<WaitForTerminalExitResponse>,
                    _cx: ConnectionTo<Agent>| {
            let terminals = Arc::clone(&terminals);
            // Waiting must not hold the dispatch loop.
            tokio::spawn(async move {
                let id = request.terminal_id.0.to_string();
                let _ = match terminals.wait(&id).await {
                    Some(exit) => {
                        responder.respond(WaitForTerminalExitResponse::new(exit_status(exit)))
                    }
                    None => responder.respond_with_error(unknown_terminal()),
                };
            });
            Ok(())
        }
    };
    let on_kill = {
        let terminals = Arc::clone(&terminals);
        async move |request: KillTerminalRequest,
                    responder: Responder<KillTerminalResponse>,
                    _cx: ConnectionTo<Agent>| {
            if terminals.kill(&request.terminal_id.0) {
                responder.respond(KillTerminalResponse::new())
            } else {
                responder.respond_with_error(unknown_terminal())
            }
        }
    };
    let on_release = {
        let terminals = Arc::clone(&terminals);
        async move |request: ReleaseTerminalRequest,
                    responder: Responder<ReleaseTerminalResponse>,
                    _cx: ConnectionTo<Agent>| {
            terminals.release(&request.terminal_id.0);
            responder.respond(ReleaseTerminalResponse::new())
        }
    };

    let main = {
        let transcript = Arc::clone(&transcript);
        let events = events.clone();
        let host = Arc::clone(&host);
        let terminals = Arc::clone(&terminals);
        let views = Arc::clone(&views);
        async move |cx: ConnectionTo<Agent>| {
            let result = session_main(
                controls,
                cx,
                config,
                host,
                transcript,
                terminals,
                views,
                events,
                commands,
                requests_tx,
                requests_rx,
                replaying,
                stderr_live,
            )
            .await;
            result.map_err(|message| agent_client_protocol::Error::internal_error().data(message))
        }
    };

    Client
        .builder()
        .name("mazemail")
        .on_receive_notification(on_update, agent_client_protocol::on_receive_notification!())
        .on_receive_request(on_permission, agent_client_protocol::on_receive_request!())
        .on_receive_request(
            on_create_terminal,
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(on_output, agent_client_protocol::on_receive_request!())
        .on_receive_request(on_wait, agent_client_protocol::on_receive_request!())
        .on_receive_request(on_kill, agent_client_protocol::on_receive_request!())
        .on_receive_request(on_release, agent_client_protocol::on_receive_request!())
        .connect_with(transport, main)
        .await
        .map_err(describe_error)
}

fn describe_error(error: agent_client_protocol::Error) -> String {
    match error.data.as_ref().and_then(|d| d.as_str()) {
        Some(data) if !data.is_empty() => format!("{}: {data}", error.message),
        _ => error.message,
    }
}

fn session_ended() -> agent_client_protocol::Error {
    agent_client_protocol::Error::new(-32603, "the session is ending")
}

fn unknown_terminal() -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data("unknown terminal id")
}

fn exit_status(exit: ExitInfo) -> TerminalExitStatus {
    TerminalExitStatus::new()
        .exit_code(exit.code)
        .signal(exit.signal)
}

fn tail(text: &str) -> String {
    if text.len() <= TERMINAL_TAIL {
        return text.to_owned();
    }
    let mut cut = text.len() - TERMINAL_TAIL;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    text[cut..].to_owned()
}

fn update_view(
    views: &TerminalViews,
    transcript: &Mutex<Transcript>,
    events: &async_channel::Sender<SessionEvent>,
    id: &str,
    output: &str,
    exit: Option<&ExitInfo>,
) {
    let entry = {
        let mut views = views.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(view) = views.get_mut(id) else {
            return;
        };
        view.output = tail(output);
        if let Some(exit) = exit {
            view.status = TerminalStatus::Exited {
                code: exit.code,
                signal: exit.signal.clone(),
            };
        }
        view.clone()
    };
    emit_to(transcript, events, EntryUpdate::Terminal(entry));
}

#[allow(clippy::too_many_arguments)]
async fn session_main(
    controls: SharedControls,
    cx: ConnectionTo<Agent>,
    config: SessionConfig,
    host: Arc<dyn AgentHost>,
    transcript: Arc<Mutex<Transcript>>,
    terminals: Arc<TerminalHost>,
    views: TerminalViews,
    events: async_channel::Sender<SessionEvent>,
    commands: async_channel::Receiver<SessionCommand>,
    requests_tx: async_channel::Sender<HostRequest>,
    requests_rx: async_channel::Receiver<HostRequest>,
    replaying: Arc<AtomicBool>,
    stderr_live: Option<async_channel::Receiver<String>>,
) -> Result<(), String> {
    let status = |status| {
        let _ = events.try_send(SessionEvent::Status(status));
    };
    status(SessionStatus::Starting);
    // Terminal logins: ACP's `auth.terminal`, and the older `_meta["terminal-auth"]`
    // that Claude Code's adapter also reads.
    let mut meta = serde_json::Map::new();
    meta.insert("terminal-auth".to_owned(), serde_json::Value::Bool(true));
    let init = cx
        .send_request(
            InitializeRequest::new(ProtocolVersion::V1)
                .client_capabilities(
                    ClientCapabilities::new()
                        .terminal(true)
                        .auth(AuthCapabilities::new().terminal(true))
                        .session(
                            ClientSessionCapabilities::new().config_options(
                                SessionConfigOptionsCapabilities::new()
                                    .boolean(BooleanConfigOptionCapabilities::new()),
                            ),
                        )
                        .meta(meta),
                )
                .client_info(
                    Implementation::new("mazemail", env!("CARGO_PKG_VERSION"))
                        .title("Maze".to_owned()),
                ),
        )
        .block_task()
        .await
        .map_err(|e| format!("the agent didn't start: {}", describe_error(e)))?;
    if let Some(info) = &init.agent_info {
        let _ = events.try_send(SessionEvent::AgentInfo {
            name: info.title.clone().unwrap_or_else(|| info.name.clone()),
            version: info.version.clone(),
        });
    }
    let methods: Vec<AuthMethodInfo> = init.auth_methods.iter().map(auth_info).collect();
    let mut sign_in = SignIn {
        cx: cx.clone(),
        events: events.clone(),
        transcript: Arc::clone(&transcript),
        stderr: stderr_live,
        inbox: config.open_urls.clone().map(UrlInbox::new),
        methods,
        key_env: config.key_env.clone(),
        tried_key: false,
    };

    let mut resume = config
        .resume_acp_session
        .clone()
        .filter(|_| init.agent_capabilities.load_session);
    let session = loop {
        let attempt = match resume.take() {
            Some(previous) => {
                replaying.store(true, Ordering::SeqCst);
                let loaded = cx
                    .send_request(LoadSessionRequest::new(
                        previous.clone(),
                        config.cwd.clone(),
                    ))
                    .block_task()
                    .await;
                replaying.store(false, Ordering::SeqCst);
                match loaded {
                    Ok(response) => Ok((
                        SessionId::new(previous),
                        controls::controls(
                            response.config_options.as_deref(),
                            response.modes.as_ref(),
                        ),
                    )),
                    Err(error) if error.code == ErrorCode::AuthRequired => Err(error),
                    Err(_) => {
                        emit_to(
                            &transcript,
                            &events,
                            EntryUpdate::Notice(
                                "The agent couldn't restore the earlier session, so it starts \
                                 fresh and won't remember the messages above."
                                    .to_owned(),
                                NoticeLevel::Info,
                            ),
                        );
                        continue;
                    }
                }
            }
            None => cx
                .send_request(NewSessionRequest::new(config.cwd.clone()))
                .block_task()
                .await
                .map(|response| {
                    (
                        response.session_id,
                        controls::controls(
                            response.config_options.as_deref(),
                            response.modes.as_ref(),
                        ),
                    )
                }),
        };
        match attempt {
            Ok((session, initial)) => {
                *controls.lock().unwrap_or_else(PoisonError::into_inner) = initial;
                break session;
            }
            Err(error) if error.code == ErrorCode::AuthRequired => {
                if !sign_in.required(auth_message(&error), &commands).await {
                    return Ok(());
                }
                status(SessionStatus::Starting);
            }
            Err(error) => {
                return Err(format!(
                    "the agent refused to start a session: {}",
                    describe_error(error)
                ));
            }
        }
    };
    let title = transcript
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .title();
    if let Err(error) = host
        .update_agent_thread(config.agent_thread, title.as_deref(), Some(&session.0))
        .await
    {
        tracing::warn!(%error, "could not store the agent session id");
    }
    let initial = controls
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let _ = events.try_send(SessionEvent::Controls(initial));
    status(SessionStatus::Ready);

    let mut state = Loop {
        session,
        config,
        cx,
        host,
        transcript,
        terminals,
        views,
        events,
        requests: requests_tx,
        next_prompt: 0,
        pending_tools: HashMap::new(),
        pending_commands: HashMap::new(),
        allowed_programs: HashSet::new(),
        context_sent: None,
        sign_in,
        controls,
    };
    state.apply_preferences().await;
    state.run(commands, requests_rx).await
}

/// How often links from the agent's `open` are picked up.
const LINK_POLL: Duration = Duration::from_millis(300);

/// What a sign-in attempt came to.
enum Authenticated {
    /// The agent accepted it.
    Yes,
    /// It failed (reported to the UI).
    No,
    /// The user picked another method meanwhile.
    Switch(String),
    /// The session ended meanwhile.
    Ended,
}

/// Sign-in state: the agent's methods, and where its links come from.
struct SignIn {
    cx: ConnectionTo<Agent>,
    events: async_channel::Sender<SessionEvent>,
    transcript: Arc<Mutex<Transcript>>,
    stderr: Option<async_channel::Receiver<String>>,
    inbox: Option<UrlInbox>,
    methods: Vec<AuthMethodInfo>,
    key_env: Option<String>,
    tried_key: bool,
}

impl SignIn {
    fn send(&self, event: SessionEvent) {
        let _ = self.events.try_send(event);
    }

    /// Links the agent handed to its `open` since the last call.
    fn relay_opened(&mut self, open: bool) {
        if let Some(inbox) = self.inbox.as_mut() {
            for url in inbox.take() {
                let _ = self.events.try_send(SessionEvent::Link { url, open });
            }
        }
    }

    /// The next stderr line; pending forever without one.
    async fn stderr_line(&mut self) -> Option<String> {
        match &self.stderr {
            Some(stderr) => match stderr.recv().await {
                Ok(line) => Some(line),
                Err(_) => {
                    self.stderr = None;
                    None
                }
            },
            None => std::future::pending().await,
        }
    }

    /// Shows links the agent printed (not opened: the user clicks them).
    fn relay_printed(&self, line: &str) {
        for url in links_in(line) {
            self.send(SessionEvent::Link { url, open: false });
        }
    }

    /// The agent wants a sign-in: tells the UI and waits until the user signed in
    /// through `authenticate` (`true`) or the session ended (`false`). Terminal logins
    /// and keys restart the session from the UI.
    async fn required(
        &mut self,
        message: Option<String>,
        commands: &async_channel::Receiver<SessionCommand>,
    ) -> bool {
        self.send(SessionEvent::Status(SessionStatus::SignInRequired));
        self.send(SessionEvent::SignInRequired(SignInRequest {
            methods: self.methods.clone(),
            message,
        }));
        // With the user's key in the environment, an API-key method needs no choice.
        if self.key_env.is_some() && !self.tried_key {
            self.tried_key = true;
            if let Some(method) = self.methods.iter().find(|m| m.api_key_provider.is_some()) {
                match self.authenticate(method.id.clone(), commands).await {
                    Authenticated::Yes => return true,
                    Authenticated::Ended => return false,
                    Authenticated::No | Authenticated::Switch(_) => {}
                }
            }
        }
        let mut tick = tokio::time::interval(LINK_POLL);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Ok(SessionCommand::Authenticate(method)) => {
                        let mut method = method;
                        loop {
                            match self.authenticate(method, commands).await {
                                Authenticated::Yes => return true,
                                Authenticated::Ended => return false,
                                Authenticated::No => break,
                                Authenticated::Switch(other) => method = other,
                            }
                        }
                    }
                    Ok(SessionCommand::Shutdown) | Err(_) => return false,
                    Ok(SessionCommand::Prompt(_)) => self.sign_in_first(),
                    Ok(_) => {}
                },
                _ = tick.tick() => self.relay_opened(true),
                line = self.stderr_line() => {
                    if let Some(line) = line {
                        self.relay_printed(&line);
                    }
                }
            }
        }
    }

    fn sign_in_first(&self) {
        emit_to(
            &self.transcript,
            &self.events,
            EntryUpdate::Notice("Sign in to the agent first.".to_owned(), NoticeLevel::Info),
        );
    }

    /// Runs `authenticate`. Meanwhile links the agent opens are opened for it (it
    /// can't start a browser in the sandbox) and links it prints are shown.
    async fn authenticate(
        &mut self,
        method: String,
        commands: &async_channel::Receiver<SessionCommand>,
    ) -> Authenticated {
        self.send(SessionEvent::SignIn(SignInProgress::Started(
            method.clone(),
        )));
        let request = self
            .cx
            .send_request(AuthenticateRequest::new(method.clone()))
            .block_task();
        tokio::pin!(request);
        let mut tick = tokio::time::interval(LINK_POLL);
        loop {
            tokio::select! {
                result = &mut request => {
                    self.relay_opened(true);
                    return match result {
                        Ok(_) => {
                            self.send(SessionEvent::SignIn(SignInProgress::Succeeded));
                            Authenticated::Yes
                        }
                        Err(error) => {
                            let message = describe_error(error);
                            for url in links_in(&message) {
                                self.send(SessionEvent::Link { url, open: false });
                            }
                            self.send(SessionEvent::SignIn(SignInProgress::Failed(message)));
                            Authenticated::No
                        }
                    };
                }
                _ = tick.tick() => self.relay_opened(true),
                line = self.stderr_line() => {
                    if let Some(line) = line {
                        self.relay_printed(&line);
                    }
                }
                command = commands.recv() => match command {
                    Ok(SessionCommand::Shutdown) | Err(_) => return Authenticated::Ended,
                    Ok(SessionCommand::Authenticate(other)) if other != method => {
                        return Authenticated::Switch(other);
                    }
                    Ok(SessionCommand::Prompt(_)) => self.sign_in_first(),
                    Ok(_) => {}
                },
            }
        }
    }
}

/// The agent's explanation of an auth error, if it gave more than the code's name.
fn auth_message(error: &agent_client_protocol::Error) -> Option<String> {
    let data = error
        .data
        .as_ref()
        .and_then(|data| match data {
            serde_json::Value::String(text) => Some(text.clone()),
            serde_json::Value::Object(map) => map
                .get("message")
                .or_else(|| map.get("reason"))
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            _ => None,
        })
        .filter(|text| !text.trim().is_empty());
    let message = Some(error.message.clone())
        .filter(|m| !m.trim().is_empty() && m != "Authentication required");
    match (message, data) {
        (Some(message), Some(data)) if !message.contains(&data) => {
            Some(format!("{message} {data}"))
        }
        (Some(message), _) => Some(message),
        (None, data) => data,
    }
}

fn auth_info(method: &AuthMethod) -> AuthMethodInfo {
    let meta = method.meta();
    let api_key_provider = meta
        .and_then(|meta| meta.get("api-key"))
        .and_then(|value| value.get("provider"))
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    let terminal = match method {
        AuthMethod::Terminal(terminal) => Some(TerminalAuth {
            command: None,
            args: terminal.args.clone(),
            env: terminal
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }),
        // The older form: an agent method with `_meta["terminal-auth"]`.
        _ => meta
            .and_then(|meta| meta.get("terminal-auth"))
            .and_then(|value| {
                let command = value.get("command")?.as_str()?.to_owned();
                let strings = |key: &str| -> Vec<String> {
                    value
                        .get(key)
                        .and_then(|v| v.as_array())
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|v| v.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let env = value
                    .get("env")
                    .and_then(|v| v.as_object())
                    .map(|map| {
                        map.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                            .collect()
                    })
                    .unwrap_or_default();
                Some(TerminalAuth {
                    command: Some(command),
                    args: strings("args"),
                    env,
                })
            }),
    };
    AuthMethodInfo {
        id: method.id().0.to_string(),
        name: method.name().to_owned(),
        description: method.description().map(str::to_owned),
        terminal,
        api_key_provider,
    }
}

impl Loop {
    fn emit(&self, update: EntryUpdate) {
        emit_to(&self.transcript, &self.events, update);
    }

    fn status(&self, status: SessionStatus) {
        let _ = self.events.try_send(SessionEvent::Status(status));
    }

    async fn run(
        &mut self,
        commands: async_channel::Receiver<SessionCommand>,
        requests: async_channel::Receiver<HostRequest>,
    ) -> Result<(), String> {
        let mut prompt: Option<PromptFuture> = None;
        let cx = self.cx.clone();
        let mut tick = tokio::time::interval(LINK_POLL);
        loop {
            tokio::select! {
                biased;
                request = requests.recv() => {
                    if let Ok(request) = request {
                        self.on_request(request);
                    }
                }
                _ = tick.tick() => self.sign_in.relay_opened(false),
                command = commands.recv() => match command {
                    Ok(SessionCommand::Shutdown) | Err(_) => break,
                    Ok(SessionCommand::Prompt(text)) => {
                        if prompt.is_some() {
                            self.emit(EntryUpdate::Notice(
                                "The agent is still working; wait or stop it first.".to_owned(),
                                NoticeLevel::Info,
                            ));
                        } else {
                            prompt = Some(self.start_prompt(text));
                            self.status(SessionStatus::Working);
                        }
                    }
                    Ok(SessionCommand::Cancel) => self.cancel(),
                    Ok(SessionCommand::Permission { id, choice }) => self.decide(id, choice),
                    Ok(SessionCommand::Authenticate(method)) if prompt.is_none() => {
                        if !self.sign_in_again(method, &commands).await {
                            break;
                        }
                    }
                    Ok(SessionCommand::Authenticate(_)) => {}
                    Ok(SessionCommand::SetAttached(attached)) => {
                        self.config.attached = attached;
                    }
                    Ok(SessionCommand::SetControl { id, value }) => {
                        if prompt.is_some() {
                            self.emit(EntryUpdate::Notice(
                                "The agent is working; change this when it's done.".to_owned(),
                                NoticeLevel::Info,
                            ));
                            self.emit_controls();
                        } else if let Err(error) = self.set_control(&id, value).await {
                            self.emit(EntryUpdate::Notice(
                                format!("The agent didn't take the change: {error}"),
                                NoticeLevel::Error,
                            ));
                            self.emit_controls();
                        }
                    }
                },
                result = async {
                    match prompt.as_mut() {
                        Some(future) => future.await,
                        None => std::future::pending().await,
                    }
                } => {
                    prompt = None;
                    // Updates sent before the response are queued first; take them in.
                    while let Ok(request) = requests.try_recv() {
                        self.on_request(request);
                    }
                    self.turn_ended(result).await;
                }
                () = cx.incoming_closed() => {
                    return Err("the agent stopped".to_owned());
                }
            }
        }
        if prompt.is_some() {
            self.cancel();
        }
        Ok(())
    }

    fn start_prompt(&mut self, text: String) -> PromptFuture {
        self.emit(EntryUpdate::UserMessage(text.clone()));
        let mut blocks = Vec::new();
        if let Some(attached) = &self.config.attached
            && self.context_sent.as_deref() != Some(attached.public_id.as_str())
        {
            blocks.push(ContentBlock::Text(TextContent::new(format!(
                "Context: the mail thread {id} selected in Maze is exported to ./{THREAD_FILE} \
                 in your working directory. Use `mazemail show {id} --format md` and the other \
                 `mazemail` commands described in AGENTS.md for anything else in the mail.",
                id = attached.public_id
            ))));
            self.context_sent = Some(attached.public_id.clone());
        }
        blocks.push(ContentBlock::Text(TextContent::new(text)));
        Box::pin(
            self.cx
                .send_request(PromptRequest::new(self.session.clone(), blocks))
                .block_task(),
        )
    }

    fn cancel(&mut self) {
        if let Err(error) = self
            .cx
            .send_notification(CancelNotification::new(self.session.clone()))
        {
            tracing::debug!(%error, "could not send session/cancel");
        }
        // ACP: after cancelling, pending permission requests are answered "cancelled".
        let pending: Vec<u64> = self.pending_tools.keys().copied().collect();
        for id in pending {
            if let Some((_, responder)) = self.pending_tools.remove(&id) {
                let _ = responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ));
                self.emit(EntryUpdate::PermissionDecided {
                    id,
                    choice: PermissionChoice::Deny,
                });
            }
        }
    }

    fn emit_controls(&self) {
        let controls = self
            .controls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let _ = self.events.try_send(SessionEvent::Controls(controls));
    }

    /// Applies the remembered choices the agent offers, one after the other (a model
    /// change can change the other options).
    async fn apply_preferences(&mut self) {
        for (id, value) in self.config.preferred.clone() {
            if let Err(error) = self.set_control(&id, value).await {
                tracing::debug!(%error, id, "a remembered choice wasn't applied");
            }
        }
    }

    /// Changes a control through the agent; nothing to do if it already has `value`
    /// or doesn't offer it.
    async fn set_control(&mut self, id: &str, value: ControlValue) -> Result<(), String> {
        let control = self
            .controls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|c| c.id == id)
            .cloned();
        let Some(control) = control else {
            return Err(format!("the agent has no setting {id}"));
        };
        if control.value() == value {
            return Ok(());
        }
        if !control.accepts(&value) {
            return Err(format!("{} doesn't offer that choice", control.name));
        }
        if control.legacy_mode {
            let ControlValue::Choice(mode) = &value else {
                return Err("modes are choices".to_owned());
            };
            self.cx
                .send_request(SetSessionModeRequest::new(
                    self.session.clone(),
                    mode.clone(),
                ))
                .block_task()
                .await
                .map_err(describe_error)?;
            if let Some(mode) = self
                .controls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter_mut()
                .find(|c| c.id == control.id)
            {
                mode.set(&value);
            }
        } else {
            let response = self
                .cx
                .send_request(SetSessionConfigOptionRequest::new(
                    self.session.clone(),
                    control.id.clone(),
                    value.to_acp(),
                ))
                .block_task()
                .await
                .map_err(describe_error)?;
            *self.controls.lock().unwrap_or_else(PoisonError::into_inner) =
                controls::from_config_options(&response.config_options);
        }
        self.emit_controls();
        Ok(())
    }

    /// Signs in after a prompt failed for lack of a login. `false` if the session ended.
    async fn sign_in_again(
        &mut self,
        method: String,
        commands: &async_channel::Receiver<SessionCommand>,
    ) -> bool {
        let mut method = method;
        loop {
            match self.sign_in.authenticate(method, commands).await {
                Authenticated::Yes => {
                    self.emit(EntryUpdate::Notice(
                        "Signed in. Send your message again.".to_owned(),
                        NoticeLevel::Info,
                    ));
                    self.status(SessionStatus::Ready);
                    return true;
                }
                Authenticated::No => return true,
                Authenticated::Ended => return false,
                Authenticated::Switch(other) => method = other,
            }
        }
    }

    async fn turn_ended(&mut self, result: Result<PromptResponse, agent_client_protocol::Error>) {
        let mut signed_out = false;
        match result {
            Ok(response) => {
                let note = match response.stop_reason {
                    StopReason::EndTurn => None,
                    StopReason::Cancelled => Some("Stopped."),
                    StopReason::MaxTokens => Some("The agent hit its output limit."),
                    StopReason::MaxTurnRequests => {
                        Some("The agent hit its limit of steps for one turn.")
                    }
                    StopReason::Refusal => Some("The agent declined to continue."),
                    _ => None,
                };
                if let Some(note) = note {
                    self.emit(EntryUpdate::Notice(note.to_owned(), NoticeLevel::Info));
                }
            }
            // Claude Code only finds out at the first prompt that it isn't signed in.
            Err(error) if error.code == ErrorCode::AuthRequired => {
                signed_out = true;
                self.status(SessionStatus::SignInRequired);
                self.sign_in
                    .send(SessionEvent::SignInRequired(SignInRequest {
                        methods: self.sign_in.methods.clone(),
                        message: auth_message(&error),
                    }));
            }
            Err(error) => self.emit(EntryUpdate::Notice(
                format!("The agent reported an error: {}", describe_error(error)),
                NoticeLevel::Error,
            )),
        }
        if !signed_out {
            self.status(SessionStatus::Ready);
        }
        let (json, title) = {
            let transcript = self
                .transcript
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (transcript.to_json(), transcript.title())
        };
        if let Err(error) = self
            .host
            .save_transcript(self.config.agent_thread, json)
            .await
        {
            tracing::warn!(%error, "could not save the agent transcript");
        }
        if let Err(error) = self
            .host
            .update_agent_thread(
                self.config.agent_thread,
                title.as_deref(),
                Some(&self.session.0),
            )
            .await
        {
            tracing::warn!(%error, "could not update the agent thread");
        }
    }

    fn on_request(&mut self, request: HostRequest) {
        match request {
            HostRequest::Permission(pending) => {
                let (request, responder) = *pending;
                self.next_prompt += 1;
                let id = self.next_prompt;
                let title =
                    request.tool_call.fields.title.clone().unwrap_or_else(|| {
                        format!("{} wants to use a tool", self.config.agent_name)
                    });
                self.emit(EntryUpdate::Permission(PermissionEntry {
                    id,
                    title,
                    kind: PermissionKind::Tool,
                    choices: all_choices(),
                    decision: None,
                }));
                self.pending_tools.insert(id, (request, responder));
            }
            HostRequest::CreateTerminal(pending) => {
                let (request, responder) = *pending;
                self.create_terminal(request, responder);
            }
            HostRequest::TerminalExited(id) => {
                if let Some(output) = self.terminals.output(&id) {
                    update_view(
                        &self.views,
                        &self.transcript,
                        &self.events,
                        &id,
                        &output.output,
                        output.exit.as_ref(),
                    );
                }
            }
        }
    }

    fn create_terminal(
        &mut self,
        request: CreateTerminalRequest,
        responder: Responder<CreateTerminalResponse>,
    ) {
        let line = display_command(&request.command, &request.args);
        match classify(&request.command, &request.args) {
            CommandClass::Mazemail(args) => {
                let Some(cli) = self.config.cli_program.clone() else {
                    let _ = responder.respond_with_error(agent_client_protocol::Error::new(
                        -32603,
                        "the mazemail CLI isn't available in this session",
                    ));
                    return;
                };
                let spec = LaunchSpec {
                    program: cli,
                    args,
                    env: self.command_env(&request),
                    cwd: self.config.cwd.clone(),
                };
                self.spawn_terminal(&spec, &request, responder, line, true, None);
            }
            CommandClass::Other { program } => {
                if self.allowed_programs.contains(&program) {
                    let spec = self.approved_spec(&request);
                    self.spawn_terminal(&spec, &request, responder, line, false, None);
                    return;
                }
                self.next_prompt += 1;
                let id = self.next_prompt;
                let terminal_id = format!("pending-{id}");
                self.show_terminal(
                    &terminal_id,
                    TerminalEntry {
                        id: terminal_id.clone(),
                        command: line.clone(),
                        automatic: false,
                        status: TerminalStatus::AwaitingApproval,
                        output: String::new(),
                    },
                );
                self.emit(EntryUpdate::Permission(PermissionEntry {
                    id,
                    title: format!("Run `{line}`"),
                    kind: PermissionKind::Command,
                    choices: all_choices(),
                    decision: None,
                }));
                self.pending_commands.insert(
                    id,
                    PendingCommand {
                        request,
                        responder,
                        program,
                        terminal_id,
                    },
                );
            }
        }
    }

    fn command_env(&self, request: &CreateTerminalRequest) -> BTreeMap<String, String> {
        let mut env = self.config.terminal_env.clone();
        for var in &request.env {
            // The session token and the data dir stay Maze's.
            if var.name != local_api::SESSION_ENV && var.name != "MAZEMAIL_DATA_DIR" {
                env.insert(var.name.clone(), var.value.clone());
            }
        }
        env
    }

    fn approved_spec(&self, request: &CreateTerminalRequest) -> LaunchSpec {
        let cwd = request
            .cwd
            .clone()
            .filter(|cwd| cwd.is_absolute() && cwd.is_dir())
            .unwrap_or_else(|| self.config.cwd.clone());
        let (program, args) = if request.args.is_empty() {
            (
                PathBuf::from("/bin/sh"),
                vec!["-c".to_owned(), request.command.clone()],
            )
        } else {
            (PathBuf::from(&request.command), request.args.clone())
        };
        LaunchSpec {
            program,
            args,
            env: self.command_env(request),
            cwd,
        }
    }

    /// Shows a terminal entry; `host_id` is the terminal host's id that later output
    /// and exit updates arrive under (the entry keeps its own display id).
    fn show_terminal(&self, host_id: &str, entry: TerminalEntry) {
        self.views
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(host_id.to_owned(), entry.clone());
        self.emit(EntryUpdate::Terminal(entry));
    }

    fn spawn_terminal(
        &mut self,
        spec: &LaunchSpec,
        request: &CreateTerminalRequest,
        responder: Responder<CreateTerminalResponse>,
        line: String,
        automatic: bool,
        display_id: Option<String>,
    ) {
        match self.terminals.spawn(
            spec,
            self.config.terminal_policy.as_ref(),
            request.output_byte_limit,
        ) {
            Ok(id) => {
                self.show_terminal(
                    &id,
                    TerminalEntry {
                        id: display_id.unwrap_or_else(|| id.clone()),
                        command: line,
                        automatic,
                        status: TerminalStatus::Running,
                        output: String::new(),
                    },
                );
                let _ = responder.respond(CreateTerminalResponse::new(id.clone()));
                let terminals = Arc::clone(&self.terminals);
                let requests = self.requests.clone();
                tokio::spawn(async move {
                    terminals.wait(&id).await;
                    let _ = requests.try_send(HostRequest::TerminalExited(id));
                });
            }
            Err(error) => {
                let message = error.to_string();
                let id = display_id.unwrap_or_else(|| format!("failed-{}", self.next_prompt));
                self.show_terminal(
                    &id.clone(),
                    TerminalEntry {
                        id,
                        command: line,
                        automatic,
                        status: TerminalStatus::Failed {
                            message: message.clone(),
                        },
                        output: String::new(),
                    },
                );
                let _ = responder
                    .respond_with_error(agent_client_protocol::Error::new(-32603, message));
            }
        }
    }

    fn decide(&mut self, id: u64, choice: PermissionChoice) {
        if let Some((request, responder)) = self.pending_tools.remove(&id) {
            let outcome = match pick_option(&request, choice) {
                Some(option) => {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option))
                }
                None => RequestPermissionOutcome::Cancelled,
            };
            let _ = responder.respond(RequestPermissionResponse::new(outcome));
            self.emit(EntryUpdate::PermissionDecided { id, choice });
            return;
        }
        let Some(pending) = self.pending_commands.remove(&id) else {
            return;
        };
        self.emit(EntryUpdate::PermissionDecided { id, choice });
        let line = display_command(&pending.request.command, &pending.request.args);
        self.views
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&pending.terminal_id);
        match choice {
            PermissionChoice::Deny => {
                self.emit(EntryUpdate::Terminal(TerminalEntry {
                    id: pending.terminal_id,
                    command: line,
                    automatic: false,
                    status: TerminalStatus::Denied,
                    output: String::new(),
                }));
                let _ = pending
                    .responder
                    .respond_with_error(agent_client_protocol::Error::new(
                        -32603,
                        "the user denied running this command",
                    ));
            }
            PermissionChoice::AllowOnce | PermissionChoice::AllowSession => {
                if choice == PermissionChoice::AllowSession {
                    self.allowed_programs.insert(pending.program.clone());
                }
                // The approved run takes over the waiting entry.
                let spec = self.approved_spec(&pending.request);
                self.spawn_terminal(
                    &spec,
                    &pending.request,
                    pending.responder,
                    line,
                    false,
                    Some(pending.terminal_id),
                );
            }
        }
    }
}

fn all_choices() -> Vec<PermissionChoice> {
    vec![
        PermissionChoice::AllowOnce,
        PermissionChoice::AllowSession,
        PermissionChoice::Deny,
    ]
}

/// The agent's option for the user's choice: "for this session" prefers the agent's
/// "always" option, "deny" its "reject once".
fn pick_option(
    request: &RequestPermissionRequest,
    choice: PermissionChoice,
) -> Option<agent_client_protocol::schema::v1::PermissionOptionId> {
    let find = |kinds: &[PermissionOptionKind]| {
        kinds.iter().find_map(|kind| {
            request
                .options
                .iter()
                .find(|option| option.kind == *kind)
                .map(|option| option.option_id.clone())
        })
    };
    match choice {
        PermissionChoice::AllowOnce => find(&[
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ]),
        PermissionChoice::AllowSession => find(&[
            PermissionOptionKind::AllowAlways,
            PermissionOptionKind::AllowOnce,
        ]),
        PermissionChoice::Deny => find(&[
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ]),
    }
}

fn display_command(command: &str, args: &[String]) -> String {
    let mut line = command.to_owned();
    for arg in args {
        line.push(' ');
        if arg.is_empty() || arg.contains(|c: char| c.is_whitespace() || c == '\'' || c == '"') {
            line.push('\'');
            line.push_str(&arg.replace('\'', "'\\''"));
            line.push('\'');
        } else {
            line.push_str(arg);
        }
    }
    line
}

/// Text of a content block for the transcript.
fn content_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::ResourceLink(link) => format!("[{}]({})", link.name, link.uri),
        ContentBlock::Image(_) => "[image]".to_owned(),
        ContentBlock::Audio(_) => "[audio]".to_owned(),
        _ => String::new(),
    }
}

fn tool_content_text(content: &[ToolCallContent]) -> String {
    content
        .iter()
        .map(|item| match item {
            ToolCallContent::Content(content) => content_text(&content.content),
            ToolCallContent::Diff(diff) => {
                format!("Edit {}:\n{}", diff.path.display(), diff.new_text)
            }
            ToolCallContent::Terminal(terminal) => format!("(terminal {})", terminal.terminal_id.0),
            _ => String::new(),
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn tool_status(status: ToolCallStatus) -> ToolStatus {
    match status {
        ToolCallStatus::Pending => ToolStatus::Pending,
        ToolCallStatus::InProgress => ToolStatus::Running,
        ToolCallStatus::Completed => ToolStatus::Completed,
        ToolCallStatus::Failed => ToolStatus::Failed,
        _ => ToolStatus::Running,
    }
}

fn kind_name(kind: impl serde::Serialize) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "other".to_owned())
}

/// Maps one ACP session update to transcript updates.
pub fn map_update(update: SessionUpdate) -> Vec<EntryUpdate> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            let text = content_text(&chunk.content);
            if text.is_empty() {
                Vec::new()
            } else {
                vec![EntryUpdate::AgentChunk(text)]
            }
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            let text = content_text(&chunk.content);
            if text.is_empty() {
                Vec::new()
            } else {
                vec![EntryUpdate::ThoughtChunk(text)]
            }
        }
        SessionUpdate::ToolCall(call) => {
            let mut detail = tool_content_text(&call.content);
            if detail.is_empty()
                && let Some(input) = &call.raw_input
            {
                detail = serde_json::to_string_pretty(input).unwrap_or_default();
            }
            vec![EntryUpdate::ToolCall(ToolCallEntry {
                id: call.tool_call_id.0.to_string(),
                title: call.title,
                kind: kind_name(call.kind),
                status: tool_status(call.status),
                detail,
            })]
        }
        SessionUpdate::ToolCallUpdate(update) => vec![EntryUpdate::ToolCallUpdate {
            id: update.tool_call_id.0.to_string(),
            title: update.fields.title,
            status: update.fields.status.map(tool_status),
            detail: update
                .fields
                .content
                .as_deref()
                .map(tool_content_text)
                .filter(|text| !text.is_empty()),
        }],
        SessionUpdate::Plan(plan) => vec![EntryUpdate::Plan(
            plan.entries
                .into_iter()
                .map(|entry| PlanItem {
                    content: entry.content,
                    status: match entry.status {
                        PlanEntryStatus::Pending => PlanStatus::Pending,
                        PlanEntryStatus::InProgress => PlanStatus::InProgress,
                        PlanEntryStatus::Completed => PlanStatus::Completed,
                        _ => PlanStatus::Pending,
                    },
                })
                .collect(),
        )],
        SessionUpdate::Notice(notice) => {
            let text = match notice.description {
                Some(description) => format!("{}: {description}", notice.title),
                None => notice.title,
            };
            vec![EntryUpdate::Notice(text, NoticeLevel::Info)]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_errors_keep_the_agents_explanation() {
        let plain = agent_client_protocol::Error::auth_required();
        assert_eq!(auth_message(&plain), None);
        let cursor = agent_client_protocol::Error::auth_required()
            .data("Authentication required. Please run 'cursor-agent login' first.".to_owned());
        assert_eq!(
            auth_message(&cursor).as_deref(),
            Some("Authentication required. Please run 'cursor-agent login' first.")
        );
        let gemini = agent_client_protocol::Error::new(
            -32000,
            "Gemini API key is missing or not configured.",
        );
        assert_eq!(
            auth_message(&gemini).as_deref(),
            Some("Gemini API key is missing or not configured.")
        );
    }

    #[test]
    fn commands_are_displayed_quoted() {
        assert_eq!(
            display_command("mazemail", &["search".into(), "from:anna has:pdf".into()]),
            "mazemail search 'from:anna has:pdf'"
        );
    }
}
