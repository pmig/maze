//! A scripted in-process ACP agent for tests (feature `test-support`).
//!
//! It answers `initialize`, `authenticate`, `session/new`, `session/load` and plays a
//! [`FakeStep`] script for every `session/prompt`: streamed thoughts and messages, tool
//! calls, a plan, permission requests and terminal commands. What the client answered
//! is recorded in [`FakeAgent::log`].

use std::sync::{Arc, Mutex, PoisonError};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthMethod, AuthMethodAgent, AuthenticateRequest, AuthenticateResponse,
    ContentBlock, ContentChunk, CreateTerminalRequest, Implementation, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, PromptRequest, PromptResponse, ReleaseTerminalRequest,
    RequestPermissionOutcome, RequestPermissionRequest, SessionNotification, SessionUpdate,
    StopReason, TerminalOutputRequest, TextContent, ToolCall, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, WaitForTerminalExitRequest,
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
    /// Advertise `loadSession`.
    pub load_session: bool,
    /// The steps of every turn.
    pub script: Vec<FakeStep>,
    log: Arc<Mutex<Vec<String>>>,
}

impl FakeAgent {
    /// An agent playing `script`.
    pub fn new(script: Vec<FakeStep>) -> Self {
        Self {
            script,
            ..Self::default()
        }
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
        let authenticated = Arc::new(Mutex::new(!self.require_auth));
        let load_session = self.load_session;
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
                record(format!(
                    "initialize:terminal={}:fs_read={}",
                    request.client_capabilities.terminal,
                    request.client_capabilities.fs.read_text_file
                ));
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(AgentCapabilities::new().load_session(load_session))
                        .auth_methods(vec![AuthMethod::Agent(
                            AuthMethodAgent::new("fake-login", "Sign in with Fake")
                                .description("Opens the fake login".to_owned()),
                        )])
                        .agent_info(Implementation::new("fake-agent", "1.0.0")),
                )
            }
        };
        let on_authenticate = {
            let record = record.clone();
            let authenticated = Arc::clone(&authenticated);
            async move |request: AuthenticateRequest,
                        responder: Responder<AuthenticateResponse>,
                        _cx: ConnectionTo<Client>| {
                record(format!("authenticate:{}", request.method_id.0));
                *authenticated.lock().unwrap_or_else(PoisonError::into_inner) = true;
                responder.respond(AuthenticateResponse::new())
            }
        };
        let on_new = {
            let record = record.clone();
            let authenticated = Arc::clone(&authenticated);
            async move |request: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        _cx: ConnectionTo<Client>| {
                if !*authenticated.lock().unwrap_or_else(PoisonError::into_inner) {
                    record("new_session:auth_required".to_owned());
                    return responder
                        .respond_with_error(agent_client_protocol::Error::auth_required());
                }
                record(format!("new_session:{}", request.cwd.display()));
                responder.respond(NewSessionResponse::new("fake-session-1"))
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
                record(format!("prompt:{}", text.join(" | ")));
                let script = Arc::clone(&script);
                let record = record.clone();
                let task_cx = cx.clone();
                // Play the script outside the dispatch loop: it waits for the client.
                cx.spawn(async move {
                    for step in script.iter() {
                        play(&task_cx, &request, step, &record).await?;
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
    }
}

async fn play(
    cx: &ConnectionTo<Client>,
    request: &PromptRequest,
    step: &FakeStep,
    record: &(impl Fn(String) + Send + Sync),
) -> Result<(), agent_client_protocol::Error> {
    let session = request.session_id.clone();
    let notify = |update: SessionUpdate| {
        cx.send_notification(SessionNotification::new(session.clone(), update))
    };
    match step {
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
