//! [`CoreAgentHost`]: the `agent` crate's [`AgentHost`] over the core: conversations
//! and transcripts in the encrypted store, thread exports through the local API's
//! `show`, session tokens in the `api_token` table (sharing
//! the token authorizer's cache, so agent commands from a session are authorized), and
//! the proposal review.

use std::collections::BTreeMap;
use std::sync::Arc;

use agent::{
    AgentConversationInfo, AgentConversationKey, AgentHost, AgentKey, HostError,
    ProposalItemStatus, ProposalItemView, ProposalView, ThreadExport,
};
use async_trait::async_trait;
use local_api::TokenStore;
use mazemail_core::api::{self, StoreTokenStore, TokenAuthorizer};
use mazemail_core::{
    AgentConversationId, CoreHandle, ProposalId, ProposalItemId, ProposalItemState,
};
use mazemail_types::{PublicId as _, Sensitive, ThreadId};

/// The agent host of the running app.
#[derive(Clone)]
pub struct CoreAgentHost {
    core: CoreHandle,
    tokens: Arc<StoreTokenStore>,
}

impl std::fmt::Debug for CoreAgentHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreAgentHost").finish_non_exhaustive()
    }
}

impl CoreAgentHost {
    /// The host over `core`; `authorizer` must be the one the core was opened with.
    pub fn new(core: CoreHandle, authorizer: &TokenAuthorizer) -> Self {
        Self {
            tokens: Arc::new(StoreTokenStore::new(
                core.clone(),
                Arc::clone(authorizer.cache()),
            )),
            core,
        }
    }
}

fn host_error(error: impl std::fmt::Display) -> HostError {
    HostError::new(error)
}

fn item_status(state: ProposalItemState) -> ProposalItemStatus {
    match state {
        ProposalItemState::Pending => ProposalItemStatus::Pending,
        ProposalItemState::Approved | ProposalItemState::Applied => ProposalItemStatus::Applied,
        ProposalItemState::Rejected => ProposalItemStatus::Rejected,
        ProposalItemState::Failed => ProposalItemStatus::Failed,
    }
}

#[async_trait]
impl AgentHost for CoreAgentHost {
    fn token_store(&self) -> Arc<dyn TokenStore> {
        self.tokens.clone()
    }

    async fn thread_export(&self, thread: ThreadId) -> Result<Option<ThreadExport>, HostError> {
        let markdown = api::thread_markdown_for_agents(&self.core, thread)
            .await
            .map_err(host_error)?;
        Ok(markdown.map(|markdown| ThreadExport {
            public_id: thread.to_public(),
            markdown: Sensitive::new(markdown),
        }))
    }

    async fn create_agent_conversation(
        &self,
        agent_id: &str,
        title: Option<&str>,
        mail_thread: Option<ThreadId>,
    ) -> Result<AgentConversationKey, HostError> {
        self.core
            .create_agent_conversation(agent_id, title, mail_thread)
            .await
            .map(|id| AgentConversationKey(id.0))
            .map_err(host_error)
    }

    async fn update_agent_conversation(
        &self,
        key: AgentConversationKey,
        title: Option<&str>,
        acp_session_id: Option<&str>,
    ) -> Result<(), HostError> {
        self.core
            .update_agent_conversation(AgentConversationId(key.0), title, acp_session_id)
            .await
            .map_err(host_error)
    }

    async fn save_transcript(
        &self,
        key: AgentConversationKey,
        json: Vec<u8>,
    ) -> Result<(), HostError> {
        self.core
            .save_agent_transcript(AgentConversationId(key.0), json)
            .await
            .map_err(host_error)
    }

    async fn load_transcript(
        &self,
        key: AgentConversationKey,
    ) -> Result<Option<Vec<u8>>, HostError> {
        self.core
            .agent_transcript(AgentConversationId(key.0))
            .await
            .map_err(host_error)
    }

    async fn agent_conversations(
        &self,
        mail_thread: Option<ThreadId>,
    ) -> Result<Vec<AgentConversationInfo>, HostError> {
        let conversations = self
            .core
            .agent_conversations(mail_thread)
            .map_err(host_error)?;
        Ok(conversations
            .into_iter()
            .map(|conversation| AgentConversationInfo {
                key: AgentConversationKey(conversation.id.0),
                agent_id: conversation.agent_id,
                title: conversation.title,
                mail_thread: conversation.mail_thread,
                acp_session_id: conversation.acp_session_id,
                updated_at: conversation.updated_at,
            })
            .collect())
    }

    async fn pending_proposals(&self) -> Result<Vec<ProposalView>, HostError> {
        let proposals = self.core.pending_proposals().map_err(host_error)?;
        Ok(proposals
            .into_iter()
            .map(|proposal| ProposalView {
                id: proposal.id.0,
                title: proposal.title,
                source: proposal.source,
                items: proposal
                    .items
                    .into_iter()
                    .map(|item| ProposalItemView {
                        id: item.id.0,
                        summary: item.summary,
                        reason: item.reason.map(Sensitive::new),
                        status: item_status(item.state),
                    })
                    .collect(),
            })
            .collect())
    }

    async fn decide_proposal_item(&self, item: i64, accept: bool) -> Result<(), HostError> {
        self.core
            .decide_proposal_item(ProposalItemId(item), accept)
            .await
            .map_err(host_error)
    }

    async fn decide_proposal(&self, proposal: i64, accept: bool) -> Result<(), HostError> {
        self.core
            .decide_proposal(ProposalId(proposal), accept)
            .await
            .map_err(host_error)
    }

    async fn agent_key(&self, agent_id: &str) -> Result<Option<AgentKey>, HostError> {
        let key = self.core.agent_secret(agent_id).map_err(host_error)?;
        Ok(key.map(|(env, key)| AgentKey { env, key }))
    }

    async fn set_agent_key(&self, agent_id: &str, key: AgentKey) -> Result<(), HostError> {
        self.core
            .set_agent_secret(agent_id, &key.env, key.key)
            .await
            .map_err(host_error)
    }

    async fn delete_agent_key(&self, agent_id: &str) -> Result<(), HostError> {
        self.core
            .delete_agent_secret(agent_id)
            .await
            .map_err(host_error)
    }

    async fn agent_key_envs(&self) -> Result<BTreeMap<String, String>, HostError> {
        Ok(self
            .core
            .agent_secret_envs()
            .map_err(host_error)?
            .into_iter()
            .collect())
    }
}
