//! Opening IMAP sessions: the [`SessionFactory`] and [`CredentialProvider`] traits and
//! the real factory on top of [`imap::ImapClient`].

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use imap::{Caps, ImapAuth, ImapClient, ImapConfig, ImapSession, SelectInfo};
use thiserror::Error;

use crate::error::Result;

/// A boxed IMAP session, as the sync engine holds them.
pub type BoxSession = Box<dyn ImapSession>;

/// Opens authenticated IMAP sessions for one account.
///
/// The account actor calls [`connect`](SessionFactory::connect) for its IDLE session
/// and for each worker session, again after every connection loss (with backoff).
/// The real implementation is [`ImapSessionFactory`]; tests hand out
/// `imap::fake::FakeImap` sessions.
#[async_trait]
pub trait SessionFactory: Send + Sync + 'static {
    /// Opens a new authenticated session.
    ///
    /// # Errors
    ///
    /// Network errors are retried with backoff; [`SyncError::is_auth`] errors stop
    /// the retries until the credentials change.
    async fn connect(&self) -> Result<BoxSession>;
}

/// Hands out the current credentials of an account (implemented by `auth`).
///
/// For OAuth2 accounts the provider refreshes the access token as needed before
/// returning it.
#[async_trait]
pub trait CredentialProvider: Send + Sync + 'static {
    /// The credentials to log in with: a password or an XOAUTH2 access token.
    ///
    /// # Errors
    ///
    /// [`CredentialError`] with `needs_user` set when the user must sign in again
    /// (no password stored, refresh token revoked); otherwise the attempt is retried
    /// later.
    async fn credentials(&self) -> Result<ImapAuth, CredentialError>;

    /// The server rejected these credentials. An OAuth2 provider drops the cached
    /// access token so the next [`credentials`](CredentialProvider::credentials) call
    /// refreshes it. The default does nothing.
    async fn rejected(&self, credentials: &ImapAuth) {
        let _ = credentials;
    }
}

/// Credentials could not be provided.
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct CredentialError {
    /// Human-readable reason (no secrets).
    pub message: String,
    /// The user must act (sign in again); retrying on a timer won't help.
    pub needs_user: bool,
}

impl CredentialError {
    /// The user must sign in again.
    pub fn needs_sign_in(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            needs_user: true,
        }
    }

    /// A temporary failure (keychain locked, token endpoint unreachable).
    pub fn temporary(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            needs_user: false,
        }
    }
}

/// The production [`SessionFactory`]: [`ImapClient::connect`] with credentials from a
/// [`CredentialProvider`].
///
/// When the server rejects the credentials, the provider is told
/// ([`CredentialProvider::rejected`]) and the login is retried once with fresh
/// credentials, which covers an OAuth2 access token that expired early.
pub struct ImapSessionFactory {
    config: ImapConfig,
    credentials: Arc<dyn CredentialProvider>,
}

impl fmt::Debug for ImapSessionFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImapSessionFactory")
            .field("port", &self.config.port)
            .field("security", &self.config.security)
            .finish_non_exhaustive()
    }
}

impl ImapSessionFactory {
    /// A factory for the server in `config`. `config.auth` is replaced by the
    /// provider's credentials on every connect.
    pub fn new(config: ImapConfig, credentials: Arc<dyn CredentialProvider>) -> Self {
        Self {
            config,
            credentials,
        }
    }

    async fn attempt(&self) -> Result<(ImapAuth, Result<ImapClient, imap::ImapError>)> {
        let auth = self.credentials.credentials().await?;
        let mut config = self.config.clone();
        config.auth = auth.clone();
        Ok((auth, ImapClient::connect(&config).await))
    }
}

#[async_trait]
impl SessionFactory for ImapSessionFactory {
    async fn connect(&self) -> Result<BoxSession> {
        let (auth, result) = self.attempt().await?;
        match result {
            Ok(client) => Ok(Box::new(client)),
            Err(imap::ImapError::Auth(reason)) => {
                tracing::info!(%reason, "IMAP login rejected; retrying with fresh credentials");
                self.credentials.rejected(&auth).await;
                let (_, retry) = self.attempt().await?;
                Ok(Box::new(retry?))
            }
            Err(err) => Err(err.into()),
        }
    }
}

/// A worker session with what the engine remembers about it.
pub(crate) struct Conn {
    pub(crate) session: BoxSession,
    pub(crate) caps: Caps,
    /// The folder selected on this session, with its SELECT result.
    selected: Option<(String, SelectInfo)>,
    /// A command failed in a way that left the session unusable.
    pub(crate) broken: bool,
}

impl fmt::Debug for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conn")
            .field("selected", &self.selected.is_some())
            .field("broken", &self.broken)
            .finish_non_exhaustive()
    }
}

impl Conn {
    /// Wraps a fresh session and reads its capabilities.
    pub(crate) async fn new(mut session: BoxSession) -> Result<Self> {
        let caps = session.capabilities().await?;
        Ok(Self {
            session,
            caps,
            selected: None,
            broken: false,
        })
    }

    /// Records the outcome of a session call: an unrecoverable error breaks the session.
    pub(crate) fn track<T>(&mut self, result: imap::Result<T>) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(err) => {
                if !err.is_recoverable() {
                    self.broken = true;
                    self.selected = None;
                }
                Err(err.into())
            }
        }
    }

    /// `SELECT` (with QRESYNC arguments if given) and remember the selection.
    pub(crate) async fn select(
        &mut self,
        folder: &str,
        qresync: Option<imap::QresyncArgs>,
    ) -> Result<SelectInfo> {
        self.selected = None;
        let result = self.session.select(folder, qresync).await;
        let info = self.track(result)?;
        self.selected = Some((folder.to_owned(), info.clone()));
        Ok(info)
    }

    /// Selects `folder` unless it already is selected; returns its UIDVALIDITY.
    pub(crate) async fn ensure_selected(&mut self, folder: &str) -> Result<u32> {
        if let Some((name, info)) = &self.selected
            && name == folder
        {
            return Ok(info.uidvalidity);
        }
        Ok(self.select(folder, None).await?.uidvalidity)
    }

    /// The selected folder's name, if any.
    pub(crate) fn selected_folder(&self) -> Option<&str> {
        self.selected.as_ref().map(|(name, _)| name.as_str())
    }

    /// Forget the selection (after RENAME/DELETE of the selected folder).
    pub(crate) fn forget_selection(&mut self) {
        self.selected = None;
    }

    /// Drains unsolicited events; returns server `[ALERT]` texts.
    pub(crate) fn drain_alerts(&mut self) -> Vec<String> {
        self.session
            .take_events()
            .into_iter()
            .filter_map(|e| match e {
                imap::SessionEvent::Alert(text) => Some(text),
                _ => None,
            })
            .collect()
    }
}
