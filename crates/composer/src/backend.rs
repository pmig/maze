//! What the composer needs from the rest of Maze.

use std::future::Future;
use std::pin::Pin;

use mazemail_types::compose::Draft;
use mazemail_types::{Address, DraftId, IdentityId};

/// A boxed, sendable future (the backend runs its work on the core's runtime).
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// An identity offered in the From picker.
#[derive(Clone, PartialEq, Eq)]
pub struct IdentityChoice {
    /// The identity.
    pub id: IdentityId,
    /// The display name (`Philip Miglinci`), if set.
    pub name: Option<String>,
    /// The address.
    pub email: String,
    /// The account's name, to tell identities of several accounts apart.
    pub account: String,
}

impl std::fmt::Debug for IdentityChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdentityChoice")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl IdentityChoice {
    /// `Philip <philip@example.org>`, or the bare address.
    pub fn from_line(&self) -> String {
        Address::new(self.name.clone(), self.email.clone()).to_string()
    }
}

/// What happened to a sent message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// The server accepted it.
    Sent,
    /// It waits in the outbox and goes out when the server can be reached again.
    Queued {
        /// Why it couldn't be sent right now (for a notification).
        reason: String,
    },
}

/// A failure the composer shows to the user (sending refused, saving failed, …).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ComposeError {
    /// What went wrong and what to do, for people (no message content).
    pub message: String,
}

impl ComposeError {
    /// An error with this text.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// The composer's view of Maze. Implemented by the `mazemail` binary over the core.
pub trait ComposeBackend: Send + Sync + 'static {
    /// The identities to send from, the default ones first.
    fn identities(&self) -> Vec<IdentityChoice>;

    /// Saves the draft (a new one for `id: None`) and returns its id.
    fn save_draft(
        &self,
        id: Option<DraftId>,
        draft: Draft,
    ) -> BoxFuture<Result<DraftId, ComposeError>>;

    /// Deletes a saved draft (Discard).
    fn delete_draft(&self, id: DraftId) -> BoxFuture<Result<(), ComposeError>>;

    /// Sends the draft; a saved draft (`id`) is deleted once the message is on its way.
    fn send(
        &self,
        id: Option<DraftId>,
        draft: Draft,
    ) -> BoxFuture<Result<SendOutcome, ComposeError>>;

    /// Address completion for `prefix` (known correspondents). Optional.
    fn complete_address(&self, prefix: &str) -> Vec<Address> {
        let _ = prefix;
        Vec::new()
    }

    /// The footer (Markdown) added below messages sent from `identity`
    /// (`compose.footer`, per account), shown under the body as a preview. Empty: none.
    fn footer(&self, identity: Option<IdentityId>) -> String {
        let _ = identity;
        String::new()
    }

    /// Whether messages sent from `identity` get the "Sent with Maze" line
    /// (`compose.support_maze`, per account). Only a setting: the composer just previews
    /// it.
    fn sent_with_maze(&self, identity: Option<IdentityId>) -> bool {
        let _ = identity;
        true
    }
}
