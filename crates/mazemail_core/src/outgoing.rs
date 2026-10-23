//! Sending (PLAN §4.5): SMTP servers per identity, the identity's route (server and
//! credentials), what follows a delivery (the Sent copy), and the outbox runner.
//!
//! # SMTP servers
//!
//! Every identity has an SMTP server (`smtp_server` row). It is derived from the
//! account when the account is added, and for accounts added before Maze could send
//! when the core opens ([`suggest_smtp_server`]):
//!
//! - Gmail: `smtp.gmail.com`, TLS on 465. Microsoft (Outlook.com, Microsoft 365):
//!   `smtp.office365.com`, STARTTLS on 587. Both with the account's OAuth sign-in
//!   (XOAUTH2), or its password.
//! - Other well-known providers (iCloud, Yahoo, Fastmail, GMX, web.de, …) from a
//!   built-in list next to [`suggest_imap_server`](crate::suggest_imap_server)'s.
//! - Everything else: the IMAP host with `imap.` replaced by `smtp.` (or
//!   `smtp.<domain>`), submission with STARTTLS on 587.
//!
//! The login is the account's: its IMAP user name and its stored password,
//! or its OAuth token manager (shared with sync, so a refresh never races).
//!
//! # Sent copies
//!
//! `smtp_server.saves_sent` decides: `auto` means the server files the copy itself
//! for Gmail and Microsoft 365 (so nothing is uploaded, the Sent folder is just
//! refreshed), and `APPEND` to the identity's Sent folder (or the account's) for
//! everyone else, through a pending op queued in the transaction that marks the
//! message sent.
//!
//! # The runner
//!
//! With a sync engine, a task recovers items a crash left in `sending` (the send log
//! decides whether they went out), then sends whatever is due every minute or when
//! woken, and prunes sent items after a week.

use std::sync::Arc;
use std::time::Duration;

use crate::ops::OpPayload;
use async_trait::async_trait;
use auth::{AccountSecrets, TokenProvider as _};
use mazemail_types::{AccountId, Flags, FolderId, FolderRole, IdentityId};
use smtp::{Credentials, Route, Routes, Security, SentHook, SmtpConfig, SmtpError};
use store::{
    Account, AccountKind, Identity, OutboxItem, Queries as _, SavesSent, SmtpAuth, SmtpSecurity,
    SmtpServerInput, Store, WriteTx,
};
use tokio::sync::Notify;

use crate::accounts::{AccountAuth, AccountConfig, ImapSecurity};
use crate::sync_engine::SyncEngine;

/// How often the runner looks for due outbox items without being woken.
const RUNNER_INTERVAL: Duration = Duration::from_secs(60);

/// How long sent items stay in the outbox (their blob feeds the Sent copy's `APPEND`).
const KEEP_SENT: Duration = Duration::from_secs(7 * 24 * 3600);

/// An SMTP server as the Add Account dialog and the CLI describe it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SmtpServerSettings {
    /// Host name.
    pub host: String,
    /// Port.
    pub port: u16,
    /// Transport security (`tls` = 465, `starttls` = 587).
    pub security: ImapSecurity,
}

impl SmtpServerSettings {
    /// A server with the usual submission port for `security`.
    pub fn new(host: impl Into<String>, security: ImapSecurity) -> Self {
        Self {
            host: host.into(),
            port: smtp_default_port(security),
            security,
        }
    }

    /// The transport configuration.
    pub fn config(&self) -> SmtpConfig {
        SmtpConfig::new(self.host.clone(), smtp_security(self.security)).with_port(self.port)
    }
}

/// The usual submission port: 465 for TLS, 587 for STARTTLS, 25 without encryption.
pub fn smtp_default_port(security: ImapSecurity) -> u16 {
    smtp_security(security).default_port()
}

fn smtp_security(security: ImapSecurity) -> Security {
    match security {
        ImapSecurity::Tls => Security::Tls,
        ImapSecurity::Starttls => Security::StartTls,
        ImapSecurity::Insecure => Security::PlainInsecure,
    }
}

fn store_security(security: ImapSecurity) -> SmtpSecurity {
    match security {
        ImapSecurity::Tls => SmtpSecurity::Tls,
        ImapSecurity::Starttls => SmtpSecurity::StartTls,
        ImapSecurity::Insecure => SmtpSecurity::None,
    }
}

