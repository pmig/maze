//! A tiny ACP agent over stdio for the manager's end-to-end test (feature
//! `test-support`, never shipped). A prompt `probe <path>` answers with what the agent
//! process sees: its `HOME`, working directory, whether it got a session token, the
//! session folder's files, whether `<path>` is readable, and whether Maze's own
//! environment leaked into it.

#![allow(clippy::print_stderr)]

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionNotification,
    SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo, Responder};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

fn probe(path: &str) -> String {
    let env = |name: &str| std::env::var(name).unwrap_or_else(|_| "none".to_owned());
    let cwd = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let readable = |p: &std::path::Path| std::fs::read(p).is_ok();
    let here = std::path::Path::new(".");
    [
        format!("home={}", env("HOME")),
        format!("cwd={cwd}"),
        format!(
            "token={}",
            std::env::var("MAZEMAIL_SESSION").is_ok_and(|t| t.starts_with("mazemail_s_"))
        ),
        format!("data_dir={}", env("MAZEMAIL_DATA_DIR")),
        format!("agents_md={}", readable(&here.join("AGENTS.md"))),
        format!(
            "thread={}",
            std::fs::read_to_string(here.join("thread.md")).unwrap_or_else(|_| "none".to_owned())
        ),
        format!(
            "secret={}",
            if readable(std::path::Path::new(path)) {
                "readable"
            } else {
                "denied"
            }
        ),
        format!("leaked={}", env("CARGO_MANIFEST_DIR")),
        format!(
            "tmp_writable={}",
            std::fs::write(std::env::temp_dir().join("probe"), b"x").is_ok()
        ),
    ]
    .join("\n")
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let transport = ByteStreams::new(
        tokio::io::stdout().compat_write(),
        tokio::io::stdin().compat(),
    );
    let result = Agent
        .builder()
        .on_receive_request(
            async |_request: InitializeRequest,
                   responder: Responder<InitializeResponse>,
                   _cx: ConnectionTo<Client>| {
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_request: NewSessionRequest,
                   responder: Responder<NewSessionResponse>,
                   _cx: ConnectionTo<Client>| {
                responder.respond(NewSessionResponse::new("probe-session"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: PromptRequest,
                   responder: Responder<PromptResponse>,
                   cx: ConnectionTo<Client>| {
                let text: String = request
                    .prompt
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let path = text
                    .lines()
                    .find_map(|line| line.strip_prefix("probe "))
                    .unwrap_or("/nonexistent");
                cx.send_notification(SessionNotification::new(
                    request.session_id,
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(probe(path)),
                    ))),
                ))?;
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(transport)
        .await;
    if let Err(error) = result {
        eprintln!("probe agent: {error:?}");
    }
}
