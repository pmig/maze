//! The conversation of one agent session as Maze shows and stores it.
//!
//! The session runner turns ACP traffic into [`EntryUpdate`]s; the agent column applies
//! them to its own [`Transcript`] and the runner keeps one too, which is stored
//! (encrypted, in the `agent_thread` blob) after every turn. Transcripts hold mail
//! content: never log them.

use serde::{Deserialize, Serialize};

/// What the user decided for a permission prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionChoice {
    /// Allow this one call.
    AllowOnce,
    /// Allow for the rest of the session.
    AllowSession,
    /// Refuse.
    Deny,
}

impl PermissionChoice {
    /// Button label.
    pub fn label(self) -> &'static str {
        match self {
            Self::AllowOnce => "Allow once",
            Self::AllowSession => "Allow for this session",
            Self::Deny => "Deny",
        }
    }
}

/// What a permission prompt is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    /// One of the agent's own tools (`session/request_permission`).
    Tool,
    /// A terminal command that isn't `mazemail …`.
    Command,
}

/// A permission prompt, inline in the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionEntry {
    /// Prompt id within the session.
    pub id: u64,
    /// What is asked ("Run `git status`", the tool call's title).
    pub title: String,
    /// Tool or command.
    pub kind: PermissionKind,
    /// The choices offered.
    pub choices: Vec<PermissionChoice>,
    /// The decision, once made.
    pub decision: Option<PermissionChoice>,
}

/// A tool call's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Not started (waiting for input or permission).
    Pending,
    /// Running.
    Running,
    /// Done.
    Completed,
    /// Failed.
    Failed,
}

/// One tool call of the agent, shown collapsed with its status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallEntry {
    /// The agent's id for it.
    pub id: String,
    /// Title ("Read thread.md").
    pub title: String,
    /// Kind (`read`, `edit`, `execute`, …).
    pub kind: String,
    /// State.
    pub status: ToolStatus,
    /// Text content, diffs and input, for the expanded view.
    pub detail: String,
}

/// A plan step's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Not started.
    Pending,
    /// Being worked on.
    InProgress,
    /// Done.
    Completed,
}

/// One step of the agent's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    /// What the step is.
    pub content: String,
    /// Its state.
    pub status: PlanStatus,
}

/// A terminal command's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TerminalStatus {
    /// Waiting for the user's approval.
    AwaitingApproval,
    /// Running.
    Running,
    /// Finished.
    Exited {
        /// Exit code.
        code: Option<u32>,
        /// Signal that ended it.
        signal: Option<String>,
    },
    /// The user refused it.
    Denied,
    /// Couldn't start.
    Failed {
        /// Why.
        message: String,
    },
}

/// A command Maze ran for the agent (ACP terminals), shown inline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalEntry {
    /// Terminal id.
    pub id: String,
    /// The command line.
    pub command: String,
    /// Whether it ran without asking (`mazemail …`).
    pub automatic: bool,
    /// State.
    pub status: TerminalStatus,
    /// The end of its output.
    pub output: String,
}

/// How serious a notice is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    /// Information.
    Info,
    /// Something went wrong.
    Error,
}

/// One entry of the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    /// The user's prompt.
    User {
        /// Text.
        text: String,
    },
    /// The agent's answer (Markdown).
    Agent {
        /// Text.
        text: String,
    },
    /// The agent's reasoning.
    Thought {
        /// Text.
        text: String,
    },
    /// A tool call.
    ToolCall(ToolCallEntry),
    /// The agent's plan.
    Plan {
        /// Steps.
        items: Vec<PlanItem>,
    },
    /// A terminal command.
    Terminal(TerminalEntry),
    /// A permission prompt.
    Permission(PermissionEntry),
    /// A note from Maze.
    Notice {
        /// Text.
        text: String,
        /// Level.
        level: NoticeLevel,
    },
}

