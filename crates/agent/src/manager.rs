//! [`AgentManager`]: the agent column's entry point. It lists agents, installs them and
//! starts sessions: scratch dir, session token, sandbox policy, environment, and the
//! sandboxed process as the ACP transport.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use mazemail_types::ThreadId;
use settings::Settings;

use crate::catalog::{self, AgentEntry, AgentSource};
use crate::host::{AgentHost, AgentThreadInfo, HostError};
use crate::http::Http;
use crate::install::{self, InstallContext, InstallError, InstallProgress, InstalledAgent, Launch};
use crate::node;
use crate::paths::{AgentPaths, safe_id};
use crate::policy::{PolicyError, PolicyInput, session_policy};
use crate::process::{AgentProcess, LaunchSpec};
use crate::registry::{self, Registry, RegistryError};
use crate::scratch::{self, Scratch};
use crate::session::{
    AttachedThread, SessionConfig, SessionHandle, SessionResources, StderrTail,
    start_with_transport,
};
use crate::tokens::{self, SessionTokenError};
use crate::transcript::Transcript;

/// What the manager needs.
pub struct ManagerConfig {
    /// The data directory layout.
    pub paths: AgentPaths,
    /// The user's home directory (hidden from agents).
    pub home: Option<PathBuf>,
    /// The `mazemail` CLI (the running executable).
    pub cli_program: Option<PathBuf>,
    /// The API socket agents may reach.
    pub socket: Option<PathBuf>,
    /// Maze's side of sessions.
    pub host: Arc<dyn AgentHost>,
    /// Downloads.
    pub http: Arc<dyn Http>,
}

impl std::fmt::Debug for ManagerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagerConfig")
            .field("paths", &self.paths)
            .field("cli_program", &self.cli_program)
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

/// An agent in the picker with its install state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    /// The entry.
    pub entry: AgentEntry,
    /// The installed version, if any.
    pub installed: Option<InstalledAgent>,
}

impl AgentStatus {
    /// Ready to start (installed, or a custom agent).
    pub fn is_ready(&self) -> bool {
        self.installed.is_some() || matches!(self.entry.source, AgentSource::Custom(_))
    }

    /// The registry has a newer version than the installed one.
    pub fn update_available(&self) -> bool {
        match (&self.installed, &self.entry.version) {
            (Some(installed), Some(latest)) => &installed.version != latest,
            _ => false,
        }
    }
}

/// What to start.
#[derive(Debug, Clone)]
pub struct StartRequest {
    /// Agent id or alias.
    pub agent_id: String,
    /// The mail thread selected in the window, attached as context.
    pub mail_thread: Option<ThreadId>,
    /// An earlier conversation to continue.
    pub resume: Option<AgentThreadInfo>,
    /// Current settings.
    pub settings: Settings,
}

/// A started session.
#[derive(Debug)]
pub struct StartedSession {
    /// The session.
    pub handle: SessionHandle,
    /// The conversation so far (empty for a new one).
    pub transcript: Transcript,
    /// The attached thread.
    pub attached: Option<AttachedThread>,
    /// Why the selected thread wasn't attached.
    pub context_note: Option<String>,
    /// Whether the agent process runs sandboxed.
    pub sandboxed: bool,
    /// The effective policy, for the padlock.
    pub policy_description: Option<String>,
}