fn from_store_security(security: SmtpSecurity) -> ImapSecurity {
    match security {
        SmtpSecurity::Tls => ImapSecurity::Tls,
        SmtpSecurity::StartTls => ImapSecurity::Starttls,
        SmtpSecurity::None => ImapSecurity::Insecure,
    }
}

/// What [`suggest_smtp_server`] proposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpSuggestion {
    /// The server.
    pub server: SmtpServerSettings,
    /// The provider is in the built-in list (otherwise the server is a guess).
    pub known: bool,
}

/// Built-in submission servers of common providers: `(domains, IMAP host, SMTP host,
/// security)`.
const KNOWN_SMTP: &[(&[&str], &str, &str, ImapSecurity)] = &[
    (
        &["gmail.com", "googlemail.com"],
        "imap.gmail.com",
        "smtp.gmail.com",
        ImapSecurity::Tls,
    ),
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
        "smtp.office365.com",
        ImapSecurity::Starttls,
    ),
    (
        &["icloud.com", "me.com", "mac.com"],
        "imap.mail.me.com",
        "smtp.mail.me.com",
        ImapSecurity::Starttls,
    ),
    (
        &["yahoo.com", "yahoo.de", "yahoo.co.uk", "ymail.com"],
        "imap.mail.yahoo.com",
        "smtp.mail.yahoo.com",
        ImapSecurity::Tls,
    ),
    (
        &["aol.com"],
        "imap.aol.com",
        "smtp.aol.com",
        ImapSecurity::Tls,
    ),
    (
        &["fastmail.com", "fastmail.fm", "messagingengine.com"],
        "imap.fastmail.com",
        "smtp.fastmail.com",
        ImapSecurity::Tls,
    ),
    (
        &["gmx.de", "gmx.net", "gmx.at", "gmx.ch"],
        "imap.gmx.net",
        "mail.gmx.net",
        ImapSecurity::Starttls,
    ),
    (
        &["gmx.com"],
        "imap.gmx.com",
        "mail.gmx.com",
        ImapSecurity::Starttls,
    ),
    (
        &["web.de"],
        "imap.web.de",
        "smtp.web.de",
        ImapSecurity::Starttls,
    ),
    (
        &["posteo.de", "posteo.net"],
        "posteo.de",
        "posteo.de",
        ImapSecurity::Starttls,
    ),
    (
        &["mailbox.org"],
        "imap.mailbox.org",
        "smtp.mailbox.org",
        ImapSecurity::Tls,
    ),
    (
        &["zoho.com", "zohomail.com"],
        "imap.zoho.com",
        "smtp.zoho.com",
        ImapSecurity::Tls,
    ),
    (
        &["t-online.de"],
        "secureimap.t-online.de",
        "securesmtp.t-online.de",
        ImapSecurity::Tls,
    ),
    (
        &["freenet.de"],
        "mx.freenet.de",
        "mx.freenet.de",
        ImapSecurity::Starttls,
    ),
];

/// Suggests the SMTP server for an address and its account's IMAP host: a well-known
/// provider's (by the address's domain or the IMAP host), else the IMAP host with
/// `imap.` replaced by `smtp.`, else `smtp.<domain>`, both with STARTTLS on 587.
/// `None` without a domain or host.
pub fn suggest_smtp_server(email: &str, imap_host: Option<&str>) -> Option<SmtpSuggestion> {
    let domain = email
        .trim()
        .rsplit_once('@')
        .map(|(_, d)| d.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|d| d.contains('.'));
    let host = imap_host
        .map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|h| !h.is_empty());
    let known = KNOWN_SMTP.iter().find(|(domains, imap, _, _)| {
        domain.as_deref().is_some_and(|d| domains.contains(&d))
            || host.as_deref().is_some_and(|h| h == *imap)
    });
    if let Some((_, _, smtp, security)) = known {
        return Some(SmtpSuggestion {
            server: SmtpServerSettings::new(*smtp, *security),
            known: true,
        });
    }
    if host.as_deref().is_some_and(is_microsoft_host) {
        return Some(SmtpSuggestion {
            server: SmtpServerSettings::new("smtp.office365.com", ImapSecurity::Starttls),
            known: true,
        });
    }
    let guess = match (host.as_deref(), domain.as_deref()) {
        (Some(h), _) if h.starts_with("imap.") => format!("smtp.{}", &h["imap.".len()..]),
        (_, Some(d)) => format!("smtp.{d}"),
        (Some(h), None) => h.to_owned(),
        (None, None) => return None,
    };
    Some(SmtpSuggestion {
        server: SmtpServerSettings::new(guess, ImapSecurity::Starttls),
        known: false,
    })
}

