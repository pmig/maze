//! The OAuth2 client: authorization code flow with PKCE and a loopback redirect,
//! code exchange and refresh.

use std::fmt;
use std::time::Duration;

use mazemail_types::Sensitive;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::Deserialize;
use tokio::time::Instant;
use url::Url;

use crate::error::{OAuthErrorDetails, classify, classify_for};
use crate::loopback::{CallbackParams, LoopbackListener};
use crate::pkce::{PkceVerifier, STATE_BYTES, constant_time_eq, random_token};
use crate::provider::scope_matches;
use crate::{
    AuthError, BrowserOpener, Capabilities, Capability, ClientCredentials, Provider, ProviderKind,
    TokenResource, TokenSet,
};

/// How long the browser sign-in may take by default.
pub const DEFAULT_AUTHORIZE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Options for one browser sign-in.
#[derive(Debug, Clone)]
pub struct AuthorizeOptions {
    login_hint: Option<Sensitive<String>>,
    timeout: Duration,
    capabilities: Capabilities,
    incremental: bool,
}

impl Default for AuthorizeOptions {
    fn default() -> Self {
        Self {
            login_hint: None,
            timeout: DEFAULT_AUTHORIZE_TIMEOUT,
            capabilities: Capabilities::MAIL,
            incremental: false,
        }
    }
}

impl AuthorizeOptions {
    /// No login hint, [`DEFAULT_AUTHORIZE_TIMEOUT`], mail only.
    pub fn new() -> Self {
        Self::default()
    }

    /// What to ask permission for (default [`Capabilities::MAIL`]; empty means
    /// mail). The authorization request carries the scopes of all of them
    /// ([`Provider::scopes_for_all`]).
    ///
    /// Microsoft gives one access token per resource: the sign-in's token is for the
    /// first capability's resource (mail when mail is asked for), the others are
    /// fetched with the refresh token when needed
    /// ([`OAuthTokenManager::access_token_for`](crate::OAuthTokenManager::access_token_for)).
    #[must_use]
    pub fn capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Asks for [`capabilities`](Self::capabilities) on top of what the account
    /// already granted, e.g. contacts for an account that signed in for mail:
    ///
    /// - Google: `include_granted_scopes=true`, so the new grant (and its refresh
    ///   token) covers the earlier scopes too, and `prompt=consent`, so Google shows
    ///   the consent page and issues a new refresh token with the combined scopes.
    /// - Microsoft: nothing extra. Consent is kept per app and user, and the new
    ///   refresh token works for every consented resource.
    ///
    /// Ask only for the new capabilities (e.g. [`Capabilities::CONTACTS`]): the
    /// consent page then doesn't offer to untick mail.
    #[must_use]
    pub fn incremental_consent(mut self) -> Self {
        self.incremental = true;
        self
    }

    /// Pre-fills the account at the provider (`login_hint`), as Thunderbird does.
    #[must_use]
    pub fn login_hint(mut self, email: impl Into<String>) -> Self {
        self.login_hint = Some(Sensitive::new(email.into()));
        self
    }

    /// How long to wait for the browser redirect before failing with
    /// [`AuthError::Timeout`].
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// An OAuth2 client for one [`Provider`] with configured credentials.
///
/// Talks to the token endpoint with `reqwest` (rustls). Cheap to clone.
#[derive(Debug, Clone)]
pub struct OAuthClient {
    provider: Provider,
    credentials: ClientCredentials,
    http: reqwest::Client,
}

impl OAuthClient {
    /// A client for `provider`; fails with [`AuthError::MissingClientId`] if it has no
    /// (or a blank) client ID.
    pub fn new(provider: Provider) -> Result<Self, AuthError> {
        let credentials = provider
            .credentials()
            .filter(|credentials| !credentials.client_id().trim().is_empty())
            .cloned()
            .ok_or(AuthError::MissingClientId {
                provider: provider.kind(),
            })?;
        Ok(Self {
            provider,
            credentials,
            http: default_http_client()?,
        })
    }

