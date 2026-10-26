//! Mail accounts: adding (IMAP with a password, or Google/Microsoft sign-in), listing
//! with sync status, and removing (PLAN §4.6, CONCEPT §6).
//!
//! # Adding an account
//!
//! [`CoreHandle::add_imap_account`] and [`CoreHandle::add_oauth_account`] first prove
//! that the account works: they connect, log in and list the folders. Only then is
//! anything saved: the account row (with its [`AccountConfig`] in `config_json`), a
//! default identity, and the secret under the account's UUID (the password, or the
//! OAuth refresh token) in the encrypted store ([`crate::secrets`]). If saving the
//! secret fails, the account row is removed again. Then the account's sync actor starts. The identity gets an SMTP
//! server: the one given, or the derived one ([`suggest_smtp_server`]).
//!
//! The account kind follows the server: a server with Gmail's `X-GM-EXT-1` is a Gmail
//! account (labels, archive by removing `\Inbox`), Microsoft hosts are Microsoft 365,
//! everything else is IMAP. Gmail and Microsoft 365 accounts store their built-in
//! sender-verification [`ProviderProfile`] in the config. IMAP accounts store none:
//! when a message is verified they get an automatic profile from the address's
//! domain, its MX hosts and the IMAP host ([`ProviderProfile::automatic`]), which
//! trusts the `Authentication-Results` of the server that accepted the message from
//! outside. A profile stored by an earlier version (the IMAP host as the only
//! trusted host, which real servers almost never match) is replaced the same way.
//!
//! # Removing an account
//!
//! [`CoreHandle::remove_account`] stops its sync, deletes its folders, messages,
//! threads, identities and pending ops from the store (blob references are released and
//! the blobs collected), drops its index documents and deletes its secrets, so
//! nothing of it remains that the user couldn't decrypt or delete later.
//!
//! # Server suggestions
//!
//! [`suggest_imap_server`] fills in host, port and security for well-known providers
//! (and `imap.<domain>` otherwise) and says when the provider wants a browser sign-in.

use std::sync::Arc;
use std::time::Duration;

use auth::{
    AccountSecrets, AuthorizeOptions, ClientCredentials, MicrosoftTenant, OAuthTokenManager,
    ProviderKind, ProviderRegistry, Uuid,
};
use imap::ImapAuth;
use mazemail_types::{AccountId, Sensitive};
use security::{HostMatcher, ProviderProfile};
use serde::{Deserialize, Serialize};
use store::{Account, AccountKind, IdentityInput, NewAccount, Queries as _};

use crate::credentials::AccountCredentials;
use crate::handle::CoreHandle;
use crate::outgoing::{SmtpServerSettings, smtp_server_input, suggest_smtp_server};
use crate::sync_bridge::SyncStatus;
use crate::{Error, Result};

/// How long validating a new account may take (connect, log in, list folders).
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Transport security of an IMAP connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImapSecurity {
    /// TLS from the first byte (port 993).
    #[default]
    Tls,
    /// Plain TCP upgraded with `STARTTLS` (port 143); never falls back to plain text.
    Starttls,
    /// No encryption. Only for local test servers: the password travels in clear text.
    #[serde(rename = "none")]
    Insecure,
}

impl ImapSecurity {
    /// The usual port.
    pub fn default_port(self) -> u16 {
        match self {
            Self::Tls => 993,
            Self::Starttls | Self::Insecure => 143,
        }
    }

    /// The name in configs and on the command line (`tls`, `starttls`, `none`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tls => "tls",
            Self::Starttls => "starttls",
            Self::Insecure => "none",
        }
    }

    pub(crate) fn to_imap(self) -> imap::Security {
        match self {
            Self::Tls => imap::Security::Tls,
            Self::Starttls => imap::Security::StartTls,
            Self::Insecure => imap::Security::PlainInsecure,
        }
    }
}

impl std::str::FromStr for ImapSecurity {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "tls" | "ssl" | "ssl/tls" => Ok(Self::Tls),
            "starttls" => Ok(Self::Starttls),
            "none" | "plain" | "insecure" => Ok(Self::Insecure),
            other => Err(format!(
                "unknown security {other:?} (expected tls, starttls or none)"
            )),
        }
    }
}

/// An IMAP server.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImapServer {
    /// Host name.
    pub host: String,
    /// Port.
    pub port: u16,
    /// Transport security.
    pub security: ImapSecurity,
}

impl ImapServer {
    /// A server with the usual port for `security`.
    pub fn new(host: impl Into<String>, security: ImapSecurity) -> Self {
        Self {
            host: host.into(),
            port: security.default_port(),
            security,
        }
    }
}

/// How an account logs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "method", content = "provider", rename_all = "snake_case")]
pub enum AccountAuth {
    /// A stored password.
    Password,
    /// OAuth2 with this provider (XOAUTH2).
    OAuth(ProviderKind),
}

