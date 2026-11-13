//! Editing an account ("Edit Account…", `mazemail account set`): its name, its
//! password, its IMAP and SMTP servers, or a new browser sign-in.
//!
//! # Where a saved password may go
//!
//! A saved password is only ever sent to the servers it was saved for. Changing the
//! IMAP server (host, port or security), the login name or the SMTP server therefore
//! needs the password again ([`AccountChanges::password`]); a new name or a new
//! password alone doesn't. Accounts that sign in with Google or Microsoft have fixed
//! servers: they can be renamed or signed in again ([`CoreHandle::sign_in_again`]).
//!
//! # Applying changes
//!
//! [`CoreHandle::update_account`] first proves that the new settings work, like adding
//! an account: it connects, logs in and lists the folders (with the new password, or
//! the saved one when only the name changes). A rename is checked against the other
//! accounts and against `settings.json` before anything happens. Then the account's
//! sync stops, the new password is saved (the old one is put back if a later step
//! fails), the account row and its SMTP server are updated, the settings move to the
//! new name ([`settings::rename_account`]: `accounts.<name>`, `agent.accounts.<name>`
//! and the favorite folders), and sync starts again with the new settings. That also
//! ends a "sign-in required" state.
//!
//! An SMTP server that identities of other accounts share (Thunderbird imports can do
//! that) isn't changed for them: this account gets its own copy.

use std::collections::BTreeSet;
use std::sync::Arc;

use auth::{
    AccountSecrets, AuthorizeOptions, MicrosoftTenant, OAuthTokenManager, TokenProvider as _,
};
use imap::ImapAuth;
use mazemail_types::{AccountId, Sensitive};
use store::{Account, Queries as _, SmtpServer, SmtpServerId, SmtpServerInput, WriteTx};

use super::{
    AccountAuth, AccountConfig, AccountErrorKind, AccountInfo, ConnectionTest, ImapServer,
    account_error, check_server, missing_client_message,
};
use crate::handle::CoreHandle;
use crate::outgoing::{SmtpServerSettings, set_smtp_settings, smtp_server_input, smtp_settings_of};
use crate::{Error, Result};

/// What to change about an account; `None` keeps what it has.
#[derive(Clone, Default)]
pub struct AccountChanges {
    /// The name in the folder pane (and the account's label in `settings.json`).
    pub name: Option<String>,
    /// The IMAP server.
    pub server: Option<ImapServer>,
    /// The login name; empty: the address.
    pub username: Option<String>,
    /// A new password. Required when the server, the login name or the SMTP server
    /// changes (see the [module docs](self)).
    pub password: Option<Sensitive<String>>,
    /// The SMTP server for sending (it logs in like IMAP).
    pub smtp: Option<SmtpServerSettings>,
}

impl std::fmt::Debug for AccountChanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountChanges")
            .field("name", &self.name.is_some())
            .field(
                "server",
                &self.server.as_ref().map(|s| (s.port, s.security)),
            )
            .field("username", &self.username.is_some())
            .field("password", &self.password.is_some())
            .field("smtp", &self.smtp.as_ref().map(|s| (s.port, s.security)))
            .finish()
    }
}

/// An account's settings as the edit dialog shows them.
#[derive(Clone)]
pub struct AccountDetails {
    /// Name, kind, address, IMAP server, sign-in method and sync state.
    pub info: AccountInfo,
    /// The login name as configured (empty: the address).
    pub username: String,
    /// The SMTP server of its default identity.
    pub smtp: Option<SmtpServerSettings>,
    /// The Microsoft tenant it signed in with.
    pub tenant: Option<MicrosoftTenant>,
    /// Whether a password is saved (password accounts; imported ones may have none).
    pub has_password: bool,
}

impl std::fmt::Debug for AccountDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountDetails")
            .field("info", &self.info)
            .field("tenant", &self.tenant)
            .field("has_password", &self.has_password)
            .finish_non_exhaustive()
    }
}

