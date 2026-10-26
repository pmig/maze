//! The sync engine inside core: one `sync` account actor per IMAP, Gmail and
//! Microsoft 365 account (PLAN §4.5, CONCEPT §7).
//!
//! [`SyncEngine`] is core's [`SyncBridge`] when
//! [`CoreConfig::sync_engine`](crate::CoreConfig::sync_engine) is set:
//!
//! - **Start.** When the core opens, every account with an IMAP config gets an actor
//!   ([`sync::spawn_account`]) with the real ingest pipeline ([`CoreIngest`]), an
//!   [`ImapConnector`] for sessions and its [`AccountCredentials`] from `auth`. Adding
//!   an account starts its actor; removing one stops it.
//! - **Status.** Each actor's status (`sync::SyncStatus`) is forwarded into the core's
//!   event batches as [`SyncStatus`] with the engine's sentence ("Offline: … Maze will
//!   try again in 30 seconds."), and kept for
//!   [`CoreHandle::accounts_with_status`](crate::CoreHandle::accounts_with_status).
//!   New messages, flag changes and bodies need nothing extra: the engine writes them
//!   to the store, whose change sets become event batches (thread list refreshes).
//! - **Requests.** [`SyncBridge::fetch_body`] sends an interactive body fetch to the
//!   message's account, [`SyncBridge::refresh`] a "sync now", and
//!   [`SyncBridge::ops_queued`] a replay nudge.
//! - **Sleep and network.** A watchdog compares the wall clock with the monotonic clock
//!   (which stops while the computer sleeps) once a minute; a jump means the computer
//!   slept, and every actor reconnects and syncs once.
//!   [`CoreHandle::network_changed`](crate::CoreHandle::network_changed) and
//!   [`CoreHandle::system_woke`](crate::CoreHandle::system_woke) are the hooks for
//!   platform notifications (`NSWorkspaceDidWakeNotification`, reachability).
//! - **Shutdown.** [`CoreHandle::shutdown`](crate::CoreHandle::shutdown) stops every
//!   actor (each finishes its batch and logs out) before the store closes.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use auth::{
    AccountSecrets, BrowserOpener, OAuthTokenManager, ProviderRegistry, SecretStore, SystemBrowser,
};
use imap::{ImapAuth, ImapConfig};
use mazemail_types::{AccountId, MessageId};
use settings::LoadedSettings;
use store::{Account, OpId, Queries as _, Store};
use sync::{
    AccountHandle, AccountSyncConfig, BodyPriority, CredentialProvider, ImapSessionFactory,
    SessionFactory, SyncCommand, SyncDeps, SyncScope, SyncTiming,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::accounts::{AccountAuth, AccountConfig, ImapServer, registry_from_settings};
use crate::credentials::{AccountCredentials, Method};
use crate::events::CoreEvent;
use crate::sync_bridge::{FetchPriority, RefreshScope, SyncBridge, SyncState, SyncStatus};
use crate::sync_ingest::CoreIngest;

/// How often the sleep watchdog looks at the clocks.
const WAKE_CHECK: Duration = Duration::from_secs(60);

/// A wall-clock jump beyond the monotonic clock of more than this means the computer
/// slept.
const SLEEP_THRESHOLD: Duration = Duration::from_secs(30);

/// How long shutdown waits for one actor to log out.
const ACTOR_SHUTDOWN: Duration = Duration::from_secs(5);

/// Opens IMAP sessions for an account: the real network in production, a fake server
/// in tests.
pub trait ImapConnector: Send + Sync + fmt::Debug {
    /// A session factory for `server` that logs in with `credentials`.
    fn sessions(
        &self,
        server: &ImapServer,
        credentials: Arc<dyn CredentialProvider>,
    ) -> Arc<dyn SessionFactory>;
}

/// The real network: [`ImapSessionFactory`] over `imap::ImapClient` with the platform
/// TLS verifier.
#[derive(Debug, Clone, Copy, Default)]
pub struct NetworkImapConnector;

impl ImapConnector for NetworkImapConnector {
    fn sessions(
        &self,
        server: &ImapServer,
        credentials: Arc<dyn CredentialProvider>,
    ) -> Arc<dyn SessionFactory> {
        // The placeholder login is replaced by the provider's on every connect.
        let config = ImapConfig::new(server.host.clone(), server.port, ImapAuth::password("", ""))
            .with_security(server.security.to_imap());
        Arc::new(ImapSessionFactory::new(config, credentials))
    }
}

/// Where the sync engine keeps account passwords and refresh tokens.
#[derive(Clone)]
pub enum AccountSecretsLocation {
    /// In the encrypted store ([`StoreSecretStore`](crate::StoreSecretStore), ADR-017),
    /// as in the app. `legacy` is where earlier versions kept them (the OS keychain,
    /// [`auth::legacy_system_secret_store`]): the core moves those items into the store
    /// when it opens (see [`crate::secrets`]).
    Encrypted {
        /// The keychain store of earlier versions, if any.
        legacy: Option<Arc<dyn SecretStore>>,
    },
    /// In this store instead (tests' in-memory store, the insecure development file).
    Custom(Arc<dyn SecretStore>),
}

impl fmt::Debug for AccountSecretsLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encrypted { legacy } => {
                f.debug_struct("Encrypted").field("legacy", legacy).finish()
            }
            Self::Custom(store) => f.debug_tuple("Custom").field(store).finish(),
        }
    }
}

