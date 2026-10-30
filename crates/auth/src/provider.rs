//! OAuth providers: endpoints, scopes and client credentials, mirroring Thunderbird's
//! `OAuth2Providers.sys.mjs`.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use mazemail_types::{MicrosoftTenant, Sensitive};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{AuthError, Capabilities, Capability, OAuthClient, TokenResource};

/// A built-in OAuth provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Gmail and Google Workspace.
    Google,
    /// Outlook.com and Microsoft 365.
    Microsoft,
}

/// Email domains of Google's consumer mail. Google Workspace domains can't be
/// recognised by name; the user picks Google explicitly.
const GOOGLE_EMAIL_DOMAINS: &[&str] = &["gmail.com", "googlemail.com"];

/// Email domains of Microsoft's consumer mail (Outlook.com and its older brands).
/// Microsoft 365 tenants use their own domains and are recognised by the server host.
const MICROSOFT_EMAIL_DOMAINS: &[&str] = &[
    "outlook.com",
    "hotmail.com",
    "live.com",
    "msn.com",
    "outlook.de",
    "outlook.fr",
    "outlook.es",
    "outlook.it",
    "outlook.jp",
    "hotmail.co.uk",
    "hotmail.de",
    "hotmail.fr",
    "hotmail.es",
    "hotmail.it",
    "live.co.uk",
    "live.de",
    "live.fr",
    "live.it",
    "live.nl",
];

/// Server host suffixes per provider, like Thunderbird's `kHostnames`: a host matches
/// if it equals the suffix or ends with `.` + suffix.
const GOOGLE_HOST_SUFFIXES: &[&str] = &["gmail.com", "googlemail.com"];
const MICROSOFT_HOST_SUFFIXES: &[&str] = &[
    // outlook.office365.com, smtp.office365.com
    "office365.com",
    // outlook.office.com
    "outlook.office.com",
    // imap-mail.outlook.com, smtp-mail.outlook.com
    "outlook.com",
    // imap-mail.hotmail.com (legacy)
    "hotmail.com",
];

impl ProviderKind {
    /// Every built-in provider.
    pub const ALL: [Self; 2] = [Self::Google, Self::Microsoft];

    /// The technical name, as in the settings keys `auth.<name>.client_id`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Microsoft => "microsoft",
        }
    }

    /// The name people read, e.g. in "Sign in with Google".
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Google => "Google",
            Self::Microsoft => "Microsoft",
        }
    }

    /// The provider for a mail address's domain, if it is a well-known consumer domain.
    pub fn from_email(email: &str) -> Option<Self> {
        let (_, domain) = email.rsplit_once('@')?;
        let domain = normalize_host(domain);
        if GOOGLE_EMAIL_DOMAINS.contains(&domain.as_str()) {
            Some(Self::Google)
        } else if MICROSOFT_EMAIL_DOMAINS.contains(&domain.as_str()) {
            Some(Self::Microsoft)
        } else {
            None
        }
    }

    /// The provider for an IMAP or SMTP server host name.
    pub fn from_host(host: &str) -> Option<Self> {
        let host = normalize_host(host);
        let matches = |suffixes: &[&str]| {
            suffixes.iter().any(|suffix| {
                host == *suffix
                    || host
                        .strip_suffix(suffix)
                        .is_some_and(|prefix| prefix.ends_with('.'))
            })
        };
        if matches(GOOGLE_HOST_SUFFIXES) {
            Some(Self::Google)
        } else if matches(MICROSOFT_HOST_SUFFIXES) {
            Some(Self::Microsoft)
        } else {
            None
        }
    }

    /// Detects the provider from the server host first (it also covers custom domains
    /// hosted on Microsoft 365), then from the address. `None` means the user has to
    /// choose, e.g. for Google Workspace.
    pub fn detect(email: Option<&str>, host: Option<&str>) -> Option<Self> {
        host.and_then(Self::from_host)
            .or_else(|| email.and_then(Self::from_email))
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Error for [`ProviderKind::from_str`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown OAuth provider {0:?} (expected \"google\" or \"microsoft\")")]
pub struct ParseProviderKindError(String);

impl FromStr for ProviderKind {
    type Err = ParseProviderKindError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| ParseProviderKindError(s.to_owned()))
    }
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// The OAuth client Maze registers as with a provider, from settings
/// (`auth.<provider>.client_id`, `auth.<provider>.client_secret`).
///
/// Google desktop clients come with a "client secret" that isn't secret (it ships in
/// every copy of the app); it is still kept [`Sensitive`] so it doesn't end up in
/// logs. Microsoft public clients have none.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientCredentials {
    client_id: String,
    client_secret: Option<Sensitive<String>>,
}