/// What "Test Connection" found for changed settings: IMAP works (else the test
/// failed), and whether sending works too.
#[derive(Debug)]
pub struct ChangesTest {
    /// The IMAP login and folder list.
    pub imap: ConnectionTest,
    /// The SMTP login, or why it failed.
    pub smtp: Result<SmtpServerSettings>,
}

/// An account as it is now.
struct Current {
    account: Account,
    config: Option<AccountConfig>,
    smtp: Option<SmtpServerSettings>,
}

/// An account as it will be.
#[derive(Debug, Clone, PartialEq)]
struct Resolved {
    name: String,
    config: Option<AccountConfig>,
    /// The SMTP server, if it changes.
    smtp: Option<SmtpServerSettings>,
    /// Whether it logs in differently: validate, then restart sync.
    reconnect: bool,
}

/// The account after `changes`, or why they can't be made (see the [module docs](self)
/// for the password rule).
fn resolve(current: &Current, changes: &AccountChanges) -> Result<Resolved> {
    let name = match &changes.name {
        Some(name) if name.trim().is_empty() => {
            return Err(account_error(
                AccountErrorKind::InvalidInput,
                "enter a name for the account",
            ));
        }
        Some(name) => name.trim().to_owned(),
        None => current.account.name.clone(),
    };
    let wants_login_change = changes.server.is_some()
        || changes.username.is_some()
        || changes.password.is_some()
        || changes.smtp.is_some();
    let Some(config) = &current.config else {
        if wants_login_change {
            return Err(account_error(
                AccountErrorKind::InvalidInput,
                "this account has no server to change (Local Folders)",
            ));
        }
        return Ok(Resolved {
            name,
            config: None,
            smtp: None,
            reconnect: false,
        });
    };

    if let Some(AccountAuth::OAuth(provider)) = config.auth() {
        let server_changed = changes
            .server
            .as_ref()
            .is_some_and(|server| *server != config.server());
        let login_changed = changes
            .username
            .as_ref()
            .is_some_and(|username| username.trim() != config.username.trim());
        let smtp_changed = changes.smtp.is_some() && changes.smtp != current.smtp;
        if server_changed || login_changed || smtp_changed || changes.password.is_some() {
            return Err(account_error(
                AccountErrorKind::InvalidInput,
                format!(
                    "this account signs in with {}: its servers are fixed and it has no \
                     password. Sign in again instead",
                    provider.display_name()
                ),
            ));
        }
        return Ok(Resolved {
            name,
            config: Some(config.clone()),
            smtp: None,
            reconnect: false,
        });
    }

    let mut next = config.clone();
    if let Some(server) = &changes.server {
        let server = check_server(server)?;
        next.host = server.host;
        next.port = server.port;
        next.security = server.security;
    }
    if let Some(username) = &changes.username {
        username.trim().clone_into(&mut next.username);
    }
    let password = changes.password.as_ref().filter(|p| !p.expose().is_empty());
    if changes.password.is_some() && password.is_none() {
        return Err(account_error(
            AccountErrorKind::InvalidInput,
            "enter the account's password",
        ));
    }
    if config.auth().is_none() {
        // An importer's method sync can't use (Kerberos, say): a password replaces it.
        if password.is_none() && wants_login_change {
            return Err(account_error(
                AccountErrorKind::InvalidInput,
                format!(
                    "Maze can't sign in with this account's method ({}): enter a password",
                    config.auth
                ),
            ));
        }
        if password.is_some() {
            "password".clone_into(&mut next.auth);
        }
    }
    let smtp = match &changes.smtp {
        Some(smtp) => Some(check_smtp(smtp)?).filter(|smtp| Some(smtp) != current.smtp.as_ref()),
        None => None,
    };
    let moves_password =
        next.server() != config.server() || next.login() != config.login() || smtp.is_some();
    if moves_password && password.is_none() {
        return Err(account_error(
            AccountErrorKind::InvalidInput,
            "enter the password again: Maze only sends a saved password to the servers it was \
             saved for",
        ));
    }
    Ok(Resolved {
        name,
        reconnect: moves_password || password.is_some() || next.auth != config.auth,
        config: Some(next),
        smtp,
    })
}

