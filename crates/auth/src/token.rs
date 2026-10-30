//! Token lifecycle: [`TokenSet`], the [`TokenProvider`] trait the `imap` and `smtp`
//! crates call, and [`OAuthTokenManager`], which keeps access tokens in memory and
//! refresh tokens in the account's [`SecretStore`](crate::SecretStore).

use std::fmt;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mazemail_types::Sensitive;

use crate::provider::scope_matches;

use crate::{
    AccountSecrets, AuthError, AuthorizeOptions, BrowserOpener, Capabilities, Capability,
    OAuthClient, ReauthReason, SecretKind, TokenResource,
};

/// How long before expiry an access token is refreshed (PLAN §4.6).
pub const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// The tokens from one token endpoint response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSet {
    /// The bearer access token. Kept in memory only.
    pub access_token: Sensitive<String>,
    /// The refresh token, if the provider sent one. Persisted in the secret store.
    pub refresh_token: Option<Sensitive<String>>,
    /// When the access token expires; `None` if the provider didn't say, in which
    /// case it is used until [`TokenProvider::invalidate`] (as in Thunderbird).
    pub expires_at: Option<Instant>,
    /// The granted scopes (the requested ones if the response didn't list them).
    pub scopes: Vec<String>,
    /// The API the access token is for.
    pub resource: TokenResource,
    /// The capabilities the granted scopes cover
    /// ([`Provider::capabilities_of`](crate::Provider::capabilities_of)).
    pub capabilities: Capabilities,
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
    /// varies; a Graph scope also in its short form).
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| scope_matches(scope, s))
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

/// How long a "the grant doesn't cover this" answer for a secondary resource
/// (Microsoft Graph) is remembered before the next call asks the provider again, e.g.
/// after an administrator approved the permission. A sign-in and
/// [`OAuthTokenManager::invalidate_for`] forget it at once.
pub const MISSING_SCOPE_RETRY: Duration = Duration::from_secs(60 * 60);

/// The cached token of one [`TokenResource`].
#[derive(Default)]
struct Slot {
    tokens: Option<TokenSet>,
    /// Until when the provider's "not consented" answer stands (secondary resources).
    missing_until: Option<Instant>,
}

/// One OAuth account's tokens: signs in through the browser, keeps access tokens in
/// memory, refreshes them [`REFRESH_MARGIN`] before expiry and stores the refresh
/// token under `mazemail/<account-uuid>/oauth-refresh`.
///
/// Access tokens are cached per [`TokenResource`]: mail ([`TokenProvider`]) uses the
/// primary token; [`access_token_for`](Self::access_token_for) hands out the token a
/// [`Capability`] needs (for Microsoft contacts a separate Graph token from the same
/// refresh token). Each resource refreshes on its own; concurrent callers of one
/// resource share one refresh (its cache lock is held while refreshing).
///
/// A capability the grant doesn't cover fails with
/// [`ReauthReason::MissingScope`] for that capability only; the mail token, the
/// stored refresh token and mail's sign-in state are left alone.
pub struct OAuthTokenManager {
    client: OAuthClient,
    secrets: AccountSecrets,
    /// One per [`TokenResource`], by [`TokenResource::index`]. Lock in index order
    /// when holding several.
    slots: [tokio::sync::Mutex<Slot>; 2],
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
            slots: Default::default(),
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

    fn slot(&self, resource: TokenResource) -> &tokio::sync::Mutex<Slot> {
        &self.slots[resource.index()]
    }

    /// Runs the browser sign-in ([`OAuthClient::authorize`]) and keeps the result.
    /// `options` say which capabilities to ask for (mail by default).
    pub async fn sign_in(
        &self,
        browser: &dyn BrowserOpener,
        options: &AuthorizeOptions,
    ) -> Result<(), AuthError> {
        let tokens = self.client.authorize(browser, options).await?;
        self.store_tokens(tokens).await
    }

