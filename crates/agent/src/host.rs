//! What agent sessions need from the rest of Maze, as one trait so this crate doesn't
//! depend on `mazemail_core` (same layer). The `mazemail` binary implements
//! [`AgentHost`] over the core; [`MemoryHost`] serves tests.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use local_api::{AccountPolicy, MemoryTokenStore, StaticAccountPolicy, TokenStore};
use mazemail_types::{AccountId, PublicId as _, Sensitive, ThreadId};
use serde::{Deserialize, Serialize};

/// An agent conversation in the store (`agent_thread`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AgentThreadKey(pub i64);

/// An agent conversation as listed in the thread history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentThreadInfo {
    /// Its key.
    pub key: AgentThreadKey,
    /// The agent's id.
    pub agent_id: String,
    /// Title (the first prompt, shortened).
    pub title: Option<String>,
    /// The mail thread it belongs to.
    pub mail_thread: Option<ThreadId>,
    /// The agent's ACP session id, for `session/load`.
    pub acp_session_id: Option<String>,
    /// Last change (Unix seconds).
    pub updated: i64,
}

/// A mail thread exported for an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadExport {
    /// The thread's public id (`t_…`).
    pub public_id: String,
    /// The thread as Markdown (`mazemail show --format md`).
    pub markdown: Sensitive<String>,
}

/// The state of a proposed change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalItemStatus {
    /// Waiting for review.
    Pending,
    /// Accepted and applied.
    Applied,
    /// Rejected.
    Rejected,
    /// Accepted, but applying failed.
    Failed,
}

/// One proposed change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalItemView {
    /// Item id.
    pub id: i64,
    /// What it does ("Archive 3 messages").
    pub summary: String,
    /// The agent's reason.
    pub reason: Option<Sensitive<String>>,
    /// Its state.
    pub status: ProposalItemStatus,
}

/// A proposal with its items (CONCEPT §10.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalView {
    /// Proposal id.
    pub id: i64,
    /// Title.
    pub title: String,
    /// Who proposed it (the token's label).
    pub source: Option<String>,
    /// The items.
    pub items: Vec<ProposalItemView>,
}

impl ProposalView {
    /// Items still waiting for review.
    pub fn pending(&self) -> impl Iterator<Item = &ProposalItemView> {
        self.items
            .iter()
            .filter(|item| item.status == ProposalItemStatus::Pending)
    }
}

/// A host call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HostError(pub String);

impl HostError {
    /// Wraps any error.
    pub fn new(error: impl std::fmt::Display) -> Self {
        Self(error.to_string())
    }
}

/// Maze's side of agent sessions.
#[async_trait]
pub trait AgentHost: Send + Sync {
    /// Where session tokens are stored.
    fn token_store(&self) -> Arc<dyn TokenStore>;

    /// The per-account AI policy.
    fn account_policy(&self) -> Arc<dyn AccountPolicy>;

    /// The thread as Markdown, `None` when its account doesn't share mail with external
    /// agents (or it doesn't exist).
    async fn thread_export(&self, thread: ThreadId) -> Result<Option<ThreadExport>, HostError>;

    /// Starts a stored agent conversation.
    async fn create_agent_thread(
        &self,
        agent_id: &str,
        title: Option<&str>,
        mail_thread: Option<ThreadId>,
    ) -> Result<AgentThreadKey, HostError>;

    /// Updates title and ACP session id.
    async fn update_agent_thread(
        &self,
        key: AgentThreadKey,
        title: Option<&str>,
        acp_session_id: Option<&str>,
    ) -> Result<(), HostError>;

    /// Replaces the stored (encrypted) transcript.
    async fn save_transcript(&self, key: AgentThreadKey, json: Vec<u8>) -> Result<(), HostError>;

    /// The stored transcript.
    async fn load_transcript(&self, key: AgentThreadKey) -> Result<Option<Vec<u8>>, HostError>;

    /// Conversations, newest first; `Some(thread)` limits to one mail thread.
    async fn agent_threads(
        &self,
        mail_thread: Option<ThreadId>,
    ) -> Result<Vec<AgentThreadInfo>, HostError>;

    /// Proposals waiting for review, newest first.
    async fn pending_proposals(&self) -> Result<Vec<ProposalView>, HostError>;

    /// Accepts (applies) or rejects one item.
    async fn decide_proposal_item(&self, item: i64, accept: bool) -> Result<(), HostError>;

    /// Accepts or rejects every pending item of a proposal.
    async fn decide_proposal(&self, proposal: i64, accept: bool) -> Result<(), HostError>;
}

#[derive(Debug, Default)]
struct MemoryState {
    exports: BTreeMap<ThreadId, String>,
    threads: Vec<(AgentThreadInfo, Option<Vec<u8>>)>,
    proposals: Vec<ProposalView>,
    clock: i64,
}

/// An in-memory [`AgentHost`] for tests and demos.
#[derive(Debug)]
pub struct MemoryHost {
    tokens: Arc<MemoryTokenStore>,
    policy: Arc<StaticAccountPolicy>,
    state: Mutex<MemoryState>,
}

impl Default for MemoryHost {
    fn default() -> Self {
        Self {
            tokens: Arc::new(MemoryTokenStore::new()),
            policy: Arc::new(StaticAccountPolicy::default()),
            state: Mutex::default(),
        }
    }
}