    /// Uses `http` for token requests (custom proxy settings, tests). It should not
    /// follow redirects.
    #[must_use]
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// The provider.
    pub fn provider(&self) -> &Provider {
        &self.provider
    }

    /// Runs the whole browser sign-in: starts the loopback listener, opens the
    /// authorization URL with `browser`, waits for the redirect, checks `state` and
    /// exchanges the code.
    ///
    /// Dropping the future cancels the sign-in and closes the listener.
    pub async fn authorize(
        &self,
        browser: &dyn BrowserOpener,
        options: &AuthorizeOptions,
    ) -> Result<TokenSet, AuthError> {
        let request = self.begin_authorization(options).await?;
        browser.open(request.url()).map_err(AuthError::Browser)?;
        request.finish().await
    }

    /// Starts a sign-in without opening a browser, for UIs that open the URL
    /// themselves or offer to copy it when the browser can't be opened. The timeout
    /// starts now.
    pub async fn begin_authorization(
        &self,
        options: &AuthorizeOptions,
    ) -> Result<AuthorizationRequest<'_>, AuthError> {
        let listener = LoopbackListener::bind().await?;
        let state = random_token(STATE_BYTES)?;
        let verifier = if self.provider.use_pkce() {
            Some(PkceVerifier::generate()?)
        } else {
            None
        };

        let capabilities = if options.capabilities.is_empty() {
            Capabilities::MAIL
        } else {
            options.capabilities
        };
        let scopes = self.provider.scopes_for_all(capabilities);
        // The sign-in's access token is for the first capability's resource.
        let capability = capabilities.iter().next().unwrap_or(Capability::Mail);
        let incremental = options.incremental && self.provider.kind() == ProviderKind::Google;

        let mut url = self.provider.authorization_endpoint().clone();
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", self.credentials.client_id())
                .append_pair("redirect_uri", listener.redirect_uri())
                .append_pair("state", &state);
            if !scopes.is_empty() {
                query.append_pair("scope", &scopes.join(" "));
            }
            if incremental {
                query
                    .append_pair("include_granted_scopes", "true")
                    .append_pair("prompt", "consent");
            }
            if let Some(verifier) = &verifier {
                query
                    .append_pair("code_challenge", &verifier.challenge())
                    .append_pair("code_challenge_method", PkceVerifier::challenge_method());
            }
            if let Some(hint) = &options.login_hint {
                query.append_pair("login_hint", hint.expose());
            }
        }