fn check_smtp(smtp: &SmtpServerSettings) -> Result<SmtpServerSettings> {
    let host = smtp.host.trim().trim_end_matches('.').to_owned();
    if host.is_empty() || host.contains(char::is_whitespace) || host.contains('/') {
        return Err(account_error(
            AccountErrorKind::InvalidInput,
            "enter the SMTP server's host name (smtp.example.com)",
        ));
    }
    if smtp.port == 0 {
        return Err(account_error(
            AccountErrorKind::InvalidInput,
            "the SMTP port must be between 1 and 65535",
        ));
    }
    Ok(SmtpServerSettings {
        host,
        port: smtp.port,
        security: smtp.security,
    })
}

fn input_of(server: SmtpServer) -> SmtpServerInput {
    SmtpServerInput {
        name: server.name,
        host: server.host,
        port: server.port,
        security: server.security,
        auth: server.auth,
        username: server.username,
        saves_sent: server.saves_sent,
    }
}

/// The SMTP server of the account's default identity.
fn default_smtp(
    reader: &impl store::Queries,
    account: AccountId,
) -> store::Result<Option<(SmtpServerId, SmtpServerInput)>> {
    let identities = reader.identities(Some(account))?;
    let Some(server) = identities
        .iter()
        .find(|i| i.input.is_default)
        .or_else(|| identities.first())
        .and_then(|i| i.input.smtp)
    else {
        return Ok(None);
    };
    Ok(reader
        .smtp_server(server)?
        .map(|row| (server, input_of(row))))
}

/// What the account's SMTP servers need after a change.
struct SmtpChange {
    old_name: String,
    old_login: String,
    name: String,
    login: String,
    /// New host, port and security for the default identity's server.
    server: Option<SmtpServerSettings>,
}

/// Updates the SMTP servers of `account`'s identities: new host, port and security
/// for the default identity's, the login name and the name where they followed the
/// account's. A server that identities of other accounts use too is copied first.
fn update_smtp_servers(
    w: &mut WriteTx<'_>,
    account: AccountId,
    config: &AccountConfig,
    change: &SmtpChange,
) -> store::Result<()> {
    let identities = w.identities(Some(account))?;
    let default = identities
        .iter()
        .find(|i| i.input.is_default)
        .or_else(|| identities.first())
        .cloned();
    let Some(default) = default else {
        return Ok(());
    };
    let Some(default_server) = default.input.smtp else {
        // Accounts added before Maze could send: give the default identity one.
        if let Some(input) = smtp_server_input(&change.name, config, change.server.clone()) {
            let server = w.create_smtp_server(&input)?;
            let mut changed = default.input.clone();
            changed.smtp = Some(server);
            w.update_identity(default.id, &changed)?;
        }
        return Ok(());
    };
    let servers: BTreeSet<SmtpServerId> = identities.iter().filter_map(|i| i.input.smtp).collect();
    let everyone = w.identities(None)?;
    for id in servers {
        let Some(row) = w.smtp_server(id)? else {
            continue;
        };
        let before = input_of(row);
        let mut input = before.clone();
        if id == default_server
            && let Some(server) = &change.server
        {
            set_smtp_settings(&mut input, server);
        }
        if input
            .username
            .as_deref()
            .is_none_or(|u| u.trim().is_empty() || u == change.old_login)
        {
            input.username = Some(change.login.clone());
        }
        if input.name.as_deref() == Some(change.old_name.as_str()) {
            input.name = Some(change.name.clone());
        }
        if input == before {
            continue;
        }
        let shared = everyone
            .iter()
            .any(|i| i.account != account && i.input.smtp == Some(id));
        if !shared {
            w.update_smtp_server(id, &input)?;
            continue;
        }
        let copy = w.create_smtp_server(&input)?;
        for identity in identities.iter().filter(|i| i.input.smtp == Some(id)) {
            let mut changed = identity.input.clone();
            changed.smtp = Some(copy);
            w.update_identity(identity.id, &changed)?;
        }
    }
    Ok(())
}