/// What core needs to run the sync engine (see the [module docs](self)).
#[derive(Clone)]
pub struct SyncEngineConfig {
    /// Where passwords and refresh tokens live (the encrypted store in production).
    pub secrets: AccountSecretsLocation,
    /// Opens IMAP sessions.
    pub connector: Arc<dyn ImapConnector>,
    /// Opens the system browser for OAuth sign-ins.
    pub browser: Arc<dyn BrowserOpener>,
    /// OAuth providers; `None`: the built-in ones with the client IDs from the settings
    /// (`auth.google.client_id`, …), read at each use.
    pub providers: Option<ProviderRegistry>,
    /// The engine's timers.
    pub timing: SyncTiming,
    /// Submits mail (SMTP over the network in production).
    pub mailer: Arc<dyn smtp::Mailer>,
}

impl fmt::Debug for SyncEngineConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncEngineConfig")
            .field("secrets", &self.secrets)
            .field("connector", &self.connector)
            .field("timing", &self.timing)
            .field("mailer", &self.mailer)
            .finish_non_exhaustive()
    }
}

impl SyncEngineConfig {
    /// The production setup: account secrets in the encrypted store, moved there from
    /// `legacy` (the keychain of earlier versions) at startup; the network, the system
    /// browser, the settings' OAuth clients, default timers.
    pub fn encrypted(legacy: Option<Arc<dyn SecretStore>>) -> Self {
        Self::at(AccountSecretsLocation::Encrypted { legacy })
    }

    /// Like [`SyncEngineConfig::encrypted`], with the account secrets in `secrets`
    /// instead (tests, the insecure development file).
    pub fn new(secrets: Arc<dyn SecretStore>) -> Self {
        Self::at(AccountSecretsLocation::Custom(secrets))
    }

    /// The production setup with the account secrets at `secrets`.
    pub fn at(secrets: AccountSecretsLocation) -> Self {
        Self {
            secrets,
            connector: Arc::new(NetworkImapConnector),
            browser: Arc::new(SystemBrowser),
            providers: None,
            timing: SyncTiming::default(),
            mailer: Arc::new(smtp::LettreMailer),
        }
    }

    /// Submits mail with `mailer`.
    #[must_use]
    pub fn with_mailer(mut self, mailer: Arc<dyn smtp::Mailer>) -> Self {
        self.mailer = mailer;
        self
    }

    /// Opens sessions with `connector`.
    #[must_use]
    pub fn with_connector(mut self, connector: Arc<dyn ImapConnector>) -> Self {
        self.connector = connector;
        self
    }

    /// Uses these timers.
    #[must_use]
    pub fn with_timing(mut self, timing: SyncTiming) -> Self {
        self.timing = timing;
        self
    }