/// The connection settings core keeps in an account's `config_json`.
///
/// The field names match what the Thunderbird importer writes (`host`, `port`,
/// `security`, `username`, `auth`), so imported IMAP accounts sync once their
/// password is entered. Unknown keys are kept when the config is rewritten.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountConfig {
    /// The account's address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// IMAP host.
    pub host: String,
    /// IMAP port (0: the default for `security`).
    #[serde(default)]
    pub port: u16,
    /// Transport security.
    #[serde(default)]
    pub security: ImapSecurity,
    /// IMAP login name.
    #[serde(default)]
    pub username: String,
    /// `password` or `oauth2` (other values: an importer's, not supported for sync).
    #[serde(default = "default_auth")]
    pub auth: String,
    /// The OAuth provider for `oauth2`; detected from host and address if missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_provider: Option<ProviderKind>,
    /// The Microsoft tenant the account signed in with (`auth.microsoft.tenant` at
    /// the time); token refreshes use it. Missing (accounts of earlier versions):
    /// `common`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_tenant: Option<String>,
    /// How to recognize the provider's trusted `Authentication-Results`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_profile: Option<ProviderProfile>,
    /// Everything else (the importer's origin, the hierarchy delimiter, …).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn default_auth() -> String {
    "password".to_owned()
}

impl AccountConfig {
    /// Parses an account's `config_json`; `None` for accounts without an IMAP server
    /// (legacy Local Folders, empty configs).
    pub fn parse(config_json: &str) -> Option<Self> {
        serde_json::from_str::<Self>(config_json)
            .ok()
            .filter(|c| !c.host.trim().is_empty())
    }

    /// The server.
    pub fn server(&self) -> ImapServer {
        ImapServer {
            host: self.host.clone(),
            port: if self.port == 0 {
                self.security.default_port()
            } else {
                self.port
            },
            security: self.security,
        }
    }

    /// How the account logs in; `None` for methods sync doesn't support.
    pub fn auth(&self) -> Option<AccountAuth> {
        match self.auth.as_str() {
            "password" => Some(AccountAuth::Password),
            "oauth2" | "oauth" => self
                .oauth_provider
                .or_else(|| ProviderKind::detect(self.email.as_deref(), Some(&self.host)))
                .map(AccountAuth::OAuth),
            _ => None,
        }
    }

    /// The Microsoft tenant for token refreshes: the stored one, `common` if there is
    /// none or it isn't valid.
    pub fn microsoft_tenant(&self) -> MicrosoftTenant {
        match self.oauth_tenant.as_deref().map(MicrosoftTenant::parse) {
            Some(Ok(tenant)) => tenant,
            Some(Err(err)) => {
                tracing::warn!(%err, "an account's stored Microsoft tenant; using common");
                MicrosoftTenant::common()
            }
            None => MicrosoftTenant::common(),
        }
    }

    /// The IMAP login name (the address if none is set).
    pub fn login(&self) -> String {
        if self.username.trim().is_empty() {
            self.email.clone().unwrap_or_default()
        } else {
            self.username.clone()
        }
    }

    /// The stored provider profile, unless it is the one Maze stored for IMAP
    /// accounts before automatic profiles (the IMAP host as the only inbound host and
    /// `authserv-id`, which real servers almost never match).
    pub fn custom_provider_profile(&self) -> Option<ProviderProfile> {
        let host = self.host.trim().trim_end_matches('.').to_ascii_lowercase();
        let legacy = ProviderProfile::generic(host.clone(), vec![HostMatcher::Exact(host)]);
        self.provider_profile
            .clone()
            .filter(|profile| *profile != legacy)
    }

    /// The domain of the account's address, for its automatic provider profile.
    pub fn address_domain(&self) -> Option<String> {
        let address = self.email.clone().unwrap_or_else(|| self.login());
        address
            .rsplit_once('@')
            .map(|(_, domain)| domain.trim().to_ascii_lowercase())
            .filter(|domain| !domain.is_empty())
    }

    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }
}

/// The sender-verification profile stored for a new account (see the
/// [module docs](self)): the built-in profile for Gmail and Microsoft 365. Other
/// accounts store none and get an automatic one when a message is verified
/// ([`AccountConfig::custom_provider_profile`]).
pub fn provider_profile(kind: AccountKind) -> Option<ProviderProfile> {
    match kind {
        AccountKind::Gmail => Some(ProviderProfile::gmail()),
        AccountKind::M365 => Some(ProviderProfile::microsoft365()),
        _ => None,
    }
}

/// What [`suggest_imap_server`] proposes for an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSuggestion {
    /// The IMAP server.
    pub server: ImapServer,
    /// The provider wants (or only allows) a browser sign-in.
    pub oauth: Option<ProviderKind>,
    /// The provider is in the built-in list (otherwise the server is a guess).
    pub known: bool,
}

