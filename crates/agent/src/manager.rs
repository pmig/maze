//! [`AgentManager`]: the agent column's entry point. It lists agents, installs them and
//! starts sessions: scratch dir, session token, sandbox policy, environment, and the
//! sandboxed process as the ACP transport.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use mazemail_types::ThreadId;
use settings::Settings;

use mazemail_types::Sensitive;

use crate::auth::{self, KeyError, LoginCommand};
use crate::catalog::{self, AgentEntry, AgentSource};
use crate::controls::{ControlValue, Preferences};
use crate::host::{AgentHost, AgentKey, AgentThreadInfo, HostError};
use crate::http::Http;
use crate::install::{self, InstallContext, InstallError, InstallProgress, InstalledAgent, Launch};
use crate::node;
use crate::paths::{AgentPaths, safe_id};
use crate::policy::{PolicyError, PolicyInput, session_policy};
use crate::process::{AgentProcess, LaunchSpec};
use crate::registry::{self, Registry, RegistryError};
use crate::scratch::{self, Scratch};
use crate::session::{
    AttachedThread, AuthMethodInfo, SessionConfig, SessionHandle, SessionResources, StderrTail,
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
    /// The environment variable of the user's saved key, if the agent got one.
    pub key_env: Option<String>,
}

/// A terminal login, written as a script to open in a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginScript {
    /// The script (`.command` on macOS, mode 0700, no secrets inside).
    pub path: PathBuf,
    /// The login command as shown in the sign-in card.
    pub display: String,
    /// Whether it runs in the agent sandbox.
    pub sandboxed: bool,
}

/// How an agent process is started (before the session's own additions).
struct Launch0 {
    program: PathBuf,
    args: Vec<String>,
    /// Directories to read besides the defaults.
    extra_read: Vec<PathBuf>,
    /// `PATH` entries of the agent (Node's bin dir, …).
    path_dirs: Vec<PathBuf>,
    /// The agent's own environment (registry, custom agent, `agent.env`).
    env: BTreeMap<String, String>,
    /// The installed version it runs (registry agents).
    version: Option<String>,
}

/// What an update round did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateReport {
    /// Agents updated: (id, name, new version).
    pub updated: Vec<(String, String, String)>,
    /// Agents whose update failed: (id, name, error).
    pub failed: Vec<(String, String, String)>,
}

/// Versions running sessions use: (agent id, version) → sessions.
type InUse = Arc<Mutex<HashMap<(String, String), usize>>>;

/// Keeps an agent version installed while a session runs on it; when the last one
/// ends, versions older than the newest are removed.
struct VersionLease {
    in_use: InUse,
    paths: AgentPaths,
    key: (String, String),
}

impl Drop for VersionLease {
    fn drop(&mut self) {
        let keep: Vec<String> = {
            let mut in_use = self.in_use.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(count) = in_use.get_mut(&self.key) {
                *count -= 1;
                if *count == 0 {
                    in_use.remove(&self.key);
                }
            }
            in_use
                .keys()
                .filter(|(id, _)| *id == self.key.0)
                .map(|(_, version)| version.clone())
                .collect()
        };
        remove_unused(&self.paths, &self.key.0, keep);
    }
}