    /// Uses these OAuth providers instead of the settings'.
    #[must_use]
    pub fn with_providers(mut self, providers: ProviderRegistry) -> Self {
        self.providers = Some(providers);
        self
    }

    /// Opens sign-in pages with `browser`.
    #[must_use]
    pub fn with_browser(mut self, browser: Arc<dyn BrowserOpener>) -> Self {
        self.browser = browser;
        self
    }
}

/// One running account actor.
struct Running {
    handle: AccountHandle,
    forwarder: JoinHandle<()>,
}

/// The engine: account actors and the bridge to them. See the [module docs](self).
pub struct SyncEngine {
    store: Store,
    ingest: Arc<CoreIngest>,
    config: SyncEngineConfig,
    /// Where account secrets live ([`SyncEngineConfig::secrets`], resolved).
    secrets: Arc<dyn SecretStore>,
    settings: watch::Receiver<Arc<LoadedSettings>>,
    events: mpsc::UnboundedSender<CoreEvent>,
    runtime: tokio::runtime::Handle,
    actors: Mutex<HashMap<AccountId, Running>>,
    /// One OAuth token manager per account, shared by sync and sending, so a refresh
    /// (which may rotate the refresh token) never races.
    tokens: Mutex<HashMap<AccountId, Arc<OAuthTokenManager>>>,
    statuses: Arc<Mutex<HashMap<AccountId, SyncStatus>>>,
    watchdog: Mutex<Option<JoinHandle<()>>>,
}

impl fmt::Debug for SyncEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncEngine")
            .field("accounts", &lock(&self.actors).len())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Core's view of an engine status.
pub(crate) fn core_status(account: AccountId, status: &sync::SyncStatus) -> Option<SyncStatus> {
    let state = match status {
        sync::SyncStatus::Connecting => SyncState::Connecting,
        sync::SyncStatus::Syncing { done, total } => SyncState::Syncing {
            folder: None,
            done: *done as u64,
            total: Some(*total as u64),
        },
        sync::SyncStatus::Idle => SyncState::Idle,
        sync::SyncStatus::Offline { .. } => SyncState::Offline,
        sync::SyncStatus::AuthRequired { .. } => SyncState::AuthRequired,
        sync::SyncStatus::Error { message } => SyncState::Error {
            message: message.clone(),
        },
        // Stopped actors belong to removed accounts or a closing core.
        sync::SyncStatus::Stopped => return None,
        _ => SyncState::Error {
            message: status.text(),
        },
    };
    Some(SyncStatus {
        account,
        state,
        detail: Some(status.text()),
    })
}