        tracing::info!(
            provider = %self.provider.kind(),
            ?capabilities,
            incremental = options.incremental,
            "starting OAuth sign-in"
        );
        Ok(AuthorizationRequest {
            client: self,
            listener,
            state,
            verifier,
            url,
            scopes,
            capability,
            deadline: Instant::now() + options.timeout,
        })
    }

    /// Gets a new access token for mail with a refresh token
    /// ([`refresh_for`](Self::refresh_for) with [`Capability::Mail`]).
    pub async fn refresh(&self, refresh_token: &Sensitive<String>) -> Result<TokenSet, AuthError> {
        self.refresh_for(refresh_token, Capability::Mail).await
    }

    /// Gets a new access token for `capability`'s resource
    /// ([`Provider::resource_for`]) with a refresh token. The result carries a new
    /// refresh token if the provider rotated it (Microsoft does).
    ///
    /// Microsoft gets `scope` with the resource's scopes (Outlook's for mail, Graph's
    /// `Contacts.ReadWrite offline_access` for contacts); Google gets none and
    /// answers with a token for every granted scope. A Graph request the grant
    /// doesn't cover fails with [`ReauthReason::MissingScope`](crate::ReauthReason::MissingScope).
    pub async fn refresh_for(
        &self,
        refresh_token: &Sensitive<String>,
        capability: Capability,
    ) -> Result<TokenSet, AuthError> {
        tracing::debug!(provider = %self.provider.kind(), %capability, "refreshing OAuth access token");
        let scopes = self.provider.token_request_scopes(capability);
        let joined = scopes.map(|scopes| scopes.join(" "));
        let mut params = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.expose().as_str()),
        ];
        if let Some(joined) = &joined {
            params.push(("scope", joined));
        }
        // Without `scope` in the request or the response, the mail scopes are what
        // the grant surely has (RFC 6749 §5.1: the requested ones).
        let requested = scopes.unwrap_or(self.provider.scopes());
        self.token_request(&params, requested, capability).await
    }

    async fn exchange_code(
        &self,
        code: &Sensitive<String>,
        redirect_uri: &str,
        verifier: Option<&PkceVerifier>,
        authorized_scopes: &[String],
        capability: Capability,
    ) -> Result<TokenSet, AuthError> {
        let scopes = self.provider.token_request_scopes(capability);
        let joined = scopes.map(|scopes| scopes.join(" "));
        let mut params = vec![
            ("grant_type", "authorization_code"),
            ("code", code.expose().as_str()),
            ("redirect_uri", redirect_uri),
        ];
        if let Some(verifier) = verifier {
            params.push(("code_verifier", verifier.as_str()));
        }
        if let Some(joined) = &joined {
            // Microsoft: an authorization for several resources is redeemed for one.
            params.push(("scope", joined));
        }
        let requested = scopes.unwrap_or(authorized_scopes);
        self.token_request(&params, requested, capability).await
    }

    /// POSTs a token request (RFC 6749 §4.1.3, §6) for `capability`'s token and
    /// parses the response. `requested` are the scopes asked for, which count as
    /// granted if the response doesn't list any.
    async fn token_request(
        &self,
        params: &[(&str, &str)],
        requested: &[String],
        capability: Capability,
    ) -> Result<TokenSet, AuthError> {
        let body = {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.append_pair("client_id", self.credentials.client_id());
            if let Some(secret) = self.credentials.client_secret() {
                form.append_pair("client_secret", secret.expose());
            }
            for (key, value) in params {
                form.append_pair(key, value);
            }
            form.finish()
        };

        let sent_at = Instant::now();
        let response = self
            .http
            .post(self.provider.token_endpoint().clone())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(AuthError::network)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(AuthError::network)?;
        self.parse_token_response(status, &bytes, sent_at, requested, capability)
    }

    fn parse_token_response(
        &self,
        status: reqwest::StatusCode,
        body: &[u8],
        sent_at: Instant,
        requested: &[String],
        capability: Capability,
    ) -> Result<TokenSet, AuthError> {
        let resource = self.provider.resource_for(capability);
        // serde_json's messages can quote the input, which may contain tokens; only
        // the position goes into the error.
        let raw: RawTokenResponse = match serde_json::from_slice(body) {
            Ok(raw) => raw,
            Err(_) if !status.is_success() => {
                return Err(AuthError::Http {
                    status: status.as_u16(),
                });
            }
            Err(err) => {
                return Err(AuthError::InvalidResponse(format!(
                    "the token response is not valid JSON (line {}, column {})",
                    err.line(),
                    err.column()
                )));
            }
        };

        if let Some(error) = raw.error {
            let details = OAuthErrorDetails::new(error, raw.error_description, raw.error_codes);
            tracing::warn!(
                provider = %self.provider.kind(),
                %details,
                http_status = status.as_u16(),
                "token endpoint returned an error"
            );
            // A secondary resource's "not consented" is a missing capability.
            let secondary = (resource != TokenResource::Primary).then_some(capability);
            return Err(classify_for(&self.provider, details, secondary));
        }
        if !status.is_success() {
            return Err(AuthError::Http {
                status: status.as_u16(),
            });
        }

        let access_token = raw
            .access_token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                AuthError::InvalidResponse("the token response has no access_token".to_owned())
            })?;
        if let Some(token_type) = &raw.token_type
            && !token_type.eq_ignore_ascii_case("bearer")
        {
            return Err(AuthError::InvalidResponse(format!(
                "unsupported token type {token_type:?}"
            )));
        }

        let scopes: Vec<String> = match &raw.scope {
            Some(scope) => scope.split_whitespace().map(str::to_owned).collect(),
            // RFC 6749 §5.1: omitted means the requested scopes were granted.
            None => requested.to_vec(),
        };
        let missing: Vec<&str> = requested
            .iter()
            .filter(|s| !s.eq_ignore_ascii_case(Provider::OFFLINE_ACCESS_SCOPE))
            .filter(|s| !scopes.iter().any(|g| scope_matches(s, g)))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            // Like Thunderbird, only warn: the server will refuse what isn't granted,
            // and a missing capability is reported when it is used.
            tracing::warn!(provider = %self.provider.kind(), ?missing, "not all requested scopes were granted");
        }
        let capabilities = self.provider.capabilities_of(&scopes);

        Ok(TokenSet {
            access_token: Sensitive::new(access_token),
            refresh_token: raw
                .refresh_token
                .filter(|token| !token.is_empty())
                .map(Sensitive::new),
            expires_at: raw
                .expires_in
                .and_then(|secs| sent_at.checked_add(Duration::from_secs(secs)))
                .map(Instant::into_std),
            scopes,
            resource,
            capabilities,
        })
    }
}