    /// Asks a signed-in account for more capabilities (e.g. contacts) with an
    /// incremental consent in the browser
    /// ([`AuthorizeOptions::incremental_consent`]), keeps the new refresh token and
    /// returns which of `capabilities` the grant now covers (the user may untick
    /// some). Mail keeps working whatever the outcome.
    ///
    /// For Microsoft this fetches the Graph token right away to see whether contacts
    /// were granted.
    pub async fn grant(
        &self,
        browser: &dyn BrowserOpener,
        capabilities: Capabilities,
        options: AuthorizeOptions,
    ) -> Result<Capabilities, AuthError> {
        let options = options.capabilities(capabilities).incremental_consent();
        self.sign_in(browser, &options).await?;
        let mut granted = Capabilities::NONE;
        for capability in capabilities.iter() {
            if self.has_capability(capability).await? {
                granted = granted.with(capability);
            }
        }
        tracing::info!(provider = %self.client.provider().kind(), requested = ?capabilities, ?granted, "permission granted");
        Ok(granted)
    }

    /// Keeps tokens from a sign-in the caller ran itself
    /// ([`OAuthClient::begin_authorization`]): the refresh token goes to the
    /// secret store, the access token into memory as its resource's token.
    /// Forgets remembered missing-scope answers, as the grant may have changed.
    pub async fn store_tokens(&self, mut tokens: TokenSet) -> Result<(), AuthError> {
        let mut primary = self.slot(TokenResource::Primary).lock().await;
        let mut graph = self.slot(TokenResource::Graph).lock().await;
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
        primary.missing_until = None;
        graph.missing_until = None;
        match tokens.resource {
            TokenResource::Primary => primary.tokens = Some(tokens),
            TokenResource::Graph => graph.tokens = Some(tokens),
        }
        Ok(())
    }

    /// Forgets the access tokens and deletes the stored refresh token. Doesn't revoke
    /// anything at the provider.
    pub async fn sign_out(&self) -> Result<(), AuthError> {
        let mut primary = self.slot(TokenResource::Primary).lock().await;
        let mut graph = self.slot(TokenResource::Graph).lock().await;
        *primary = Slot::default();
        *graph = Slot::default();
        self.secrets.delete(SecretKind::OAuthRefresh).await?;
        Ok(())
    }

    /// Whether a refresh token is stored, i.e. the account can get tokens without
    /// the browser.
    pub async fn is_signed_in(&self) -> Result<bool, AuthError> {
        Ok(self.secrets.load(SecretKind::OAuthRefresh).await?.is_some())
    }

    /// A currently valid access token for `capability`, refreshed if it expires
    /// within [`REFRESH_MARGIN`]. [`Capability::Mail`] is what
    /// [`TokenProvider::access_token`] returns.
    ///
    /// - Google: one token for every capability; contacts need the contacts scopes
    ///   in it.
    /// - Microsoft: contacts get a Graph token, refreshed with
    ///   `scope=https://graph.microsoft.com/Contacts.ReadWrite offline_access` and
    ///   cached apart from the mail token.
    ///
    /// # Errors
    ///
    /// [`ReauthReason::MissingScope`] (see [`AuthError::missing_capability`]) when
    /// the grant doesn't cover a capability other than mail; the answer of a Graph
    /// refresh is remembered for [`MISSING_SCOPE_RETRY`]. Else as
    /// [`TokenProvider::access_token`].
    pub async fn access_token_for(
        &self,
        capability: Capability,
    ) -> Result<Sensitive<String>, AuthError> {
        let resource = self.client.provider().resource_for(capability);
        let mut slot = self.slot(resource).lock().await;
        let now = Instant::now();
        if capability != Capability::Mail && slot.missing_until.is_some_and(|until| now < until) {
            return Err(AuthError::missing_scope(capability));
        }
        if let Some(tokens) = slot.tokens.as_ref()
            && !tokens.needs_refresh(now, self.refresh_margin)
        {
            return token_for(tokens, capability);
        }
        self.refresh(resource, capability, &mut slot).await
    }

