//! The sandbox policy of one agent session (PLAN §4.18, "Default policy").

use std::path::{Path, PathBuf};

use sandbox::{NetworkAccess, SandboxPolicy};
use settings::AgentSandboxSettings;

use crate::paths::AgentPaths;

/// What the policy is built from.
#[derive(Debug, Clone)]
pub struct PolicyInput<'a> {
    /// The data directory layout.
    pub paths: &'a AgentPaths,
    /// The user's real home directory (hidden).
    pub home: Option<&'a Path>,
    /// The agent's id (for `agent_paths`).
    pub agent_id: &'a str,
    /// This agent's `HOME`.
    pub agent_home: &'a Path,
    /// This session's scratch dir.
    pub scratch: &'a Path,
    /// This session's `TMPDIR`.
    pub tmp: &'a Path,
    /// The directory of the `mazemail` CLI.
    pub cli_dir: Option<&'a Path>,
    /// Other directories the agent runs from (a system Node in the home directory, a
    /// custom agent's binary).
    pub extra_read: &'a [PathBuf],
    /// The API socket.
    pub socket: Option<&'a Path>,
    /// `agent_sandbox` settings.
    pub settings: &'a AgentSandboxSettings,
}

/// The settings ask for something the sandbox can't do yet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// `network_hosts` with hosts needs the HTTP proxy, which isn't implemented.
    #[error(
        "\"agent_sandbox.network_hosts\" lists hosts, but Maze can't limit agents to \
         certain hosts yet. Set it to null (any host) or [] (no network)."
    )]
    HostAllowlistUnsupported,
}

/// The session's policy:
///
/// - hidden: `$HOME`, all session scratch dirs, all agent homes
/// - readable: installed agents, runtimes, the CLI, this session's scratch dir, this
///   agent's home, `allow_read`, the agent's extra `read` paths
/// - writable: the scratch dir, the per-session `TMPDIR`, the agent home, the agent's
///   extra `write` paths
/// - never readable: the mail store, `deny_read`, `$XDG_RUNTIME_DIR` (Linux)
/// - sockets: `mazemail.sock` only
pub fn session_policy(input: &PolicyInput<'_>) -> Result<SandboxPolicy, PolicyError> {
    let paths = input.paths;
    let expand = |value: &String| expand_home(value, input.home);
    let rules = input.settings.agent_paths.get(input.agent_id).or_else(|| {
        crate::catalog::built_in(input.agent_id).and_then(|b| {
            b.aliases
                .iter()
                .find_map(|a| input.settings.agent_paths.get(*a))
        })
    });

    let mut hide = Vec::new();
    if let Some(home) = input.home {
        hide.push(home.to_owned());
    }
    // These matter when the data directory lives outside the home directory.
    hide.push(paths.sessions_dir());
    hide.push(paths.agent_homes_dir());

    let mut read = vec![paths.agents_dir(), paths.runtimes_dir()];
    read.extend(input.cli_dir.map(Path::to_owned));
    read.extend(input.extra_read.iter().cloned());
    read.extend(input.settings.allow_read.iter().filter_map(expand));
    if let Some(rules) = rules {
        read.extend(rules.read.iter().filter_map(expand));
    }
    // macOS: agents keep their logins in the Keychain (Claude Code does), which reads
    // the keychain files in-process. Items stay protected by their access lists, and
    // Maze's own key only opens for Maze (PLAN §4.18, "Keychain").
    if cfg!(target_os = "macos")
        && let Some(home) = input.home
    {
        read.push(home.join("Library/Keychains"));
    }
    if let Some(socket) = input.socket {
        read.push(socket.to_owned());
    }
    read.push(paths.socket_path_file());

    let mut write = vec![
        input.scratch.to_owned(),
        input.tmp.to_owned(),
        input.agent_home.to_owned(),
    ];
    if let Some(rules) = rules {
        write.extend(rules.write.iter().filter_map(expand));
    }

    let mut deny_read = vec![paths.mail_dir()];
    deny_read.extend(input.settings.deny_read.iter().filter_map(expand));
    if cfg!(target_os = "linux")
        && let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
        && runtime.is_absolute()
    {
        deny_read.push(runtime);
    }

    let network = match &input.settings.network_hosts {
        None => NetworkAccess::All,
        Some(hosts) if hosts.is_empty() => NetworkAccess::None,
        Some(_) => return Err(PolicyError::HostAllowlistUnsupported),
    };

    Ok(SandboxPolicy {
        hide,
        read: dedup(read),
        write: dedup(write),
        deny_read: dedup(deny_read),
        unix_sockets: input.socket.map(Path::to_owned).into_iter().collect(),
        network,
    })
}