/// Removes the versions of `id` except the newest installed one and `keep`.
fn remove_unused(paths: &AgentPaths, id: &str, mut keep: Vec<String>) {
    if let Some(newest) = install::installed(paths, id, None) {
        keep.push(newest.version);
    }
    let keep: Vec<&str> = keep.iter().map(String::as_str).collect();
    install::remove_versions_except(paths, id, &keep);
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
    /// The login script couldn't be written.
    #[error("the login script couldn't be written: {0}")]
    Login(std::io::Error),
    /// The method isn't a terminal login.
    #[error("{0} isn't a terminal login")]
    NotATerminalLogin(String),
    /// A pasted key was refused.
    #[error(transparent)]
    Key(#[from] KeyError),
}

/// Picks an in-process agent instead of the process `LaunchSpec` describes (tests).
#[cfg(any(test, feature = "test-support"))]
pub type TestAgents = Arc<dyn Fn(&LaunchSpec) -> Option<crate::fake::FakeAgent> + Send + Sync>;

/// Lists, installs and starts agents.
pub struct AgentManager {
    config: ManagerConfig,
    registry: Mutex<Option<Registry>>,
    in_use: InUse,
    #[cfg(any(test, feature = "test-support"))]
    test_agents: Mutex<Option<TestAgents>>,
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
            in_use: InUse::default(),
            #[cfg(any(test, feature = "test-support"))]
            test_agents: Mutex::new(None),
        }
    }

    /// Runs sessions with in-process agents from `agents` instead of processes, after
    /// everything else (environment, keys, sandbox policy) is prepared as usual. For
    /// UI tests (feature `test-support`).
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_test_agents(&self, agents: TestAgents) {
        *self
            .test_agents
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(agents);
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
        // Running sessions keep their version until they end.
        let keep = self.versions_in_use(&installed.id);
        remove_unused(&self.config.paths, &installed.id, keep);
        Ok(installed)
    }

    fn versions_in_use(&self, id: &str) -> Vec<String> {
        self.in_use
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .filter(|(agent, _)| agent == id)
            .map(|(_, version)| version.clone())
            .collect()
    }

    /// Installs the registry's newer version of every installed agent, and Maze's
    /// pinned Node runtime when an npm agent needs it. New sessions use the new
    /// versions; running ones keep theirs until they end.
    pub async fn update_installed(&self, settings: &Settings) -> UpdateReport {
        let mut report = UpdateReport::default();
        let mut needs_node = false;
        for status in self.catalog(settings, true) {
            let Some(installed) = &status.installed else {
                continue;
            };
            needs_node |= matches!(installed.launch, Launch::Node { .. });
            if !status.update_available() || status.entry.registry_agent().is_none() {
                continue;
            }
            let entry = &status.entry;
            tracing::info!(id = entry.id, "updating an agent");
            match self.install(&entry.id, settings, &|_| {}).await {
                Ok(new) => {
                    report
                        .updated
                        .push((entry.id.clone(), entry.name.clone(), new.version));
                }
                Err(error) => {
                    tracing::warn!(id = entry.id, %error, "an agent update failed");
                    report
                        .failed
                        .push((entry.id.clone(), entry.name.clone(), error.to_string()));
                }
            }
        }
        let node_dir = self.config.paths.node_dir();
        if needs_node && node::installed(&node_dir).is_none() {
            // Maze pins a newer Node: get it, and drop older ones nothing runs on.
            match node::ensure(self.config.http.as_ref(), &node_dir, &|_, _| {}).await {
                Ok(runtime) if runtime.managed => {
                    if self.versions_in_use(NODE_LEASE).is_empty() {
                        remove_old_node(&node_dir, &runtime.prefix);
                    }
                }
                Ok(_) => {}
                Err(error) => report.failed.push((
                    "node".to_owned(),
                    "Node runtime".to_owned(),
                    error.to_string(),
                )),
            }
        }
        report
    }

    /// One automatic update round (when `agent.auto_update` is on): refreshes the
    /// registry when the cached copy is older than a day, then
    /// [`AgentManager::update_installed`].
    pub async fn auto_update(&self, settings: &Settings) -> Result<UpdateReport, RegistryError> {
        if !settings.agent.auto_update {
            return Ok(UpdateReport::default());
        }
        self.load_registry(false).await?;
        Ok(self.update_installed(settings).await)
    }

    fn lease(&self, id: &str, version: &str) -> VersionLease {
        let key = (id.to_owned(), version.to_owned());
        *self
            .in_use
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key.clone())
            .or_default() += 1;
        VersionLease {
            in_use: Arc::clone(&self.in_use),
            paths: self.config.paths.clone(),
            key,
        }
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

        let launch = self.launch(&entry, settings)?;
        let Launch0 {
            program,
            args,
            extra_read,
            path_dirs,
            env: agent_env,
            version,
        } = launch;
        let mut leases: Vec<VersionLease> = Vec::new();
        if let Some(version) = &version {
            leases.push(self.lease(&entry.id, version));
        }
        if let Some(prefix) = node::managed_prefix(&paths.node_dir())
            && program.starts_with(&prefix)
            && let Some(name) = prefix.file_name()
        {
            leases.push(self.lease(NODE_LEASE, &name.to_string_lossy()));
        }

        // The session's folder, home and token.
        let session_id = scratch::new_session_id();
        let scratch = Scratch::create(paths, &session_id, &local_api::registry::agents_md())?;
        let agent_home = paths.agent_home(&safe_id(&entry.id));
        std::fs::create_dir_all(&agent_home)?;
        let cli_bin = scratch.dir().join(".bin");
        std::fs::create_dir_all(&cli_bin)?;
        if let Some(cli) = &self.config.cli_program {
            std::os::unix::fs::symlink(cli, cli_bin.join("mazemail"))?;
        }
        // Agents can't start a browser in the sandbox: Maze opens links for them.
        let open_shim = auth::install_open_shim(&cli_bin)?;
        let open_urls = scratch.dir().join(".open-urls");

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
        let mut env = base_env(paths, &agent_home, scratch.tmp(), path);
        env.insert(
            local_api::SESSION_ENV.to_owned(),
            minted.token.expose().to_owned(),
        );
        env.insert(
            auth::OPEN_URLS_ENV.to_owned(),
            open_urls.display().to_string(),
        );
        env.insert("BROWSER".to_owned(), open_shim.display().to_string());
        let terminal_env = env.clone();
        env.extend(agent_env);
        // The user's saved key (encrypted store) wins over `agent.env`.
        let key = host.agent_key(&entry.id).await?;
        let key_env = key.as_ref().map(|key| key.env.clone());
        if let Some(AgentKey { env: name, key }) = key {
            env.insert(name, key.into_inner());
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

        let (stderr_tx, stderr_rx) = async_channel::bounded(256);
        let stderr = StderrTail::new(stderr_tx);
        let sink = {
            let stderr = stderr.clone();
            Arc::new(move |line: String| stderr.push(line))
        };
        let spec = LaunchSpec {
            program,
            args,
            env,
            cwd: scratch.dir().to_owned(),
        };
        #[cfg(any(test, feature = "test-support"))]
        let test_agent = self
            .test_agents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(|agents| agents(&spec));

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
            open_urls: Some(open_urls),
            key_env: key_env.clone(),
            preferred: self.preferences().get(&entry.id),
        };
        let resources = SessionResources {
            scratch: Some(scratch),
            token: Some((token_store, minted.record.id)),
            stderr: Some(stderr),
            lease: Some(Box::new(leases)),
        };
        #[cfg(any(test, feature = "test-support"))]
        if let Some(agent) = test_agent {
            let handle =
                start_with_transport(agent.transport(), config, host, resources, Some(stderr_rx));
            return Ok(StartedSession {
                handle,
                transcript,
                attached,
                context_note,
                sandboxed: agent_policy.is_some(),
                policy_description,
                key_env,
            });
        }
        let process = AgentProcess::new(spec, agent_policy.clone()).with_stderr(sink);
        let handle = start_with_transport(process, config, host, resources, Some(stderr_rx));
        Ok(StartedSession {
            handle,
            transcript,
            attached,
            context_note,
            sandboxed: agent_policy.is_some(),
            policy_description,
            key_env,
        })
    }

    /// How to start `entry`: program, arguments and its own environment.
    fn launch(&self, entry: &AgentEntry, settings: &Settings) -> Result<Launch0, StartError> {
        let paths = &self.config.paths;
        let mut extra_read = Vec::new();
        let mut path_dirs = Vec::new();
        let mut env = BTreeMap::new();
        let mut version = None;
        let (program, args) = match &entry.source {
            AgentSource::Custom(custom) => {
                let program = resolve_program(&custom.command)
                    .ok_or_else(|| StartError::ProgramNotFound(custom.command.clone()))?;
                if let Some(dir) = program.parent() {
                    extra_read.push(dir.to_owned());
                }
                env.extend(custom.env.clone());
                (program, custom.args.clone())
            }
            AgentSource::Registry(_) => {
                let installed = install::installed(paths, &entry.id, entry.version.as_deref())
                    .ok_or_else(|| StartError::NotInstalled(entry.name.clone()))?;
                env.extend(installed.env.clone());
                path_dirs.extend(installed.path_dirs.clone());
                version = Some(installed.version.clone());
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
        let agent_home = paths.agent_home(&safe_id(&entry.id));
        env.extend(auth::home_env(&entry.id, &agent_home));
        let configured = settings.agent.env.get(&entry.id).or_else(|| {
            catalog::built_in(&entry.id)
                .and_then(|b| b.aliases.iter().find_map(|a| settings.agent.env.get(*a)))
        });
        if let Some(extra) = configured {
            env.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        Ok(Launch0 {
            program,
            args,
            extra_read,
            path_dirs,
            env,
            version,
        })
    }

    /// Writes the script for a terminal login of `agent_id` with `method` (the agent's
    /// command plus the method's arguments, or the method's own command), in the agent
    /// sandbox with the agent's home and environment, minus Maze's session token and
    /// the user's key. Open it with a terminal; it opens the login's links in the
    /// browser.
    pub fn terminal_login(
        &self,
        agent_id: &str,
        method: &AuthMethodInfo,
        settings: &Settings,
    ) -> Result<LoginScript, StartError> {
        let terminal = method
            .terminal
            .as_ref()
            .ok_or_else(|| StartError::NotATerminalLogin(method.name.clone()))?;
        let entry = self
            .entry(settings, agent_id)
            .ok_or_else(|| StartError::UnknownAgent(agent_id.to_owned()))?;
        let paths = &self.config.paths;
        let Launch0 {
            program,
            args,
            extra_read,
            path_dirs,
            env: agent_env,
            version: _,
        } = self.launch(&entry, settings)?;
        let agent_home = paths.agent_home(&safe_id(&entry.id));
        let tmp = agent_home.join(".tmp");
        std::fs::create_dir_all(&tmp).map_err(StartError::Login)?;
        let login_dir = paths.agents_dir().join("login");
        let shim_dir = login_dir.join("bin");
        let open_shim = auth::install_open_shim(&shim_dir).map_err(StartError::Login)?;
        let urls = agent_home.join(".mazemail-open-urls");

        let mut path = vec![shim_dir];
        path.extend(path_dirs.iter().cloned());
        let mut env = base_env(paths, &agent_home, &tmp, path);
        env.insert(auth::OPEN_URLS_ENV.to_owned(), urls.display().to_string());
        env.insert("BROWSER".to_owned(), open_shim.display().to_string());
        env.extend(agent_env);
        env.extend(terminal.env.clone());

        let (program, args) = match &terminal.command {
            None => {
                let mut args = args;
                args.extend(terminal.args.clone());
                (program, args)
            }
            Some(command) => {
                let program = if Path::new(command).is_absolute() {
                    PathBuf::from(command)
                } else {
                    path_dirs
                        .iter()
                        .map(|dir| dir.join(command))
                        .find(|candidate| candidate.is_file())
                        .or_else(|| resolve_program(command))
                        .ok_or_else(|| StartError::ProgramNotFound(command.clone()))?
                };
                (program, terminal.args.clone())
            }
        };
        let sandboxed = settings.agent_sandbox.enabled;
        let policy = if sandboxed {
            sandbox::check_available()?;
            let mut read = extra_read;
            read.push(login_dir.clone());
            Some(session_policy(&PolicyInput {
                paths,
                home: self.config.home.as_deref(),
                agent_id: &entry.id,
                agent_home: &agent_home,
                scratch: &agent_home,
                tmp: &tmp,
                cli_dir: None,
                extra_read: &read,
                socket: None,
                settings: &settings.agent_sandbox,
            })?)
        } else {
            None
        };
        let display = std::iter::once(program.display().to_string())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        let command = LoginCommand {
            title: format!("{}: {}", entry.name, method.name),
            program,
            args,
            env,
            cwd: agent_home,
            policy,
            urls,
        };
        let script = auth::login_script(&command)?;
        let extension = if cfg!(target_os = "macos") {
            "command"
        } else {
            "sh"
        };
        let path = login_dir.join(format!("{}.{extension}", safe_id(&entry.id)));
        auth::write_script(&path, &script).map_err(StartError::Login)?;
        Ok(LoginScript {
            path,
            display,
            sandboxed,
        })
    }

    /// Saves a pasted key for `agent_id` (encrypted store) and returns the environment
    /// variable it will be passed in. `provider` is the provider of the agent's API-key
    /// sign-in method, for agents Maze doesn't know.
    pub async fn save_key(
        &self,
        agent_id: &str,
        provider: Option<&str>,
        key: &str,
    ) -> Result<String, StartError> {
        let key = auth::clean_key(key)?;
        let env = auth::key_env(agent_id, provider, key).ok_or(KeyError::Unsupported)?;
        self.config
            .host
            .set_agent_key(
                catalog::resolve_alias(agent_id),
                AgentKey {
                    env: env.to_owned(),
                    key: Sensitive::new(key.to_owned()),
                },
            )
            .await?;
        Ok(env.to_owned())
    }

    /// Deletes the saved key of `agent_id`.
    pub async fn remove_key(&self, agent_id: &str) -> Result<(), StartError> {
        Ok(self
            .config
            .host
            .delete_agent_key(catalog::resolve_alias(agent_id))
            .await?)
    }

    /// The remembered model, mode and other choices per agent.
    pub fn preferences(&self) -> Preferences {
        Preferences::new(self.config.paths.agents_dir().join("preferences.json"))
    }

    /// Remembers a choice for `agent_id`'s next sessions.
    pub fn remember_control(
        &self,
        agent_id: &str,
        id: &str,
        value: &ControlValue,
    ) -> std::io::Result<()> {
        self.preferences()
            .remember(catalog::resolve_alias(agent_id), id, value)
    }

    /// Agent id → environment variable of every saved key.
    pub async fn key_envs(&self) -> Result<BTreeMap<String, String>, StartError> {
        Ok(self.config.host.agent_key_envs().await?)
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

/// The lease key of the managed Node runtime.
const NODE_LEASE: &str = "#node";

/// Removes managed Node runtimes other than `current`.
fn remove_old_node(node_dir: &Path, current: &Path) {
    let Ok(entries) = std::fs::read_dir(node_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path != current && name.to_string_lossy().starts_with("node-") && path.is_dir() {
            tracing::info!(runtime = %name.to_string_lossy(), "removing an old Node runtime");
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// The environment every agent process and login starts from: nothing of Maze's own
/// environment except `LANG`, `USER`, `LOGNAME` and a system `SHELL`.
fn base_env(
    paths: &AgentPaths,
    agent_home: &Path,
    tmp: &Path,
    mut path: Vec<PathBuf>,
) -> BTreeMap<String, String> {
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
        ("TMPDIR".to_owned(), tmp.display().to_string()),
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
    env
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
    use crate::MemoryHost;
    use crate::archive::tests::{sha256_hex, tar_gz};
    use crate::fake::{FakeAgent, FakeStep};
    use crate::http::tests::MemoryHttp;
    use crate::registry::{BinaryTarget, Distribution, RegistryAgent, platform_key};
    use crate::session::{SessionEvent, SessionStatus};

    fn registry(version: &str, url: &str, sha256: String) -> Registry {
        let mut targets = BTreeMap::new();
        if let Some(key) = platform_key() {
            targets.insert(
                key.to_owned(),
                BinaryTarget {
                    archive: url.to_owned(),
                    sha256: Some(sha256),
                    cmd: "./cursor-agent".to_owned(),
                    args: vec!["acp".to_owned()],
                    env: BTreeMap::new(),
                },
            );
        }
        Registry {
            version: "1.0.0".to_owned(),
            agents: vec![RegistryAgent {
                id: "cursor".to_owned(),
                name: "Cursor".to_owned(),
                version: version.to_owned(),
                description: None,
                repository: None,
                website: None,
                authors: Vec::new(),
                license: None,
                icon: None,
                distribution: Distribution {
                    binary: Some(targets),
                    npx: None,
                    uvx: None,
                },
            }],
        }
    }

    async fn ready(handle: &SessionHandle) {
        let events = handle.events();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Ok(event) = events.recv().await {
                if event == SessionEvent::Status(SessionStatus::Ready) {
                    return;
                }
            }
            panic!("the session ended");
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn updates_install_beside_running_sessions_and_old_versions_go_later() {
        if platform_key().is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = AgentPaths::new(dir.path());
        let http = Arc::new(MemoryHttp::default());
        let mut sums = Vec::new();
        for (version, url) in [
            ("1.0.0", "https://downloads.example/agent-1.tar.gz"),
            ("2.0.0", "https://downloads.example/agent-2.tar.gz"),
            ("3.0.0", "https://downloads.example/agent-3.tar.gz"),
        ] {
            let script = format!("#!/bin/sh\necho {version}\n");
            let archive = tar_gz(&[("cursor-agent", script.as_bytes(), true)]);
            http.insert(url, &archive);
            sums.push((version, url, sha256_hex(&archive)));
        }
        let manager = AgentManager::new(ManagerConfig {
            paths: paths.clone(),
            home: None,
            cli_program: None,
            socket: None,
            host: Arc::new(MemoryHost::default()),
            http: Arc::clone(&http) as Arc<dyn Http>,
        });
        let programs: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        {
            let programs = Arc::clone(&programs);
            manager.set_test_agents(Arc::new(move |spec| {
                programs.lock().unwrap().push(spec.program.clone());
                Some(FakeAgent::new(vec![FakeStep::Message("hi".into())]))
            }));
        }
        let mut settings = Settings::default();
        settings.agent_sandbox.enabled = false;
        let start = |settings: Settings| StartRequest {
            agent_id: "cursor".to_owned(),
            mail_thread: None,
            resume: None,
            settings,
        };

        let (v1, url1, sum1) = sums[0].clone();
        manager.set_registry(registry(v1, url1, sum1));
        manager.install("cursor", &settings, &|_| {}).await.unwrap();
        let first = manager.start(start(settings.clone())).await.unwrap();
        ready(&first.handle).await;

        // The registry has 2.0.0: the update installs it next to the running 1.0.0.
        let (v2, url2, sum2) = sums[1].clone();
        manager.set_registry(registry(v2, url2, sum2));
        let report = manager.update_installed(&settings).await;
        assert_eq!(
            report.updated,
            vec![("cursor".to_owned(), "Cursor".to_owned(), "2.0.0".to_owned())]
        );
        assert!(report.failed.is_empty());
        assert!(paths.install_dir("cursor", "2.0.0").is_dir());
        assert!(
            paths.install_dir("cursor", "1.0.0").is_dir(),
            "still in use"
        );
        // The running session keeps working.
        first.handle.prompt("still there?");
        ready(&first.handle).await;
        // New sessions run the new version.
        let second = manager.start(start(settings.clone())).await.unwrap();
        ready(&second.handle).await;
        let programs_now = programs.lock().unwrap().clone();
        assert!(programs_now[0].starts_with(paths.install_dir("cursor", "1.0.0")));
        assert!(programs_now[1].starts_with(paths.install_dir("cursor", "2.0.0")));
        // Nothing left to update.
        assert_eq!(
            manager.update_installed(&settings).await,
            UpdateReport::default()
        );

        // The old version goes once its last session ended.
        drop(first);
        for _ in 0..500 {
            if !paths.install_dir("cursor", "1.0.0").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!paths.install_dir("cursor", "1.0.0").exists());
        assert!(paths.install_dir("cursor", "2.0.0").is_dir());

        // auto_update off: nothing is fetched or installed.
        let (v3, url3, sum3) = sums[2].clone();
        manager.set_registry(registry(v3, url3, sum3));
        settings.agent.auto_update = false;
        let requests = http.requests();
        assert_eq!(
            manager.auto_update(&settings).await.unwrap(),
            UpdateReport::default()
        );
        assert_eq!(http.requests(), requests);
        assert!(!paths.install_dir("cursor", "3.0.0").exists());
        drop(second);
    }

    #[test]
    fn programs_resolve() {
        assert_eq!(resolve_program("/bin/sh"), Some(PathBuf::from("/bin/sh")));
        assert!(resolve_program("sh").is_some());
        assert!(resolve_program("/nonexistent/agent").is_none());
    }
}