fn is_microsoft_host(host: &str) -> bool {
    host == "outlook.office365.com" || host.ends_with(".outlook.com")
}

/// The `smtp_server` row for an account: `server`, or the derived one.
pub(crate) fn smtp_server_input(
    account_name: &str,
    config: &AccountConfig,
    server: Option<SmtpServerSettings>,
) -> Option<SmtpServerInput> {
    let email = config.email.clone().unwrap_or_else(|| config.login());
    let server =
        server.or_else(|| suggest_smtp_server(&email, Some(&config.host)).map(|s| s.server))?;
    let auth = match config.auth() {
        Some(AccountAuth::OAuth(_)) => SmtpAuth::OAuth2,
        _ => SmtpAuth::Plain,
    };
    Some(SmtpServerInput {
        name: Some(account_name.to_owned()),
        host: server.host,
        port: Some(server.port),
        security: store_security(server.security),
        auth,
        username: Some(config.login()),
        saves_sent: SavesSent::Auto,
    })
}

/// Gives every identity of a synced account without an SMTP server the derived one
/// (accounts added before Maze could send). Returns how many identities got one.
pub(crate) fn ensure_smtp_servers(w: &mut WriteTx<'_>) -> store::Result<usize> {
    let mut assigned = 0;
    for account in w.accounts()? {
        let Some(config) = AccountConfig::parse(&account.config_json) else {
            continue;
        };
        let identities: Vec<Identity> = w
            .identities(Some(account.id))?
            .into_iter()
            .filter(|i| i.input.smtp.is_none())
            .collect();
        if identities.is_empty() {
            continue;
        }
        let Some(input) = smtp_server_input(&account.name, &config, None) else {
            continue;
        };
        let server = w.create_smtp_server(&input)?;
        for identity in identities {
            let mut changed = identity.input.clone();
            changed.smtp = Some(server);
            w.update_identity(identity.id, &changed)?;
            assigned += 1;
        }
    }
    Ok(assigned)
}

/// Whether the server files the Sent copy itself (PLAN §4.5).
pub(crate) fn server_saves_sent(saves_sent: SavesSent, kind: AccountKind) -> bool {
    match saves_sent {
        SavesSent::Yes => true,
        SavesSent::No => false,
        SavesSent::Auto => matches!(kind, AccountKind::Gmail | AccountKind::M365),
    }
}

/// An identity with its account and server, as sending needs them.
pub(crate) struct Sender {
    pub(crate) identity: Identity,
    pub(crate) account: Account,
    pub(crate) config: AccountConfig,
    pub(crate) server: SmtpServerInput,
}

/// Looks up what sending as `identity` needs; the server is derived if the identity
/// has none yet.
pub(crate) fn sender(store: &Store, identity: IdentityId) -> Result<Sender, SmtpError> {
    let invalid = |what: &str| SmtpError::Invalid(what.to_owned());
    let reader = store
        .reader()
        .map_err(|err| SmtpError::Invalid(err.to_string()))?;
    let identity = reader
        .identity(identity)
        .map_err(|err| SmtpError::Invalid(err.to_string()))?
        .ok_or_else(|| invalid("its identity was deleted"))?;
    let account = reader
        .account(identity.account)
        .map_err(|err| SmtpError::Invalid(err.to_string()))?
        .ok_or_else(|| invalid("its account was removed"))?;
    let config = AccountConfig::parse(&account.config_json)
        .ok_or_else(|| invalid("its account has no server settings"))?;
    let stored = identity
        .input
        .smtp
        .map(|id| reader.smtp_server(id))
        .transpose()
        .map_err(|err| SmtpError::Invalid(err.to_string()))?
        .flatten();
    let server = match stored {
        Some(server) => SmtpServerInput {
            name: server.name,
            host: server.host,
            port: server.port,
            security: server.security,
            auth: server.auth,
            username: server.username,
            saves_sent: server.saves_sent,
        },
        None => smtp_server_input(&account.name, &config, None)
            .ok_or_else(|| invalid("no SMTP server is configured for this identity"))?,
    };
    Ok(Sender {
        identity,
        account,
        config,
        server,
    })
}