impl SyncEngine {
    pub(crate) fn new(
        store: Store,
        ingest: CoreIngest,
        config: SyncEngineConfig,
        secrets: Arc<dyn SecretStore>,
        settings: watch::Receiver<Arc<LoadedSettings>>,
        events: mpsc::UnboundedSender<CoreEvent>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            store,
            ingest: Arc::new(ingest),
            config,
            secrets,
            settings,
            events,
            runtime,
            actors: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
            statuses: Arc::new(Mutex::new(HashMap::new())),
            watchdog: Mutex::new(None),
        }
    }

    pub(crate) fn connector(&self) -> &Arc<dyn ImapConnector> {
        &self.config.connector
    }

    pub(crate) fn secrets(&self) -> Arc<dyn SecretStore> {
        Arc::clone(&self.secrets)
    }

    pub(crate) fn browser(&self) -> &Arc<dyn BrowserOpener> {
        &self.config.browser
    }

    pub(crate) fn mailer(&self) -> Arc<dyn smtp::Mailer> {
        Arc::clone(&self.config.mailer)
    }

    /// The OAuth token manager of an OAuth account (shared by sync and sending);
    /// `Err` says why there is none (no client ID configured, say).
    pub(crate) fn token_manager(
        &self,
        account: &Account,
        config: &AccountConfig,
    ) -> Result<Arc<OAuthTokenManager>, String> {
        if let Some(tokens) = lock(&self.tokens).get(&account.id) {
            return Ok(Arc::clone(tokens));
        }
        let Some(AccountAuth::OAuth(provider)) = config.auth() else {
            return Err("this account doesn't sign in with OAuth".to_owned());
        };
        // Refreshes go to the tenant the account signed in with, whatever
        // `auth.microsoft.tenant` says now.
        let tenant = (provider == auth::ProviderKind::Microsoft).then(|| config.microsoft_tenant());
        let manager = self
            .registry(&self.settings.borrow().settings)
            .client_for_tenant(provider, tenant.as_ref())
            .map(|client| {
                Arc::new(OAuthTokenManager::new(
                    client,
                    AccountSecrets::new(self.secrets(), account.uuid),
                ))
            })
            .map_err(|err| err.to_string())?;
        Ok(Arc::clone(
            lock(&self.tokens).entry(account.id).or_insert(manager),
        ))
    }

    /// The OAuth providers: the configured ones, or the settings' client IDs.
    pub(crate) fn registry(&self, settings: &settings::Settings) -> ProviderRegistry {
        self.config
            .providers
            .clone()
            .unwrap_or_else(|| registry_from_settings(settings))
    }

    /// What the actor of `account` is doing, if it runs.
    pub(crate) fn activity(&self, account: AccountId) -> Option<sync::SyncActivity> {
        self.handle(account).map(|handle| handle.activity())
    }

    /// The last status of `account`, if it is synced.
    pub(crate) fn status(&self, account: AccountId) -> Option<SyncStatus> {
        lock(&self.statuses).get(&account).cloned()
    }

    fn handle(&self, account: AccountId) -> Option<AccountHandle> {
        lock(&self.actors)
            .get(&account)
            .map(|running| running.handle.clone())
    }

    fn handles(&self) -> Vec<AccountHandle> {
        lock(&self.actors)
            .values()
            .map(|running| running.handle.clone())
            .collect()
    }

    /// Starts an actor for every synced account and the sleep watchdog.
    pub(crate) fn start_all(self: &Arc<Self>) -> crate::Result<()> {
        let accounts = self.store.reader()?.accounts()?;
        for account in &accounts {
            self.start(account, None);
        }
        let engine = Arc::downgrade(self);
        let watchdog = self.runtime.spawn(async move {
            let mut wall = SystemTime::now();
            let mut mono = Instant::now();
            loop {
                tokio::time::sleep(WAKE_CHECK).await;
                let Some(engine) = engine.upgrade() else {
                    return;
                };
                let wall_elapsed = wall.elapsed().unwrap_or_default();
                let mono_elapsed = mono.elapsed();
                if wall_elapsed > mono_elapsed + SLEEP_THRESHOLD {
                    tracing::info!(
                        slept_secs = (wall_elapsed - mono_elapsed).as_secs(),
                        "the computer slept; reconnecting every account"
                    );
                    engine.wake();
                }
                wall = SystemTime::now();
                mono = Instant::now();
            }
        });
        *lock(&self.watchdog) = Some(watchdog);
        Ok(())
    }

    /// The credentials of an account, or `None` if its login method isn't supported.
    fn credentials(
        &self,
        account: &Account,
        config: &AccountConfig,
        tokens: Option<Arc<OAuthTokenManager>>,
    ) -> Option<Arc<AccountCredentials>> {
        let secrets = AccountSecrets::new(self.secrets(), account.uuid);
        let method = match config.auth()? {
            AccountAuth::Password => Method::Password(secrets),
            AccountAuth::OAuth(_) => Method::OAuth(match tokens {
                Some(tokens) => {
                    lock(&self.tokens).insert(account.id, Arc::clone(&tokens));
                    Ok(tokens)
                }
                None => self.token_manager(account, config),
            }),
        };
        Some(Arc::new(AccountCredentials::new(config.login(), method)))
    }

    /// Starts the actor of `account` if it is synced and not running yet. `tokens` is
    /// the token manager of a sign-in that just happened.
    pub(crate) fn start(&self, account: &Account, tokens: Option<Arc<OAuthTokenManager>>) {
        if !matches!(
            account.kind,
            store::AccountKind::Imap | store::AccountKind::Gmail | store::AccountKind::M365
        ) {
            return;
        }
        let Some(config) = AccountConfig::parse(&account.config_json) else {
            return;
        };
        let Some(credentials) = self.credentials(account, &config, tokens) else {
            tracing::info!(account = %account.id, method = %config.auth, "login method not supported for sync");
            return;
        };
        let mut actors = lock(&self.actors);
        if actors.contains_key(&account.id) {
            return;
        }
        let account_settings = self.settings.borrow().settings.account(&account.name);
        let mut sync_config = AccountSyncConfig::new(account.id).with_offline(
            account_settings.offline,
            account_settings.offline_recent_days,
        );
        sync_config.timing = self.config.timing;
        let deps = SyncDeps {
            store: self.store.clone(),
            ingest: Arc::clone(&self.ingest) as Arc<dyn sync::MessageIngest>,
            sessions: self
                .config
                .connector
                .sessions(&config.server(), credentials),
        };
        let handle = {
            let _runtime = self.runtime.enter();
            sync::spawn_account(sync_config, deps)
        };
        // Known at once, so `accounts_with_status` right after adding says "Connecting…".
        if let Some(status) = core_status(account.id, &handle.status()) {
            lock(&self.statuses).insert(account.id, status);
        }
        let forwarder = self.runtime.spawn(forward_status(
            account.id,
            handle.status_watch(),
            handle.subscribe(),
            self.events.clone(),
            Arc::clone(&self.statuses),
        ));
        actors.insert(account.id, Running { handle, forwarder });
        tracing::info!(account = %account.id, kind = %account.kind.as_str(), "sync started");
    }

    /// Stops the actor of `account` and waits until it logged out.
    pub(crate) async fn stop(&self, account: AccountId) {
        let running = lock(&self.actors).remove(&account);
        lock(&self.tokens).remove(&account);
        if let Some(running) = running {
            // No more status reports for an account that is going away.
            running.forwarder.abort();
            if tokio::time::timeout(ACTOR_SHUTDOWN, running.handle.shutdown())
                .await
                .is_err()
            {
                tracing::warn!(%account, "the sync actor didn't stop in time");
            }
        }
        lock(&self.statuses).remove(&account);
    }

    /// Stops every actor and the watchdog.
    pub(crate) async fn shutdown(&self) {
        if let Some(watchdog) = lock(&self.watchdog).take() {
            watchdog.abort();
        }
        let running: Vec<(AccountId, Running)> = lock(&self.actors).drain().collect();
        let mut stops = tokio::task::JoinSet::new();
        for (account, running) in running {
            running.forwarder.abort();
            stops.spawn(async move {
                if tokio::time::timeout(ACTOR_SHUTDOWN, running.handle.shutdown())
                    .await
                    .is_err()
                {
                    tracing::warn!(%account, "the sync actor didn't stop in time");
                }
            });
        }
        while stops.join_next().await.is_some() {}
        lock(&self.statuses).clear();
    }

    /// Every actor reconnects and syncs once.
    pub(crate) fn wake(&self) {
        for handle in self.handles() {
            let _ = handle.wake();
        }
    }

    /// Every actor reconnects and syncs once.
    pub(crate) fn network_changed(&self) {
        for handle in self.handles() {
            let _ = handle.network_changed();
        }
    }

    /// Accounts with a running actor.
    pub(crate) fn running(&self) -> Vec<AccountId> {
        let mut ids: Vec<AccountId> = lock(&self.actors).keys().copied().collect();
        ids.sort();
        ids
    }

    /// The account of a message (a quick reader query).
    fn account_of_message(&self, message: MessageId) -> Option<AccountId> {
        match self.store.reader().and_then(|r| r.message(message)) {
            Ok(detail) => detail.map(|d| d.account),
            Err(err) => {
                tracing::warn!(%err, %message, "looking up a message for a body fetch");
                None
            }
        }
    }
}