/// Built-in IMAP servers of common providers: `(domains, host)`, all TLS on 993.
const KNOWN_SERVERS: &[(&[&str], &str)] = &[
    (&["gmail.com", "googlemail.com"], "imap.gmail.com"),
    (
        &[
            "outlook.com",
            "hotmail.com",
            "live.com",
            "msn.com",
            "outlook.de",
            "hotmail.de",
            "live.de",
            "hotmail.co.uk",
            "live.co.uk",
            "outlook.fr",
            "hotmail.fr",
            "live.fr",
        ],
        "outlook.office365.com",
    ),
    (&["icloud.com", "me.com", "mac.com"], "imap.mail.me.com"),
    (
        &["yahoo.com", "yahoo.de", "yahoo.co.uk", "ymail.com"],
        "imap.mail.yahoo.com",
    ),
    (&["aol.com"], "imap.aol.com"),
    (
        &["fastmail.com", "fastmail.fm", "messagingengine.com"],
        "imap.fastmail.com",
    ),
    (&["gmx.de", "gmx.net", "gmx.at", "gmx.ch"], "imap.gmx.net"),
    (&["gmx.com"], "imap.gmx.com"),
    (&["web.de"], "imap.web.de"),
    (&["posteo.de", "posteo.net"], "posteo.de"),
    (&["mailbox.org"], "imap.mailbox.org"),
    (&["zoho.com", "zohomail.com"], "imap.zoho.com"),
    (&["t-online.de"], "secureimap.t-online.de"),
    (&["freenet.de"], "mx.freenet.de"),
];

/// Suggests the IMAP server for `email`: a well-known provider's, else
/// `imap.<domain>` with TLS on port 993. `None` without a domain.
pub fn suggest_imap_server(email: &str) -> Option<ServerSuggestion> {
    let (_, domain) = email.trim().rsplit_once('@')?;
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() || !domain.contains('.') {
        return None;
    }
    let oauth = ProviderKind::from_email(&domain_address(&domain));
    if let Some((_, host)) = KNOWN_SERVERS
        .iter()
        .find(|(domains, _)| domains.contains(&domain.as_str()))
    {
        return Some(ServerSuggestion {
            server: ImapServer::new(*host, ImapSecurity::Tls),
            oauth,
            known: true,
        });
    }
    Some(ServerSuggestion {
        server: ImapServer::new(format!("imap.{domain}"), ImapSecurity::Tls),
        oauth,
        known: false,
    })
}

fn domain_address(domain: &str) -> String {
    format!("x@{domain}")
}

/// What adding an IMAP account with a password needs.
#[derive(Clone)]
pub struct AddImapAccount {
    /// The name in the folder pane; the address if `None`.
    pub display_name: Option<String>,
    /// The account's address (also the identity's).
    pub email: String,
    /// The server.
    pub server: ImapServer,
    /// The login name; the address if `None`.
    pub username: Option<String>,
    /// The password (goes to the encrypted store).
    pub password: Sensitive<String>,
    /// The SMTP server for sending; `None` derives it ([`suggest_smtp_server`]). The
    /// login is the account's.
    pub smtp: Option<SmtpServerSettings>,
}

impl std::fmt::Debug for AddImapAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddImapAccount")
            .field("port", &self.server.port)
            .field("security", &self.server.security)
            .finish_non_exhaustive()
    }
}

/// What adding an account with a browser sign-in needs.
#[derive(Clone)]
pub struct AddOAuthAccount {
    /// Google or Microsoft.
    pub provider: ProviderKind,
    /// The account's address (the sign-in hint and the identity's address).
    pub email: String,
    /// The name in the folder pane; the address if `None`.
    pub display_name: Option<String>,
    /// Microsoft: the tenant to sign in with; `None` uses `auth.microsoft.tenant`.
    /// Ignored for Google.
    pub tenant: Option<MicrosoftTenant>,
}

impl std::fmt::Debug for AddOAuthAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddOAuthAccount")
            .field("provider", &self.provider)
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

/// An account as the account list shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct AccountInfo {
    /// The account.
    pub id: AccountId,
    /// Its stable UUID (its secrets are keyed by it).
    pub uuid: Uuid,
    /// The name in the folder pane.
    pub name: String,
    /// The kind.
    pub kind: AccountKind,
    /// The address, if known.
    pub email: Option<String>,
    /// The IMAP server, for synced accounts.
    pub server: Option<ImapServer>,
    /// How it logs in, for synced accounts.
    pub auth: Option<AccountAuth>,
    /// The sync state; `None` when the account isn't synced (legacy Local Folders, no sync
    /// engine, an unsupported login method).
    pub sync: Option<SyncStatus>,
}

impl std::fmt::Debug for AccountInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountInfo")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("auth", &self.auth)
            .field("sync", &self.sync)
            .finish_non_exhaustive()
    }
}

/// Why adding or removing an account failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AccountErrorKind {
    /// The input is incomplete or malformed.
    InvalidInput,
    /// The account exists already.
    Duplicate,
    /// The server can't be reached (or TLS failed).
    Unreachable,
    /// The server rejected the login.
    AuthFailed,
    /// No OAuth client ID is configured for the provider.
    OAuthNotConfigured,
    /// The browser sign-in failed or was cancelled.
    SignInFailed,
    /// The account's password or sign-in couldn't be saved or deleted.
    Secrets,
}

fn account_error(kind: AccountErrorKind, message: impl Into<String>) -> Error {
    Error::Account {
        kind,
        message: message.into(),
    }
}