/// Routes over the store and the sync engine's credentials (stored passwords, shared
/// OAuth token managers).
pub(crate) struct CoreRoutes {
    pub(crate) store: Store,
    pub(crate) engine: Arc<SyncEngine>,
}

impl CoreRoutes {
    async fn credentials(&self, sender: &Sender) -> Result<Credentials, SmtpError> {
        let user = sender
            .server
            .username
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| sender.config.login());
        match sender.server.auth {
            SmtpAuth::None => Ok(Credentials::None),
            SmtpAuth::OAuth2 => {
                let tokens = self
                    .engine
                    .token_manager(&sender.account, &sender.config)
                    .map_err(|message| SmtpError::Credentials {
                        message,
                        needs_user: true,
                    })?;
                let access_token =
                    tokens
                        .access_token()
                        .await
                        .map_err(|err| SmtpError::Credentials {
                            message: format!("the sign-in for sending failed: {err}"),
                            needs_user: err.requires_reauth(),
                        })?;
                Ok(Credentials::XOAuth2 { user, access_token })
            }
            SmtpAuth::Plain | SmtpAuth::Login => {
                let secrets = AccountSecrets::new(self.engine.secrets(), sender.account.uuid);
                match secrets.password().await {
                    Ok(Some(password)) => Ok(Credentials::Password { user, password }),
                    Ok(None) => Err(SmtpError::Credentials {
                        message: "no password is stored for this account".to_owned(),
                        needs_user: true,
                    }),
                    Err(err) => Err(SmtpError::Credentials {
                        message: format!("the saved password can't be read: {err}"),
                        needs_user: false,
                    }),
                }
            }
        }
    }
}

#[async_trait]
impl Routes for CoreRoutes {
    async fn route(&self, identity: Option<IdentityId>) -> Result<Route, SmtpError> {
        let identity =
            identity.ok_or_else(|| SmtpError::Invalid("it has no sending identity".to_owned()))?;
        let store = self.store.clone();
        let sender = tokio::task::spawn_blocking(move || sender(&store, identity))
            .await
            .map_err(|_| SmtpError::Invalid("a background task failed".to_owned()))??;
        let security = from_store_security(sender.server.security);
        let settings = SmtpServerSettings {
            host: sender.server.host.clone(),
            port: sender
                .server
                .port
                .unwrap_or_else(|| smtp_default_port(security)),
            security,
        };
        Ok(Route {
            config: settings.config(),
            credentials: self.credentials(&sender).await?,
        })
    }

    async fn rejected(&self, identity: Option<IdentityId>, credentials: &Credentials) {
        if !matches!(credentials, Credentials::XOAuth2 { .. }) {
            return;
        }
        let Some(identity) = identity else { return };
        let store = self.store.clone();
        let found = tokio::task::spawn_blocking(move || sender(&store, identity)).await;
        if let Ok(Ok(sender)) = found
            && let Ok(tokens) = self.engine.token_manager(&sender.account, &sender.config)
        {
            tokens.invalidate().await;
        }
    }
}

/// After a delivery: queue the Sent copy's `APPEND` unless the server files it itself.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SentCopy;

/// Where the Sent copy goes, if Maze has to upload one: the identity's Sent folder,
/// else the account's.
fn sent_copy_target(
    w: &WriteTx<'_>,
    identity: IdentityId,
) -> store::Result<Option<(AccountId, FolderId)>> {
    let Some(identity) = w.identity(identity)? else {
        return Ok(None);
    };
    let Some(account) = w.account(identity.account)? else {
        return Ok(None);
    };
    let saves_sent = identity
        .input
        .smtp
        .map(|id| w.smtp_server(id))
        .transpose()?
        .flatten()
        .map_or(SavesSent::Auto, |s| s.saves_sent);
    if server_saves_sent(saves_sent, account.kind) {
        return Ok(None);
    }
    let folder = match identity.input.sent_folder {
        Some(folder) => Some(folder),
        None => w
            .folders(Some(account.id))?
            .into_iter()
            .find(|f| f.role == Some(FolderRole::Sent) && f.remote_name.is_some())
            .map(|f| f.id),
    };
    Ok(folder.map(|f| (account.id, f)))
}

