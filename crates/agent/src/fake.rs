//! A scripted in-process ACP agent for tests (feature `test-support`).
//!
//! It answers `initialize`, `authenticate`, `session/new`, `session/load` and plays a
//! [`FakeStep`] script for every `session/prompt`: streamed thoughts and messages, tool
//! calls, a plan, permission requests and terminal commands. What the client answered
//! is recorded in [`FakeAgent::log`].
//!
//! Sign-in can be scripted like the real agents do it: `session/new` fails until
//! `authenticate` (Cursor, Codex), or only the first prompt fails (Claude Code); a
//! terminal login is offered when the client supports it (Claude Code); and
//! `authenticate` can "open the browser" through the open shim's file and wait until
//! the test says the browser login finished ([`FakeAgent::finish_browser_login`]).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthMethod, AuthMethodAgent, AuthMethodTerminal, AuthenticateRequest,
    AuthenticateResponse, ConfigOptionUpdate, ContentBlock, ContentChunk, CreateTerminalRequest,
    CurrentModeUpdate, Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, PromptRequest,
    PromptResponse, ReleaseTerminalRequest, RequestPermissionOutcome, RequestPermissionRequest,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelectOption, SessionMode, SessionModeState, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
    SetSessionModeResponse, StopReason, TerminalOutputRequest, TextContent, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, WaitForTerminalExitRequest,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Responder};

/// One step of the scripted turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeStep {
    /// A reasoning chunk.
    Thought(String),
    /// An answer chunk.
    Message(String),
    /// A tool call starts.
    ToolCall {
        /// Id.
        id: String,
        /// Title.
        title: String,
    },
    /// A tool call finishes.
    ToolDone {
        /// Id.
        id: String,
    },
    /// The plan.
    Plan(Vec<String>),
    /// Asks permission; logs `permission:<title>:<option id or cancelled>`.
    AskPermission {
        /// The tool call's title.
        title: String,
    },
    /// The agent switches its mode itself (`current_mode_update`, modes-only agents).
    SwitchMode(String),
    /// The agent switches its model itself (`config_option_update`).
    SwitchModel(String),
    /// Runs a command through the client's terminal; logs
    /// `terminal:<command>:<exit code>:<output>` or `terminal:<command>:error:<message>`.
    Terminal {
        /// Command.
        command: String,
        /// Arguments.
        args: Vec<String>,
    },
}

/// The scripted agent.
#[derive(Debug, Clone, Default)]
pub struct FakeAgent {
    /// `session/new` fails with "auth required" until `authenticate` was called.
    pub require_auth: bool,
    /// Like Claude Code: `session/new` works, but prompts fail with "auth required"
    /// until `authenticate` was called.
    pub require_auth_at_prompt: bool,
    /// The auth error's explanation (its `data`).
    pub auth_message: Option<String>,
    /// Also offer a terminal login (`--login`) when the client supports terminal auth.
    pub terminal_login: bool,
    /// Also offer an API-key method (`api-key`, provider `openai`), like Codex.
    pub api_key_method: bool,
    /// `authenticate` hands this link to the "open shim" (appends it to
    /// `open_urls`) and waits for [`FakeAgent::finish_browser_login`], like Cursor.
    pub browser_login: Option<String>,
    /// The open shim's file (`MAZEMAIL_OPEN_URLS`).
    pub open_urls: Option<PathBuf>,
    /// Offer config options like Claude Code: `mode`, `model` (sonnet, opus, haiku),
    /// `effort` (not for haiku) and the switch `fast`. Changes are logged as
    /// `set_config:<id>=<value>`.
    pub config_options: bool,
    /// Offer only session modes like Gemini CLI (default, autoEdit, yolo). Changes are
    /// logged as `set_mode:<id>`.
    pub modes: bool,
    /// Advertise `loadSession`.
    pub load_session: bool,
    /// The steps of every turn.
    pub script: Vec<FakeStep>,
    log: Arc<Mutex<Vec<String>>>,
    browser_done: Arc<AtomicBool>,
}

/// The fake's current choices.
#[derive(Debug, Clone)]
struct Choices {
    mode: String,
    model: String,
    effort: String,
    fast: bool,
}

impl Default for Choices {
    fn default() -> Self {
        Self {
            mode: "default".to_owned(),
            model: "sonnet".to_owned(),
            effort: "high".to_owned(),
            fast: false,
        }
    }
}