/// The provider registry from the settings (`auth.google.*`, `auth.microsoft.*`).
pub(crate) fn registry_from_settings(settings: &settings::Settings) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry.set_microsoft_tenant(&settings.auth.microsoft.tenant);
    registry.set_credentials(
        ProviderKind::Google,
        ClientCredentials::from_settings(
            settings.auth.google.client_id.as_deref(),
            settings.auth.google.client_secret.as_deref(),
        ),
    );
    registry.set_credentials(
        ProviderKind::Microsoft,
        ClientCredentials::from_settings(settings.auth.microsoft.client_id.as_deref(), None),
    );
    registry
}

/// The fix-it text when no client ID is configured.
fn missing_client_message(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Google => "Sign in with Google needs an OAuth client ID: Maze doesn't ship \
             one yet. Create an OAuth client of type \"Desktop app\" in the Google Cloud \
             console and set auth.google.client_id and auth.google.client_secret in \
             settings.json."
            .to_owned(),
        ProviderKind::Microsoft => "Sign in with Microsoft needs an OAuth client ID: set \
             auth.microsoft.client_id in settings.json to an Entra app registration (public \
             client with the redirect URI http://localhost)."
            .to_owned(),
    }
}

/// The default IMAP server of an OAuth provider, and the account kind.
fn oauth_server(provider: ProviderKind) -> (ImapServer, AccountKind) {
    match provider {
        ProviderKind::Google => (
            ImapServer::new("imap.gmail.com", ImapSecurity::Tls),
            AccountKind::Gmail,
        ),
        ProviderKind::Microsoft => (
            ImapServer::new("outlook.office365.com", ImapSecurity::Tls),
            AccountKind::M365,
        ),
    }
}

fn check_email(email: &str) -> Result<String> {
    let email = email.trim();
    match email.rsplit_once('@') {
        Some((local, domain)) if !local.is_empty() && domain.contains('.') => Ok(email.to_owned()),
        _ => Err(account_error(
            AccountErrorKind::InvalidInput,
            "enter a full email address (name@example.com)",
        )),
    }
}

fn check_server(server: &ImapServer) -> Result<ImapServer> {
    let host = server.host.trim().trim_end_matches('.').to_owned();
    if host.is_empty() || host.contains(char::is_whitespace) || host.contains('/') {
        return Err(account_error(
            AccountErrorKind::InvalidInput,
            "enter the IMAP server's host name (imap.example.com)",
        ));
    }
    if server.port == 0 {
        return Err(account_error(
            AccountErrorKind::InvalidInput,
            "the IMAP port must be between 1 and 65535",
        ));
    }
    Ok(ImapServer {
        host,
        port: server.port,
        security: server.security,
    })
}

/// What connecting to an account's server found out: it answered, accepted the login
/// and listed its folders ([`CoreHandle::test_imap_account`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionTest {
    /// The server has Gmail's extensions (`X-GM-EXT-1`).
    pub gmail: bool,
    /// How many folders it listed.
    pub folders: usize,
}

/// The checked address, server and user name of a password login.
fn password_login(input: &AddImapAccount) -> Result<(String, ImapServer, String)> {
    let email = check_email(&input.email)?;
    let server = check_server(&input.server)?;
    if input.password.expose().is_empty() {
        return Err(account_error(
            AccountErrorKind::InvalidInput,
            "enter the account's password",
        ));
    }
    let username = input
        .username
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map_or_else(|| email.clone(), str::to_owned);
    Ok((email, server, username))
}

impl CoreHandle {
    /// Every account with its sync state (see [`AccountInfo`]).
    pub fn accounts_with_status(&self) -> Result<Vec<AccountInfo>> {
        let engine = self.engine();
        Ok(self
            .accounts()?
            .into_iter()
            .map(|account| {
                let config = AccountConfig::parse(&account.config_json);
                let email = config.as_ref().and_then(|c| c.email.clone());
                let sync = engine.as_ref().and_then(|e| e.status(account.id));
                AccountInfo {
                    id: account.id,
                    uuid: account.uuid,
                    name: account.name,
                    kind: account.kind,
                    email,
                    server: config.as_ref().map(AccountConfig::server),
                    auth: config.as_ref().and_then(AccountConfig::auth),
                    sync,
                }
            })
            .collect())
    }

    /// The sync state of `account`, if it is synced.
    pub fn sync_status(&self, account: AccountId) -> Option<SyncStatus> {
        self.engine().and_then(|engine| engine.status(account))
    }

    fn engine_or_err(&self) -> Result<Arc<crate::sync_engine::SyncEngine>> {
        self.engine().ok_or(Error::NoSyncEngine)
    }

    fn check_duplicate(&self, email: &str, host: &str) -> Result<()> {
        let exists = self.accounts()?.iter().any(|account| {
            AccountConfig::parse(&account.config_json).is_some_and(|c| {
                c.email
                    .as_deref()
                    .is_some_and(|e| e.eq_ignore_ascii_case(email))
                    && c.host.eq_ignore_ascii_case(host)
            })
        });
        if exists {
            return Err(account_error(
                AccountErrorKind::Duplicate,
                "this account has already been added",
            ));
        }
        Ok(())
    }