impl ClientCredentials {
    /// A public client without secret.
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: None,
        }
    }

    /// Adds a client secret (Google desktop clients).
    #[must_use]
    pub fn with_secret(mut self, client_secret: impl Into<String>) -> Self {
        self.client_secret = Some(Sensitive::new(client_secret.into()));
        self
    }

    /// Credentials from optional settings values; `None` if the client ID is unset or
    /// blank. A blank secret counts as none.
    pub fn from_settings(client_id: Option<&str>, client_secret: Option<&str>) -> Option<Self> {
        let client_id = client_id.map(str::trim).filter(|id| !id.is_empty())?;
        let credentials = Self::new(client_id);
        Some(
            match client_secret.map(str::trim).filter(|s| !s.is_empty()) {
                Some(secret) => credentials.with_secret(secret),
                None => credentials,
            },
        )
    }

    /// The client ID.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// The client secret, if any.
    pub fn client_secret(&self) -> Option<&Sensitive<String>> {
        self.client_secret.as_ref()
    }
}

impl fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret)
            .finish()
    }
}

/// One OAuth provider: endpoints, scopes per [`Capability`], PKCE and the configured
/// client.
///
/// Microsoft's endpoints name a tenant (`https://login.microsoftonline.com/<tenant>/…`,
/// see [`MicrosoftTenant`]); [`Provider::with_tenant`] switches it. Endpoints set
/// with [`Provider::with_endpoints`] are taken as they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    kind: ProviderKind,
    authorization_endpoint: Url,
    token_endpoint: Url,
    /// The scopes for [`Capability::Mail`].
    scopes: Vec<String>,
    /// The scopes for [`Capability::Contacts`].
    contacts_scopes: Vec<String>,
    use_pkce: bool,
    credentials: Option<ClientCredentials>,
    /// Microsoft: the login authority and tenant the endpoints were built from;
    /// `None` for fixed endpoints.
    tenancy: Option<Tenancy>,
}

/// Where Microsoft's tenant endpoints live.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tenancy {
    /// `https://login.microsoftonline.com` (a test server in tests).
    authority: Url,
    tenant: MicrosoftTenant,
}

impl Tenancy {
    /// `<authority>/<tenant>/oauth2/v2.0/{authorize,token}`.
    fn endpoints(&self) -> Result<(Url, Url), AuthError> {
        let base = self.authority.as_str().trim_end_matches('/');
        let tenant = self.tenant.as_str();
        Ok((
            parse_endpoint(&format!("{base}/{tenant}/oauth2/v2.0/authorize"))?,
            parse_endpoint(&format!("{base}/{tenant}/oauth2/v2.0/token"))?,
        ))
    }
}

impl Provider {
    /// Google's endpoints and the `https://mail.google.com/` scope (IMAP and SMTP).
    pub const GOOGLE_AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/auth";
    /// Google's token endpoint.
    pub const GOOGLE_TOKEN_ENDPOINT: &str = "https://www.googleapis.com/oauth2/v3/token";
    /// Microsoft's login authority; the endpoints are below it, per tenant.
    pub const MICROSOFT_AUTHORITY: &str = "https://login.microsoftonline.com";
    /// Microsoft's authorization endpoint for personal and work accounts (tenant
    /// `common`).
    pub const MICROSOFT_AUTHORIZATION_ENDPOINT: &str =
        "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
    /// Microsoft's token endpoint for personal and work accounts (tenant `common`).
    pub const MICROSOFT_TOKEN_ENDPOINT: &str =
        "https://login.microsoftonline.com/common/oauth2/v2.0/token";

    /// Google's scope for IMAP and SMTP.
    pub const GOOGLE_MAIL_SCOPE: &str = "https://mail.google.com/";
    /// Google People API: read and write the user's contacts ("My Contacts").
    pub const GOOGLE_CONTACTS_SCOPE: &str = "https://www.googleapis.com/auth/contacts";
    /// Google People API: read the auto-saved "Other contacts".
    pub const GOOGLE_OTHER_CONTACTS_SCOPE: &str =
        "https://www.googleapis.com/auth/contacts.other.readonly";
    /// Outlook's IMAP scope (resource `outlook.office.com`).
    pub const MICROSOFT_IMAP_SCOPE: &str = "https://outlook.office.com/IMAP.AccessAsUser.All";
    /// Outlook's SMTP scope (resource `outlook.office.com`).
    pub const MICROSOFT_SMTP_SCOPE: &str = "https://outlook.office.com/SMTP.Send";
    /// Microsoft Graph: read and write the user's contacts (resource
    /// `graph.microsoft.com`).
    pub const MICROSOFT_CONTACTS_SCOPE: &str = "https://graph.microsoft.com/Contacts.ReadWrite";
    /// Microsoft's scope for refresh tokens. Not a capability of its own.
    pub const OFFLINE_ACCESS_SCOPE: &str = "offline_access";
    /// The prefix of Microsoft Graph scopes; Graph token responses may list them
    /// without it (`Contacts.ReadWrite`).
    const MICROSOFT_GRAPH_PREFIX: &str = "https://graph.microsoft.com/";