/// A change to the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryUpdate {
    /// The user sent a prompt.
    UserMessage(String),
    /// More of the agent's answer.
    AgentChunk(String),
    /// More of the agent's reasoning.
    ThoughtChunk(String),
    /// A new tool call (or a full replacement).
    ToolCall(ToolCallEntry),
    /// Changes to a tool call.
    ToolCallUpdate {
        /// Its id.
        id: String,
        /// New title.
        title: Option<String>,
        /// New state.
        status: Option<ToolStatus>,
        /// New detail.
        detail: Option<String>,
    },
    /// The plan (replaces the previous one).
    Plan(Vec<PlanItem>),
    /// A terminal appeared or changed (matched by id).
    Terminal(TerminalEntry),
    /// A permission prompt appeared.
    Permission(PermissionEntry),
    /// A permission prompt was answered.
    PermissionDecided {
        /// Prompt id.
        id: u64,
        /// The decision.
        choice: PermissionChoice,
    },
    /// A note from Maze.
    Notice(String, NoticeLevel),
}

/// The whole conversation.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcript {
    /// Entries, oldest first.
    pub entries: Vec<Entry>,
}

impl std::fmt::Debug for Transcript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Mail content: only the size.
        f.debug_struct("Transcript")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl Transcript {
    /// Applies one update.
    pub fn apply(&mut self, update: &EntryUpdate) {
        match update {
            EntryUpdate::UserMessage(text) => self.entries.push(Entry::User { text: text.clone() }),
            EntryUpdate::AgentChunk(chunk) => match self.entries.last_mut() {
                Some(Entry::Agent { text }) => text.push_str(chunk),
                _ => self.entries.push(Entry::Agent {
                    text: chunk.clone(),
                }),
            },
            EntryUpdate::ThoughtChunk(chunk) => match self.entries.last_mut() {
                Some(Entry::Thought { text }) => text.push_str(chunk),
                _ => self.entries.push(Entry::Thought {
                    text: chunk.clone(),
                }),
            },
            EntryUpdate::ToolCall(call) => match self.tool_call_mut(&call.id) {
                Some(existing) => *existing = call.clone(),
                None => self.entries.push(Entry::ToolCall(call.clone())),
            },
            EntryUpdate::ToolCallUpdate {
                id,
                title,
                status,
                detail,
            } => {
                if let Some(call) = self.tool_call_mut(id) {
                    if let Some(title) = title {
                        call.title.clone_from(title);
                    }
                    if let Some(status) = status {
                        call.status = *status;
                    }
                    if let Some(detail) = detail {
                        call.detail.clone_from(detail);
                    }
                }
            }
            EntryUpdate::Plan(items) => {
                // One plan per conversation, updated in place.
                match self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|e| matches!(e, Entry::Plan { .. }))
                {
                    Some(Entry::Plan { items: existing }) => existing.clone_from(items),
                    _ => self.entries.push(Entry::Plan {
                        items: items.clone(),
                    }),
                }
            }
            EntryUpdate::Terminal(terminal) => {
                let existing = self.entries.iter_mut().find_map(|e| match e {
                    Entry::Terminal(t) if t.id == terminal.id => Some(t),
                    _ => None,
                });
                match existing {
                    Some(existing) => *existing = terminal.clone(),
                    None => self.entries.push(Entry::Terminal(terminal.clone())),
                }
            }
            EntryUpdate::Permission(prompt) => self.entries.push(Entry::Permission(prompt.clone())),
            EntryUpdate::PermissionDecided { id, choice } => {
                if let Some(prompt) = self.permission_mut(*id) {
                    prompt.decision = Some(*choice);
                }
            }
            EntryUpdate::Notice(text, level) => self.entries.push(Entry::Notice {
                text: text.clone(),
                level: *level,
            }),
        }
    }

    fn tool_call_mut(&mut self, id: &str) -> Option<&mut ToolCallEntry> {
        self.entries.iter_mut().find_map(|e| match e {
            Entry::ToolCall(call) if call.id == id => Some(call),
            _ => None,
        })
    }

    fn permission_mut(&mut self, id: u64) -> Option<&mut PermissionEntry> {
        self.entries.iter_mut().find_map(|e| match e {
            Entry::Permission(p) if p.id == id => Some(p),
            _ => None,
        })
    }

    /// Permission prompts still waiting for an answer.
    pub fn pending_permissions(&self) -> impl Iterator<Item = &PermissionEntry> {
        self.entries.iter().filter_map(|e| match e {
            Entry::Permission(p) if p.decision.is_none() => Some(p),
            _ => None,
        })
    }

    /// The first user prompt, shortened, as the conversation's title.
    pub fn title(&self) -> Option<String> {
        self.entries.iter().find_map(|e| match e {
            Entry::User { text } => {
                let line = text.lines().next().unwrap_or_default().trim();
                let mut title: String = line.chars().take(60).collect();
                if line.chars().count() > 60 {
                    title.push('…');
                }
                (!title.is_empty()).then_some(title)
            }
            _ => None,
        })
    }

    /// Serialized for storage.
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Reads a stored transcript; a damaged one becomes empty.
    pub fn from_json(bytes: &[u8]) -> Self {
        serde_json::from_slice(bytes).unwrap_or_else(|error| {
            tracing::warn!(%error, "an agent transcript can't be read");
            Self::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_merge_and_tool_calls_update_in_place() {
        let mut t = Transcript::default();
        t.apply(&EntryUpdate::UserMessage("Summarize this thread".into()));
        t.apply(&EntryUpdate::ThoughtChunk("Let me ".into()));
        t.apply(&EntryUpdate::ThoughtChunk("read it.".into()));
        t.apply(&EntryUpdate::ToolCall(ToolCallEntry {
            id: "c1".into(),
            title: "Read thread.md".into(),
            kind: "read".into(),
            status: ToolStatus::Pending,
            detail: String::new(),
        }));
        t.apply(&EntryUpdate::ToolCallUpdate {
            id: "c1".into(),
            title: None,
            status: Some(ToolStatus::Completed),
            detail: None,
        });
        t.apply(&EntryUpdate::AgentChunk("Anna ".into()));
        t.apply(&EntryUpdate::AgentChunk("asks for **Friday**.".into()));
        t.apply(&EntryUpdate::Plan(vec![PlanItem {
            content: "a".into(),
            status: PlanStatus::Pending,
        }]));
        t.apply(&EntryUpdate::Plan(vec![PlanItem {
            content: "a".into(),
            status: PlanStatus::Completed,
        }]));

        assert_eq!(t.entries.len(), 5);
        assert_eq!(
            t.entries[1],
            Entry::Thought {
                text: "Let me read it.".into()
            }
        );
        let Entry::ToolCall(call) = &t.entries[2] else {
            panic!("expected a tool call")
        };
        assert_eq!(call.status, ToolStatus::Completed);
        assert_eq!(
            t.entries[3],
            Entry::Agent {
                text: "Anna asks for **Friday**.".into()
            }
        );
        let Entry::Plan { items } = &t.entries[4] else {
            panic!("expected the plan")
        };
        assert_eq!(items[0].status, PlanStatus::Completed);
        assert_eq!(t.title().as_deref(), Some("Summarize this thread"));

        let round_trip = Transcript::from_json(&t.to_json());
        assert_eq!(round_trip, t);
        assert_eq!(format!("{t:?}"), "Transcript { entries: 5 }");
    }

    #[test]
    fn permissions_are_answered_in_place() {
        let mut t = Transcript::default();
        t.apply(&EntryUpdate::Permission(PermissionEntry {
            id: 1,
            title: "Run `git status`".into(),
            kind: PermissionKind::Command,
            choices: vec![
                PermissionChoice::AllowOnce,
                PermissionChoice::AllowSession,
                PermissionChoice::Deny,
            ],
            decision: None,
        }));
        assert_eq!(t.pending_permissions().count(), 1);
        t.apply(&EntryUpdate::PermissionDecided {
            id: 1,
            choice: PermissionChoice::Deny,
        });
        assert_eq!(t.pending_permissions().count(), 0);
    }
}