/// Starting a session failed.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// No such agent.
    #[error("there is no agent called {0}")]
    UnknownAgent(String),
    /// Install it first.
    #[error("{0} isn't installed yet")]
    NotInstalled(String),
    /// A custom agent's program wasn't found.
    #[error("the program {0} of a custom agent wasn't found")]
    ProgramNotFound(String),
    /// The sandbox isn't available (the error says how to fix it).
    #[error(transparent)]
    Sandbox(#[from] sandbox::SandboxError),
    /// The sandbox settings can't be applied.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// Node is missing.
    #[error(transparent)]
    Node(#[from] node::NodeError),
    /// The scratch dir couldn't be created.
    #[error("the session folder couldn't be created: {0}")]
    Scratch(#[from] std::io::Error),
    /// Maze's store failed.
    #[error(transparent)]
    Host(#[from] HostError),
    /// The session token couldn't be created.
    #[error(transparent)]
    Token(#[from] SessionTokenError),
}

/// Lists, installs and starts agents.
pub struct AgentManager {
    config: ManagerConfig,
    registry: Mutex<Option<Registry>>,
}

impl std::fmt::Debug for AgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentManager")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AgentManager {
    /// A manager; removes stale session scratch dirs (call once at startup).
    pub fn new(config: ManagerConfig) -> Self {
        scratch::remove_stale(&config.paths);
        Self {
            config,
            registry: Mutex::new(None),
        }
    }

    /// Maze's side of sessions (threads, transcripts, proposals).
    pub fn host(&self) -> &Arc<dyn AgentHost> {
        &self.config.host
    }

    /// The data directory layout.
    pub fn paths(&self) -> &AgentPaths {
        &self.config.paths
    }

    /// Loads the registry (cache, then network). `refresh` ignores a fresh cache.
    pub async fn load_registry(&self, refresh: bool) -> Result<(), RegistryError> {
        let cache = self.config.paths.registry_cache();
        let max_age = if refresh {
            std::time::Duration::ZERO
        } else {
            registry::REGISTRY_MAX_AGE
        };
        let loaded = registry::load(self.config.http.as_ref(), &cache, max_age).await?;
        *self.registry.lock().unwrap_or_else(PoisonError::into_inner) = Some(loaded);
        Ok(())
    }

    /// Uses `registry` (tests, demos).
    pub fn set_registry(&self, registry: Registry) {
        *self.registry.lock().unwrap_or_else(PoisonError::into_inner) = Some(registry);
    }

    /// The agents for the picker: built-ins, custom agents, and (with
    /// `include_registry`) every other registry agent.
    pub fn catalog(&self, settings: &Settings, include_registry: bool) -> Vec<AgentStatus> {
        let registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        catalog::catalog(registry.as_ref(), &settings.agent.custom, include_registry)
            .into_iter()
            .map(|entry| {
                let installed = match &entry.source {
                    AgentSource::Custom(_) => None,
                    AgentSource::Registry(_) => {
                        install::installed(&self.config.paths, &entry.id, entry.version.as_deref())
                    }
                };
                AgentStatus { entry, installed }
            })
            .collect()
    }

    fn entry(&self, settings: &Settings, id: &str) -> Option<AgentEntry> {
        let id = catalog::resolve_alias(id);
        self.catalog(settings, true)
            .into_iter()
            .map(|status| status.entry)
            .find(|entry| entry.id == id)
    }

    /// Installs (or updates) an agent and removes its older versions.
    pub async fn install(
        &self,
        id: &str,
        settings: &Settings,
        progress: &(dyn Fn(InstallProgress) + Send + Sync),
    ) -> Result<InstalledAgent, InstallError> {
        let entry = self
            .entry(settings, id)
            .ok_or_else(|| InstallError::NoRegistry(id.to_owned()))?;
        let cx = InstallContext {
            paths: &self.config.paths,
            http: self.config.http.as_ref(),
            home: self.config.home.as_deref(),
            sandboxed: settings.agent_sandbox.enabled,
        };
        let installed = install::install(&entry, cx, progress).await?;
        install::remove_other_versions(&self.config.paths, &installed.id, &installed.version);
        Ok(installed)
    }

    /// Starts a session.
    pub async fn start(&self, request: StartRequest) -> Result<StartedSession, StartError> {
        let settings = &request.settings;
        let entry = self
            .entry(settings, &request.agent_id)
            .ok_or_else(|| StartError::UnknownAgent(request.agent_id.clone()))?;
        let paths = &self.config.paths;
        let sandboxed = settings.agent_sandbox.enabled;
        if sandboxed {
            sandbox::check_available()?;
        }

        // How to start the agent.
        let mut extra_read = Vec::new();
        let mut path_dirs = Vec::new();
        let mut agent_env = BTreeMap::new();
        let (program, args) = match &entry.source {
            AgentSource::Custom(custom) => {
                let program = resolve_program(&custom.command)
                    .ok_or_else(|| StartError::ProgramNotFound(custom.command.clone()))?;
                if let Some(dir) = program.parent() {
                    extra_read.push(dir.to_owned());
                }
                agent_env.extend(custom.env.clone());
                (program, custom.args.clone())
            }
            AgentSource::Registry(_) => {
                let installed = install::installed(paths, &entry.id, entry.version.as_deref())
                    .ok_or_else(|| StartError::NotInstalled(entry.name.clone()))?;
                agent_env.extend(installed.env.clone());
                path_dirs.extend(installed.path_dirs.clone());
                match &installed.launch {
                    Launch::Binary { program } => (program.clone(), installed.args.clone()),
                    Launch::Node { script } => {
                        let runtime = node::installed(&paths.node_dir())
                            .or_else(node::system_node)
                            .ok_or_else(|| node::NodeError::Missing {
                                reason: "the managed runtime isn't installed".to_owned(),
                            })?;
                        if !runtime.prefix.starts_with(paths.runtimes_dir()) {
                            extra_read.push(runtime.prefix.clone());
                        }
                        path_dirs.push(runtime.bin_dir.clone());
                        let mut args = vec![script.display().to_string()];
                        args.extend(installed.args.clone());
                        (runtime.node, args)
                    }
                }
            }
        };

        // The session's folder, home and token.
        let session_id = scratch::new_session_id();
        let scratch = Scratch::create(paths, &session_id, &local_api::registry::agents_md())?;
        let agent_home = paths.agent_home(&safe_id(&entry.id));
        std::fs::create_dir_all(&agent_home)?;
        let cli_bin = scratch.dir().join(".bin");
        if let Some(cli) = &self.config.cli_program {
            std::fs::create_dir_all(&cli_bin)?;
            std::os::unix::fs::symlink(cli, cli_bin.join("mazemail"))?;
        }

        let host = Arc::clone(&self.config.host);
        let (attached, context_note) = match request.mail_thread {
            Some(thread) => match host.thread_export(thread).await? {
                Some(export) => {
                    scratch.write_thread(export.markdown.expose())?;
                    (
                        Some(AttachedThread {
                            public_id: export.public_id,
                        }),
                        None,
                    )
                }
                None => (
                    None,
                    Some(
                        "The selected thread isn't attached: its account doesn't share mail \
                         with agents (agent.accounts in settings)."
                            .to_owned(),
                    ),
                ),
            },
            None => (None, None),
        };

        let (agent_thread, transcript) = match &request.resume {
            Some(info) => {
                let transcript = host
                    .load_transcript(info.key)
                    .await?
                    .map(|bytes| Transcript::from_json(&bytes))
                    .unwrap_or_default();
                (info.key, transcript)
            }
            None => (
                host.create_agent_thread(&entry.id, None, request.mail_thread)
                    .await?,
                Transcript::default(),
            ),
        };

        let token_store = host.token_store();
        let minted = tokens::mint_session_token(
            token_store.as_ref(),
            host.account_policy().as_ref(),
            tokens::profile(settings.agent.session_profile),
            &format!("Maze agent session: {}", entry.name),
            &session_id,
        )
        .await?;

        // Environment: only what Maze sets.
        let mut path = vec![cli_bin.clone()];
        path.extend(path_dirs);
        path.extend(
            [
                "/opt/homebrew/bin",
                "/usr/local/bin",
                "/usr/bin",
                "/bin",
                "/usr/sbin",
                "/sbin",
            ]
            .into_iter()
            .map(PathBuf::from),
        );
        let path = std::env::join_paths(&path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut env = BTreeMap::from([
            ("PATH".to_owned(), path),
            ("HOME".to_owned(), agent_home.display().to_string()),
            ("TMPDIR".to_owned(), scratch.tmp().display().to_string()),
            (
                local_api::SESSION_ENV.to_owned(),
                minted.token.expose().to_owned(),
            ),
            (
                settings::DATA_DIR_ENV.to_owned(),
                paths.data_dir().display().to_string(),
            ),
            (
                "npm_config_cache".to_owned(),
                agent_home.join(".npm").display().to_string(),
            ),
            (
                "XDG_CONFIG_HOME".to_owned(),
                agent_home.join(".config").display().to_string(),
            ),
            (
                "XDG_CACHE_HOME".to_owned(),
                agent_home.join(".cache").display().to_string(),
            ),
            (
                "XDG_DATA_HOME".to_owned(),
                agent_home.join(".local/share").display().to_string(),
            ),
            (
                "LANG".to_owned(),
                std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".to_owned()),
            ),
        ]);
        // A shell for agents that look for one; never one from the home directory.
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|shell| shell.starts_with("/bin/") || shell.starts_with("/usr/bin/"))
            .unwrap_or_else(|| "/bin/sh".to_owned());
        env.insert("SHELL".to_owned(), shell);
        for name in ["USER", "LOGNAME"] {
            if let Ok(value) = std::env::var(name) {
                env.insert(name.to_owned(), value);
            }
        }
        let terminal_env = env.clone();
        env.extend(agent_env);
        let configured = settings.agent.env.get(&entry.id).or_else(|| {
            catalog::built_in(&entry.id)
                .and_then(|b| b.aliases.iter().find_map(|a| settings.agent.env.get(*a)))
        });
        if let Some(extra) = configured {
            env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        }

        // The sandbox.
        let policy = if sandboxed {
            Some(session_policy(&PolicyInput {
                paths,
                home: self.config.home.as_deref(),
                agent_id: &entry.id,
                agent_home: &agent_home,
                scratch: scratch.dir(),
                tmp: scratch.tmp(),
                cli_dir: self.config.cli_program.as_deref().and_then(Path::parent),
                extra_read: &extra_read,
                socket: self.config.socket.as_deref(),
                settings: &settings.agent_sandbox,
            })?)
        } else {
            None
        };
        let already_sandboxed = settings
            .agent_sandbox
            .agent_paths
            .get(&entry.id)
            .is_some_and(|rules| rules.already_sandboxed);
        let agent_policy = if already_sandboxed {
            None
        } else {
            policy.clone()
        };

        let terminal_login = Some(format!(
            "HOME='{}' '{}' {}",
            agent_home.display(),
            program.display(),
            args.join(" ")
        ));
        let (stderr_tx, stderr_rx) = async_channel::bounded(256);
        let stderr = StderrTail::new(stderr_tx);
        let sink = {
            let stderr = stderr.clone();
            Arc::new(move |line: String| stderr.push(line))
        };
        let process = AgentProcess::new(
            LaunchSpec {
                program,
                args,
                env,
                cwd: scratch.dir().to_owned(),
            },
            agent_policy.clone(),
        )
        .with_stderr(sink);

        let policy_description = policy.as_ref().map(sandbox::SandboxPolicy::describe);
        let config = SessionConfig {
            session_id,
            agent_id: entry.id.clone(),
            agent_name: entry.name.clone(),
            agent_thread,
            resume_acp_session: request.resume.and_then(|info| info.acp_session_id),
            cwd: scratch.dir().to_owned(),
            terminal_env,
            cli_program: self.config.cli_program.clone(),
            terminal_policy: policy,
            attached: attached.clone(),
            transcript: transcript.clone(),
            terminal_login,
        };
        let resources = SessionResources {
            scratch: Some(scratch),
            token: Some((token_store, minted.record.id)),
            stderr: Some(stderr),
        };
        let handle = start_with_transport(process, config, host, resources, Some(stderr_rx));
        Ok(StartedSession {
            handle,
            transcript,
            attached,
            context_note,
            sandboxed: agent_policy.is_some(),
            policy_description,
        })
    }

    /// Attaches a different mail thread to a running session (rewrites `thread.md`).
    pub async fn attach_thread(
        &self,
        handle: &SessionHandle,
        thread: Option<ThreadId>,
    ) -> Result<Option<AttachedThread>, StartError> {
        let dir = self.config.paths.session_dir(handle.id());
        let file = dir.join(scratch::THREAD_FILE);
        let attached = match thread {
            Some(thread) => match self.config.host.thread_export(thread).await? {
                Some(export) => {
                    std::fs::write(&file, export.markdown.expose())?;
                    Some(AttachedThread {
                        public_id: export.public_id,
                    })
                }
                None => {
                    let _ = std::fs::remove_file(&file);
                    None
                }
            },
            None => {
                let _ = std::fs::remove_file(&file);
                None
            }
        };
        handle.set_attached(attached.clone());
        Ok(attached)
    }
}

/// An absolute program path, or a name looked up on `PATH`.
fn resolve_program(command: &str) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_owned());
    }
    let search = std::env::var_os("PATH")?;
    std::env::split_paths(&search)
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_resolve() {
        assert_eq!(resolve_program("/bin/sh"), Some(PathBuf::from("/bin/sh")));
        assert!(resolve_program("sh").is_some());
        assert!(resolve_program("/nonexistent/agent").is_none());
    }
}