/// `~` and `~/…` against `home`; other relative paths are dropped (with a warning).
fn expand_home(value: &str, home: Option<&Path>) -> Option<PathBuf> {
    let value = value.trim();
    let path = if value == "~" {
        home?.to_owned()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home?.join(rest)
    } else {
        PathBuf::from(value)
    };
    if path.is_absolute() {
        Some(path)
    } else {
        tracing::warn!("ignoring a relative path in agent_sandbox settings");
        None
    }
}

fn dedup(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for path in paths {
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use settings::AgentPathRules;

    use super::*;

    #[test]
    fn default_policy_matches_the_plan() {
        let paths = AgentPaths::new("/Users/anna/Library/Application Support/mazemail");
        let home = PathBuf::from("/Users/anna");
        let agent_home = paths.agent_home("claude-acp");
        let scratch = paths.session_dir("s1");
        let tmp = scratch.join(".tmp");
        let socket = paths.data_dir().join("mazemail.sock");
        let settings = AgentSandboxSettings {
            allow_read: vec!["~/Documents/contracts".to_owned(), "relative".to_owned()],
            deny_read: vec!["/Volumes/Backup".to_owned()],
            agent_paths: BTreeMap::from([(
                "claude".to_owned(),
                AgentPathRules {
                    read: vec!["~/Library/Keychains".to_owned()],
                    write: vec![],
                    already_sandboxed: false,
                },
            )]),
            ..AgentSandboxSettings::default()
        };
        let policy = session_policy(&PolicyInput {
            paths: &paths,
            home: Some(&home),
            agent_id: "claude-acp",
            agent_home: &agent_home,
            scratch: &scratch,
            tmp: &tmp,
            cli_dir: Some(Path::new("/Applications/Maze.app/Contents/MacOS")),
            extra_read: &[],
            socket: Some(&socket),
            settings: &settings,
        })
        .unwrap();

        assert_eq!(policy.hide[0], home);
        assert!(policy.hide.contains(&paths.sessions_dir()));
        assert!(policy.read.contains(&paths.agents_dir()));
        assert!(policy.read.contains(&paths.runtimes_dir()));
        assert!(policy.read.contains(&home.join("Documents/contracts")));
        assert!(policy.read.contains(&home.join("Library/Keychains")));
        assert!(!policy.read.iter().any(|p| p.ends_with("relative")));
        assert_eq!(policy.write, vec![scratch, tmp, agent_home]);
        assert_eq!(policy.deny_read[0], paths.mail_dir());
        assert!(policy.deny_read.contains(&PathBuf::from("/Volumes/Backup")));
        assert_eq!(policy.unix_sockets, vec![socket]);
        assert_eq!(policy.network, NetworkAccess::All);
        // Other sessions are not readable: only this session's scratch dir is.
        assert!(!policy.read.contains(&paths.sessions_dir()));
    }

    #[test]
    fn network_hosts() {
        fn network(settings: &AgentSandboxSettings) -> Result<NetworkAccess, PolicyError> {
            let paths = AgentPaths::new("/d");
            let dir = PathBuf::from("/d/x");
            session_policy(&PolicyInput {
                paths: &paths,
                home: None,
                agent_id: "a",
                agent_home: &dir,
                scratch: &dir,
                tmp: &dir,
                cli_dir: None,
                extra_read: &[],
                socket: None,
                settings,
            })
            .map(|policy| policy.network)
        }
        let mut settings = AgentSandboxSettings::default();
        assert_eq!(network(&settings), Ok(NetworkAccess::All));
        settings.network_hosts = Some(Vec::new());
        assert_eq!(network(&settings), Ok(NetworkAccess::None));
        settings.network_hosts = Some(vec!["api.anthropic.com".to_owned()]);
        assert_eq!(
            network(&settings),
            Err(PolicyError::HostAllowlistUnsupported)
        );
    }
}