/// Forwards an actor's status into the core's events, logs alerts and stuck ops.
async fn forward_status(
    account: AccountId,
    mut status: watch::Receiver<sync::SyncStatus>,
    mut batches: tokio::sync::broadcast::Receiver<Arc<sync::SyncEventBatch>>,
    events: mpsc::UnboundedSender<CoreEvent>,
    statuses: Arc<Mutex<HashMap<AccountId, SyncStatus>>>,
) {
    let report = |current: &sync::SyncStatus| -> bool {
        let Some(status) = core_status(account, current) else {
            return true;
        };
        lock(&statuses).insert(account, status.clone());
        events.send(CoreEvent::Sync(status)).is_ok()
    };
    let initial = status.borrow_and_update().clone();
    if !report(&initial) {
        return;
    }
    let mut batches_open = true;
    loop {
        tokio::select! {
            changed = status.changed() => {
                if changed.is_err() {
                    return;
                }
                let current = status.borrow_and_update().clone();
                if !report(&current) {
                    return;
                }
            }
            batch = batches.recv(), if batches_open => match batch {
                Ok(batch) => {
                    for alert in &batch.alerts {
                        // Server alerts are the server's text, not mail content.
                        tracing::warn!(%account, alert = %alert, "IMAP server alert");
                    }
                    for op in &batch.ops {
                        if let sync::OpEvent::Stuck { op, error } = op {
                            tracing::warn!(%account, %op, %error, "a pending op is stuck");
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => batches_open = false,
            },
        }
    }
}

impl SyncBridge for SyncEngine {
    fn fetch_body(&self, message: MessageId, priority: FetchPriority) {
        let Some(account) = self.account_of_message(message) else {
            return;
        };
        let Some(handle) = self.handle(account) else {
            tracing::debug!(%message, "body requested for an account without sync");
            return;
        };
        let priority = match priority {
            FetchPriority::Interactive => BodyPriority::Interactive,
            FetchPriority::Prefetch | FetchPriority::Background => BodyPriority::Background,
        };
        let sent = handle.send(SyncCommand::FetchBody {
            message,
            priority,
            reply: None,
        });
        if sent.is_err() {
            tracing::debug!(%message, "the sync actor stopped before the body fetch");
        }
    }

    fn refresh(&self, scope: RefreshScope) {
        match scope {
            RefreshScope::All => {
                for handle in self.handles() {
                    let _ = handle.sync_now(SyncScope::Account);
                }
            }
            RefreshScope::Account(account) => {
                if let Some(handle) = self.handle(account) {
                    let _ = handle.sync_now(SyncScope::Account);
                }
            }
            RefreshScope::Folder(folder) => {
                let account = self
                    .store
                    .reader()
                    .and_then(|r| r.folder(folder))
                    .ok()
                    .flatten()
                    .map(|f| f.account);
                if let Some(handle) = account.and_then(|a| self.handle(a)) {
                    let _ = handle.sync_now(SyncScope::Folder(folder));
                }
            }
        }
    }

    fn ops_enqueued(&self, accounts: &[AccountId]) {
        for account in accounts {
            if let Some(handle) = self.handle(*account) {
                let _ = handle.sync_now(SyncScope::Account);
            }
        }
    }

    fn ops_queued(&self, ops: &[(AccountId, OpId)]) {
        for (account, op) in ops {
            if let Some(handle) = self.handle(*account) {
                let _ = handle.apply_op(*op);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_statuses_map_to_core_states() {
        let account = AccountId(3);
        let offline = sync::SyncStatus::Offline {
            reason: "connection refused".into(),
            retry_in: Some(Duration::from_secs(30)),
        };
        let status = core_status(account, &offline).unwrap();
        assert_eq!(status.state, SyncState::Offline);
        assert_eq!(
            status.text(),
            "Offline: connection refused. Maze will try again in 30 seconds."
        );
        let syncing =
            core_status(account, &sync::SyncStatus::Syncing { done: 1, total: 4 }).unwrap();
        assert_eq!(
            syncing.state,
            SyncState::Syncing {
                folder: None,
                done: 1,
                total: Some(4)
            }
        );
        assert_eq!(
            core_status(
                account,
                &sync::SyncStatus::AuthRequired {
                    message: "no password".into()
                }
            )
            .unwrap()
            .state,
            SyncState::AuthRequired
        );
        assert_eq!(core_status(account, &sync::SyncStatus::Stopped), None);
    }
}