impl SentHook for SentCopy {
    fn on_sent(&self, w: &mut WriteTx<'_>, item: &OutboxItem) -> store::Result<()> {
        let (Some(identity), Some(blob)) = (item.identity, item.blob) else {
            return Ok(());
        };
        match sent_copy_target(w, identity)? {
            Some((account, folder)) => {
                OpPayload::Append {
                    folder,
                    blob,
                    flags: Flags::SEEN,
                }
                .enqueue(w, account, "user")?;
            }
            None => tracing::debug!(outbox = %item.id, "no Sent copy to upload"),
        }
        Ok(())
    }
}

/// The account of an identity and its Sent folder (to refresh it after sending).
pub(crate) fn sent_folder_of(
    store: &Store,
    identity: IdentityId,
) -> Option<(AccountId, Option<FolderId>)> {
    let reader = store.reader().ok()?;
    let identity = reader.identity(identity).ok()??;
    let sent = match identity.input.sent_folder {
        Some(folder) => Some(folder),
        None => reader
            .folders(Some(identity.account))
            .ok()?
            .into_iter()
            .find(|f| f.role == Some(FolderRole::Sent))
            .map(|f| f.id),
    };
    Some((identity.account, sent))
}

/// The runner's wake-up call.
#[derive(Debug, Default)]
pub(crate) struct OutboxWake(pub(crate) Notify);