fn settings_error(error: &settings::EditError) -> Error {
    account_error(
        AccountErrorKind::InvalidInput,
        format!("can't rename the account: {error}"),
    )
}

impl CoreHandle {
    fn current_account(&self, id: AccountId) -> Result<Current> {
        let reader = self.store().reader()?;
        let account = reader
            .account(id)?
            .ok_or_else(|| Error::not_found("account", id))?;
        let smtp = default_smtp(&reader, id)?.map(|(_, server)| smtp_settings_of(&server));
        Ok(Current {
            config: AccountConfig::parse(&account.config_json),
            account,
            smtp,
        })
    }

    /// Everything the edit dialog shows about an account.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown account.
    pub async fn account_details(&self, id: AccountId) -> Result<AccountDetails> {
        let current = self.current_account(id)?;
        let info = self.info_of(&current.account)?;
        let has_password = match (&current.config, self.engine()) {
            (Some(config), Some(engine)) if config.auth() == Some(AccountAuth::Password) => {
                let secrets = AccountSecrets::new(engine.secrets(), current.account.uuid);
                let core = self.clone();
                core.run(async move { Ok(secrets.password().await.ok().flatten().is_some()) })
                    .await?
            }
            _ => false,
        };
        Ok(AccountDetails {
            info,
            username: current
                .config
                .as_ref()
                .map(|c| c.username.clone())
                .unwrap_or_default(),
            smtp: current.smtp,
            tenant: current
                .config
                .as_ref()
                .filter(|c| c.auth() == Some(AccountAuth::OAuth(auth::ProviderKind::Microsoft)))
                .map(AccountConfig::microsoft_tenant),
            has_password,
        })
    }

    /// Changes an account's name, password or servers (see the [module docs](self)):
    /// checks the new settings against the server, saves them and restarts its sync.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown account; [`Error::Account`] for bad input
    /// (a changed server without the password, an empty name), a name another account
    /// has, a rename `settings.json` can't take, an unreachable server, a rejected
    /// login or a failure to save the password; [`Error::NoSyncEngine`] for a new login
    /// without a sync engine.
    pub async fn update_account(
        &self,
        id: AccountId,
        changes: AccountChanges,
    ) -> Result<AccountInfo> {
        let core = self.clone();
        self.run(async move { core.update_account_inner(id, changes).await })
            .await
    }

    /// The password to log in with after `changes`: the new one, else the saved one.
    async fn login_password(
        &self,
        account: &Account,
        changes: &AccountChanges,
    ) -> Result<Sensitive<String>> {
        if let Some(password) = &changes.password {
            return Ok(password.clone());
        }
        let engine = self.engine_or_err()?;
        AccountSecrets::new(engine.secrets(), account.uuid)
            .password()
            .await
            .map_err(|err| {
                account_error(
                    AccountErrorKind::Secrets,
                    format!("can't read the saved password: {err}"),
                )
            })?
            .ok_or_else(|| {
                account_error(
                    AccountErrorKind::InvalidInput,
                    "no password is saved for this account: enter it",
                )
            })
    }

