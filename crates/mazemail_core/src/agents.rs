//! Agent conversations and the review of agent proposals (PLAN §4.12, CONCEPT §10.3).
//!
//! - **Conversations** (`agent_thread`): one per agent session, linked to a mail
//!   thread, with the agent's ACP session id for `session/load` and the transcript as
//!   an encrypted blob.
//! - **Proposals**: changes agents proposed under the Triage profile (`mazemail
//!   archive|move|tag|…`, `mazemail propose --file`). Accepting an item applies its
//!   command with [`Origin::Agent`](crate::Origin::Agent) without asking the authorizer
//!   again (the user's decision is the authorization); rejecting records the decision.
//!   A proposal is `applied`, `rejected` or `partial` once no item is pending.
//! - **Sign-in keys** (`agent_secret`): an API key or token per agent that the user
//!   pasted into the agent column, encrypted like everything in the store (ADR-017) and
//!   passed to the agent in one environment variable. Never in settings.json or logs.

use mazemail_types::{Sensitive, ThreadId};
use store::{
    AgentThread, AgentThreadId, Proposal, ProposalId, ProposalItemId, ProposalItemState,
    ProposalState, Queries as _,
};

use crate::api::proposal_summary;
use crate::commands::MailCommand;
use crate::handle::CoreHandle;
use crate::{Error, Result};

/// One proposed command for the review UI.
#[derive(Clone, PartialEq, Eq)]
pub struct ProposalItemSummary {
    /// Item id.
    pub id: ProposalItemId,
    /// What it does, without message content ("Archive 3 messages").
    pub summary: String,
    /// The agent's reason (may quote mail).
    pub reason: Option<String>,
    /// Its state.
    pub state: ProposalItemState,
    /// The command, if it can be read.
    pub command: Option<MailCommand>,
}

impl std::fmt::Debug for ProposalItemSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProposalItemSummary")
            .field("id", &self.id)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// A proposal for the review UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalSummary {
    /// Proposal id.
    pub id: ProposalId,
    /// Title.
    pub title: String,
    /// The proposing token's label ("Maze agent session: Claude Code").
    pub source: Option<String>,
    /// When it was made (Unix seconds).
    pub created: i64,
    /// Its state.
    pub state: ProposalState,
    /// The items.
    pub items: Vec<ProposalItemSummary>,
}

impl CoreHandle {
    // ---- conversations ------------------------------------------------------------

    /// Agent conversations, newest first; `Some(thread)` only those of one mail thread.
    pub fn agent_threads(&self, mail_thread: Option<ThreadId>) -> Result<Vec<AgentThread>> {
        let mut threads = self.store().reader()?.agent_threads()?;
        if let Some(thread) = mail_thread {
            threads.retain(|t| t.mail_thread == Some(thread));
        }
        Ok(threads)
    }

    /// Starts a conversation.
    pub async fn create_agent_thread(
        &self,
        agent_id: &str,
        title: Option<&str>,
        mail_thread: Option<ThreadId>,
    ) -> Result<AgentThreadId> {
        let agent_id = agent_id.to_owned();
        let title = title.map(str::to_owned);
        Ok(self
            .store()
            .write(move |w| w.create_agent_thread(&agent_id, title.as_deref(), mail_thread))
            .await?)
    }

    /// Updates a conversation's title and ACP session id.
    pub async fn update_agent_thread(
        &self,
        id: AgentThreadId,
        title: Option<&str>,
        acp_session_id: Option<&str>,
    ) -> Result<()> {
        let title = title.map(str::to_owned);
        let session = acp_session_id.map(str::to_owned);
        Ok(self
            .store()
            .write(move |w| w.update_agent_thread(id, title.as_deref(), session.as_deref()))
            .await?)
    }

    /// Stores a conversation's transcript (encrypted, compressed blob).
    pub async fn save_agent_transcript(
        &self,
        id: AgentThreadId,
        transcript: Vec<u8>,
    ) -> Result<()> {
        let store = self.store().clone();
        let staged = tokio::task::spawn_blocking(move || store.blobs().put(&transcript))
            .await
            .map_err(|_| Error::Task("storing a transcript panicked"))??;
        Ok(self
            .store()
            .write(move |w| w.set_agent_transcript(id, &staged))
            .await?)
    }

    /// A conversation's stored transcript.
    pub async fn agent_transcript(&self, id: AgentThreadId) -> Result<Option<Vec<u8>>> {
        let Some(thread) = self.store().reader()?.agent_thread(id)? else {
            return Ok(None);
        };
        let Some(blob) = thread.transcript else {
            return Ok(None);
        };
        let store = self.store().clone();
        let bytes = tokio::task::spawn_blocking(move || store.blobs().read(&blob))
            .await
            .map_err(|_| Error::Task("reading a transcript panicked"))??;
        Ok(Some(bytes))
    }

    /// Deletes a conversation.
    pub async fn delete_agent_thread(&self, id: AgentThreadId) -> Result<()> {
        Ok(self
            .store()
            .write(move |w| w.delete_agent_thread(id))
            .await?)
    }

    // ---- sign-in keys --------------------------------------------------------------

    /// An agent's sign-in key: the environment variable it is passed in, and the key.
    pub fn agent_secret(&self, agent_id: &str) -> Result<Option<(String, Sensitive<String>)>> {
        Ok(self
            .store()
            .reader()?
            .agent_secret(agent_id)?
            .map(|(env, secret)| (env, Sensitive::new(secret.as_str().to_owned()))))
    }

