//! Where agents live in the data directory (PLAN §4.3, §4.12):
//!
//! ```text
//! <data_dir>/agents/<id>/<version>/   installed agents (one dir per version)
//! <data_dir>/agents/registry.json     the cached ACP registry
//! <data_dir>/agent-home/<id>/         each agent's HOME (config, login state, npm cache)
//! <data_dir>/runtimes/node/           the managed Node runtime
//! <data_dir>/sessions/<id>/           one scratch dir per agent session (plaintext)
//! ```

use std::path::{Path, PathBuf};

/// The agent directories of one data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaths {
    data_dir: PathBuf,
}

impl AgentPaths {
    /// The directories under `data_dir`.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    /// The data directory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The encrypted mail store; never readable by agents.
    pub fn mail_dir(&self) -> PathBuf {
        self.data_dir.join("mail")
    }

    /// Installed agents.
    pub fn agents_dir(&self) -> PathBuf {
        self.data_dir.join("agents")
    }

    /// The install directory of one agent version.
    pub fn install_dir(&self, id: &str, version: &str) -> PathBuf {
        self.agents_dir().join(id).join(version)
    }

    /// The cached registry index.
    pub fn registry_cache(&self) -> PathBuf {
        self.agents_dir().join("registry.json")
    }

    /// All agent homes.
    pub fn agent_homes_dir(&self) -> PathBuf {
        self.data_dir.join("agent-home")
    }

    /// One agent's `HOME`.
    pub fn agent_home(&self, id: &str) -> PathBuf {
        self.agent_homes_dir().join(id)
    }

    /// Runtimes for agents.
    pub fn runtimes_dir(&self) -> PathBuf {
        self.data_dir.join("runtimes")
    }

    /// The managed Node runtime.
    pub fn node_dir(&self) -> PathBuf {
        self.runtimes_dir().join("node")
    }

    /// All session scratch dirs.
    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    /// One session's scratch dir.
    pub fn session_dir(&self, session: &str) -> PathBuf {
        self.sessions_dir().join(session)
    }

    /// The file the server writes when the socket lives outside the data directory.
    pub fn socket_path_file(&self) -> PathBuf {
        self.data_dir.join("socket-path")
    }
}

/// A file-system-safe form of an agent id (registry ids are already safe; custom ids
/// come from settings).
pub fn safe_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    match cleaned.trim_matches('.') {
        "" => "agent".to_owned(),
        trimmed => trimmed.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let paths = AgentPaths::new("/data");
        assert_eq!(
            paths.install_dir("claude-acp", "0.91.0"),
            PathBuf::from("/data/agents/claude-acp/0.91.0")
        );
        assert_eq!(
            paths.agent_home("cursor"),
            PathBuf::from("/data/agent-home/cursor")
        );
        assert_eq!(paths.session_dir("s1"), PathBuf::from("/data/sessions/s1"));
        assert_eq!(paths.node_dir(), PathBuf::from("/data/runtimes/node"));
    }

    #[test]
    fn ids_become_safe_directory_names() {
        assert_eq!(safe_id("claude-acp"), "claude-acp");
        assert_eq!(safe_id("../evil"), "_evil");
        assert_eq!(safe_id("my agent/2"), "my_agent_2");
        assert_eq!(safe_id(".."), "agent");
    }
}
