//! Token lifecycle: [`TokenSet`], the [`TokenProvider`] trait the `imap` and `smtp`
//! crates call, and [`OAuthTokenManager`], which keeps access tokens in memory and
//! refresh tokens in the keychain.

use std::fmt;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mazemail_types::Sensitive;

use crate::{
    AccountSecrets, AuthError, AuthorizeOptions, BrowserOpener, OAuthClient, ReauthReason,
    SecretKind,
};

/// How long before expiry an access token is refreshed (PLAN §4.6).
pub const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// The tokens from one token endpoint response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSet {
    /// The bearer access token. Kept in memory only.
    pub access_token: Sensitive<String>,
    /// The refresh token, if the provider sent one. Persisted in the keychain.
    pub refresh_token: Option<Sensitive<String>>,
    /// When the access token expires; `None` if the provider didn't say, in which
    /// case it is used until [`TokenProvider::invalidate`] (as in Thunderbird).
    pub expires_at: Option<Instant>,
    /// The granted scopes (the requested ones if the response didn't list them).
    pub scopes: Vec<String>,
}

impl TokenSet {
    /// Whether the access token expires within `margin` of `now`.
    pub fn needs_refresh(&self, now: Instant, margin: Duration) -> bool {
        self.expires_at
            .is_some_and(|expires_at| now.checked_add(margin).is_none_or(|t| t >= expires_at))
    }

    /// Whether the access token has expired at `now`.
    pub fn is_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|expires_at| now >= expires_at)
    }

    /// Whether `scope` was granted (ASCII case-insensitive, as Microsoft's casing
    /// varies).
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s.eq_ignore_ascii_case(scope))
    }
}

/// Hands out valid OAuth access tokens. The `imap` and `smtp` crates call
/// [`access_token`](Self::access_token) before every `AUTHENTICATE XOAUTH2` and
/// [`invalidate`](Self::invalidate) when the server rejects a token.
#[async_trait]
pub trait TokenProvider: Send + Sync {
    /// A currently valid access token, refreshed if it expires within
    /// [`REFRESH_MARGIN`]. [`AuthError::requires_reauth`] tells whether the user has to
    /// sign in again.
    async fn access_token(&self) -> Result<Sensitive<String>, AuthError>;

    /// Forgets the cached access token, so the next call refreshes it. Call this when
    /// the server rejected the token before its expiry (revoked, password changed).
    async fn invalidate(&self);
}

/// A [`TokenProvider`] with a fixed token, for tests in downstream crates.
#[derive(Debug, Clone)]
pub struct StaticTokenProvider {
    token: Sensitive<String>,
}

impl StaticTokenProvider {
    /// Always returns `token`.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: Sensitive::new(token.into()),
        }
    }
}

#[async_trait]
impl TokenProvider for StaticTokenProvider {
    async fn access_token(&self) -> Result<Sensitive<String>, AuthError> {
        Ok(self.token.clone())
    }

    async fn invalidate(&self) {}
}

/// One OAuth account's tokens: signs in through the browser, keeps the access token
/// in memory, refreshes it [`REFRESH_MARGIN`] before expiry and stores the refresh
/// token under `mazemail/<account-uuid>/oauth-refresh`.
///
/// Concurrent callers share one refresh: the cache lock is held while refreshing.
pub struct OAuthTokenManager {
    client: OAuthClient,
    secrets: AccountSecrets,
    cached: tokio::sync::Mutex<Option<TokenSet>>,
    refresh_margin: Duration,
}

impl fmt::Debug for OAuthTokenManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthTokenManager")
            .field("provider", &self.client.provider().kind())
            .field("account", &self.secrets.account())
            .finish_non_exhaustive()
    }
}

impl OAuthTokenManager {
    /// A manager for the account behind `secrets`, using `client` to talk to the
    /// provider.
    pub fn new(client: OAuthClient, secrets: AccountSecrets) -> Self {
        Self {
            client,
            secrets,
            cached: tokio::sync::Mutex::new(None),
            refresh_margin: REFRESH_MARGIN,
        }
    }

    /// Changes how long before expiry the token is refreshed (default
    /// [`REFRESH_MARGIN`]).
    #[must_use]
    pub fn with_refresh_margin(mut self, margin: Duration) -> Self {
        self.refresh_margin = margin;
        self
    }

    /// The OAuth client.
    pub fn client(&self) -> &OAuthClient {
        &self.client
    }

    /// The account's secrets.
    pub fn secrets(&self) -> &AccountSecrets {
        &self.secrets
    }

    /// Runs the browser sign-in ([`OAuthClient::authorize`]) and keeps the result.
    pub async fn sign_in(
        &self,
        browser: &dyn BrowserOpener,
        options: &AuthorizeOptions,
    ) -> Result<(), AuthError> {
        let tokens = self.client.authorize(browser, options).await?;
        self.store_tokens(tokens).await
    }

    /// Keeps tokens from a sign-in the caller ran itself
    /// ([`OAuthClient::begin_authorization`]): the refresh token goes to the
    /// keychain, the access token into memory.
    pub async fn store_tokens(&self, mut tokens: TokenSet) -> Result<(), AuthError> {
        let mut cached = self.cached.lock().await;
        match tokens.refresh_token.take() {
            Some(refresh_token) => {
                self.secrets
                    .store(SecretKind::OAuthRefresh, refresh_token)
                    .await?;
            }
            None => tracing::warn!(
                provider = %self.client.provider().kind(),
                "the sign-in returned no refresh token; keeping the stored one, if any"
            ),
        }
        *cached = Some(tokens);
        Ok(())
    }

