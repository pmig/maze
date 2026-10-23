//! The agents Maze offers: the built-in registry agents (Claude Code, Codex, Gemini,
//! Cursor, pi; PLAN §4.12, ADR-016), the rest of the registry, and custom agents from
//! settings.

use std::collections::BTreeMap;

use settings::CustomAgentSettings;

use crate::registry::{Registry, RegistryAgent};

/// Shown when pi is selected (ADR-016).
pub const PI_NOTICE: &str = "pi runs its tools without asking; the sandbox is the boundary.";

/// Shown for Cursor (ADR-016).
pub const CURSOR_NOTICE: &str = "Cursor's agent is proprietary. Maze downloads it from Cursor \
     when you install it, and you sign in with your Cursor account.";

/// A built-in agent: always listed, with the registry entry when it is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltIn {
    /// Registry id.
    pub id: &'static str,
    /// Display name in Maze.
    pub name: &'static str,
    /// Short names accepted in settings.
    pub aliases: &'static [&'static str],
    /// One line for the picker when the registry isn't loaded.
    pub description: &'static str,
    /// More npm packages installed next to the agent's (pi's coding agent).
    pub extra_packages: &'static [&'static str],
    /// A note shown in the agent column.
    pub notice: Option<&'static str>,
}

/// The built-in agents, in picker order.
pub const BUILT_INS: &[BuiltIn] = &[
    BuiltIn {
        id: "claude-acp",
        name: "Claude Code",
        aliases: &["claude", "claude-code"],
        description: "Anthropic's Claude through the Claude Agent SDK",
        extra_packages: &[],
        notice: None,
    },
    BuiltIn {
        id: "codex-acp",
        name: "Codex",
        aliases: &["codex"],
        description: "OpenAI's coding agent",
        extra_packages: &[],
        notice: None,
    },
    BuiltIn {
        id: "gemini",
        name: "Gemini CLI",
        aliases: &["gemini-cli"],
        description: "Google's Gemini CLI",
        extra_packages: &[],
        notice: None,
    },
    BuiltIn {
        id: "cursor",
        name: "Cursor",
        aliases: &["cursor-agent"],
        description: "Cursor's agent CLI (cursor-agent acp)",
        extra_packages: &[],
        notice: Some(CURSOR_NOTICE),
    },
    BuiltIn {
        id: "pi-acp",
        name: "pi",
        aliases: &["pi"],
        description: "The pi coding agent through pi-acp",
        extra_packages: &["@earendil-works/pi-coding-agent"],
        notice: Some(PI_NOTICE),
    },
];

/// The built-in agent with `id` or alias.
pub fn built_in(id: &str) -> Option<&'static BuiltIn> {
    BUILT_INS
        .iter()
        .find(|b| b.id == id || b.aliases.contains(&id))
}

/// A registry id for a settings value (`claude` → `claude-acp`).
pub fn resolve_alias(id: &str) -> &str {
    built_in(id).map_or(id, |b| b.id)
}

/// Where an agent comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSource {
    /// The ACP registry; `None` while the registry isn't loaded.
    Registry(Option<Box<RegistryAgent>>),
    /// `agent.custom` in settings.
    Custom(CustomAgentSettings),
}

/// One agent in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEntry {
    /// Registry id or custom key.
    pub id: String,
    /// Display name.
    pub name: String,
    /// One line.
    pub description: String,
    /// The registry's current version.
    pub version: Option<String>,
    /// Where it comes from.
    pub source: AgentSource,
    /// One of [`BUILT_INS`].
    pub built_in: bool,
    /// More npm packages to install next to it.
    pub extra_packages: Vec<String>,
    /// A note for the agent column.
    pub notice: Option<&'static str>,
}

impl AgentEntry {
    /// Whether this is pi (its tools don't ask for permission).
    pub fn is_pi(&self) -> bool {
        self.id == "pi-acp"
    }