fn default_http_client() -> Result<reqwest::Client, AuthError> {
    reqwest::Client::builder()
        .user_agent(concat!("mazemail/", env!("CARGO_PKG_VERSION")))
        .referer(false)
        // Never re-send a code or refresh token to wherever a redirect points.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(AuthError::network)
}

/// A token endpoint response: success (RFC 6749 §5.1) and error (§5.2) fields
/// together, because some servers send errors with status 200.
#[derive(Deserialize)]
struct RawTokenResponse {
    access_token: Option<String>,
    token_type: Option<String>,
    #[serde(default, deserialize_with = "lenient_seconds")]
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
    /// Microsoft Entra's numeric `AADSTS` codes.
    #[serde(default)]
    error_codes: Vec<u32>,
}

/// `expires_in` as a number or a numeric string (older Microsoft endpoints).
fn lenient_seconds<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Seconds {
        Int(u64),
        Float(f64),
        Text(String),
    }
    Ok(match Option::<Seconds>::deserialize(deserializer)? {
        Some(Seconds::Int(secs)) => Some(secs),
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(Seconds::Float(secs)) if secs.is_finite() && secs >= 0.0 => Some(secs as u64),
        Some(Seconds::Text(text)) => text.trim().parse().ok(),
        _ => None,
    })
}

/// A started browser sign-in, from [`OAuthClient::begin_authorization`]. Open
/// [`url`](Self::url) in a browser, then await [`finish`](Self::finish).
pub struct AuthorizationRequest<'a> {
    client: &'a OAuthClient,
    listener: LoopbackListener,
    state: String,
    verifier: Option<PkceVerifier>,
    url: Url,
    /// The scopes in the authorization URL.
    scopes: Vec<String>,
    /// The capability whose resource the code is redeemed for.
    capability: Capability,
    deadline: Instant,
}

impl fmt::Debug for AuthorizationRequest<'_> {
    // The URL holds the state, the challenge and maybe the address (`login_hint`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizationRequest")
            .field("provider", &self.client.provider.kind())
            .field("redirect_uri", &self.listener.redirect_uri())
            .finish_non_exhaustive()
    }
}

impl AuthorizationRequest<'_> {
    /// The authorization URL to open in the browser. Don't log it: it contains the
    /// `state` and possibly the user's address.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// The loopback `redirect_uri`, `http://127.0.0.1:<port>`.
    pub fn redirect_uri(&self) -> &str {
        self.listener.redirect_uri()
    }

    /// Waits for the browser's redirect, checks `state`, exchanges the code and shows
    /// a success or failure page in the browser.
    pub async fn finish(self) -> Result<TokenSet, AuthError> {
        let Self {
            client,
            listener,
            state,
            verifier,
            url: _,
            scopes,
            capability,
            deadline,
        } = self;
        let redirect_uri = listener.redirect_uri().to_owned();
        let (params, pending) = listener.wait(deadline).await?;
        let redemption = Redemption {
            client,
            state,
            verifier,
            scopes,
            capability,
        };
        let result = redemption.complete(&redirect_uri, params).await;
        match &result {
            Ok(_) => {
                tracing::info!(provider = %client.provider.kind(), "OAuth sign-in succeeded");
                pending.succeed().await;
            }
            Err(err) => {
                tracing::warn!(provider = %client.provider.kind(), %err, "OAuth sign-in failed");
                pending.fail(err).await;
            }
        }
        result
    }
}