    /// Connects with `auth`, logs in and lists the folders.
    async fn validate(&self, server: &ImapServer, auth: ImapAuth) -> Result<ConnectionTest> {
        let engine = self.engine_or_err()?;
        let sessions = engine
            .connector()
            .sessions(server, Arc::new(AccountCredentials::fixed(auth)));
        let where_ = format!("{}:{}", server.host, server.port);
        let run = async move {
            let mut session = sessions.connect().await?;
            let caps = session.capabilities().await?;
            let folders = session.list().await?;
            if let Err(err) = session.logout().await {
                tracing::debug!(%err, "logging out after validation");
            }
            Ok::<_, sync::SyncError>((caps.gmail(), folders.len()))
        };
        match tokio::time::timeout(VALIDATION_TIMEOUT, run).await {
            Err(_) => Err(account_error(
                AccountErrorKind::Unreachable,
                format!("{where_} didn't answer in time"),
            )),
            Ok(Err(err)) if err.is_auth() => Err(account_error(
                AccountErrorKind::AuthFailed,
                format!("{where_} rejected the login: {err}"),
            )),
            Ok(Err(err)) => Err(account_error(
                AccountErrorKind::Unreachable,
                format!("can't use the IMAP server {where_}: {err}"),
            )),
            Ok(Ok((gmail, folders))) => {
                tracing::info!(gmail, folders, "account validated");
                Ok(ConnectionTest { gmail, folders })
            }
        }
    }

    /// Creates the account row and its default identity; returns the account.
    async fn create_account_rows(
        &self,
        kind: AccountKind,
        name: String,
        config: AccountConfig,
        display_name: Option<String>,
        email: String,
        smtp: Option<SmtpServerSettings>,
    ) -> Result<Account> {
        let ordinal = i64::try_from(self.accounts()?.len()).unwrap_or(i64::MAX);
        let config_json = config.to_json();
        let smtp = smtp_server_input(&name, &config, smtp);
        let id = self
            .store()
            .write(move |w| {
                let id = w.create_account(&NewAccount {
                    kind,
                    name,
                    config_json,
                    ordinal,
                })?;
                let smtp = smtp.map(|input| w.create_smtp_server(&input)).transpose()?;
                w.create_identity(
                    id,
                    &IdentityInput {
                        label: None,
                        display_name,
                        email,
                        reply_to: None,
                        signature: None,
                        smtp,
                        sent_folder: None,
                        drafts_folder: None,
                        archive_folder: None,
                        // Thunderbird's default: yearly archive folders.
                        archive_granularity: 1,
                        color: None,
                        match_patterns_json: None,
                        is_default: true,
                        ordinal: 0,
                    },
                )?;
                Ok(id)
            })
            .await?;
        self.store()
            .reader()?
            .account(id)?
            .ok_or_else(|| Error::not_found("account", id))
    }

    /// Deletes an account row again after a later step of adding it failed.
    async fn roll_back(&self, id: AccountId) {
        if let Err(err) = self.store().write(move |w| w.delete_account(id)).await {
            tracing::error!(%err, account = %id, "removing a half-added account");
        }
    }

    fn info_of(&self, account: &Account) -> Result<AccountInfo> {
        self.accounts_with_status()?
            .into_iter()
            .find(|a| a.id == account.id)
            .ok_or_else(|| Error::not_found("account", account.id))
    }

    /// Adds an IMAP account with a password (see the [module docs](self)): validates
    /// it against the server, saves it with the password and starts
    /// syncing.
    ///
    /// # Errors
    ///
    /// [`Error::Account`] for bad input, a duplicate, an unreachable server, a rejected
    /// login or a failure to save the password; [`Error::NoSyncEngine`] without a sync engine.
    pub async fn add_imap_account(&self, input: AddImapAccount) -> Result<AccountInfo> {
        let core = self.clone();
        self.run(async move { core.add_imap_account_inner(input).await })
            .await
    }