    async fn update_account_inner(
        &self,
        id: AccountId,
        changes: AccountChanges,
    ) -> Result<AccountInfo> {
        let current = self.current_account(id)?;
        let next = resolve(&current, &changes)?;
        let old_name = current.account.name.clone();
        let renamed = next.name != old_name;
        if renamed {
            if self
                .accounts()?
                .iter()
                .any(|a| a.id != id && a.name == next.name)
            {
                return Err(account_error(
                    AccountErrorKind::Duplicate,
                    format!(
                        "another account is called \u{201c}{}\u{201d} already",
                        next.name
                    ),
                ));
            }
            self.rename_account_settings(&old_name, &next.name, true)
                .map_err(|err| settings_error(&err))?;
        }
        let engine = self.engine();
        if next.reconnect {
            let Some(config) = &next.config else {
                return Err(Error::not_found("account", id));
            };
            let host_changed = current
                .config
                .as_ref()
                .is_none_or(|c| !c.host.eq_ignore_ascii_case(&config.host));
            if host_changed && let Some(email) = &config.email {
                self.check_duplicate_except(id, email, &config.host)?;
            }
            let password = self.login_password(&current.account, &changes).await?;
            self.validate(
                &config.server(),
                ImapAuth::Password {
                    user: config.login(),
                    password,
                },
            )
            .await?;
        }
        if !next.reconnect && !renamed && next.smtp.is_none() {
            // Nothing that changes how it works or what it's called.
            return self.info_of(&current.account);
        }

        if let Some(engine) = &engine {
            engine.stop(id).await;
        }
        // The old password, to put back if a later step fails.
        let secrets = engine
            .as_ref()
            .map(|engine| AccountSecrets::new(engine.secrets(), current.account.uuid));
        let mut saved = None;
        if let (Some(password), Some(secrets)) = (&changes.password, &secrets) {
            let old = secrets.password().await.ok().flatten();
            if let Err(err) = secrets.set_password(password.clone()).await {
                self.restart(engine.as_ref(), &current.account);
                return Err(account_error(
                    AccountErrorKind::Secrets,
                    format!("can't save the password: {err}"),
                ));
            }
            saved = Some(old);
        }

        let write = {
            let name = next.name.clone();
            let config = next.config.clone();
            let config_json = config.as_ref().map_or_else(
                || current.account.config_json.clone(),
                AccountConfig::to_json,
            );
            let ordinal = current.account.ordinal;
            let change = config.as_ref().map(|config| SmtpChange {
                old_name: old_name.clone(),
                old_login: current
                    .config
                    .as_ref()
                    .map(AccountConfig::login)
                    .unwrap_or_default(),
                name: name.clone(),
                login: config.login(),
                server: next.smtp.clone(),
            });
            self.store()
                .write(move |w| {
                    w.update_account(id, &name, &config_json, ordinal)?;
                    if let (Some(config), Some(change)) = (&config, &change) {
                        update_smtp_servers(w, id, config, change)?;
                    }
                    Ok(())
                })
                .await
        };
        if let Err(err) = write {
            if let (Some(old), Some(secrets)) = (saved, &secrets) {
                let restored = match old {
                    Some(old) => secrets.set_password(old).await,
                    None => secrets.delete_password().await,
                };
                if let Err(err) = restored {
                    tracing::error!(%err, account = %id, "putting the old password back");
                }
            }
            self.restart(engine.as_ref(), &current.account);
            return Err(err.into());
        }

        if renamed {
            // Checked above; only a change of the file in between can make this fail.
            if let Err(err) = self.rename_account_settings(&old_name, &next.name, false) {
                tracing::warn!(%err, account = %id, "moving a renamed account's settings");
            }
            // Aliases listed under the new name (there was no settings change to sync
            // them if nothing had to move).
            let settings = self.settings();
            if let Err(err) = self
                .store()
                .write(move |w| crate::aliases::sync(w, &settings.settings))
                .await
            {
                tracing::warn!(%err, "updating a renamed account's alias identities");
            }
        }
        let account = self
            .store()
            .reader()?
            .account(id)?
            .ok_or_else(|| Error::not_found("account", id))?;
        self.restart(engine.as_ref(), &account);
        tracing::info!(
            account = %id,
            renamed,
            reconnect = next.reconnect,
            smtp = next.smtp.is_some(),
            "account updated"
        );
        self.info_of(&account)
    }

    fn restart(&self, engine: Option<&Arc<crate::sync_engine::SyncEngine>>, account: &Account) {
        if let Some(engine) = engine {
            engine.start(account, None);
        }
    }