    /// The built-in provider, without client credentials.
    pub fn builtin(kind: ProviderKind) -> Self {
        let (authorization, token, scopes, contacts_scopes): (_, _, &[&str], &[&str]) = match kind {
            ProviderKind::Google => (
                Self::GOOGLE_AUTHORIZATION_ENDPOINT,
                Self::GOOGLE_TOKEN_ENDPOINT,
                &[Self::GOOGLE_MAIL_SCOPE],
                &[
                    Self::GOOGLE_CONTACTS_SCOPE,
                    Self::GOOGLE_OTHER_CONTACTS_SCOPE,
                ],
            ),
            ProviderKind::Microsoft => (
                Self::MICROSOFT_AUTHORIZATION_ENDPOINT,
                Self::MICROSOFT_TOKEN_ENDPOINT,
                &[
                    Self::MICROSOFT_IMAP_SCOPE,
                    Self::MICROSOFT_SMTP_SCOPE,
                    Self::OFFLINE_ACCESS_SCOPE,
                ],
                &[Self::MICROSOFT_CONTACTS_SCOPE, Self::OFFLINE_ACCESS_SCOPE],
            ),
        };
        let owned =
            |scopes: &[&str]| -> Vec<String> { scopes.iter().map(|&s| s.to_owned()).collect() };
        Self {
            kind,
            authorization_endpoint: builtin_url(authorization),
            token_endpoint: builtin_url(token),
            scopes: owned(scopes),
            contacts_scopes: owned(contacts_scopes),
            use_pkce: true,
            credentials: None,
            tenancy: (kind == ProviderKind::Microsoft).then(|| Tenancy {
                authority: builtin_url(Self::MICROSOFT_AUTHORITY),
                tenant: MicrosoftTenant::common(),
            }),
        }
    }

    /// Microsoft with the endpoints of `tenant`, without client credentials.
    pub fn microsoft(tenant: &MicrosoftTenant) -> Self {
        Self::builtin(ProviderKind::Microsoft).with_tenant(tenant)
    }

    /// Uses `tenant`'s endpoints (Microsoft). No change for Google and for endpoints
    /// set with [`Provider::with_endpoints`].
    #[must_use]
    pub fn with_tenant(mut self, tenant: &MicrosoftTenant) -> Self {
        if let Some(tenancy) = &mut self.tenancy {
            tenancy.tenant = tenant.clone();
            // The authority was accepted before and the tenant is a valid path
            // segment, so this doesn't fail; keep the old endpoints if it did.
            if let Ok((authorization, token)) = tenancy.endpoints() {
                self.authorization_endpoint = authorization;
                self.token_endpoint = token;
            }
        }
        self
    }

    /// Moves Microsoft's tenant endpoints below another login authority (a test
    /// server; sovereign clouds). HTTPS, or HTTP on a loopback host.
    pub fn with_authority(mut self, authority: &str) -> Result<Self, AuthError> {
        let authority = parse_endpoint(authority)?;
        if let Some(tenancy) = &mut self.tenancy {
            tenancy.authority = authority;
            let (authorization, token) = tenancy.endpoints()?;
            self.authorization_endpoint = authorization;
            self.token_endpoint = token;
        }
        Ok(self)
    }

    /// The Microsoft tenant the endpoints are for; `None` for Google and fixed
    /// endpoints.
    pub fn tenant(&self) -> Option<&MicrosoftTenant> {
        self.tenancy.as_ref().map(|t| &t.tenant)
    }

    /// Replaces the endpoints, e.g. with a test server or a single-tenant Microsoft
    /// endpoint. Endpoints must be HTTPS; plain HTTP is only accepted for loopback
    /// hosts.
    pub fn with_endpoints(
        mut self,
        authorization_endpoint: &str,
        token_endpoint: &str,
    ) -> Result<Self, AuthError> {
        self.authorization_endpoint = parse_endpoint(authorization_endpoint)?;
        self.token_endpoint = parse_endpoint(token_endpoint)?;
        self.tenancy = None;
        Ok(self)
    }

