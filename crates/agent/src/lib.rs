//! Maze's ACP client (PLAN §4.12, ADR-006, ADR-016): external agents like Claude Code,
//! Codex, Gemini CLI, Cursor and pi, run as sandboxed subprocesses over the Agent Client
//! Protocol (`agent-client-protocol` 3.x, protocol V1).
//!
//! | Module | What |
//! | ------ | ---- |
//! | [`registry`], [`catalog`] | the ACP registry (cached), built-in and custom agents |
//! | [`install`], [`node`], [`archive`] | installing npx and binary agents, the managed Node runtime |
//! | [`policy`], [`process`] | the session's sandbox policy and sandboxed processes |
//! | [`session`], [`terminal`], [`transcript`] | one ACP session: prompts, streamed updates, permission prompts, the terminal host |
//! | [`auth`] | signing in: API keys, terminal logins, links agents open |
//! | [`controls`] | model, mode, thinking and other per-session choices, remembered per agent |
//! | [`scratch`], [`tokens`] | the session's scratch dir and its `MAZEMAIL_SESSION` token |
//! | [`host`] | what sessions need from Maze (implemented over the core by the app) |
//! | [`manager`] | [`AgentManager`], the agent column's entry point |
//!
//! # Guarantees
//!
//! - Agents reach mail only through the `mazemail` CLI with a session token whose scope
//!   never includes accounts whose AI policy is _off_ or _local models only_.
//! - The agent process and every terminal command run in the agent sandbox
//!   (`sandbox` crate) with an empty environment plus what Maze sets, and the whole
//!   process group is killed when the session ends.
//! - Maze advertises `terminal: true` and `fs: false`. Plain `mazemail …` commands run
//!   automatically; anything else asks the user first.
//! - Transcripts go to the encrypted store; the scratch dir is the only plaintext and
//!   is deleted when the session ends (stale ones at startup).
//!
//! This crate is layer L2. It depends on `local_api` (same layer) for the session
//! token types and the AGENTS.md generator, never on `mazemail_core`: the app
//! implements [`AgentHost`] over the core.

pub mod archive;
pub mod auth;
pub mod catalog;
pub mod controls;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod host;
pub mod http;
pub mod install;
pub mod manager;
pub mod node;
pub mod paths;
pub mod policy;
pub mod process;
pub mod registry;
pub mod scratch;
pub mod session;
#[cfg(test)]
mod session_tests;
pub mod terminal;
pub mod tokens;
pub mod transcript;

pub use catalog::{AgentEntry, AgentSource, PI_NOTICE};
pub use controls::{Choice, Control, ControlCategory, ControlKind, ControlValue};
pub use host::{
    AgentHost, AgentKey, AgentThreadInfo, AgentThreadKey, HostError, MemoryHost,
    ProposalItemStatus, ProposalItemView, ProposalView, ThreadExport,
};
pub use http::{Http, ReqwestHttp};
pub use install::{InstallError, InstallProgress, InstalledAgent};
#[cfg(any(test, feature = "test-support"))]
pub use manager::TestAgents;
pub use manager::{
    AgentManager, AgentStatus, LoginScript, ManagerConfig, StartError, StartRequest,
    StartedSession, UpdateReport,
};
pub use paths::AgentPaths;
pub use process::AgentProcess;
pub use session::{
    AttachedThread, AuthMethodInfo, SessionConfig, SessionEvent, SessionHandle, SessionResources,
    SessionStatus, SignInProgress, SignInRequest, TerminalAuth, start_with_transport,
};
pub use transcript::{
    Entry, EntryUpdate, NoticeLevel, PermissionChoice, PermissionEntry, PermissionKind, PlanItem,
    PlanStatus, TerminalEntry, TerminalStatus, ToolCallEntry, ToolStatus, Transcript,
};