impl MemoryHost {
    /// A host whose AI policy shares `accounts`.
    pub fn new(accounts: impl IntoIterator<Item = AccountId>) -> Self {
        Self {
            policy: Arc::new(StaticAccountPolicy::new(accounts)),
            ..Self::default()
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The token store (to inspect tokens in tests).
    pub fn tokens(&self) -> &Arc<MemoryTokenStore> {
        &self.tokens
    }

    /// Makes `thread` exportable with `markdown`.
    pub fn add_thread_export(&self, thread: ThreadId, markdown: &str) {
        self.state().exports.insert(thread, markdown.to_owned());
    }

    /// Adds a pending proposal.
    pub fn add_proposal(&self, proposal: ProposalView) {
        self.state().proposals.insert(0, proposal);
    }

    /// Every proposal, decided or not.
    pub fn proposals(&self) -> Vec<ProposalView> {
        self.state().proposals.clone()
    }

    /// A stored transcript, for assertions.
    pub fn transcript(&self, key: AgentThreadKey) -> Option<Vec<u8>> {
        self.state()
            .threads
            .iter()
            .find(|(info, _)| info.key == key)
            .and_then(|(_, transcript)| transcript.clone())
    }
}

#[async_trait]
impl AgentHost for MemoryHost {
    fn token_store(&self) -> Arc<dyn TokenStore> {
        self.tokens.clone()
    }

    fn account_policy(&self) -> Arc<dyn AccountPolicy> {
        self.policy.clone()
    }

    async fn thread_export(&self, thread: ThreadId) -> Result<Option<ThreadExport>, HostError> {
        Ok(self
            .state()
            .exports
            .get(&thread)
            .map(|markdown| ThreadExport {
                public_id: thread.to_public(),
                markdown: Sensitive::new(markdown.clone()),
            }))
    }

    async fn create_agent_thread(
        &self,
        agent_id: &str,
        title: Option<&str>,
        mail_thread: Option<ThreadId>,
    ) -> Result<AgentThreadKey, HostError> {
        let mut state = self.state();
        state.clock += 1;
        let key = AgentThreadKey(state.threads.len() as i64 + 1);
        let updated = state.clock;
        state.threads.push((
            AgentThreadInfo {
                key,
                agent_id: agent_id.to_owned(),
                title: title.map(str::to_owned),
                mail_thread,
                acp_session_id: None,
                updated,
            },
            None,
        ));
        Ok(key)
    }

    async fn update_agent_thread(
        &self,
        key: AgentThreadKey,
        title: Option<&str>,
        acp_session_id: Option<&str>,
    ) -> Result<(), HostError> {
        let mut state = self.state();
        state.clock += 1;
        let clock = state.clock;
        let (info, _) = state
            .threads
            .iter_mut()
            .find(|(info, _)| info.key == key)
            .ok_or_else(|| HostError("no such agent thread".to_owned()))?;
        info.title = title.map(str::to_owned);
        info.acp_session_id = acp_session_id.map(str::to_owned);
        info.updated = clock;
        Ok(())
    }

    async fn save_transcript(&self, key: AgentThreadKey, json: Vec<u8>) -> Result<(), HostError> {
        let mut state = self.state();
        let (_, transcript) = state
            .threads
            .iter_mut()
            .find(|(info, _)| info.key == key)
            .ok_or_else(|| HostError("no such agent thread".to_owned()))?;
        *transcript = Some(json);
        Ok(())
    }

    async fn load_transcript(&self, key: AgentThreadKey) -> Result<Option<Vec<u8>>, HostError> {
        Ok(self.transcript(key))
    }

    async fn agent_threads(
        &self,
        mail_thread: Option<ThreadId>,
    ) -> Result<Vec<AgentThreadInfo>, HostError> {
        let mut threads: Vec<AgentThreadInfo> = self
            .state()
            .threads
            .iter()
            .map(|(info, _)| info.clone())
            .filter(|info| mail_thread.is_none() || info.mail_thread == mail_thread)
            .collect();
        threads.sort_by_key(|info| std::cmp::Reverse(info.updated));
        Ok(threads)
    }

    async fn pending_proposals(&self) -> Result<Vec<ProposalView>, HostError> {
        Ok(self
            .state()
            .proposals
            .iter()
            .filter(|p| p.pending().next().is_some())
            .cloned()
            .collect())
    }

    async fn decide_proposal_item(&self, item: i64, accept: bool) -> Result<(), HostError> {
        let mut state = self.state();
        let found = state
            .proposals
            .iter_mut()
            .flat_map(|p| p.items.iter_mut())
            .find(|i| i.id == item)
            .ok_or_else(|| HostError("no such proposal item".to_owned()))?;
        found.status = if accept {
            ProposalItemStatus::Applied
        } else {
            ProposalItemStatus::Rejected
        };
        Ok(())
    }

    async fn decide_proposal(&self, proposal: i64, accept: bool) -> Result<(), HostError> {
        let mut state = self.state();
        let found = state
            .proposals
            .iter_mut()
            .find(|p| p.id == proposal)
            .ok_or_else(|| HostError("no such proposal".to_owned()))?;
        for item in &mut found.items {
            if item.status == ProposalItemStatus::Pending {
                item.status = if accept {
                    ProposalItemStatus::Applied
                } else {
                    ProposalItemStatus::Rejected
                };
            }
        }
        Ok(())
    }
}
