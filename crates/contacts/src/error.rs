//! What can go wrong talking to a contact server.

use crate::model::Person;

/// Errors of a [`ContactSource`](crate::ContactSource). The texts are for people;
/// none of them contain contact data (a [`Conflict`](Self::Conflict)'s person is only
/// in the payload, and its `Debug` is redacted), so every error is safe to log.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ContactError {
    /// The server can't be reached. Contacts can't be saved while offline (there is
    /// no queue, ADR-024); syncing tries again later.
    #[error(
        "Maze can't reach the contacts server right now ({message}); try again when you're online"
    )]
    Offline {
        /// The network error, without URL.
        message: String,
    },
    /// The account's sign-in doesn't cover contacts (the user hasn't connected them
    /// yet, or unticked the permission). Mail is not affected.
    #[error("contacts need permission: connect the account's contacts in Maze")]
    MissingScope,
    /// The Google Cloud project of the OAuth client doesn't have the People API
    /// enabled.
    #[error(
        "the People API isn't enabled in the Google Cloud project of your OAuth client: \
         enable it in the Google Cloud console (APIs & Services → Library → People API)"
    )]
    ApiDisabled,
    /// The account can't get tokens at all (no OAuth client configured). The text
    /// says what to set.
    #[error("{0}")]
    NotConfigured(String),
    /// The server rejected the sign-in even after a fresh token.
    #[error("the contacts server rejected the sign-in; sign in to the account again")]
    Unauthorized,
    /// Getting a token failed: the account has to sign in again, or the provider
    /// refused.
    #[error(transparent)]
    Auth(auth::AuthError),
    /// The contact changed on the server since it was read (etag mismatch). Carries
    /// the server's current version, to show instead.
    #[error("the contact was changed elsewhere; Maze shows the current version")]
    Conflict(Box<Person>),
    /// The contact no longer exists on the server.
    #[error("the contact no longer exists on the server")]
    NotFound,
    /// An incremental sync's token expired (Google: after 7 days); a full sync
    /// follows.
    #[error("the sync token expired")]
    SyncTokenExpired,
    /// The address book can't be changed (Google's "Other contacts", or the account
    /// hasn't connected its contacts).
    #[error("this address book is read-only")]
    ReadOnly,
    /// The contact has nothing to save (no name, address or phone number).
    #[error("enter a name, an email address or a phone number")]
    Empty,
    /// The server refused the request (HTTP 4xx). `message` is the server's text.
    #[error("the contacts server refused the request (HTTP {status}): {message}")]
    Rejected {
        /// The HTTP status.
        status: u16,
        /// The server's explanation.
        message: String,
    },
    /// The server kept failing or rate-limiting (HTTP 429 or 5xx) after retries.
    #[error("the contacts server answered HTTP {status}; Maze will try again later")]
    Server {
        /// The last HTTP status.
        status: u16,
    },
    /// The server's answer didn't make sense.
    #[error("invalid answer from the contacts server: {0}")]
    InvalidResponse(String),
}

impl ContactError {
    /// Whether trying again later without the user doing anything may work
    /// (offline, rate limits, server errors).
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Offline { .. } | Self::Server { .. } => true,
            Self::Auth(err) => err.is_transient(),
            _ => false,
        }
    }

    /// Whether the account has to grant contacts permission (or sign in again)
    /// before anything works.
    pub fn needs_consent(&self) -> bool {
        match self {
            Self::MissingScope | Self::Unauthorized => true,
            Self::Auth(err) => err.requires_reauth(),
            _ => false,
        }
    }

    pub(crate) fn network(err: &reqwest::Error) -> Self {
        let message = match std::error::Error::source(err) {
            Some(source) => format!("{}: {source}", without_url(err)),
            None => without_url(err),
        };
        Self::Offline { message }
    }

    /// A token error as a contact error.
    pub fn from_auth(err: auth::AuthError) -> Self {
        if err.missing_capability() == Some(auth::Capability::Contacts) {
            Self::MissingScope
        } else if let auth::AuthError::Network { message, .. } = &err {
            Self::Offline {
                message: message.clone(),
            }
        } else {
            Self::Auth(err)
        }
    }
}

fn without_url(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "timed out".to_owned()
    } else if err.is_connect() {
        "can't connect".to_owned()
    } else {
        "network error".to_owned()
    }
}
