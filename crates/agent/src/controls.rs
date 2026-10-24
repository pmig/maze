//! What the agent lets you choose for a session: its model, mode, thinking effort and
//! other switches (ACP session config options, or the older session modes), and the
//! choices Maze remembers per agent.
//!
//! - Agents that send `configOptions` (Claude Code: `mode`, `model`, `effort`, `fast`;
//!   Codex: `mode`, `model`, `reasoning_effort`, …) are driven only through them
//!   (`session/set_config_option`), as the protocol asks; their `modes` are ignored.
//! - Agents that only send `modes` (Gemini CLI) get one [`ControlCategory::Mode`]
//!   control that uses `session/set_mode`.
//! - The UI places controls by category, never by id: Claude's effort option is
//!   `effort`, Codex's is `reasoning_effort`; both are [`ControlCategory::ThoughtLevel`].
//!
//! [`Preferences`] keeps the last choice per agent and control id in
//! `<data_dir>/agents/preferences.json` (no secrets: model and mode names), and sessions
//! apply them when they start ([`crate::SessionConfig::preferred`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelectOptions, SessionModeState,
};
use serde::{Deserialize, Serialize};

/// The id of the control made from session modes.
pub const MODE_CONTROL: &str = "mode";

/// A value of a control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ControlValue {
    /// An on/off switch.
    Toggle(bool),
    /// One of the choices of a select.
    Choice(String),
}

impl ControlValue {
    pub(crate) fn to_acp(&self) -> SessionConfigOptionValue {
        match self {
            Self::Toggle(on) => SessionConfigOptionValue::boolean(*on),
            Self::Choice(value) => SessionConfigOptionValue::value_id(value.clone()),
        }
    }
}

/// Where a control belongs in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlCategory {
    /// How the agent works: asks before edits, plans, runs on its own.
    Mode,
    /// The model.
    Model,
    /// A setting of the model (Claude's "fast").
    ModelConfig,
    /// Thinking or reasoning effort.
    ThoughtLevel,
    /// Anything else.
    Other,
}

/// One choice of a select.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// What is sent.
    pub value: String,
    /// What is shown.
    pub name: String,
    /// More text.
    pub description: Option<String>,
    /// The group it is listed under, if the agent groups them.
    pub group: Option<String>,
}

/// A select or a switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlKind {
    /// One of several choices.
    Select {
        /// The current value.
        current: String,
        /// The choices, in the agent's order.
        choices: Vec<Choice>,
    },
    /// On or off.
    Toggle(bool),
}

/// One thing the agent lets the user choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    /// The agent's id for it (`model`, `effort`, … or [`MODE_CONTROL`]).
    pub id: String,
    /// Label.
    pub name: String,
    /// More text.
    pub description: Option<String>,
    /// Where it belongs.
    pub category: ControlCategory,
    /// Select or switch.
    pub kind: ControlKind,
    /// Made from the older session modes (`session/set_mode`).
    pub legacy_mode: bool,
}

impl Control {
    /// The current value.
    pub fn value(&self) -> ControlValue {
        match &self.kind {
            ControlKind::Select { current, .. } => ControlValue::Choice(current.clone()),
            ControlKind::Toggle(on) => ControlValue::Toggle(*on),
        }
    }

    /// The current value's name, for the control's button.
    pub fn current_name(&self) -> String {
        match &self.kind {
            ControlKind::Select { current, choices } => choices
                .iter()
                .find(|c| &c.value == current)
                .map_or_else(|| current.clone(), |c| c.name.clone()),
            ControlKind::Toggle(true) => format!("{}: on", self.name),
            ControlKind::Toggle(false) => format!("{}: off", self.name),
        }
    }

    /// Whether `value` is one the control offers.
    pub fn accepts(&self, value: &ControlValue) -> bool {
        match (&self.kind, value) {
            (ControlKind::Select { choices, .. }, ControlValue::Choice(value)) => {
                choices.iter().any(|c| &c.value == value)
            }
            (ControlKind::Toggle(_), ControlValue::Toggle(_)) => true,
            _ => false,
        }
    }

    /// The value after the current one (cycling with a key).
    pub fn next(&self) -> Option<ControlValue> {
        match &self.kind {
            ControlKind::Select { current, choices } => {
                if choices.len() < 2 {
                    return None;
                }
                let at = choices.iter().position(|c| &c.value == current);
                let next = at.map_or(0, |at| (at + 1) % choices.len());
                Some(ControlValue::Choice(choices[next].value.clone()))
            }
            ControlKind::Toggle(on) => Some(ControlValue::Toggle(!on)),
        }
    }