    /// Whether the account's grant covers `capability`: gets its token
    /// ([`access_token_for`](Self::access_token_for)), so it may ask the provider.
    /// `Ok(false)` means [`ReauthReason::MissingScope`]; other errors (offline, the
    /// refresh token was revoked) are returned as they are.
    pub async fn has_capability(&self, capability: Capability) -> Result<bool, AuthError> {
        match self.access_token_for(capability).await {
            Ok(_) => Ok(true),
            Err(err) if err.missing_capability() == Some(capability) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// The capabilities the cached access tokens show, without asking the provider.
    /// Says nothing about resources that have no cached token yet (a Microsoft Graph
    /// token is only fetched when contacts are used): use
    /// [`has_capability`](Self::has_capability) for a definite answer.
    pub async fn known_capabilities(&self) -> Capabilities {
        let mut known = Capabilities::NONE;
        for slot in &self.slots {
            if let Some(tokens) = &slot.lock().await.tokens {
                known = known.union(tokens.capabilities);
            }
        }
        known
    }

    /// Forgets the cached access token `capability` uses (and a remembered
    /// missing-scope answer), so the next call refreshes it. Call this when an API
    /// rejected the token before its expiry. For Google this is the mail token too.
    pub async fn invalidate_for(&self, capability: Capability) {
        let resource = self.client.provider().resource_for(capability);
        *self.slot(resource).lock().await = Slot::default();
    }

    async fn refresh(
        &self,
        resource: TokenResource,
        capability: Capability,
        slot: &mut Slot,
    ) -> Result<Sensitive<String>, AuthError> {
        let Some(refresh_token) = self.secrets.load(SecretKind::OAuthRefresh).await? else {
            slot.tokens = None;
            return Err(AuthError::ReauthRequired {
                reason: ReauthReason::NotSignedIn,
            });
        };

        match self.client.refresh_for(&refresh_token, capability).await {
            Ok(mut tokens) => {
                if let Some(rotated) = tokens.refresh_token.take()
                    && rotated != refresh_token
                {
                    // The new access token is good either way, and the old refresh
                    // token usually stays valid for a while, so a storage failure
                    // here doesn't fail the call.
                    if let Err(err) = self.secrets.store(SecretKind::OAuthRefresh, rotated).await {
                        tracing::warn!(%err, "could not store the rotated refresh token");
                    }
                }
                slot.missing_until = None;
                let result = token_for(&tokens, capability);
                slot.tokens = Some(tokens);
                result
            }
            Err(err) if err.missing_capability().is_some() => {
                // Only this resource lacks consent: the refresh token stays.
                slot.tokens = None;
                slot.missing_until = Instant::now().checked_add(MISSING_SCOPE_RETRY);
                tracing::info!(provider = %self.client.provider().kind(), %capability, "the grant doesn't cover this capability");
                Err(err)
            }
            Err(err) if err.requires_reauth() => {
                slot.tokens = None;
                // The refresh token is dead; drop it so later calls fail fast. Only
                // the primary token's refresh decides that, so a secondary
                // resource's quirks can't sign mail out.
                if resource == TokenResource::Primary
                    && let Err(delete_err) = self.secrets.delete(SecretKind::OAuthRefresh).await
                {
                    tracing::warn!(err = %delete_err, "could not delete the rejected refresh token");
                }
                Err(err)
            }
            Err(err) => {
                // A token that is about to expire but still valid beats an error when
                // the refresh failed for a transient reason.
                if err.is_transient()
                    && let Some(tokens) = slot.tokens.as_ref()
                    && !tokens.is_expired(Instant::now())
                {
                    tracing::warn!(%err, "refresh failed; using the still-valid access token");
                    return token_for(tokens, capability);
                }
                slot.tokens = None;
                Err(err)
            }
        }
    }
}

/// `tokens`' access token if it serves `capability`. Mail is never checked (the
/// server refuses what isn't granted, as before capabilities existed).
fn token_for(tokens: &TokenSet, capability: Capability) -> Result<Sensitive<String>, AuthError> {
    if capability == Capability::Mail || tokens.capabilities.contains(capability) {
        Ok(tokens.access_token.clone())
    } else {
        Err(AuthError::missing_scope(capability))
    }
}

#[async_trait]
impl TokenProvider for OAuthTokenManager {
    async fn access_token(&self) -> Result<Sensitive<String>, AuthError> {
        self.access_token_for(Capability::Mail).await
    }

    async fn invalidate(&self) {
        self.invalidate_for(Capability::Mail).await;
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
            resource: TokenResource::Primary,
            capabilities: Capabilities::NONE,
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
        let graph = TokenSet {
            scopes: vec!["Contacts.ReadWrite".to_owned(), "profile".to_owned()],
            ..set
        };
        assert!(graph.has_scope("https://graph.microsoft.com/Contacts.ReadWrite"));
        assert!(!graph.has_scope("https://outlook.office.com/Contacts.ReadWrite"));
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