impl Choices {
    fn options(&self) -> Vec<SessionConfigOption> {
        let select = |id: &'static str,
                      name: &'static str,
                      current: &str,
                      values: &[(&'static str, &'static str)]| {
            SessionConfigOption::select(
                id,
                name,
                current.to_owned(),
                values
                    .iter()
                    .map(|(value, name)| SessionConfigSelectOption::new(*value, *name))
                    .collect::<Vec<_>>(),
            )
        };
        let mut options = vec![
            select(
                "mode",
                "Mode",
                &self.mode,
                &[
                    ("default", "Manual"),
                    ("acceptEdits", "Accept Edits"),
                    ("plan", "Plan"),
                ],
            )
            .category(SessionConfigOptionCategory::Mode),
            select(
                "model",
                "Model",
                &self.model,
                &[("sonnet", "Sonnet"), ("opus", "Opus"), ("haiku", "Haiku")],
            )
            .category(SessionConfigOptionCategory::Model),
        ];
        if self.model != "haiku" {
            options.push(
                select(
                    "effort",
                    "Effort",
                    &self.effort,
                    &[("low", "Low"), ("medium", "Medium"), ("high", "High")],
                )
                .category(SessionConfigOptionCategory::ThoughtLevel),
            );
        }
        options.push(
            SessionConfigOption::boolean("fast", "Fast", self.fast)
                .category(SessionConfigOptionCategory::ModelConfig),
        );
        options
    }

    fn modes(&self) -> SessionModeState {
        SessionModeState::new(
            self.mode.clone(),
            vec![
                SessionMode::new("default", "Default"),
                SessionMode::new("autoEdit", "Auto Edit"),
                SessionMode::new("yolo", "YOLO"),
            ],
        )
    }
}

impl FakeAgent {
    /// An agent playing `script`.
    pub fn new(script: Vec<FakeStep>) -> Self {
        Self {
            script,
            ..Self::default()
        }
    }

    /// The user finished the browser login ([`FakeAgent::browser_login`]).
    pub fn finish_browser_login(&self) {
        self.browser_done.store(true, Ordering::SeqCst);
    }

