//! The error type of the sync engine.

use mazemail_types::{FolderId, MessageId};
use thiserror::Error;

use crate::connect::CredentialError;
use crate::ingest::IngestError;

/// Result alias for this crate.
pub type Result<T, E = SyncError> = std::result::Result<T, E>;

/// Everything that can go wrong while syncing an account.
///
/// No variant carries message content, addresses or subjects, so every error is safe
/// to log and to show in the activity log.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SyncError {
    /// The IMAP layer failed (network, server `NO`/`BAD`, protocol).
    #[error(transparent)]
    Imap(#[from] imap::ImapError),

    /// The local store failed.
    #[error("store: {0}")]
    Store(#[from] store::Error),

    /// The ingest pipeline rejected a message or body.
    #[error("ingest: {0}")]
    Ingest(#[from] IngestError),

    /// No usable credentials (the user has to sign in again).
    #[error(transparent)]
    Credentials(#[from] CredentialError),

    /// A pending op's payload can't be replayed as it is.
    #[error("invalid pending op: {0}")]
    InvalidOp(String),

    /// The folder doesn't exist locally (anymore) or has no server name.
    #[error("folder {0} is not synced with the server")]
    FolderNotSynced(FolderId),

    /// The message doesn't exist locally (anymore).
    #[error("message {0} not found")]
    MessageNotFound(MessageId),

    /// The message has no copy on the server that a body could be fetched from.
    #[error("message {0} has no copy on the server")]
    NoServerCopy(MessageId),

    /// The account is offline; the request needs the server.
    #[error("the account is offline")]
    Offline,

    /// The sync engine of this account has stopped.
    #[error("the sync engine stopped")]
    Stopped,

    /// A background task failed unexpectedly (a panic or a cancelled blocking task).
    #[error("internal error: {0}")]
    Internal(String),
}

impl SyncError {
    /// Whether the failure is about the connection rather than the request: retrying
    /// on a fresh connection may succeed, and the session that produced it is gone.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Imap(err) => err.is_transient() || !err.is_recoverable(),
            Self::Offline => true,
            _ => false,
        }
    }

    /// Whether the server rejected the credentials, or there are none: the user has
    /// to sign in again, retrying on a timer won't help.
    pub fn is_auth(&self) -> bool {
        match self {
            Self::Imap(imap::ImapError::Auth(_)) => true,
            Self::Credentials(err) => err.needs_user,
            _ => false,
        }
    }

    /// Whether the IMAP session that produced this error is unusable afterwards.
    pub(crate) fn breaks_session(&self) -> bool {
        matches!(self, Self::Imap(err) if !err.is_recoverable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        let lost = SyncError::Imap(imap::ImapError::ConnectionLost(None));
        assert!(lost.is_transient());
        assert!(lost.breaks_session());
        let no = SyncError::Imap(imap::ImapError::No {
            command: "UID MOVE",
            code: None,
            text: "nope".into(),
        });
        assert!(!no.is_transient());
        assert!(!no.breaks_session());
        let auth = SyncError::Imap(imap::ImapError::Auth("bad password".into()));
        assert!(auth.is_auth());
        assert!(SyncError::Credentials(CredentialError::needs_sign_in("expired")).is_auth());
        assert!(!SyncError::Credentials(CredentialError::temporary("keychain locked")).is_auth());
    }
}