    /// The registry entry, if known.
    pub fn registry_agent(&self) -> Option<&RegistryAgent> {
        match &self.source {
            AgentSource::Registry(Some(agent)) => Some(agent),
            _ => None,
        }
    }
}

/// The picker list: built-ins first (with registry data when available), then custom
/// agents, then the rest of the registry (when `include_registry` is set).
pub fn catalog(
    registry: Option<&Registry>,
    custom: &BTreeMap<String, CustomAgentSettings>,
    include_registry: bool,
) -> Vec<AgentEntry> {
    let mut out = Vec::new();
    for built in BUILT_INS {
        let agent = registry.and_then(|r| r.get(built.id)).cloned();
        out.push(AgentEntry {
            id: built.id.to_owned(),
            name: built.name.to_owned(),
            description: agent
                .as_ref()
                .and_then(|a| a.description.clone())
                .unwrap_or_else(|| built.description.to_owned()),
            version: agent.as_ref().map(|a| a.version.clone()),
            source: AgentSource::Registry(agent.map(Box::new)),
            built_in: true,
            extra_packages: built
                .extra_packages
                .iter()
                .map(|p| (*p).to_owned())
                .collect(),
            notice: built.notice,
        });
    }
    for (id, settings) in custom {
        if out.iter().any(|entry| &entry.id == id) {
            tracing::warn!(id, "a custom agent can't reuse a registry id");
            continue;
        }
        out.push(AgentEntry {
            id: id.clone(),
            name: settings.name.clone().unwrap_or_else(|| id.clone()),
            description: format!("Custom agent: {}", settings.command),
            version: None,
            source: AgentSource::Custom(settings.clone()),
            built_in: false,
            extra_packages: Vec::new(),
            notice: None,
        });
    }
    if include_registry && let Some(registry) = registry {
        for agent in &registry.agents {
            if out.iter().any(|entry| entry.id == agent.id) {
                continue;
            }
            out.push(AgentEntry {
                id: agent.id.clone(),
                name: agent.name.clone(),
                description: agent.description.clone().unwrap_or_default(),
                version: Some(agent.version.clone()),
                source: AgentSource::Registry(Some(Box::new(agent.clone()))),
                built_in: false,
                extra_packages: Vec::new(),
                notice: None,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::tests::FIXTURE;

    #[test]
    fn built_ins_come_first_with_registry_data() {
        let registry = Registry::parse(FIXTURE.as_bytes()).unwrap();
        let mut custom = BTreeMap::new();
        custom.insert(
            "mine".to_owned(),
            CustomAgentSettings {
                name: Some("My agent".to_owned()),
                command: "/usr/local/bin/my-agent".to_owned(),
                args: vec!["--acp".to_owned()],
                env: BTreeMap::new(),
            },
        );
        let list = catalog(Some(&registry), &custom, true);
        let ids: Vec<&str> = list.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            &ids[..6],
            [
                "claude-acp",
                "codex-acp",
                "gemini",
                "cursor",
                "pi-acp",
                "mine"
            ]
        );
        assert!(ids.contains(&"kilo"));
        assert_eq!(list[0].name, "Claude Code");
        assert_eq!(list[0].version.as_deref(), Some("0.91.0"));
        let pi = &list[4];
        assert!(pi.is_pi());
        assert_eq!(pi.notice, Some(PI_NOTICE));
        assert_eq!(pi.extra_packages, vec!["@earendil-works/pi-coding-agent"]);
        assert!(matches!(list[5].source, AgentSource::Custom(_)));
    }

    #[test]
    fn built_ins_are_listed_without_a_registry() {
        let list = catalog(None, &BTreeMap::new(), true);
        assert_eq!(list.len(), BUILT_INS.len());
        assert!(list.iter().all(|e| e.registry_agent().is_none()));
    }

    #[test]
    fn aliases_resolve() {
        assert_eq!(resolve_alias("claude"), "claude-acp");
        assert_eq!(resolve_alias("pi"), "pi-acp");
        assert_eq!(resolve_alias("mine"), "mine");
    }
}
