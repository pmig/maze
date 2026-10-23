//! Credentials for the sync engine: `sync::CredentialProvider` over `auth` (PLAN §4.6).
//!
//! - **Password accounts** read the IMAP password from the account's secrets
//!   (`mazemail/<account-uuid>/password`, [`AccountSecrets`], in the encrypted store)
//!   on every connect, so a changed password is picked up at the next reconnect.
//! - **OAuth accounts** (Google, Microsoft) get an access token from an
//!   [`OAuthTokenManager`]: cached in memory, refreshed 60 s before expiry with the
//!   stored refresh token. When the server rejects a token, the cached one
//!   is dropped so the retry refreshes it.
//!
//! "The user has to act" (no password stored, refresh token revoked, no OAuth client
//! ID configured) is reported with `needs_user`, which puts the account into
//! "sign-in required" instead of retrying on a timer. Secrets never reach logs or
//! error texts.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use auth::{AccountSecrets, AuthError, OAuthTokenManager, SecretStoreError, TokenProvider as _};
use imap::ImapAuth;
use sync::{CredentialError, CredentialProvider};

/// How an account logs in.
pub(crate) enum Method {
    /// The stored password.
    Password(AccountSecrets),
    /// An OAuth access token; `Err` holds why no token manager could be built (no
    /// client ID configured, say).
    OAuth(Result<Arc<OAuthTokenManager>, String>),
    /// Fixed credentials, for validating a new account before anything is saved.
    Fixed(ImapAuth),
}

/// An account's credentials for the sync engine. See the [module docs](self).
pub(crate) struct AccountCredentials {
    user: String,
    method: Method,
}

impl fmt::Debug for AccountCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let method = match &self.method {
            Method::Password(_) => "password",
            Method::OAuth(_) => "oauth",
            Method::Fixed(_) => "fixed",
        };
        f.debug_struct("AccountCredentials")
            .field("method", &method)
            .finish_non_exhaustive()
    }
}

impl AccountCredentials {
    /// Credentials of `user` (the IMAP login name) by `method`.
    pub(crate) fn new(user: impl Into<String>, method: Method) -> Self {
        Self {
            user: user.into(),
            method,
        }
    }

    /// Fixed credentials (validation of a new account).
    pub(crate) fn fixed(auth: ImapAuth) -> Self {
        Self {
            user: String::new(),
            method: Method::Fixed(auth),
        }
    }
}

fn secrets_error(err: &SecretStoreError) -> CredentialError {
    // The store is busy or closing (or, with a custom store, a locked keychain): try
    // again later.
    CredentialError::temporary(format!("the saved password can't be read: {err}"))
}

fn token_error(err: &AuthError) -> CredentialError {
    if err.requires_reauth() || matches!(err, AuthError::MissingClientId { .. }) {
        CredentialError::needs_sign_in(err.to_string())
    } else {
        CredentialError::temporary(err.to_string())
    }
}

#[async_trait]
impl CredentialProvider for AccountCredentials {
    async fn credentials(&self) -> Result<ImapAuth, CredentialError> {
        match &self.method {
            Method::Fixed(auth) => Ok(auth.clone()),
            Method::Password(secrets) => match secrets.password().await {
                Ok(Some(password)) => Ok(ImapAuth::Password {
                    user: self.user.clone(),
                    password,
                }),
                Ok(None) => Err(CredentialError::needs_sign_in(
                    "no password is stored for this account",
                )),
                Err(err) => Err(secrets_error(&err)),
            },
            Method::OAuth(Err(reason)) => Err(CredentialError::needs_sign_in(reason.clone())),
            Method::OAuth(Ok(tokens)) => match tokens.access_token().await {
                Ok(access_token) => Ok(ImapAuth::XOAuth2 {
                    user: self.user.clone(),
                    access_token,
                }),
                Err(err) => Err(token_error(&err)),
            },
        }
    }

    async fn rejected(&self, credentials: &ImapAuth) {
        if let (Method::OAuth(Ok(tokens)), ImapAuth::XOAuth2 { .. }) = (&self.method, credentials) {
            tokens.invalidate().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use auth::{MemorySecretStore, Uuid};
    use mazemail_types::Sensitive;

    use super::*;

    const ACCOUNT: Uuid = Uuid::from_u128(0x1234);

    #[tokio::test]
    async fn passwords_come_from_the_secret_store_on_every_connect() {
        let store = Arc::new(MemorySecretStore::new());
        let secrets = AccountSecrets::new(store, ACCOUNT);
        let credentials = AccountCredentials::new("alice", Method::Password(secrets.clone()));

        let missing = credentials.credentials().await.unwrap_err();
        assert!(missing.needs_user, "{missing}");

        secrets
            .set_password(Sensitive::new("first".to_owned()))
            .await
            .unwrap();
        let ImapAuth::Password { user, password } = credentials.credentials().await.unwrap() else {
            panic!("not a password");
        };
        assert_eq!(
            (user.as_str(), password.expose().as_str()),
            ("alice", "first")
        );

        // A changed password is used at the next connect.
        secrets
            .set_password(Sensitive::new("second".to_owned()))
            .await
            .unwrap();
        let ImapAuth::Password { password, .. } = credentials.credentials().await.unwrap() else {
            panic!("not a password");
        };
        assert_eq!(password.expose(), "second");
        assert!(!format!("{credentials:?}").contains("second"));
    }

    #[tokio::test]
    async fn oauth_without_a_client_needs_the_user() {
        let credentials = AccountCredentials::new(
            "bob@gmail.com",
            Method::OAuth(Err("no OAuth client ID is configured for Google".to_owned())),
        );
        let err = credentials.credentials().await.unwrap_err();
        assert!(err.needs_user);
        assert!(err.message.contains("client ID"));
    }

    #[test]
    fn token_errors_are_classified() {
        let reauth = AuthError::ReauthRequired {
            reason: auth::ReauthReason::NotSignedIn,
        };
        assert!(token_error(&reauth).needs_user);
        let network = AuthError::Network {
            message: "connection refused".into(),
            timeout: false,
        };
        assert!(!token_error(&network).needs_user);
        assert!(
            token_error(&AuthError::MissingClientId {
                provider: auth::ProviderKind::Google
            })
            .needs_user
        );
    }
}