    /// `(agent id, environment variable)` of every stored sign-in key, no secrets.
    pub fn agent_secret_envs(&self) -> Result<Vec<(String, String)>> {
        Ok(self.store().reader()?.agent_secret_envs()?)
    }

    /// Stores an agent's sign-in key (replacing an earlier one).
    pub async fn set_agent_secret(
        &self,
        agent_id: &str,
        env: &str,
        secret: Sensitive<String>,
    ) -> Result<()> {
        let agent_id = agent_id.to_owned();
        let env = env.to_owned();
        let secret = zeroize::Zeroizing::new(secret.into_inner());
        Ok(self
            .store()
            .write(move |w| w.set_agent_secret(&agent_id, &env, &secret))
            .await?)
    }

    /// Deletes an agent's sign-in key ("Remove key"); fine if there is none.
    pub async fn delete_agent_secret(&self, agent_id: &str) -> Result<()> {
        let agent_id = agent_id.to_owned();
        Ok(self
            .store()
            .write(move |w| w.delete_agent_secret(&agent_id))
            .await?)
    }

    // ---- proposals ----------------------------------------------------------------

    /// Proposals with pending items, newest first.
    pub fn pending_proposals(&self) -> Result<Vec<ProposalSummary>> {
        let reader = self.store().reader()?;
        let proposals = reader.proposals(Some(ProposalState::Pending))?;
        proposals
            .into_iter()
            .map(|proposal| {
                let source = match proposal.token {
                    Some(token) => reader.api_token(token)?.and_then(|t| t.label),
                    None => None,
                };
                Ok(summarize(proposal, source))
            })
            .collect()
    }

    /// Accepts (applies) or rejects one pending item.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if there is no such pending item. A command that fails to
    /// apply marks the item `failed` and returns the error.
    pub async fn decide_proposal_item(&self, item: ProposalItemId, accept: bool) -> Result<()> {
        let proposal = self
            .store()
            .reader()?
            .proposals(Some(ProposalState::Pending))?
            .into_iter()
            .find(|p| p.items.iter().any(|i| i.id == item))
            .ok_or_else(|| Error::not_found("proposal item", item.0))?;
        self.decide(&proposal, &[item], accept).await
    }

    /// Accepts or rejects every pending item of a proposal.
    pub async fn decide_proposal(&self, id: ProposalId, accept: bool) -> Result<()> {
        let proposal = self
            .store()
            .reader()?
            .proposal(id)?
            .ok_or_else(|| Error::not_found("proposal", id.0))?;
        let items: Vec<ProposalItemId> = proposal
            .items
            .iter()
            .filter(|i| i.state == ProposalItemState::Pending)
            .map(|i| i.id)
            .collect();
        self.decide(&proposal, &items, accept).await
    }

    async fn decide(
        &self,
        proposal: &Proposal,
        items: &[ProposalItemId],
        accept: bool,
    ) -> Result<()> {
        let mut first_error = None;
        for item in proposal.items.iter().filter(|i| items.contains(&i.id)) {
            if item.state != ProposalItemState::Pending {
                continue;
            }
            let state = if accept {
                let applied = match serde_json::from_str::<MailCommand>(&item.command_json) {
                    Ok(command) => self
                        .apply_accepted(command, proposal.token)
                        .await
                        .map(|_| ()),
                    Err(_) => Err(Error::InvalidCommand("the proposed command can't be read")),
                };
                match applied {
                    Ok(()) => ProposalItemState::Applied,
                    Err(error) => {
                        tracing::warn!(%error, "an accepted proposal item failed");
                        first_error.get_or_insert(error);
                        ProposalItemState::Failed
                    }
                }
            } else {
                ProposalItemState::Rejected
            };
            let id = item.id;
            self.store()
                .write(move |w| w.set_proposal_item_state(id, state))
                .await?;
        }
        self.settle(proposal.id).await?;
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Records the proposal's overall state once no item is pending.
    async fn settle(&self, id: ProposalId) -> Result<()> {
        let Some(proposal) = self.store().reader()?.proposal(id)? else {
            return Ok(());
        };
        let states: Vec<ProposalItemState> = proposal.items.iter().map(|i| i.state).collect();
        if states.contains(&ProposalItemState::Pending) {
            return Ok(());
        }
        let accepted = states
            .iter()
            .filter(|s| matches!(s, ProposalItemState::Applied | ProposalItemState::Approved))
            .count();
        let state = if accepted == states.len() {
            ProposalState::Applied
        } else if accepted == 0 && states.iter().all(|s| *s == ProposalItemState::Rejected) {
            ProposalState::Rejected
        } else {
            ProposalState::Partial
        };
        let decision = if state == ProposalState::Rejected {
            "proposal_rejected"
        } else {
            "proposal_accepted"
        };
        let detail = serde_json::json!({ "proposal": id.0, "state": state.as_str() }).to_string();
        Ok(self
            .store()
            .write(move |w| {
                w.set_proposal_state(id, state)?;
                w.audit("user", decision, Some(&detail))?;
                Ok(())
            })
            .await?)
    }
}

fn summarize(proposal: Proposal, source: Option<String>) -> ProposalSummary {
    ProposalSummary {
        id: proposal.id,
        title: proposal.title,
        source,
        created: proposal.created,
        state: proposal.state,
        items: proposal
            .items
            .into_iter()
            .map(|item| {
                let command = serde_json::from_str::<MailCommand>(&item.command_json).ok();
                ProposalItemSummary {
                    id: item.id,
                    summary: command
                        .as_ref()
                        .map_or_else(|| "Unknown change".to_owned(), proposal_summary),
                    reason: item.reason,
                    state: item.state,
                    command,
                }
            })
            .collect(),
    }
}