/// Recovers interrupted items, then sends due items every [`RUNNER_INTERVAL`] or when
/// woken, and prunes old sent items.
pub(crate) async fn run_outbox(
    outbox: smtp::Outbox,
    routes: Arc<CoreRoutes>,
    wake: Arc<OutboxWake>,
    after_sent: impl Fn(&[IdentityId]) + Send + Sync + 'static,
) {
    let hook: Arc<dyn SentHook> = Arc::new(SentCopy);
    match outbox.recover(Arc::clone(&hook)).await {
        Ok(recovery) if recovery.marked_sent + recovery.requeued > 0 => tracing::info!(
            marked_sent = recovery.marked_sent,
            requeued = recovery.requeued,
            "outbox recovered after an interrupted send"
        ),
        Ok(_) => {}
        Err(err) => tracing::error!(%err, "recovering the outbox"),
    }
    let mut last_prune = std::time::Instant::now() - Duration::from_secs(3600);
    loop {
        match outbox.send_due(routes.as_ref(), Arc::clone(&hook)).await {
            Ok(results) => {
                let mut sent = Vec::new();
                for (id, delivery) in &results {
                    match delivery {
                        smtp::Delivery::Sent | smtp::Delivery::AlreadySent => {
                            tracing::info!(outbox = %id, "message sent");
                            if let Ok(Some(item)) =
                                routes.store.reader().and_then(|r| r.outbox_item(*id))
                                && let Some(identity) = item.identity
                            {
                                sent.push(identity);
                            }
                        }
                        // The server's reply may name a recipient: log only its kind.
                        smtp::Delivery::Retrying { error, .. } => {
                            tracing::info!(outbox = %id, kind = error.kind(), "sending failed; will retry");
                        }
                        smtp::Delivery::Failed(error) => {
                            tracing::warn!(outbox = %id, kind = error.kind(), "sending failed for good");
                        }
                        smtp::Delivery::Skipped => {}
                    }
                }
                if !sent.is_empty() {
                    after_sent(&sent);
                }
            }
            Err(err) => tracing::error!(%err, "sending the outbox"),
        }
        if last_prune.elapsed() >= Duration::from_secs(3600) {
            last_prune = std::time::Instant::now();
            match outbox.prune(KEEP_SENT).await {
                Ok(0) => {}
                Ok(pruned) => tracing::debug!(pruned, "old sent items removed from the outbox"),
                Err(err) => tracing::warn!(%err, "pruning the outbox"),
            }
        }
        tokio::select! {
            () = wake.0.notified() => {}
            () = tokio::time::sleep(RUNNER_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn smtp_servers_are_derived() {
        let gmail = suggest_smtp_server("anna@gmail.com", None).unwrap();
        assert_eq!(
            gmail.server,
            SmtpServerSettings {
                host: "smtp.gmail.com".into(),
                port: 465,
                security: ImapSecurity::Tls
            }
        );
        assert!(gmail.known);
        // Google Workspace: recognized by the IMAP host.
        let workspace = suggest_smtp_server("bob@company.example", Some("imap.gmail.com")).unwrap();
        assert_eq!(workspace.server.host, "smtp.gmail.com");

        let m365 =
            suggest_smtp_server("you@glasskube.example", Some("outlook.office365.com")).unwrap();
        assert_eq!(
            m365.server,
            SmtpServerSettings {
                host: "smtp.office365.com".into(),
                port: 587,
                security: ImapSecurity::Starttls
            }
        );
        let outlook = suggest_smtp_server("c@hotmail.de", None).unwrap();
        assert_eq!(outlook.server.host, "smtp.office365.com");

        let icloud = suggest_smtp_server("d@icloud.com", Some("imap.mail.me.com")).unwrap();
        assert_eq!(
            (icloud.server.host.as_str(), icloud.server.port),
            ("smtp.mail.me.com", 587)
        );
        let gmx = suggest_smtp_server("e@gmx.at", None).unwrap();
        assert_eq!(gmx.server.host, "mail.gmx.net");

        let own = suggest_smtp_server("f@example.org", Some("imap.example.org")).unwrap();
        assert_eq!(
            own,
            SmtpSuggestion {
                server: SmtpServerSettings {
                    host: "smtp.example.org".into(),
                    port: 587,
                    security: ImapSecurity::Starttls
                },
                known: false
            }
        );
        let other_host = suggest_smtp_server("g@example.net", Some("mail.hoster.example")).unwrap();
        assert_eq!(other_host.server.host, "smtp.example.net");
        assert_eq!(suggest_smtp_server("nobody", None), None);
        assert_eq!(smtp_default_port(ImapSecurity::Tls), 465);
        assert_eq!(smtp_default_port(ImapSecurity::Starttls), 587);
    }

    #[test]
    fn sent_copies_follow_the_provider() {
        assert!(server_saves_sent(SavesSent::Auto, AccountKind::Gmail));
        assert!(server_saves_sent(SavesSent::Auto, AccountKind::M365));
        assert!(!server_saves_sent(SavesSent::Auto, AccountKind::Imap));
        assert!(server_saves_sent(SavesSent::Yes, AccountKind::Imap));
        assert!(!server_saves_sent(SavesSent::No, AccountKind::Gmail));
    }

    #[test]
    fn oauth_accounts_send_with_their_sign_in() {
        let config = AccountConfig::parse(
            r#"{"email":"a@gmail.com","host":"imap.gmail.com","auth":"oauth2","oauth_provider":"google"}"#,
        )
        .unwrap();
        let input = smtp_server_input("Gmail", &config, None).unwrap();
        assert_eq!(input.auth, SmtpAuth::OAuth2);
        assert_eq!(input.host, "smtp.gmail.com");
        assert_eq!(input.username.as_deref(), Some("a@gmail.com"));
        let password = AccountConfig::parse(
            r#"{"email":"b@example.org","host":"imap.example.org","username":"bee"}"#,
        )
        .unwrap();
        let input = smtp_server_input(
            "Work",
            &password,
            Some(SmtpServerSettings::new(
                "mail.example.org",
                ImapSecurity::Tls,
            )),
        )
        .unwrap();
        assert_eq!(
            (input.host.as_str(), input.port, input.security, input.auth),
            (
                "mail.example.org",
                Some(465),
                SmtpSecurity::Tls,
                SmtpAuth::Plain
            )
        );
        assert_eq!(input.username.as_deref(), Some("bee"));
        assert_eq!(input.saves_sent, SavesSent::Auto);
    }
}
