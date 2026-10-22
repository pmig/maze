//! OAuth providers: endpoints, scopes and client credentials, mirroring Thunderbird's
//! `OAuth2Providers.sys.mjs`.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use mazemail_types::Sensitive;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{AuthError, OAuthClient};

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

/// One OAuth provider: endpoints, scopes, PKCE and the configured client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    kind: ProviderKind,
    authorization_endpoint: Url,
    token_endpoint: Url,
    scopes: Vec<String>,
    use_pkce: bool,
    credentials: Option<ClientCredentials>,
}

impl Provider {
    /// Google's endpoints and the `https://mail.google.com/` scope (IMAP and SMTP).
    pub const GOOGLE_AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/auth";
    /// Google's token endpoint.
    pub const GOOGLE_TOKEN_ENDPOINT: &str = "https://www.googleapis.com/oauth2/v3/token";
    /// Microsoft's authorization endpoint for personal and work accounts.
    pub const MICROSOFT_AUTHORIZATION_ENDPOINT: &str =
        "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
    /// Microsoft's token endpoint for personal and work accounts.
    pub const MICROSOFT_TOKEN_ENDPOINT: &str =
        "https://login.microsoftonline.com/common/oauth2/v2.0/token";

    /// The built-in provider, without client credentials.
    pub fn builtin(kind: ProviderKind) -> Self {
        let (authorization, token, scopes): (_, _, &[&str]) = match kind {
            ProviderKind::Google => (
                Self::GOOGLE_AUTHORIZATION_ENDPOINT,
                Self::GOOGLE_TOKEN_ENDPOINT,
                &["https://mail.google.com/"],
            ),
            ProviderKind::Microsoft => (
                Self::MICROSOFT_AUTHORIZATION_ENDPOINT,
                Self::MICROSOFT_TOKEN_ENDPOINT,
                &[
                    "https://outlook.office.com/IMAP.AccessAsUser.All",
                    "https://outlook.office.com/SMTP.Send",
                    "offline_access",
                ],
            ),
        };
        Self {
            kind,
            authorization_endpoint: builtin_url(authorization),
            token_endpoint: builtin_url(token),
            scopes: scopes.iter().map(|&s| s.to_owned()).collect(),
            use_pkce: true,
            credentials: None,
        }
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
        Ok(self)
    }

    /// Replaces the requested scopes.
    #[must_use]
    pub fn with_scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes = scopes.into_iter().map(Into::into).collect();
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

    /// The scopes Maze requests.
    pub fn scopes(&self) -> &[String] {
        &self.scopes
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
}
