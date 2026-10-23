//! Starting sandboxed processes: the agent itself and the commands of its terminals.
//!
//! The SDK's `AcpAgent` inherits Maze's whole environment and can't set a working
//! directory, so Maze spawns agents itself and hands their stdio to the SDK as
//! `ByteStreams`. Every process:
//!
//! - runs through [`sandbox::wrap`] (unless the user turned the sandbox off, or the
//!   agent is marked "already sandboxed"),
//! - starts with an **empty environment** plus exactly the variables Maze sets, so
//!   secrets in Maze's own environment (API keys, `SSH_AUTH_SOCK`, …) don't leak,
//! - leads its own process group, which is killed as a whole when the guard drops.
//!   `sandbox-exec` execs the agent in place and `bwrap` is the group leader, so that
//!   ends the whole sandboxed tree (PLAN §4.12, process teardown).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use agent_client_protocol::{ByteStreams, Client, ConnectTo, Role};
use sandbox::SandboxPolicy;
use tokio::io::AsyncBufReadExt as _;
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

/// A program to start, before sandboxing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Program (absolute path).
    pub program: PathBuf,
    /// Arguments.
    pub args: Vec<String>,
    /// The complete environment.
    pub env: BTreeMap<String, String>,
    /// Working directory.
    pub cwd: PathBuf,
}

/// Starting a process failed.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// The sandbox couldn't wrap the command.
    #[error(transparent)]
    Sandbox(#[from] sandbox::SandboxError),
    /// The OS refused to start it.
    #[error("could not start {program}: {source}")]
    Spawn {
        /// What was started.
        program: String,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },
}

/// A Tokio command for `spec` under `policy` (`None`: unsandboxed), with a cleared
/// environment, its own process group and `kill_on_drop`.
pub fn command(
    spec: &LaunchSpec,
    policy: Option<&SandboxPolicy>,
) -> Result<tokio::process::Command, SpawnError> {
    let wrapped = match policy {
        Some(policy) => sandbox::wrap(&spec.program, &spec.args, Some(&spec.cwd), policy)?,
        None => sandbox::unsandboxed(&spec.program, &spec.args),
    };
    let mut command = tokio::process::Command::new(&wrapped.program);
    command
        .args(&wrapped.args)
        .env_clear()
        .envs(&spec.env)
        .current_dir(&spec.cwd)
        .process_group(0)
        .kill_on_drop(true);
    Ok(command)
}

/// Kills a process group when dropped.
#[derive(Debug)]
pub struct ProcessGroupGuard(Option<i32>);

impl ProcessGroupGuard {
    /// A guard for the group led by `pid`.
    pub fn new(pid: Option<u32>) -> Self {
        Self(pid.and_then(|pid| i32::try_from(pid).ok()))
    }

    /// Kills the group now.
    pub fn kill(&mut self) {
        if let Some(pid) = self.0.take()
            && let Some(pid) = rustix::process::Pid::from_raw(pid)
        {
            // ESRCH just means the group is gone already.
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Receives the agent's stderr lines (for the "agent stopped" error and debugging).
pub type StderrSink = Arc<dyn Fn(String) + Send + Sync>;

/// A sandboxed agent process as an ACP transport (`ConnectTo<Client>`).
pub struct AgentProcess {
    spec: LaunchSpec,
    policy: Option<SandboxPolicy>,
    stderr: Option<StderrSink>,
}

impl std::fmt::Debug for AgentProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentProcess")
            .field("program", &self.spec.program)
            .field("sandboxed", &self.policy.is_some())
            .finish_non_exhaustive()
    }
}

impl AgentProcess {
    /// The agent `spec`, sandboxed under `policy` (`None`: unsandboxed).
    pub fn new(spec: LaunchSpec, policy: Option<SandboxPolicy>) -> Self {
        Self {
            spec,
            policy,
            stderr: None,
        }
    }

    /// Forwards stderr lines to `sink`.
    #[must_use]
    pub fn with_stderr(mut self, sink: StderrSink) -> Self {
        self.stderr = Some(sink);
        self
    }
}

impl ConnectTo<Client> for AgentProcess {
    async fn connect_to(
        self,
        client: impl ConnectTo<<Client as Role>::Counterpart>,
    ) -> Result<(), agent_client_protocol::Error> {
        let mut command = command(&self.spec, self.policy.as_ref())
            .map_err(agent_client_protocol::Error::into_internal_error)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|source| {
            agent_client_protocol::Error::into_internal_error(SpawnError::Spawn {
                program: self.spec.program.display().to_string(),
                source,
            })
        })?;
        let mut guard = ProcessGroupGuard::new(child.id());
        let missing = || agent_client_protocol::Error::internal_error().data("agent stdio missing");
        let stdin = child.stdin.take().ok_or_else(missing)?;
        let stdout = child.stdout.take().ok_or_else(missing)?;
        if let Some(stderr) = child.stderr.take() {
            let sink = self.stderr.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(sink) = &sink {
                        sink(line);
                    }
                }
            });
        }
        let streams = ByteStreams::new(stdin.compat_write(), stdout.compat());
        let result = ConnectTo::<Client>::connect_to(streams, client).await;
        guard.kill();
        let _ = child.wait().await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn environment_is_cleared() {
        let spec = LaunchSpec {
            program: PathBuf::from("/usr/bin/env"),
            args: Vec::new(),
            env: BTreeMap::from([("ONLY".to_owned(), "this".to_owned())]),
            cwd: std::env::temp_dir(),
        };
        let output = command(&spec, None).unwrap().output().await.unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.trim(), "ONLY=this");
    }

    #[tokio::test]
    async fn the_guard_kills_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survivor");
        // The shell starts a grandchild that would write the marker after a while.
        let script = format!("(sleep 1; touch '{}') & wait", marker.display());
        let spec = LaunchSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_owned(), script],
            env: BTreeMap::new(),
            cwd: dir.path().to_owned(),
        };
        let child = command(&spec, None).unwrap().spawn().unwrap();
        let guard = ProcessGroupGuard::new(child.id());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(guard);
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(!marker.exists(), "the grandchild survived the group kill");
    }
}