    /// Checks changed settings without saving anything: connects and logs in to the
    /// IMAP server (with the new password, or the saved one when the servers stay),
    /// then to the SMTP server. Accounts that sign in with Google or Microsoft test
    /// their current sign-in.
    ///
    /// # Errors
    ///
    /// As [`CoreHandle::update_account`] for the IMAP side; the SMTP outcome is in
    /// [`ChangesTest::smtp`].
    pub async fn test_account_changes(
        &self,
        id: AccountId,
        changes: AccountChanges,
    ) -> Result<ChangesTest> {
        let core = self.clone();
        self.run(async move { core.test_account_changes_inner(id, changes).await })
            .await
    }

    async fn test_account_changes_inner(
        &self,
        id: AccountId,
        changes: AccountChanges,
    ) -> Result<ChangesTest> {
        let engine = self.engine_or_err()?;
        let current = self.current_account(id)?;
        let next = resolve(&current, &changes)?;
        let Some(config) = next.config else {
            return Err(account_error(
                AccountErrorKind::InvalidInput,
                "this account has no server to test (Local Folders)",
            ));
        };
        let smtp_server = next.smtp.or(current.smtp).ok_or_else(|| {
            account_error(
                AccountErrorKind::InvalidInput,
                "enter the SMTP server's host name (smtp.example.com)",
            )
        });
        let (imap_auth, smtp_credentials) = match config.auth() {
            Some(AccountAuth::OAuth(_)) => {
                let tokens =
                    engine
                        .token_manager(&current.account, &config)
                        .map_err(|message| {
                            account_error(AccountErrorKind::OAuthNotConfigured, message)
                        })?;
                let access_token = tokens.access_token().await.map_err(|err| {
                    account_error(
                        AccountErrorKind::SignInFailed,
                        format!("the sign-in doesn't work any more: {err}. Sign in again"),
                    )
                })?;
                let user = config.email.clone().unwrap_or_else(|| config.login());
                (
                    ImapAuth::XOAuth2 {
                        user: user.clone(),
                        access_token: access_token.clone(),
                    },
                    smtp::Credentials::XOAuth2 { user, access_token },
                )
            }
            _ => {
                let password = self.login_password(&current.account, &changes).await?;
                (
                    ImapAuth::Password {
                        user: config.login(),
                        password: password.clone(),
                    },
                    smtp::Credentials::Password {
                        user: config.login(),
                        password,
                    },
                )
            }
        };
        let imap = self.validate(&config.server(), imap_auth).await?;
        let smtp = match smtp_server {
            Ok(server) => self
                .check_smtp_login(&server, &smtp_credentials)
                .await
                .map(|()| server),
            Err(err) => Err(err),
        };
        Ok(ChangesTest { imap, smtp })
    }

    /// Signs a Google or Microsoft account in again in the browser (a revoked or
    /// expired sign-in, "sign-in required"), with the account's tenant or `tenant` for
    /// Microsoft, then restarts its sync with the new sign-in. The browser sign-in must
    /// be for the account's own address: the server refuses another one.
    ///
    /// # Errors
    ///
    /// [`Error::Account`] for a password account, a missing client ID, a failed or
    /// cancelled sign-in, a rejected login or a failure to save the sign-in.
    pub async fn sign_in_again(
        &self,
        id: AccountId,
        tenant: Option<MicrosoftTenant>,
    ) -> Result<AccountInfo> {
        let core = self.clone();
        self.run(async move { core.sign_in_again_inner(id, tenant).await })
            .await
    }

    async fn sign_in_again_inner(
        &self,
        id: AccountId,
        tenant: Option<MicrosoftTenant>,
    ) -> Result<AccountInfo> {
        let engine = self.engine_or_err()?;
        let current = self.current_account(id)?;
        let Some(mut config) = current.config else {
            return Err(account_error(
                AccountErrorKind::InvalidInput,
                "this account has no sign-in (Local Folders)",
            ));
        };
        let Some(AccountAuth::OAuth(provider)) = config.auth() else {
            return Err(account_error(
                AccountErrorKind::InvalidInput,
                "this account signs in with a password: enter the new one instead",
            ));
        };
        let registry = engine.registry(&self.settings().settings);
        if registry.get(provider).credentials().is_none() {
            return Err(account_error(
                AccountErrorKind::OAuthNotConfigured,
                missing_client_message(provider),
            ));
        }
        let tenant = (provider == auth::ProviderKind::Microsoft)
            .then(|| tenant.unwrap_or_else(|| config.microsoft_tenant()));
        let client = registry
            .client_for_tenant(provider, tenant.as_ref())
            .map_err(|err| account_error(AccountErrorKind::OAuthNotConfigured, err.to_string()))?;
        let email = config.email.clone().unwrap_or_else(|| config.login());
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
            &config.server(),
            ImapAuth::XOAuth2 {
                user: email,
                access_token: tokens.access_token.clone(),
            },
        )
        .await?;