    /// Forgets the access token and deletes the stored refresh token. Doesn't revoke
    /// anything at the provider.
    pub async fn sign_out(&self) -> Result<(), AuthError> {
        let mut cached = self.cached.lock().await;
        *cached = None;
        self.secrets.delete(SecretKind::OAuthRefresh).await?;
        Ok(())
    }

    /// Whether a refresh token is stored, i.e. the account can get tokens without
    /// the browser.
    pub async fn is_signed_in(&self) -> Result<bool, AuthError> {
        Ok(self.secrets.load(SecretKind::OAuthRefresh).await?.is_some())
    }

    async fn refresh(&self, cached: &mut Option<TokenSet>) -> Result<Sensitive<String>, AuthError> {
        let Some(refresh_token) = self.secrets.load(SecretKind::OAuthRefresh).await? else {
            *cached = None;
            return Err(AuthError::ReauthRequired {
                reason: ReauthReason::NotSignedIn,
            });
        };

        match self.client.refresh(&refresh_token).await {
            Ok(mut tokens) => {
                if let Some(rotated) = tokens.refresh_token.take()
                    && rotated != refresh_token
                {
                    // The new access token is good either way, and the old refresh
                    // token usually stays valid for a while, so a keychain failure
                    // here doesn't fail the call.
                    if let Err(err) = self.secrets.store(SecretKind::OAuthRefresh, rotated).await {
                        tracing::warn!(%err, "could not store the rotated refresh token");
                    }
                }
                let access_token = tokens.access_token.clone();
                *cached = Some(tokens);
                Ok(access_token)
            }
            Err(err) if err.requires_reauth() => {
                *cached = None;
                // The refresh token is dead; drop it so later calls fail fast.
                if let Err(delete_err) = self.secrets.delete(SecretKind::OAuthRefresh).await {
                    tracing::warn!(err = %delete_err, "could not delete the rejected refresh token");
                }
                Err(err)
            }
            Err(err) => {
                // A token that is about to expire but still valid beats an error when
                // the refresh failed for a transient reason.
                if err.is_transient()
                    && let Some(tokens) = cached.as_ref()
                    && !tokens.is_expired(Instant::now())
                {
                    tracing::warn!(%err, "refresh failed; using the still-valid access token");
                    return Ok(tokens.access_token.clone());
                }
                *cached = None;
                Err(err)
            }
        }
    }
}

#[async_trait]
impl TokenProvider for OAuthTokenManager {
    async fn access_token(&self) -> Result<Sensitive<String>, AuthError> {
        let mut cached = self.cached.lock().await;
        if let Some(tokens) = cached.as_ref()
            && !tokens.needs_refresh(Instant::now(), self.refresh_margin)
        {
            return Ok(tokens.access_token.clone());
        }
        self.refresh(&mut cached).await
    }

    async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(expires_in: Option<Duration>, now: Instant) -> TokenSet {
        TokenSet {
            access_token: Sensitive::new("at".to_owned()),
            refresh_token: None,
            expires_at: expires_in.map(|d| now + d),
            scopes: vec!["https://outlook.office.com/IMAP.AccessAsUser.All".to_owned()],
        }
    }

    #[test]
    fn refreshes_sixty_seconds_before_expiry() {
        let now = Instant::now();
        let set = tokens(Some(Duration::from_secs(3600)), now);
        assert!(!set.needs_refresh(now, REFRESH_MARGIN));
        assert!(!set.needs_refresh(now + Duration::from_secs(3539), REFRESH_MARGIN));
        assert!(set.needs_refresh(now + Duration::from_secs(3540), REFRESH_MARGIN));
        assert!(!set.is_expired(now + Duration::from_secs(3540)));
        assert!(set.is_expired(now + Duration::from_secs(3600)));
    }

    #[test]
    fn unknown_expiry_never_needs_refresh() {
        let now = Instant::now();
        let set = tokens(None, now);
        assert!(!set.needs_refresh(now + Duration::from_secs(86_400), REFRESH_MARGIN));
        assert!(!set.is_expired(now + Duration::from_secs(86_400)));
    }

    #[test]
    fn scope_check_ignores_case() {
        let set = tokens(None, Instant::now());
        assert!(set.has_scope("https://outlook.office.com/imap.accessasuser.all"));
        assert!(!set.has_scope("https://outlook.office.com/SMTP.Send"));
    }

    #[test]
    fn debug_hides_tokens() {
        let mut set = tokens(None, Instant::now());
        set.refresh_token = Some(Sensitive::new("refresh-secret".to_owned()));
        let text = format!("{set:?}");
        assert!(
            !text.contains("refresh-secret") && !text.contains("\"at\""),
            "{text}"
        );
    }

    #[tokio::test]
    async fn static_provider_returns_its_token() {
        let provider = StaticTokenProvider::new("tok");
        assert_eq!(provider.access_token().await.unwrap().expose(), "tok");
        provider.invalidate().await;
        assert_eq!(provider.access_token().await.unwrap().expose(), "tok");
    }
}