    async fn add_imap_account_inner(&self, input: AddImapAccount) -> Result<AccountInfo> {
        let engine = self.engine_or_err()?;
        let (email, server, username) = password_login(&input)?;
        self.check_duplicate(&email, &server.host)?;

        let validated = self
            .validate(
                &server,
                ImapAuth::Password {
                    user: username.clone(),
                    password: input.password.clone(),
                },
            )
            .await?;
        let kind = if validated.gmail {
            AccountKind::Gmail
        } else if ProviderKind::from_host(&server.host) == Some(ProviderKind::Microsoft) {
            AccountKind::M365
        } else {
            AccountKind::Imap
        };
        let config = AccountConfig {
            email: Some(email.clone()),
            host: server.host.clone(),
            port: server.port,
            security: server.security,
            username,
            auth: "password".to_owned(),
            oauth_provider: None,
            oauth_tenant: None,
            provider_profile: provider_profile(kind),
            extra: serde_json::Map::new(),
        };
        let display_name = input
            .display_name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty());
        let name = display_name.clone().unwrap_or_else(|| email.clone());
        let account = self
            .create_account_rows(kind, name, config, display_name, email, input.smtp)
            .await?;
        let secrets = AccountSecrets::new(engine.secrets(), account.uuid);
        if let Err(err) = secrets.set_password(input.password).await {
            self.roll_back(account.id).await;
            return Err(account_error(
                AccountErrorKind::Secrets,
                format!("can't save the password: {err}"),
            ));
        }
        engine.start(&account, None);
        tracing::info!(account = %account.id, kind = %account.kind.as_str(), "account added");
        self.info_of(&account)
    }

    /// Tests an IMAP account's settings without saving anything: connects, logs in and
    /// lists the folders, as [`CoreHandle::add_imap_account`] does before it saves.
    ///
    /// # Errors
    ///
    /// [`Error::Account`] for bad input, an unreachable server or a rejected login;
    /// [`Error::NoSyncEngine`] without a sync engine.
    pub async fn test_imap_account(&self, input: AddImapAccount) -> Result<ConnectionTest> {
        let core = self.clone();
        self.run(async move {
            let (_, server, user) = password_login(&input)?;
            core.validate(
                &server,
                ImapAuth::Password {
                    user,
                    password: input.password,
                },
            )
            .await
        })
        .await
    }

    /// Tests the SMTP server of a password account without saving or sending anything:
    /// connects and logs in with the account's user name and password. Returns the
    /// server it tested (`input.smtp`, or the derived one).
    ///
    /// # Errors
    ///
    /// [`Error::Account`] for bad input, an unreachable server or a rejected login;
    /// [`Error::NoSyncEngine`] without a sync engine.
    pub async fn test_smtp_account(&self, input: AddImapAccount) -> Result<SmtpServerSettings> {
        let core = self.clone();
        self.run(async move {
            let engine = core.engine_or_err()?;
            let (email, imap, user) = password_login(&input)?;
            let server = input
                .smtp
                .clone()
                .or_else(|| suggest_smtp_server(&email, Some(&imap.host)).map(|s| s.server))
                .ok_or_else(|| {
                    account_error(
                        AccountErrorKind::InvalidInput,
                        "enter the SMTP server's host name (smtp.example.com)",
                    )
                })?;
            let where_ = format!("{}:{}", server.host, server.port);
            let credentials = smtp::Credentials::Password {
                user,
                password: input.password,
            };
            let check = engine.mailer().check(&server.config(), &credentials).await;
            match check {
                Ok(()) => Ok(server),
                Err(err) if err.is_auth() => Err(account_error(
                    AccountErrorKind::AuthFailed,
                    format!("the SMTP server {where_} rejected the login: {err}"),
                )),
                Err(err) => Err(account_error(
                    AccountErrorKind::Unreachable,
                    format!("can't use the SMTP server {where_}: {err}"),
                )),
            }
        })
        .await
    }

    /// Adds a Google or Microsoft account with a browser sign-in (OAuth2 with PKCE):
    /// opens the system browser, validates the account with the access token, saves
    /// it with the refresh token and starts syncing.
    ///
    /// # Errors
    ///
    /// [`Error::Account`] with [`AccountErrorKind::OAuthNotConfigured`] when the
    /// provider's client ID isn't set (`auth.google.client_id`), else as
    /// [`CoreHandle::add_imap_account`].
    pub async fn add_oauth_account(&self, input: AddOAuthAccount) -> Result<AccountInfo> {
        let core = self.clone();
        self.run(async move { core.add_oauth_account_inner(input).await })
            .await
    }

    async fn add_oauth_account_inner(&self, input: AddOAuthAccount) -> Result<AccountInfo> {
        let engine = self.engine_or_err()?;
        let email = check_email(&input.email)?;
        let provider = input.provider;
        let (server, kind) = oauth_server(provider);
        self.check_duplicate(&email, &server.host)?;
        let registry = engine.registry(&self.settings().settings);
        if registry.get(provider).credentials().is_none() {
            return Err(account_error(
                AccountErrorKind::OAuthNotConfigured,
                missing_client_message(provider),
            ));
        }
        // New sign-ins use the tenant asked for, else the setting's; the account keeps it.
        let tenant = (provider == ProviderKind::Microsoft).then(|| {
            input
                .tenant
                .clone()
                .or_else(|| registry.microsoft_tenant().cloned())
                .unwrap_or_default()
        });
        let client = registry
            .client_for_tenant(provider, tenant.as_ref())
            .map_err(|err| account_error(AccountErrorKind::OAuthNotConfigured, err.to_string()))?;
        let tokens = client
            .authorize(
                engine.browser().as_ref(),
                &AuthorizeOptions::new().login_hint(email.clone()),
            )
            .await
            .map_err(|err| {
                account_error(
                    AccountErrorKind::SignInFailed,
                    format!("the {provider} sign-in failed: {err}"),
                )
            })?;
        self.validate(
            &server,
            ImapAuth::XOAuth2 {
                user: email.clone(),
                access_token: tokens.access_token.clone(),
            },
        )
        .await?;
        let config = AccountConfig {
            email: Some(email.clone()),
            host: server.host.clone(),
            port: server.port,
            security: server.security,
            username: email.clone(),
            auth: "oauth2".to_owned(),
            oauth_provider: Some(provider),
            oauth_tenant: tenant.as_ref().map(ToString::to_string),
            provider_profile: provider_profile(kind),
            extra: serde_json::Map::new(),
        };
        let display_name = input
            .display_name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty());
        let name = display_name.clone().unwrap_or_else(|| email.clone());
        let account = self
            .create_account_rows(kind, name, config, display_name, email, None)
            .await?;
        let manager = Arc::new(OAuthTokenManager::new(
            client,
            AccountSecrets::new(engine.secrets(), account.uuid),
        ));
        if let Err(err) = manager.store_tokens(tokens).await {
            self.roll_back(account.id).await;
            return Err(account_error(
                AccountErrorKind::Secrets,
                format!("can't save the sign-in: {err}"),
            ));
        }
        engine.start(&account, Some(manager));
        tracing::info!(account = %account.id, %provider, ?tenant, "account added");
        self.info_of(&account)
    }

    /// Removes an account (see the [module docs](self)): stops its sync, deletes its
    /// data and its secrets from the store.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown account; [`Error::Account`] with
    /// [`AccountErrorKind::Secrets`] if the store data is gone but a secret couldn't
    /// be deleted (the message says so).
    pub async fn remove_account(&self, id: AccountId) -> Result<()> {
        let core = self.clone();
        self.run(async move { core.remove_account_inner(id).await })
            .await
    }

    async fn remove_account_inner(&self, id: AccountId) -> Result<()> {
        let account = self
            .store()
            .reader()?
            .account(id)?
            .ok_or_else(|| Error::not_found("account", id))?;
        let engine = self.engine();
        if let Some(engine) = &engine {
            engine.stop(id).await;
        }
        self.store()
            .write(move |w| {
                let servers: Vec<_> = w
                    .identities(Some(id))?
                    .iter()
                    .filter_map(|i| i.input.smtp)
                    .collect();
                w.delete_account(id)?;
                // SMTP servers only this account's identities used go with it.
                let in_use: Vec<_> = w
                    .identities(None)?
                    .iter()
                    .filter_map(|i| i.input.smtp)
                    .collect();
                for server in servers {
                    if !in_use.contains(&server) && w.smtp_server(server)?.is_some() {
                        w.delete_smtp_server(server)?;
                    }
                }
                Ok(())
            })
            .await?;
        tracing::info!(account = %id, "account removed");
        // In the encrypted store the secrets went with the account row; other stores
        // (tests, the development file) delete them here.
        if let Some(engine) = engine {
            let secrets = AccountSecrets::new(engine.secrets(), account.uuid);
            if let Err(err) = secrets.delete_all().await {
                return Err(account_error(
                    AccountErrorKind::Secrets,
                    format!(
                        "the account was removed, but its saved secrets \
                         (mazemail/{}/…) couldn't be deleted: {err}",
                        account.uuid
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The network changed (another Wi-Fi, VPN up or down): every account drops its
    /// connections, reconnects and syncs once. Hook for the platform layer.
    pub fn network_changed(&self) {
        if let Some(engine) = self.engine() {
            engine.network_changed();
        }
        self.send_outbox_now();
    }

    /// The computer woke from sleep: like [`CoreHandle::network_changed`]. The engine
    /// also notices sleep by itself (the wall clock jumps against the monotonic
    /// clock), so calling this is optional.
    pub fn system_woke(&self) {
        if let Some(engine) = self.engine() {
            engine.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn suggestions_for_well_known_and_other_domains() {
        let gmail = suggest_imap_server("Anna@GMail.com").unwrap();
        assert_eq!(
            gmail.server,
            ImapServer::new("imap.gmail.com", ImapSecurity::Tls)
        );
        assert_eq!(gmail.server.port, 993);
        assert_eq!(gmail.oauth, Some(ProviderKind::Google));
        assert!(gmail.known);

        let outlook = suggest_imap_server("bob@hotmail.de").unwrap();
        assert_eq!(outlook.server.host, "outlook.office365.com");
        assert_eq!(outlook.oauth, Some(ProviderKind::Microsoft));

        let icloud = suggest_imap_server("c@icloud.com").unwrap();
        assert_eq!(icloud.server.host, "imap.mail.me.com");
        assert_eq!(icloud.oauth, None);

        let other = suggest_imap_server("d@example.org").unwrap();
        assert_eq!(other.server.host, "imap.example.org");
        assert!(!other.known);
        assert_eq!(suggest_imap_server("nobody"), None);
        assert_eq!(suggest_imap_server("x@localhost"), None);
    }

    #[test]
    fn config_round_trips_and_reads_thunderbird_configs() {
        let config = AccountConfig {
            email: Some("alice@example.org".into()),
            host: "imap.example.org".into(),
            port: 993,
            security: ImapSecurity::Tls,
            username: "alice".into(),
            auth: "password".into(),
            oauth_provider: None,
            oauth_tenant: None,
            provider_profile: provider_profile(AccountKind::Gmail),
            extra: serde_json::Map::new(),
        };
        let json = config.to_json();
        assert_eq!(AccountConfig::parse(&json).unwrap(), config);
        assert_eq!(config.auth(), Some(AccountAuth::Password));

        // What the Thunderbird importer writes.
        let imported = AccountConfig::parse(
            r#"{"host":"imap.gmail.com","port":993,"security":"tls","username":"a@gmail.com",
                "auth":"oauth2","protocol":"imap","identity_folders":[],"import":{"source":"thunderbird"}}"#,
        )
        .unwrap();
        assert_eq!(
            imported.auth(),
            Some(AccountAuth::OAuth(ProviderKind::Google))
        );
        assert_eq!(imported.login(), "a@gmail.com");
        assert!(imported.extra.contains_key("import"));
        assert!(imported.to_json().contains("\"import\""));
        let plain = AccountConfig::parse(r#"{"host":"h","security":"none"}"#).unwrap();
        assert_eq!(plain.server().port, 143);
        assert_eq!(plain.server().security, ImapSecurity::Insecure);
        assert_eq!(AccountConfig::parse("{}"), None);
        assert_eq!(
            AccountConfig::parse(r#"{"host":"h","auth":"gssapi"}"#)
                .unwrap()
                .auth(),
            None
        );
    }

    #[test]
    fn the_microsoft_tenant_is_kept_and_defaults_to_common() {
        // Accounts of earlier versions have none: `common`, as they signed in with.
        let old = AccountConfig::parse(
            r#"{"email":"a@contoso.com","host":"outlook.office365.com","auth":"oauth2","oauth_provider":"microsoft"}"#,
        )
        .unwrap();
        assert_eq!(old.oauth_tenant, None);
        assert!(old.microsoft_tenant().is_common());
        assert!(!old.to_json().contains("oauth_tenant"), "nothing added");

        let mut config = old;
        config.oauth_tenant = Some("contoso.com".into());
        let json = config.to_json();
        assert!(json.contains(r#""oauth_tenant":"contoso.com""#), "{json}");
        let parsed = AccountConfig::parse(&json).unwrap();
        assert_eq!(parsed, config);
        assert_eq!(parsed.microsoft_tenant().as_str(), "contoso.com");

        // A broken value doesn't make the account unusable.
        config.oauth_tenant = Some("not a tenant".into());
        assert!(config.microsoft_tenant().is_common());
    }

    #[test]
    fn the_tenant_setting_reaches_the_registry() {
        let mut settings = settings::default_settings();
        assert_eq!(
            registry_from_settings(&settings).microsoft_tenant(),
            Some(&MicrosoftTenant::common())
        );
        settings.auth.microsoft.tenant = MicrosoftTenant::parse("contoso.com").unwrap();
        let registry = registry_from_settings(&settings);
        assert_eq!(
            registry
                .get(ProviderKind::Microsoft)
                .authorization_endpoint()
                .as_str(),
            "https://login.microsoftonline.com/contoso.com/oauth2/v2.0/authorize"
        );
    }

    #[test]
    fn provider_profiles_follow_the_kind() {
        assert_eq!(
            provider_profile(AccountKind::Gmail),
            Some(ProviderProfile::gmail())
        );
        assert_eq!(
            provider_profile(AccountKind::M365),
            Some(ProviderProfile::microsoft365())
        );
        assert_eq!(provider_profile(AccountKind::Imap), None);
    }

    #[test]
    fn the_legacy_imap_profile_is_replaced() {
        let mut config = AccountConfig::parse(
            r#"{"email":"Ben@Reader.Example","host":"Mail.Reader.Example.","username":"ben"}"#,
        )
        .unwrap();
        assert_eq!(config.address_domain().as_deref(), Some("reader.example"));
        assert_eq!(config.custom_provider_profile(), None);
        // What earlier versions stored for every IMAP account.
        config.provider_profile = Some(ProviderProfile::generic(
            "mail.reader.example",
            vec![HostMatcher::Exact("mail.reader.example".into())],
        ));
        assert_eq!(config.custom_provider_profile(), None);
        // A profile someone configured is kept.
        let custom = ProviderProfile::generic(
            "mx.reader.example",
            vec![HostMatcher::Exact("mx.reader.example".into())],
        );
        config.provider_profile = Some(custom.clone());
        assert_eq!(config.custom_provider_profile(), Some(custom));
        // Without an address, the login is used.
        let login = AccountConfig::parse(r#"{"host":"h","username":"a@b.example"}"#).unwrap();
        assert_eq!(login.address_domain().as_deref(), Some("b.example"));
    }

    #[test]
    fn input_checks() {
        assert!(check_email("a@b.c").is_ok());
        assert!(check_email("nope").is_err());
        assert!(check_email("@b.c").is_err());
        assert!(check_server(&ImapServer::new("", ImapSecurity::Tls)).is_err());
        assert!(check_server(&ImapServer::new("a b", ImapSecurity::Tls)).is_err());
        let mut server = ImapServer::new("imap.example.org.", ImapSecurity::Tls);
        assert_eq!(check_server(&server).unwrap().host, "imap.example.org");
        server.port = 0;
        assert!(check_server(&server).is_err());
        assert_eq!(
            "STARTTLS".parse::<ImapSecurity>(),
            Ok(ImapSecurity::Starttls)
        );
        assert!("ftp".parse::<ImapSecurity>().is_err());
    }
}