        engine.stop(id).await;
        let manager = Arc::new(OAuthTokenManager::new(
            client,
            AccountSecrets::new(engine.secrets(), current.account.uuid),
        ));
        if let Err(err) = manager.store_tokens(tokens).await {
            engine.start(&current.account, None);
            return Err(account_error(
                AccountErrorKind::Secrets,
                format!("can't save the sign-in: {err}"),
            ));
        }
        let tenant = tenant.map(|t| t.to_string());
        let mut account = current.account;
        if tenant.is_some() && tenant != config.oauth_tenant {
            config.oauth_tenant = tenant;
            let (name, config_json, ordinal) =
                (account.name.clone(), config.to_json(), account.ordinal);
            if let Err(err) = self
                .store()
                .write(move |w| w.update_account(id, &name, &config_json, ordinal))
                .await
            {
                tracing::warn!(%err, account = %id, "saving the tenant of a new sign-in");
            } else {
                account.config_json = config.to_json();
            }
        }
        engine.start(&account, Some(manager));
        tracing::info!(account = %id, %provider, "signed in again");
        self.info_of(&account)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use store::AccountKind;

    use super::*;
    use crate::accounts::ImapSecurity;

    fn current(config: &str) -> Current {
        Current {
            account: Account {
                id: AccountId(1),
                uuid: auth::Uuid::nil(),
                kind: AccountKind::Imap,
                name: "Work".into(),
                config_json: config.into(),
                sync_state_json: None,
                ordinal: 0,
            },
            config: AccountConfig::parse(config),
            smtp: Some(SmtpServerSettings::new(
                "smtp.example.org",
                ImapSecurity::Starttls,
            )),
        }
    }

    const IMAP: &str =
        r#"{"email":"alice@example.org","host":"imap.example.org","port":993,"security":"tls"}"#;

    fn password() -> Option<Sensitive<String>> {
        Some(Sensitive::new("secret".into()))
    }