    /// What happened, in order.
    pub fn log(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The agent side as an ACP transport for the client.
    pub fn transport(&self) -> impl ConnectTo<Client> + 'static {
        let log = Arc::clone(&self.log);
        let authenticated = Arc::new(Mutex::new(
            !self.require_auth && !self.require_auth_at_prompt,
        ));
        let at_prompt = self.require_auth_at_prompt;
        let auth_error = {
            let message = self.auth_message.clone();
            move || match &message {
                Some(message) => {
                    agent_client_protocol::Error::auth_required().data(message.clone())
                }
                None => agent_client_protocol::Error::auth_required(),
            }
        };
        let terminal_login = self.terminal_login;
        let api_key_method = self.api_key_method;
        let browser = self.browser_login.clone().zip(self.open_urls.clone());
        let browser_done = Arc::clone(&self.browser_done);
        let load_session = self.load_session;
        let with_options = self.config_options;
        let with_modes = self.modes;
        let choices = Arc::new(Mutex::new(Choices::default()));
        let script = Arc::new(self.script.clone());
        let record = {
            let log = Arc::clone(&log);
            move |line: String| {
                log.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(line)
            }
        };

        let on_initialize = {
            let record = record.clone();
            async move |request: InitializeRequest,
                        responder: Responder<InitializeResponse>,
                        _cx: ConnectionTo<Client>| {
                let caps = &request.client_capabilities;
                let legacy_terminal = caps
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get("terminal-auth"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                record(format!(
                    "initialize:terminal={}:fs_read={}:auth_terminal={}:meta_terminal_auth={}",
                    caps.terminal, caps.fs.read_text_file, caps.auth.terminal, legacy_terminal
                ));
                let mut methods = vec![AuthMethod::Agent(
                    AuthMethodAgent::new("fake-login", "Sign in with Fake")
                        .description("Opens the fake login".to_owned()),
                )];
                if api_key_method {
                    let mut meta = serde_json::Map::new();
                    meta.insert(
                        "api-key".to_owned(),
                        serde_json::json!({ "provider": "openai" }),
                    );
                    methods.push(AuthMethod::Agent(
                        AuthMethodAgent::new("api-key", "API Key").meta(meta),
                    ));
                }
                if terminal_login && caps.auth.terminal {
                    methods.push(AuthMethod::Terminal(
                        AuthMethodTerminal::new("fake-terminal", "Fake Subscription")
                            .description("Sign in from a terminal".to_owned())
                            .args(vec!["--login".to_owned()]),
                    ));
                }
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(AgentCapabilities::new().load_session(load_session))
                        .auth_methods(methods)
                        .agent_info(Implementation::new("fake-agent", "1.0.0")),
                )
            }
        };
        let on_authenticate = {
            let record = record.clone();
            let authenticated = Arc::clone(&authenticated);
            let browser_done = Arc::clone(&browser_done);
            async move |request: AuthenticateRequest,
                        responder: Responder<AuthenticateResponse>,
                        cx: ConnectionTo<Client>| {
                record(format!("authenticate:{}", request.method_id.0));
                let Some((url, file)) = browser.clone() else {
                    *authenticated.lock().unwrap_or_else(PoisonError::into_inner) = true;
                    return responder.respond(AuthenticateResponse::new());
                };
                // Like Cursor: run `open <url>`, then wait for the browser login.
                use std::io::Write as _;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&file)
                    .and_then(|mut f| writeln!(f, "{url}"))
                    .map_err(|e| {
                        agent_client_protocol::Error::internal_error().data(e.to_string())
                    })?;
                let authenticated = Arc::clone(&authenticated);
                let browser_done = Arc::clone(&browser_done);
                let record = record.clone();
                cx.spawn(async move {
                    for _ in 0..2000 {
                        if browser_done.load(Ordering::SeqCst) {
                            *authenticated.lock().unwrap_or_else(PoisonError::into_inner) = true;
                            record("browser_login:done".to_owned());
                            return responder.respond(AuthenticateResponse::new());
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    responder.respond_with_error(
                        agent_client_protocol::Error::invalid_params()
                            .data(format!("Login timed out. Please visit: {url}")),
                    )
                })
            }
        };
        let on_new = {
            let record = record.clone();
            let auth_error = auth_error.clone();
            let choices = Arc::clone(&choices);
            let authenticated = Arc::clone(&authenticated);
            async move |request: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                if !at_prompt && !*authenticated.lock().unwrap_or_else(PoisonError::into_inner) {
                    record("new_session:auth_required".to_owned());
                    return responder.respond_with_error(auth_error());
                }
                record(format!("new_session:{}", request.cwd.display()));
                let current = choices
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                let mut response = NewSessionResponse::new("fake-session-1");
                if with_options {
                    // Like Claude Code: both, and clients use the options.
                    response = response
                        .config_options(current.options())
                        .modes(current.modes());
                } else if with_modes {
                    response = response.modes(current.modes());
                }
                responder.respond(response)
            }
        };
        let on_set_mode = {
            let record = record.clone();
            let choices = Arc::clone(&choices);
            async move |request: SetSessionModeRequest,
                        responder: Responder<SetSessionModeResponse>,
                        _cx: ConnectionTo<Client>| {
                record(format!("set_mode:{}", request.mode_id.0));
                choices.lock().unwrap_or_else(PoisonError::into_inner).mode =
                    request.mode_id.0.to_string();
                responder.respond(SetSessionModeResponse::new())
            }
        };
        let on_set_option = {
            let record = record.clone();
            let choices = Arc::clone(&choices);
            async move |request: SetSessionConfigOptionRequest,
                        responder: Responder<SetSessionConfigOptionResponse>,
                        _cx: ConnectionTo<Client>| {
                let mut current = choices.lock().unwrap_or_else(PoisonError::into_inner);
                let value = match &request.value {
                    SessionConfigOptionValue::Boolean { value } => value.to_string(),
                    SessionConfigOptionValue::ValueId { value } => value.0.to_string(),
                    _ => String::new(),
                };
                record(format!("set_config:{}={value}", request.config_id.0));
                match (&*request.config_id.0, &request.value) {
                    ("mode", _) => current.mode = value,
                    ("model", _) => current.model = value,
                    ("effort", _) => current.effort = value,
                    ("fast", SessionConfigOptionValue::Boolean { value }) => current.fast = *value,
                    _ => {
                        return responder.respond_with_error(
                            agent_client_protocol::Error::invalid_params().data("unknown option"),
                        );
                    }
                }
                responder.respond(SetSessionConfigOptionResponse::new(current.options()))
            }
        };
        let on_load = {
            let record = record.clone();
            async move |request: LoadSessionRequest,
                        responder: Responder<LoadSessionResponse>,
                        cx: ConnectionTo<Client>| {
                record(format!("load_session:{}", request.session_id.0));
                // Replay of the old conversation (the client ignores it).
                cx.send_notification(SessionNotification::new(
                    request.session_id,
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new("replayed"),
                    ))),
                ))?;
                responder.respond(LoadSessionResponse::new())
            }
        };
        let on_prompt = {
            let script = Arc::clone(&script);
            let authenticated = Arc::clone(&authenticated);
            let choices = Arc::clone(&choices);
            async move |request: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                let text: Vec<String> = request
                    .prompt
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect();
                if !*authenticated.lock().unwrap_or_else(PoisonError::into_inner) {
                    record("prompt:auth_required".to_owned());
                    return responder.respond_with_error(auth_error());
                }
                record(format!("prompt:{}", text.join(" | ")));
                let script = Arc::clone(&script);
                let record = record.clone();
                let task_cx = cx.clone();
                let choices = Arc::clone(&choices);
                // Play the script outside the dispatch loop: it waits for the client.
                cx.spawn(async move {
                    for step in script.iter() {
                        play(&task_cx, &request, step, &record, &choices).await?;
                    }
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                })
            }
        };

        Agent
            .builder()
            .name("fake-agent")
            .on_receive_request(on_initialize, agent_client_protocol::on_receive_request!())
            .on_receive_request(
                on_authenticate,
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(on_new, agent_client_protocol::on_receive_request!())
            .on_receive_request(on_load, agent_client_protocol::on_receive_request!())
            .on_receive_request(on_prompt, agent_client_protocol::on_receive_request!())
            .on_receive_request(on_set_mode, agent_client_protocol::on_receive_request!())
            .on_receive_request(on_set_option, agent_client_protocol::on_receive_request!())
    }
}

