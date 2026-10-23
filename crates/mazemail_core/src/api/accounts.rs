//! `mazemail account list|add|remove` and `mazemail sync` (PLAN §4.11).
//!
//! # Permissions
//!
//! Adding and removing accounts changes Maze itself and handles credentials, so the
//! registry gives them the `admin` class: only a **writer** token may ask, and the user
//! approves every request in Maze (the approval never shows the password, the
//! dispatcher redacts it). Listing accounts and "sync now" are reads: they reveal no
//! mail and only make Maze look for new mail sooner, so every profile may run them,
//! limited to the accounts in the token's scope (accounts whose AI policy excludes
//! agents stay invisible, as everywhere in the local API).
//!
//! A new account's password arrives in the request (the CLI read it from the terminal
//! without echo, or from stdin) and goes straight into the encrypted store.

use local_api::registry::args::{
    AccountAddArgs, AccountRemoveArgs, OAuthProvider, SecurityArg, SyncArgs,
};
use local_api::registry::output::{AccountOutput, AccountRemoved, SyncOutput};
use local_api::{CommandError, ErrorCode, RequestContext};
use mazemail_types::PublicId as _;

use crate::accounts::{
    AccountAuth, AccountErrorKind, AccountInfo, AddImapAccount, AddOAuthAccount, ImapSecurity,
    ImapServer, suggest_imap_server,
};
use crate::handle::CoreHandle;
use crate::outgoing::{SmtpServerSettings, smtp_default_port};
use crate::sync_bridge::{RefreshScope, SyncState};
use crate::{Error, ProviderKind};

/// The wire form of an account.
pub(crate) fn account_output(info: &AccountInfo) -> AccountOutput {
    let (state, status) = match &info.sync {
        Some(sync) => (
            match &sync.state {
                SyncState::Connecting => "connecting",
                SyncState::Idle => "idle",
                SyncState::Syncing { .. } => "syncing",
                SyncState::Offline => "offline",
                SyncState::AuthRequired => "auth_required",
                SyncState::Error { .. } => "error",
            },
            sync.text(),
        ),
        None => ("not_synced", "Not synced".to_owned()),
    };
    AccountOutput {
        id: info.id.to_public(),
        name: info.name.clone(),
        kind: info.kind.as_str().to_owned(),
        email: info.email.clone(),
        server: info
            .server
            .as_ref()
            .map(|s| format!("{}:{}", s.host, s.port)),
        auth: info.auth.map(|auth| match auth {
            AccountAuth::Password => "password".to_owned(),
            AccountAuth::OAuth(provider) => format!("oauth:{}", provider.as_str()),
        }),
        state: state.to_owned(),
        status,
    }
}

/// An account error as the client sees it. The messages are written for people and
/// carry no secrets.
pub(crate) fn account_error(error: Error) -> CommandError {
    match error {
        Error::Account { kind, message } => {
            let code = match kind {
                AccountErrorKind::InvalidInput
                | AccountErrorKind::Duplicate
                | AccountErrorKind::OAuthNotConfigured
                | AccountErrorKind::AuthFailed => ErrorCode::InvalidArgs,
                _ => ErrorCode::Internal,
            };
            CommandError::new(code, message)
        }
        Error::NoSyncEngine => {
            CommandError::new(ErrorCode::Unavailable, Error::NoSyncEngine.to_string())
        }
        other => super::handler::core_error(other),
    }
}

fn security(arg: SecurityArg) -> ImapSecurity {
    match arg {
        SecurityArg::Tls => ImapSecurity::Tls,
        SecurityArg::Starttls => ImapSecurity::Starttls,
        SecurityArg::None => ImapSecurity::Insecure,
    }
}