    /// Sets the current value locally (after the agent accepted it).
    pub fn set(&mut self, value: &ControlValue) {
        match (&mut self.kind, value) {
            (ControlKind::Select { current, .. }, ControlValue::Choice(value)) => {
                value.clone_into(current);
            }
            (ControlKind::Toggle(on), ControlValue::Toggle(value)) => *on = *value,
            _ => {}
        }
    }
}

/// The first control of `category`, the one the UI shows prominently.
pub fn by_category(controls: &[Control], category: ControlCategory) -> Option<&Control> {
    controls.iter().find(|c| c.category == category)
}

fn category(category: Option<&SessionConfigOptionCategory>) -> ControlCategory {
    match category {
        Some(SessionConfigOptionCategory::Mode) => ControlCategory::Mode,
        Some(SessionConfigOptionCategory::Model) => ControlCategory::Model,
        Some(SessionConfigOptionCategory::ModelConfig) => ControlCategory::ModelConfig,
        Some(SessionConfigOptionCategory::ThoughtLevel) => ControlCategory::ThoughtLevel,
        _ => ControlCategory::Other,
    }
}

/// Controls from the agent's config options, in its order.
pub fn from_config_options(options: &[SessionConfigOption]) -> Vec<Control> {
    options
        .iter()
        .filter_map(|option| {
            let kind = match &option.kind {
                SessionConfigKind::Select(select) => {
                    let choices = match &select.options {
                        SessionConfigSelectOptions::Ungrouped(options) => options
                            .iter()
                            .map(|o| Choice {
                                value: o.value.0.to_string(),
                                name: o.name.clone(),
                                description: o.description.clone(),
                                group: None,
                            })
                            .collect(),
                        SessionConfigSelectOptions::Grouped(groups) => groups
                            .iter()
                            .flat_map(|g| {
                                g.options.iter().map(|o| Choice {
                                    value: o.value.0.to_string(),
                                    name: o.name.clone(),
                                    description: o.description.clone(),
                                    group: Some(g.name.clone()),
                                })
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    ControlKind::Select {
                        current: select.current_value.0.to_string(),
                        choices,
                    }
                }
                SessionConfigKind::Boolean(boolean) => ControlKind::Toggle(boolean.current_value),
                // A kind this Maze doesn't know: leave it out.
                _ => return None,
            };
            Some(Control {
                id: option.id.0.to_string(),
                name: option.name.clone(),
                description: option.description.clone(),
                category: category(option.category.as_ref()),
                kind,
                legacy_mode: false,
            })
        })
        .collect()
}

/// The mode control made from session modes.
pub fn from_modes(modes: &SessionModeState) -> Option<Control> {
    if modes.available_modes.is_empty() {
        return None;
    }
    Some(Control {
        id: MODE_CONTROL.to_owned(),
        name: "Mode".to_owned(),
        description: None,
        category: ControlCategory::Mode,
        kind: ControlKind::Select {
            current: modes.current_mode_id.0.to_string(),
            choices: modes
                .available_modes
                .iter()
                .map(|m| Choice {
                    value: m.id.0.to_string(),
                    name: m.name.clone(),
                    description: m.description.clone(),
                    group: None,
                })
                .collect(),
        },
        legacy_mode: true,
    })
}

/// A session's controls: its config options if it sent any, else its modes.
pub fn controls(
    config_options: Option<&[SessionConfigOption]>,
    modes: Option<&SessionModeState>,
) -> Vec<Control> {
    match config_options {
        Some(options) if !options.is_empty() => from_config_options(options),
        _ => modes.and_then(from_modes).into_iter().collect(),
    }
}

/// Choices per agent → control id → value.
pub type Choices = BTreeMap<String, BTreeMap<String, ControlValue>>;

/// The remembered choices (`<data_dir>/agents/preferences.json`).
#[derive(Debug, Clone)]
pub struct Preferences {
    path: PathBuf,
}

impl Preferences {
    /// Preferences in `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Choices {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// The remembered choices for `agent_id`.
    pub fn get(&self, agent_id: &str) -> BTreeMap<String, ControlValue> {
        self.load().remove(agent_id).unwrap_or_default()
    }

    /// Remembers `value` for control `id` of `agent_id`.
    pub fn remember(&self, agent_id: &str, id: &str, value: &ControlValue) -> std::io::Result<()> {
        let mut all = self.load();
        all.entry(agent_id.to_owned())
            .or_default()
            .insert(id.to_owned(), value.clone());
        let json = serde_json::to_vec_pretty(&all).map_err(std::io::Error::other)?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{
        SessionConfigSelectGroup, SessionConfigSelectOption, SessionMode,
    };
    use pretty_assertions::assert_eq;

    use super::*;

    fn claude_like() -> Vec<SessionConfigOption> {
        vec![
            SessionConfigOption::select(
                "mode",
                "Mode",
                "default",
                vec![
                    SessionConfigSelectOption::new("default", "Manual"),
                    SessionConfigSelectOption::new("plan", "Plan"),
                ],
            )
            .category(SessionConfigOptionCategory::Mode),
            SessionConfigOption::select(
                "model",
                "Model",
                "sonnet",
                vec![SessionConfigSelectGroup::new(
                    "claude",
                    "Claude",
                    vec![
                        SessionConfigSelectOption::new("sonnet", "Sonnet"),
                        SessionConfigSelectOption::new("opus", "Opus"),
                    ],
                )],
            )
            .category(SessionConfigOptionCategory::Model),
            SessionConfigOption::select(
                "effort",
                "Effort",
                "high",
                vec![
                    SessionConfigSelectOption::new("low", "Low"),
                    SessionConfigSelectOption::new("high", "High"),
                ],
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
            SessionConfigOption::boolean("fast", "Fast", false)
                .category(SessionConfigOptionCategory::ModelConfig),
        ]
    }

    #[test]
    fn config_options_win_over_modes() {
        let modes = SessionModeState::new("default", vec![SessionMode::new("default", "Default")]);
        let controls = controls(Some(&claude_like()), Some(&modes));
        assert_eq!(controls.len(), 4);
        assert!(controls.iter().all(|c| !c.legacy_mode));
        let model = by_category(&controls, ControlCategory::Model).unwrap();
        assert_eq!(model.current_name(), "Sonnet");
        let ControlKind::Select { choices, .. } = &model.kind else {
            panic!()
        };
        assert_eq!(choices[1].group.as_deref(), Some("Claude"));
        let effort = by_category(&controls, ControlCategory::ThoughtLevel).unwrap();
        assert_eq!(effort.id, "effort");
        assert_eq!(effort.next(), Some(ControlValue::Choice("low".into())));
        let fast = by_category(&controls, ControlCategory::ModelConfig).unwrap();
        assert_eq!(fast.next(), Some(ControlValue::Toggle(true)));
        assert!(fast.accepts(&ControlValue::Toggle(true)));
        assert!(!fast.accepts(&ControlValue::Choice("on".into())));
    }

    #[test]
    fn modes_become_a_mode_control() {
        let modes = SessionModeState::new(
            "default",
            vec![
                SessionMode::new("default", "Default"),
                SessionMode::new("yolo", "YOLO").description("Runs without asking".to_owned()),
            ],
        );
        let controls = controls(None, Some(&modes));
        assert_eq!(controls.len(), 1);
        let mut mode = controls[0].clone();
        assert!(mode.legacy_mode);
        assert_eq!(mode.id, MODE_CONTROL);
        assert!(mode.accepts(&ControlValue::Choice("yolo".into())));
        assert!(!mode.accepts(&ControlValue::Choice("plan".into())));
        mode.set(&ControlValue::Choice("yolo".into()));
        assert_eq!(mode.current_name(), "YOLO");
        assert_eq!(mode.next(), Some(ControlValue::Choice("default".into())));
        assert!(super::controls(None, None).is_empty());
    }

    #[test]
    fn preferences_are_remembered_per_agent() {
        let dir = tempfile::tempdir().unwrap();
        let prefs = Preferences::new(dir.path().join("agents/preferences.json"));
        assert!(prefs.get("claude-acp").is_empty());
        prefs
            .remember("claude-acp", "model", &ControlValue::Choice("opus".into()))
            .unwrap();
        prefs
            .remember("claude-acp", "fast", &ControlValue::Toggle(true))
            .unwrap();
        prefs
            .remember("codex-acp", "model", &ControlValue::Choice("gpt-5".into()))
            .unwrap();
        prefs
            .remember(
                "claude-acp",
                "model",
                &ControlValue::Choice("sonnet".into()),
            )
            .unwrap();
        assert_eq!(
            prefs.get("claude-acp"),
            BTreeMap::from([
                ("fast".to_owned(), ControlValue::Toggle(true)),
                ("model".to_owned(), ControlValue::Choice("sonnet".into())),
            ])
        );
        assert_eq!(prefs.get("codex-acp").len(), 1);
        // A broken file is ignored (and replaced on the next choice).
        std::fs::write(prefs.path(), "{").unwrap();
        assert!(prefs.get("claude-acp").is_empty());
    }
}