/// What redeeming the redirect needs from the [`AuthorizationRequest`].
struct Redemption<'a> {
    client: &'a OAuthClient,
    state: String,
    verifier: Option<PkceVerifier>,
    scopes: Vec<String>,
    capability: Capability,
}

impl Redemption<'_> {
    async fn complete(
        &self,
        redirect_uri: &str,
        params: CallbackParams,
    ) -> Result<TokenSet, AuthError> {
        // Check `state` first, also for error responses, so a forged redirect can't
        // inject anything.
        match &params.state {
            Some(state) if constant_time_eq(state, &self.state) => {}
            _ => return Err(AuthError::StateMismatch),
        }
        if let Some(error) = params.error {
            let details = OAuthErrorDetails::new(error, params.error_description, Vec::new());
            return Err(classify(&self.client.provider, details));
        }
        let code = params.code.ok_or_else(|| {
            AuthError::InvalidResponse("the redirect has neither a code nor an error".to_owned())
        })?;
        self.client
            .exchange_code(
                &code,
                redirect_uri,
                self.verifier.as_ref(),
                &self.scopes,
                self.capability,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use reqwest::StatusCode;

    use super::*;
    use crate::ReauthReason;

    fn client(kind: ProviderKind) -> OAuthClient {
        OAuthClient::new(Provider::builtin(kind).with_credentials(ClientCredentials::new("id")))
            .unwrap()
    }

    fn parse(status: u16, body: &str) -> Result<TokenSet, AuthError> {
        parse_for(ProviderKind::Microsoft, Capability::Mail, status, body)
    }

    fn parse_for(
        kind: ProviderKind,
        capability: Capability,
        status: u16,
        body: &str,
    ) -> Result<TokenSet, AuthError> {
        let client = client(kind);
        let requested = client.provider().scopes_for(capability).to_vec();
        client.parse_token_response(
            StatusCode::from_u16(status).unwrap(),
            body.as_bytes(),
            Instant::now(),
            &requested,
            capability,
        )
    }

    #[test]
    fn parses_a_success_response() {
        let before = std::time::Instant::now();
        let tokens = parse(
            200,
            r#"{"access_token":"at","token_type":"Bearer","expires_in":3599,
                "refresh_token":"rt","scope":"https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send"}"#,
        )
        .unwrap();
        assert_eq!(tokens.access_token.expose(), "at");
        assert_eq!(tokens.refresh_token.unwrap().expose(), "rt");
        let expires_at = tokens.expires_at.unwrap();
        assert!(expires_at > before + Duration::from_secs(3590));
        assert_eq!(tokens.scopes.len(), 2);
        assert_eq!(tokens.resource, TokenResource::Primary);
        assert_eq!(tokens.capabilities, Capabilities::MAIL);
    }

    #[test]
    fn granted_scopes_become_capabilities() {
        // Google: one token for everything granted.
        let tokens = parse_for(
            ProviderKind::Google,
            Capability::Mail,
            200,
            r#"{"access_token":"at","scope":"https://www.googleapis.com/auth/contacts.other.readonly https://mail.google.com/ https://www.googleapis.com/auth/contacts"}"#,
        )
        .unwrap();
        assert_eq!(tokens.resource, TokenResource::Primary);
        assert_eq!(
            tokens.capabilities,
            Capabilities::MAIL.with(Capability::Contacts)
        );
        // Unticking "Other contacts" on Google's consent page means no contacts.
        let tokens = parse_for(
            ProviderKind::Google,
            Capability::Mail,
            200,
            r#"{"access_token":"at","scope":"https://mail.google.com/ https://www.googleapis.com/auth/contacts"}"#,
        )
        .unwrap();
        assert_eq!(tokens.capabilities, Capabilities::MAIL);
        // Microsoft Graph lists its scopes in short form.
        let tokens = parse_for(
            ProviderKind::Microsoft,
            Capability::Contacts,
            200,
            r#"{"access_token":"gt","scope":"Contacts.ReadWrite profile openid email"}"#,
        )
        .unwrap();
        assert_eq!(tokens.resource, TokenResource::Graph);
        assert_eq!(tokens.capabilities, Capabilities::CONTACTS);
        // Omitted: the requested scopes were granted.
        let tokens = parse_for(
            ProviderKind::Microsoft,
            Capability::Contacts,
            200,
            r#"{"access_token":"gt"}"#,
        )
        .unwrap();
        assert_eq!(tokens.capabilities, Capabilities::CONTACTS);
        // Mail without SMTP.Send isn't the mail capability (mail still uses the token).
        let tokens = parse(
            200,
            r#"{"access_token":"at","scope":"https://outlook.office.com/IMAP.AccessAsUser.All"}"#,
        )
        .unwrap();
        assert_eq!(tokens.capabilities, Capabilities::NONE);
    }

    #[test]
    fn graph_consent_errors_are_missing_scopes_but_mail_errors_are_not() {
        let body = r#"{"error":"invalid_grant","error_codes":[65001],"error_description":"AADSTS65001: not consented"}"#;
        let err = parse_for(ProviderKind::Microsoft, Capability::Contacts, 400, body).unwrap_err();
        assert_eq!(err.missing_capability(), Some(Capability::Contacts));
        let err = parse_for(ProviderKind::Microsoft, Capability::Mail, 400, body).unwrap_err();
        assert!(
            matches!(err, AuthError::AdminConsentRequired { .. }),
            "{err:?}"
        );
        // Google's contacts share the mail token's request: no remapping.
        let err = parse_for(
            ProviderKind::Google,
            Capability::Contacts,
            400,
            r#"{"error":"invalid_grant"}"#,
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                AuthError::ReauthRequired {
                    reason: ReauthReason::TokenRejected
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn accepts_string_expiry_and_missing_scope() {
        let tokens = parse(200, r#"{"access_token":"at","expires_in":"60"}"#).unwrap();
        assert!(tokens.expires_at.is_some());
        assert!(tokens.refresh_token.is_none());
        assert_eq!(
            tokens.scopes,
            Provider::builtin(ProviderKind::Microsoft).scopes()
        );
        let tokens = parse(200, r#"{"access_token":"at"}"#).unwrap();
        assert!(tokens.expires_at.is_none());
    }

    #[test]
    fn rejects_bad_success_responses() {
        assert!(matches!(
            parse(200, r#"{"token_type":"Bearer"}"#),
            Err(AuthError::InvalidResponse(_))
        ));
        assert!(matches!(
            parse(200, r#"{"access_token":"at","token_type":"mac"}"#),
            Err(AuthError::InvalidResponse(_))
        ));
        let err = parse(200, r#"{"access_token": secret-token}"#).unwrap_err();
        assert!(!err.to_string().contains("secret-token"), "{err}");
    }

    #[test]
    fn maps_error_responses() {
        assert!(matches!(
            parse(
                400,
                r#"{"error":"invalid_grant","error_description":"AADSTS70008: expired"}"#
            ),
            Err(AuthError::ReauthRequired {
                reason: ReauthReason::TokenRejected
            })
        ));
        // Errors with status 200 count too.
        assert!(matches!(
            parse(200, r#"{"error":"invalid_grant"}"#),
            Err(AuthError::ReauthRequired { .. })
        ));
        assert!(matches!(
            parse(
                400,
                r#"{"error":"invalid_grant","error_codes":[65001],"error_description":"AADSTS65001: no consent"}"#
            ),
            Err(AuthError::AdminConsentRequired { code: Some(65001) })
        ));
        assert!(matches!(
            parse(503, "<html>unavailable</html>"),
            Err(AuthError::Http { status: 503 })
        ));
        assert!(matches!(
            parse(500, "{}"),
            Err(AuthError::Http { status: 500 })
        ));
    }

    #[test]
    fn missing_client_id_is_an_error() {
        let provider =
            Provider::builtin(ProviderKind::Google).with_credentials(ClientCredentials::new(" "));
        assert!(matches!(
            OAuthClient::new(provider),
            Err(AuthError::MissingClientId {
                provider: ProviderKind::Google
            })
        ));
    }

    #[tokio::test]
    async fn builds_the_authorization_url() {
        let client = OAuthClient::new(
            Provider::builtin(ProviderKind::Google)
                .with_credentials(ClientCredentials::new("cid").with_secret("not-secret")),
        )
        .unwrap();
        let request = client
            .begin_authorization(&AuthorizeOptions::new().login_hint("bob@gmail.com"))
            .await
            .unwrap();
        let url = request.url();
        assert_eq!(url.host_str(), Some("accounts.google.com"));
        assert_eq!(url.path(), "/o/oauth2/auth");
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], "cid");
        assert_eq!(query["redirect_uri"], request.redirect_uri());
        assert_eq!(query["scope"], "https://mail.google.com/");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(
            query["code_challenge"],
            request.verifier.as_ref().unwrap().challenge()
        );
        assert_eq!(query["state"], request.state);
        assert_eq!(query["login_hint"], "bob@gmail.com");
        assert!(!query.contains_key("include_granted_scopes"));
        assert!(!query.contains_key("prompt"));
        // The secret only goes to the token endpoint.
        assert!(!url.as_str().contains("not-secret"));
        assert!(!format!("{request:?}").contains("bob@gmail.com"));
    }

    async fn authorization_query(
        kind: ProviderKind,
        options: AuthorizeOptions,
    ) -> std::collections::HashMap<String, String> {
        let client = client(kind);
        let request = client.begin_authorization(&options).await.unwrap();
        request.url().query_pairs().into_owned().collect()
    }

    #[tokio::test]
    async fn google_contacts_consent_is_incremental() {
        let query = authorization_query(
            ProviderKind::Google,
            AuthorizeOptions::new()
                .capabilities(Capabilities::CONTACTS)
                .incremental_consent(),
        )
        .await;
        assert_eq!(
            query["scope"],
            "https://www.googleapis.com/auth/contacts \
             https://www.googleapis.com/auth/contacts.other.readonly"
        );
        assert_eq!(query["include_granted_scopes"], "true");
        assert_eq!(query["prompt"], "consent");

        // A new account can ask for both at once.
        let query = authorization_query(
            ProviderKind::Google,
            AuthorizeOptions::new().capabilities(Capabilities::MAIL.with(Capability::Contacts)),
        )
        .await;
        assert_eq!(
            query["scope"],
            "https://mail.google.com/ https://www.googleapis.com/auth/contacts \
             https://www.googleapis.com/auth/contacts.other.readonly"
        );
        assert!(!query.contains_key("include_granted_scopes"));
    }

    #[tokio::test]
    async fn microsoft_contacts_consent_asks_for_graph() {
        let query = authorization_query(
            ProviderKind::Microsoft,
            AuthorizeOptions::new()
                .capabilities(Capabilities::CONTACTS)
                .incremental_consent(),
        )
        .await;
        assert_eq!(
            query["scope"],
            "https://graph.microsoft.com/Contacts.ReadWrite offline_access"
        );
        // Microsoft keeps consent per app and user: nothing else to send.
        assert!(!query.contains_key("include_granted_scopes"));
        assert!(!query.contains_key("prompt"));

        let query = authorization_query(
            ProviderKind::Microsoft,
            AuthorizeOptions::new().capabilities(Capabilities::MAIL.with(Capability::Contacts)),
        )
        .await;
        assert_eq!(
            query["scope"],
            "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send \
             offline_access https://graph.microsoft.com/Contacts.ReadWrite"
        );
    }
}