/// The SMTP server from `--smtp-host`, `--smtp-port` and `--smtp-security`; `None`
/// (derived) without a host.
fn smtp_settings(
    host: Option<String>,
    port: Option<u16>,
    security_arg: Option<SecurityArg>,
) -> Option<SmtpServerSettings> {
    let host = host
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())?;
    let security = security_arg.map_or(ImapSecurity::Starttls, security);
    Some(SmtpServerSettings {
        host,
        port: port.unwrap_or_else(|| smtp_default_port(security)),
        security,
    })
}

/// `account list`.
pub(crate) fn list(
    core: &CoreHandle,
    ctx: &RequestContext,
) -> Result<Vec<AccountOutput>, CommandError> {
    Ok(core
        .accounts_with_status()
        .map_err(account_error)?
        .iter()
        .filter(|info| ctx.scope.allows_account(info.id))
        .map(account_output)
        .collect())
}

/// `account add`.
pub(crate) async fn add(
    core: &CoreHandle,
    args: AccountAddArgs,
) -> Result<AccountOutput, CommandError> {
    let email = args.email.expose().trim().to_owned();
    let info = match args.oauth {
        Some(provider) => {
            core.add_oauth_account(AddOAuthAccount {
                provider: match provider {
                    OAuthProvider::Google => ProviderKind::Google,
                    OAuthProvider::Microsoft => ProviderKind::Microsoft,
                },
                email,
                display_name: args.name,
            })
            .await
        }
        None => {
            let security = args.security.map(security);
            let suggested = suggest_imap_server(&email);
            let host = match (args.host, &suggested) {
                (Some(host), _) => host,
                (None, Some(suggestion)) => suggestion.server.host.clone(),
                (None, None) => {
                    return Err(CommandError::invalid_args(
                        "host: can't be detected from the address; pass --host",
                    ));
                }
            };
            let security = security
                .or_else(|| {
                    suggested
                        .as_ref()
                        .filter(|s| s.server.host == host)
                        .map(|s| s.server.security)
                })
                .unwrap_or_default();
            let port = args.port.unwrap_or_else(|| security.default_port());
            let password = args.password.ok_or_else(|| {
                CommandError::invalid_args("password: required unless signing in with oauth")
            })?;
            core.add_imap_account(AddImapAccount {
                display_name: args.name,
                email,
                server: ImapServer {
                    host,
                    port,
                    security,
                },
                username: args.username.map(|u| u.expose().clone()),
                password,
                smtp: smtp_settings(args.smtp_host, args.smtp_port, args.smtp_security),
            })
            .await
        }
    }
    .map_err(account_error)?;
    Ok(account_output(&info))
}

/// `account remove`.
pub(crate) async fn remove(
    core: &CoreHandle,
    ctx: &RequestContext,
    args: &AccountRemoveArgs,
) -> Result<AccountRemoved, CommandError> {
    if !ctx.scope.allows_account(args.id) {
        return Err(CommandError::not_found(format!("no account {}", args.id)));
    }
    core.remove_account(args.id).await.map_err(account_error)?;
    Ok(AccountRemoved {
        id: args.id.to_public(),
        removed: true,
    })
}

/// `sync`.
pub(crate) fn sync(
    core: &CoreHandle,
    ctx: &RequestContext,
    args: &SyncArgs,
) -> Result<SyncOutput, CommandError> {
    let synced = core.synced_accounts();
    let accounts: Vec<_> = match args.account {
        Some(account) => {
            if !ctx.scope.allows_account(account) {
                return Err(CommandError::not_found(format!("no account {account}")));
            }
            if !synced.contains(&account) {
                return Err(CommandError::invalid_args(format!(
                    "account {account} isn't synced (local folders, or it needs a sign-in method \
                     Maze can't sync)"
                )));
            }
            vec![account]
        }
        None => synced
            .into_iter()
            .filter(|account| ctx.scope.allows_account(*account))
            .collect(),
    };
    for account in &accounts {
        core.request_refresh(RefreshScope::Account(*account));
    }
    Ok(SyncOutput {
        accounts: accounts.iter().map(|a| a.to_public()).collect(),
    })
}