    /// Replaces the scopes requested for mail ([`Capability::Mail`]).
    #[must_use]
    pub fn with_scopes<I, S>(self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.with_scopes_for(Capability::Mail, scopes)
    }

    /// Replaces the scopes requested for `capability`.
    #[must_use]
    pub fn with_scopes_for<I, S>(mut self, capability: Capability, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let scopes = scopes.into_iter().map(Into::into).collect();
        match capability {
            Capability::Mail => self.scopes = scopes,
            Capability::Contacts => self.contacts_scopes = scopes,
        }
        self
    }

    /// Sets the client credentials.
    #[must_use]
    pub fn with_credentials(mut self, credentials: ClientCredentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Which provider this is.
    pub fn kind(&self) -> ProviderKind {
        self.kind
    }

    /// The authorization endpoint the browser opens.
    pub fn authorization_endpoint(&self) -> &Url {
        &self.authorization_endpoint
    }

    /// The token endpoint for code exchange and refresh.
    pub fn token_endpoint(&self) -> &Url {
        &self.token_endpoint
    }

    /// The scopes Maze requests for mail ([`Capability::Mail`]).
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    /// The scopes Maze requests for `capability`.
    ///
    /// - Google: mail `https://mail.google.com/`; contacts
    ///   `https://www.googleapis.com/auth/contacts` and
    ///   `https://www.googleapis.com/auth/contacts.other.readonly`.
    /// - Microsoft: mail `https://outlook.office.com/IMAP.AccessAsUser.All`,
    ///   `https://outlook.office.com/SMTP.Send` and `offline_access`; contacts
    ///   `https://graph.microsoft.com/Contacts.ReadWrite` and `offline_access`.
    pub fn scopes_for(&self, capability: Capability) -> &[String] {
        match capability {
            Capability::Mail => &self.scopes,
            Capability::Contacts => &self.contacts_scopes,
        }
    }

    /// The scopes of every capability in `capabilities`, without duplicates, in
    /// [`Capability::ALL`] order. Empty `capabilities` means mail.
    pub fn scopes_for_all(&self, capabilities: Capabilities) -> Vec<String> {
        let capabilities = if capabilities.is_empty() {
            Capabilities::MAIL
        } else {
            capabilities
        };
        let mut scopes: Vec<String> = Vec::new();
        for scope in capabilities.iter().flat_map(|c| self.scopes_for(c)) {
            if !scopes.iter().any(|s| s.eq_ignore_ascii_case(scope)) {
                scopes.push(scope.clone());
            }
        }
        scopes
    }

    /// Which access token `capability` needs: Microsoft's contacts live in Graph, a
    /// resource of their own; everything else uses the primary token.
    pub fn resource_for(&self, capability: Capability) -> TokenResource {
        match (self.kind, capability) {
            (ProviderKind::Microsoft, Capability::Contacts) => TokenResource::Graph,
            _ => TokenResource::Primary,
        }
    }

    /// The `scope` parameter of a token request (refresh or code exchange) for
    /// `capability`'s token: Microsoft names the resource's scopes (one access token
    /// per resource, and a refresh token for several resources would otherwise be
    /// ambiguous); Google sends none and gets every granted scope.
    pub(crate) fn token_request_scopes(&self, capability: Capability) -> Option<&[String]> {
        match self.kind {
            ProviderKind::Microsoft => Some(self.scopes_for(capability)),
            ProviderKind::Google => None,
        }
    }

    /// The capabilities `granted` covers: every scope a capability requests is
    /// among them (`offline_access` aside). Comparison ignores ASCII case, and a
    /// Graph scope also matches its short form (`Contacts.ReadWrite`).
    pub fn capabilities_of(&self, granted: &[String]) -> Capabilities {
        Capability::ALL
            .into_iter()
            .filter(|&capability| {
                let mut needed = self
                    .scopes_for(capability)
                    .iter()
                    .filter(|s| !s.eq_ignore_ascii_case(Self::OFFLINE_ACCESS_SCOPE))
                    .peekable();
                needed.peek().is_some()
                    && needed.all(|scope| granted.iter().any(|g| scope_matches(scope, g)))
            })
            .collect()
    }

    /// Whether the flow uses PKCE (RFC 7636). True for both built-in providers.
    pub fn use_pkce(&self) -> bool {
        self.use_pkce
    }

    /// The configured client, if any.
    pub fn credentials(&self) -> Option<&ClientCredentials> {
        self.credentials.as_ref()
    }
}

/// Whether the granted scope `granted` is the requested `requested`: ASCII
/// case-insensitive (Microsoft's casing varies), and Graph scopes also in their short
/// form, which Graph token responses use.
pub(crate) fn scope_matches(requested: &str, granted: &str) -> bool {
    if requested.eq_ignore_ascii_case(granted) {
        return true;
    }
    let prefix = Provider::MICROSOFT_GRAPH_PREFIX;
    requested.len() > prefix.len()
        && requested.is_char_boundary(prefix.len())
        && requested[..prefix.len()].eq_ignore_ascii_case(prefix)
        && requested[prefix.len()..].eq_ignore_ascii_case(granted)
}

/// Parses a built-in endpoint constant.
fn builtin_url(url: &'static str) -> Url {
    // The constants are valid URLs (covered by `builtin_endpoints_parse`), so this
    // never fails at runtime.
    Url::parse(url).expect("built-in OAuth endpoints are valid URLs")
}

/// Parses an endpoint and checks it is HTTPS, or HTTP on a loopback host.
pub(crate) fn parse_endpoint(url: &str) -> Result<Url, AuthError> {
    let invalid = |reason| AuthError::InvalidEndpoint {
        url: url.to_owned(),
        reason,
    };
    let parsed = Url::parse(url).map_err(|_| invalid("not a valid URL"))?;
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if is_loopback(&parsed) => Ok(parsed),
        "http" => Err(invalid("plain HTTP is only allowed for loopback hosts")),
        _ => Err(invalid("the scheme must be https")),
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// The OAuth providers Maze knows, with the client credentials from settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRegistry {
    google: Provider,
    microsoft: Provider,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    /// The built-in providers, without client credentials. There's no bundled Google
    /// client ID (bring your own, CONCEPT §6), and the Maze Entra app ID comes from
    /// settings too until it is registered.
    pub fn new() -> Self {
        Self {
            google: Provider::builtin(ProviderKind::Google),
            microsoft: Provider::builtin(ProviderKind::Microsoft),
        }
    }

    /// Sets a provider's client credentials.
    #[must_use]
    pub fn with_credentials(mut self, kind: ProviderKind, credentials: ClientCredentials) -> Self {
        self.set_credentials(kind, Some(credentials));
        self
    }

    /// Sets or clears a provider's client credentials, e.g. after a settings change.
    pub fn set_credentials(&mut self, kind: ProviderKind, credentials: Option<ClientCredentials>) {
        self.slot(kind).credentials = credentials;
    }

    /// Uses `tenant` for Microsoft sign-ins (`auth.microsoft.tenant`).
    pub fn set_microsoft_tenant(&mut self, tenant: &MicrosoftTenant) {
        self.microsoft = self.microsoft.clone().with_tenant(tenant);
    }

    /// The tenant of Microsoft sign-ins; `None` when the Microsoft provider has fixed
    /// endpoints.
    pub fn microsoft_tenant(&self) -> Option<&MicrosoftTenant> {
        self.microsoft.tenant()
    }

    /// Replaces a provider wholesale (custom endpoints, tests).
    pub fn set_provider(&mut self, provider: Provider) {
        let kind = provider.kind;
        *self.slot(kind) = provider;
    }

    /// The provider of `kind`.
    pub fn get(&self, kind: ProviderKind) -> &Provider {
        match kind {
            ProviderKind::Google => &self.google,
            ProviderKind::Microsoft => &self.microsoft,
        }
    }

    /// The provider detected from an address and/or server host
    /// ([`ProviderKind::detect`]).
    pub fn detect(&self, email: Option<&str>, host: Option<&str>) -> Option<&Provider> {
        ProviderKind::detect(email, host).map(|kind| self.get(kind))
    }

    /// An [`OAuthClient`] for `kind`; fails with [`AuthError::MissingClientId`] when no
    /// client ID is configured.
    pub fn client(&self, kind: ProviderKind) -> Result<OAuthClient, AuthError> {
        OAuthClient::new(self.get(kind).clone())
    }

    /// Like [`ProviderRegistry::client`], for Microsoft with `tenant`'s endpoints (an
    /// account keeps the tenant it signed in with). `None`, or Google: the
    /// registry's.
    pub fn client_for_tenant(
        &self,
        kind: ProviderKind,
        tenant: Option<&MicrosoftTenant>,
    ) -> Result<OAuthClient, AuthError> {
        let provider = self.get(kind).clone();
        OAuthClient::new(match tenant {
            Some(tenant) => provider.with_tenant(tenant),
            None => provider,
        })
    }

    fn slot(&mut self, kind: ProviderKind) -> &mut Provider {
        match kind {
            ProviderKind::Google => &mut self.google,
            ProviderKind::Microsoft => &mut self.microsoft,
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn builtin_endpoints_parse() {
        for kind in ProviderKind::ALL {
            let provider = Provider::builtin(kind);
            assert_eq!(provider.authorization_endpoint().scheme(), "https");
            assert_eq!(provider.token_endpoint().scheme(), "https");
            assert!(provider.use_pkce());
            assert!(provider.credentials().is_none());
        }
    }

    #[test]
    fn builtin_providers_mirror_thunderbird() {
        let google = Provider::builtin(ProviderKind::Google);
        assert_eq!(
            google.authorization_endpoint().as_str(),
            "https://accounts.google.com/o/oauth2/auth"
        );
        assert_eq!(
            google.token_endpoint().as_str(),
            "https://www.googleapis.com/oauth2/v3/token"
        );
        assert_eq!(google.scopes(), ["https://mail.google.com/"]);

        let microsoft = Provider::builtin(ProviderKind::Microsoft);
        assert_eq!(
            microsoft.authorization_endpoint().as_str(),
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize"
        );
        assert_eq!(
            microsoft.token_endpoint().as_str(),
            "https://login.microsoftonline.com/common/oauth2/v2.0/token"
        );
        assert_eq!(
            microsoft.scopes(),
            [
                "https://outlook.office.com/IMAP.AccessAsUser.All",
                "https://outlook.office.com/SMTP.Send",
                "offline_access"
            ]
        );
    }

    #[test]
    fn microsoft_endpoints_follow_the_tenant() {
        for (tenant, expected) in [
            ("common", "common"),
            ("organizations", "organizations"),
            ("consumers", "consumers"),
            ("contoso.com", "contoso.com"),
            (
                "72f988bf-86f1-41af-91ab-2d7cd011db47",
                "72f988bf-86f1-41af-91ab-2d7cd011db47",
            ),
        ] {
            let tenant = MicrosoftTenant::parse(tenant).unwrap();
            let provider = Provider::microsoft(&tenant);
            assert_eq!(
                provider.authorization_endpoint().as_str(),
                format!("https://login.microsoftonline.com/{expected}/oauth2/v2.0/authorize")
            );
            assert_eq!(
                provider.token_endpoint().as_str(),
                format!("https://login.microsoftonline.com/{expected}/oauth2/v2.0/token")
            );
            assert_eq!(provider.tenant(), Some(&tenant));
        }
        assert_eq!(
            Provider::builtin(ProviderKind::Microsoft).tenant(),
            Some(&MicrosoftTenant::common())
        );
        // Google has no tenants.
        let contoso = MicrosoftTenant::parse("contoso.com").unwrap();
        let google = Provider::builtin(ProviderKind::Google).with_tenant(&contoso);
        assert_eq!(google, Provider::builtin(ProviderKind::Google));
        assert_eq!(google.tenant(), None);
    }

    #[test]
    fn a_test_authority_keeps_the_tenant_path() {
        let contoso = MicrosoftTenant::parse("contoso.com").unwrap();
        let provider = Provider::microsoft(&contoso)
            .with_authority("http://127.0.0.1:9/")
            .unwrap();
        assert_eq!(
            provider.token_endpoint().as_str(),
            "http://127.0.0.1:9/contoso.com/oauth2/v2.0/token"
        );
        let common = provider.with_tenant(&MicrosoftTenant::common());
        assert_eq!(
            common.authorization_endpoint().as_str(),
            "http://127.0.0.1:9/common/oauth2/v2.0/authorize"
        );
        assert!(matches!(
            Provider::builtin(ProviderKind::Microsoft).with_authority("http://example.org"),
            Err(AuthError::InvalidEndpoint { .. })
        ));
        // Fixed endpoints stay as they are.
        let fixed = Provider::builtin(ProviderKind::Microsoft)
            .with_endpoints("http://127.0.0.1:9/authorize", "http://127.0.0.1:9/token")
            .unwrap();
        assert_eq!(fixed.tenant(), None);
        assert_eq!(
            fixed.with_tenant(&contoso).token_endpoint().as_str(),
            "http://127.0.0.1:9/token"
        );
    }

    #[test]
    fn the_registry_uses_the_tenant_of_the_account() {
        let contoso = MicrosoftTenant::parse("contoso.com").unwrap();
        let mut registry = ProviderRegistry::new()
            .with_credentials(ProviderKind::Microsoft, ClientCredentials::new("app"));
        assert_eq!(
            registry.microsoft_tenant(),
            Some(&MicrosoftTenant::common())
        );
        registry.set_microsoft_tenant(&contoso);
        assert_eq!(registry.microsoft_tenant(), Some(&contoso));
        assert!(
            registry
                .get(ProviderKind::Microsoft)
                .credentials()
                .is_some(),
            "the client stays"
        );
        let client = registry
            .client_for_tenant(ProviderKind::Microsoft, Some(&MicrosoftTenant::common()))
            .unwrap();
        assert_eq!(
            client.provider().token_endpoint().as_str(),
            "https://login.microsoftonline.com/common/oauth2/v2.0/token"
        );
        let client = registry
            .client_for_tenant(ProviderKind::Microsoft, None)
            .unwrap();
        assert_eq!(
            client.provider().token_endpoint().as_str(),
            "https://login.microsoftonline.com/contoso.com/oauth2/v2.0/token"
        );
    }

    #[test]
    fn detects_by_email_domain() {
        assert_eq!(
            ProviderKind::from_email("Bob@Gmail.com"),
            Some(ProviderKind::Google)
        );
        assert_eq!(
            ProviderKind::from_email("bob@googlemail.com"),
            Some(ProviderKind::Google)
        );
        for domain in ["outlook.com", "hotmail.com", "live.com", "hotmail.co.uk"] {
            assert_eq!(
                ProviderKind::from_email(&format!("bob@{domain}")),
                Some(ProviderKind::Microsoft),
                "{domain}"
            );
        }
        // Google Workspace and Microsoft 365 custom domains can't be detected by name.
        assert_eq!(ProviderKind::from_email("bob@example.org"), None);
        assert_eq!(ProviderKind::from_email("not-an-address"), None);
        assert_eq!(ProviderKind::from_email("bob@notgmail.com"), None);
    }

    #[test]
    fn detects_by_host() {
        for host in ["imap.gmail.com", "smtp.gmail.com", "imap.googlemail.com"] {
            assert_eq!(
                ProviderKind::from_host(host),
                Some(ProviderKind::Google),
                "{host}"
            );
        }
        for host in [
            "outlook.office365.com",
            "smtp.office365.com",
            "outlook.office.com",
            "smtp-mail.outlook.com",
            "OUTLOOK.OFFICE365.COM.",
        ] {
            assert_eq!(
                ProviderKind::from_host(host),
                Some(ProviderKind::Microsoft),
                "{host}"
            );
        }
        assert_eq!(ProviderKind::from_host("imap.example.org"), None);
        assert_eq!(ProviderKind::from_host("evilgmail.com"), None);
        assert_eq!(ProviderKind::from_host("gmail.com.evil.org"), None);
    }

    #[test]
    fn host_wins_over_address() {
        assert_eq!(
            ProviderKind::detect(Some("bob@example.org"), Some("outlook.office365.com")),
            Some(ProviderKind::Microsoft)
        );
        assert_eq!(
            ProviderKind::detect(Some("bob@gmail.com"), Some("imap.example.org")),
            Some(ProviderKind::Google)
        );
        assert_eq!(ProviderKind::detect(None, None), None);
    }

    #[test]
    fn kind_names_round_trip() {
        for kind in ProviderKind::ALL {
            assert_eq!(kind.as_str().parse::<ProviderKind>().unwrap(), kind);
        }
        assert!("yahoo".parse::<ProviderKind>().is_err());
    }

    #[test]
    fn endpoints_must_be_https_or_loopback() {
        let provider = Provider::builtin(ProviderKind::Google);
        assert!(
            provider
                .clone()
                .with_endpoints("http://127.0.0.1:8080/auth", "http://localhost/token")
                .is_ok()
        );
        assert!(
            provider
                .clone()
                .with_endpoints("http://[::1]/auth", "https://example.org/token")
                .is_ok()
        );
        assert!(matches!(
            provider
                .clone()
                .with_endpoints("http://example.org/auth", "https://example.org/token"),
            Err(AuthError::InvalidEndpoint { .. })
        ));
        assert!(matches!(
            provider.with_endpoints("ftp://example.org/", "https://example.org/token"),
            Err(AuthError::InvalidEndpoint { .. })
        ));
    }

    #[test]
    fn credentials_from_settings() {
        assert_eq!(ClientCredentials::from_settings(None, Some("x")), None);
        assert_eq!(ClientCredentials::from_settings(Some("  "), None), None);
        let creds = ClientCredentials::from_settings(Some(" id "), Some("")).unwrap();
        assert_eq!(creds.client_id(), "id");
        assert!(creds.client_secret().is_none());
        let creds = ClientCredentials::from_settings(Some("id"), Some("shh")).unwrap();
        assert_eq!(creds.client_secret().unwrap().expose(), "shh");
        assert!(!format!("{creds:?}").contains("shh"));
    }

    #[test]
    fn registry_requires_a_client_id() {
        let registry = ProviderRegistry::new();
        assert!(matches!(
            registry.client(ProviderKind::Google),
            Err(AuthError::MissingClientId {
                provider: ProviderKind::Google
            })
        ));
        let registry =
            registry.with_credentials(ProviderKind::Microsoft, ClientCredentials::new("app-id"));
        let client = registry.client(ProviderKind::Microsoft).unwrap();
        assert_eq!(client.provider().kind(), ProviderKind::Microsoft);
        assert_eq!(
            registry
                .detect(Some("bob@hotmail.com"), None)
                .map(Provider::kind),
            Some(ProviderKind::Microsoft)
        );
    }

    #[test]
    fn scopes_per_capability() {
        let google = Provider::builtin(ProviderKind::Google);
        assert_eq!(
            google.scopes_for(Capability::Contacts),
            [
                "https://www.googleapis.com/auth/contacts",
                "https://www.googleapis.com/auth/contacts.other.readonly"
            ]
        );
        assert_eq!(google.scopes_for(Capability::Mail), google.scopes());
        assert_eq!(
            google.resource_for(Capability::Contacts),
            TokenResource::Primary
        );
        assert_eq!(google.token_request_scopes(Capability::Contacts), None);

        let microsoft = Provider::builtin(ProviderKind::Microsoft);
        assert_eq!(
            microsoft.scopes_for(Capability::Contacts),
            [
                "https://graph.microsoft.com/Contacts.ReadWrite",
                "offline_access"
            ]
        );
        assert_eq!(
            microsoft.resource_for(Capability::Mail),
            TokenResource::Primary
        );
        assert_eq!(
            microsoft.resource_for(Capability::Contacts),
            TokenResource::Graph
        );
        assert_eq!(
            microsoft.token_request_scopes(Capability::Mail),
            Some(microsoft.scopes())
        );
        // `offline_access` once, empty means mail.
        assert_eq!(
            microsoft.scopes_for_all(Capabilities::MAIL.with(Capability::Contacts)),
            [
                "https://outlook.office.com/IMAP.AccessAsUser.All",
                "https://outlook.office.com/SMTP.Send",
                "offline_access",
                "https://graph.microsoft.com/Contacts.ReadWrite",
            ]
        );
        assert_eq!(
            microsoft.scopes_for_all(Capabilities::NONE),
            microsoft.scopes()
        );

        let custom = google.with_scopes_for(Capability::Contacts, ["x"]);
        assert_eq!(custom.scopes_for(Capability::Contacts), ["x"]);
    }

    #[test]
    fn capabilities_of_granted_scopes() {
        let owned =
            |scopes: &[&str]| -> Vec<String> { scopes.iter().map(|&s| s.to_owned()).collect() };
        let google = Provider::builtin(ProviderKind::Google);
        assert_eq!(
            google.capabilities_of(&owned(&["https://mail.google.com/"])),
            Capabilities::MAIL
        );
        assert_eq!(
            google.capabilities_of(&owned(&[
                "https://www.googleapis.com/auth/contacts",
                "https://www.googleapis.com/auth/contacts.other.readonly",
            ])),
            Capabilities::CONTACTS
        );
        assert_eq!(google.capabilities_of(&[]), Capabilities::NONE);

        let microsoft = Provider::builtin(ProviderKind::Microsoft);
        // `offline_access` isn't needed in the answer; casing doesn't matter.
        assert_eq!(
            microsoft.capabilities_of(&owned(&[
                "https://outlook.office.com/imap.accessasuser.all",
                "https://outlook.office.com/SMTP.Send",
            ])),
            Capabilities::MAIL
        );
        assert_eq!(
            microsoft.capabilities_of(&owned(&["contacts.readwrite"])),
            Capabilities::CONTACTS
        );
        assert_eq!(
            microsoft.capabilities_of(&owned(&["offline_access"])),
            Capabilities::NONE
        );
        // A capability without scopes is never granted.
        let none = microsoft.with_scopes_for(Capability::Contacts, ["offline_access"]);
        assert_eq!(
            none.capabilities_of(&owned(&["offline_access"])),
            Capabilities::NONE
        );
    }

    #[test]
    fn graph_scopes_match_their_short_form() {
        let graph = Provider::MICROSOFT_CONTACTS_SCOPE;
        assert!(scope_matches(graph, "Contacts.ReadWrite"));
        assert!(scope_matches(
            graph,
            "https://GRAPH.microsoft.com/contacts.readwrite"
        ));
        assert!(!scope_matches(graph, "Contacts.Read"));
        assert!(!scope_matches(
            Provider::MICROSOFT_IMAP_SCOPE,
            "IMAP.AccessAsUser.All"
        ));
        assert!(!scope_matches("https://graph.microsoft.com/", ""));
    }
}