    fn invalid(result: Result<Resolved>) -> String {
        match result {
            Err(Error::Account {
                kind: AccountErrorKind::InvalidInput,
                message,
            }) => message,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_new_name_or_password_needs_nothing_else() {
        let now = current(IMAP);
        let renamed = resolve(
            &now,
            &AccountChanges {
                name: Some("  Job ".into()),
                ..AccountChanges::default()
            },
        )
        .unwrap();
        assert_eq!(renamed.name, "Job");
        assert!(!renamed.reconnect);
        assert_eq!(renamed.config, now.config);

        let password = resolve(
            &now,
            &AccountChanges {
                password: password(),
                ..AccountChanges::default()
            },
        )
        .unwrap();
        assert!(password.reconnect);
        assert_eq!(password.config, now.config);

        // The values it has already count as no change.
        let same = resolve(
            &now,
            &AccountChanges {
                server: Some(ImapServer::new("imap.example.org", ImapSecurity::Tls)),
                username: Some(String::new()),
                smtp: now.smtp.clone(),
                ..AccountChanges::default()
            },
        )
        .unwrap();
        assert!(!same.reconnect);
        assert_eq!(same.smtp, None);
        assert!(
            invalid(resolve(
                &now,
                &AccountChanges {
                    name: Some(" ".into()),
                    ..AccountChanges::default()
                }
            ))
            .contains("name")
        );
    }

    #[test]
    fn the_saved_password_stays_with_its_servers() {
        let now = current(IMAP);
        let elsewhere = |changes: AccountChanges| {
            let message = invalid(resolve(&now, &changes));
            assert!(message.contains("enter the password again"), "{message}");
        };
        elsewhere(AccountChanges {
            server: Some(ImapServer::new("imap.attacker.example", ImapSecurity::Tls)),
            ..AccountChanges::default()
        });
        // The same host without encryption would show it to the network.
        elsewhere(AccountChanges {
            server: Some(ImapServer::new("imap.example.org", ImapSecurity::Insecure)),
            ..AccountChanges::default()
        });
        elsewhere(AccountChanges {
            username: Some("alice".into()),
            ..AccountChanges::default()
        });
        elsewhere(AccountChanges {
            smtp: Some(SmtpServerSettings::new(
                "smtp.attacker.example",
                ImapSecurity::Tls,
            )),
            ..AccountChanges::default()
        });

        // With the password, it goes.
        let moved = resolve(
            &now,
            &AccountChanges {
                server: Some(ImapServer {
                    host: "mail.example.org.".into(),
                    port: 143,
                    security: ImapSecurity::Starttls,
                }),
                username: Some(" alice ".into()),
                password: password(),
                smtp: Some(SmtpServerSettings::new(
                    "mail.example.org",
                    ImapSecurity::Tls,
                )),
                ..AccountChanges::default()
            },
        )
        .unwrap();
        let config = moved.config.unwrap();
        assert_eq!(
            config.server(),
            ImapServer {
                host: "mail.example.org".into(),
                port: 143,
                security: ImapSecurity::Starttls,
            }
        );
        assert_eq!(config.login(), "alice");
        assert_eq!(
            moved.smtp,
            Some(SmtpServerSettings::new(
                "mail.example.org",
                ImapSecurity::Tls
            ))
        );
        assert!(moved.reconnect);
        assert!(
            invalid(resolve(
                &now,
                &AccountChanges {
                    password: Some(Sensitive::new(String::new())),
                    ..AccountChanges::default()
                }
            ))
            .contains("password")
        );
    }

    #[test]
    fn browser_sign_ins_keep_their_servers() {
        let now = current(
            r#"{"email":"carol@gmail.com","host":"imap.gmail.com","port":993,"auth":"oauth2","oauth_provider":"google"}"#,
        );
        assert!(
            resolve(
                &now,
                &AccountChanges {
                    name: Some("Gmail".into()),
                    server: Some(ImapServer::new("imap.gmail.com", ImapSecurity::Tls)),
                    ..AccountChanges::default()
                }
            )
            .is_ok()
        );
        let message = invalid(resolve(
            &now,
            &AccountChanges {
                password: password(),
                ..AccountChanges::default()
            },
        ));
        assert!(message.contains("signs in with Google"), "{message}");
    }

    #[test]
    fn unsupported_methods_take_a_password() {
        let now = current(r#"{"email":"e@example.org","host":"h.example.org","auth":"gssapi"}"#);
        let message = invalid(resolve(
            &now,
            &AccountChanges {
                server: Some(ImapServer::new("h.example.org", ImapSecurity::Starttls)),
                ..AccountChanges::default()
            },
        ));
        assert!(message.contains("gssapi"), "{message}");
        let fixed = resolve(
            &now,
            &AccountChanges {
                password: password(),
                ..AccountChanges::default()
            },
        )
        .unwrap();
        assert_eq!(fixed.config.unwrap().auth(), Some(AccountAuth::Password));
        assert!(fixed.reconnect);

        let local = Current {
            config: None,
            ..current("{}")
        };
        assert!(
            resolve(
                &local,
                &AccountChanges {
                    name: Some("Archive".into()),
                    ..AccountChanges::default()
                }
            )
            .is_ok()
        );
        assert!(
            invalid(resolve(
                &local,
                &AccountChanges {
                    password: password(),
                    ..AccountChanges::default()
                }
            ))
            .contains("Local Folders")
        );
    }
}