async fn play(
    cx: &ConnectionTo<Client>,
    request: &PromptRequest,
    step: &FakeStep,
    record: &(impl Fn(String) + Send + Sync),
    choices: &Mutex<Choices>,
) -> Result<(), agent_client_protocol::Error> {
    let session = request.session_id.clone();
    let notify = |update: SessionUpdate| {
        cx.send_notification(SessionNotification::new(session.clone(), update))
    };
    match step {
        FakeStep::SwitchMode(mode) => {
            mode.clone_into(&mut choices.lock().unwrap_or_else(PoisonError::into_inner).mode);
            notify(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                mode.clone(),
            )))
        }
        FakeStep::SwitchModel(model) => {
            let options = {
                let mut current = choices.lock().unwrap_or_else(PoisonError::into_inner);
                model.clone_into(&mut current.model);
                current.options()
            };
            notify(SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                options,
            )))
        }
        FakeStep::Thought(text) => notify(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text.clone())),
        ))),
        FakeStep::Message(text) => notify(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text.clone())),
        ))),
        FakeStep::ToolCall { id, title } => notify(SessionUpdate::ToolCall(
            ToolCall::new(id.clone(), title.clone())
                .kind(ToolKind::Read)
                .status(ToolCallStatus::InProgress),
        )),
        FakeStep::ToolDone { id } => notify(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id.clone(),
            ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
        ))),
        FakeStep::Plan(items) => notify(SessionUpdate::Plan(Plan::new(
            items
                .iter()
                .map(|item| {
                    PlanEntry::new(
                        item.clone(),
                        PlanEntryPriority::Medium,
                        PlanEntryStatus::Pending,
                    )
                })
                .collect(),
        ))),
        FakeStep::AskPermission { title } => {
            let response = cx
                .send_request(RequestPermissionRequest::new(
                    session,
                    ToolCallUpdate::new("perm", ToolCallUpdateFields::new().title(title.clone())),
                    vec![
                        PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
                        PermissionOption::new(
                            "always",
                            "Always allow",
                            PermissionOptionKind::AllowAlways,
                        ),
                        PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
                    ],
                ))
                .block_task()
                .await?;
            let outcome = match response.outcome {
                RequestPermissionOutcome::Selected(selected) => selected.option_id.0.to_string(),
                _ => "cancelled".to_owned(),
            };
            record(format!("permission:{title}:{outcome}"));
            Ok(())
        }
        FakeStep::Terminal { command, args } => {
            let created = cx
                .send_request(
                    CreateTerminalRequest::new(session.clone(), command.clone()).args(args.clone()),
                )
                .block_task()
                .await;
            let terminal = match created {
                Ok(created) => created.terminal_id,
                Err(error) => {
                    record(format!("terminal:{command}:error:{}", error.message));
                    return Ok(());
                }
            };
            let exit = cx
                .send_request(WaitForTerminalExitRequest::new(
                    session.clone(),
                    terminal.clone(),
                ))
                .block_task()
                .await?;
            let output = cx
                .send_request(TerminalOutputRequest::new(
                    session.clone(),
                    terminal.clone(),
                ))
                .block_task()
                .await?;
            cx.send_request(ReleaseTerminalRequest::new(session, terminal))
                .block_task()
                .await?;
            record(format!(
                "terminal:{command}:{}:{}",
                exit.exit_status
                    .exit_code
                    .map_or_else(|| "none".to_owned(), |c| c.to_string()),
                output.output.trim()
            ));
            Ok(())
        }
    }
}
