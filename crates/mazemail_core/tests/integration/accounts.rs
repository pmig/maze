//! Accounts and the sync engine against the in-memory IMAP server: adding validates
//! against the server and stores the password (in the encrypted store, or a test's
//! in-memory secret store), the account actor
//! syncs through the real ingest pipeline, events reach the core's batches, body
//! fetches and pending ops reach the server, removing cleans up.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use auth::{
    AccountSecrets, BrowserOpener, MemorySecretStore, ProviderKind, SecretKey, SecretKind,
    SecretStore as _,
};
use imap::fake::{FakeMessage, FakeServer};
use imap::{ImapAuth, ImapError};
use mazemail_core::{
    AccountAuth, AccountErrorKind, AddImapAccount, AddOAuthAccount, Body, CoreEventBatch,
    CoreHandle, Error, ImapConnector, ImapSecurity, ImapServer, ListQuery, MailCommand, Origin,
    StoreSecretStore, SyncEngineConfig, SyncState,
};
use mazemail_types::{Flags, FolderRole, Sensitive};
use store::{AccountKind, ListScope, Queries as _};
use sync::{BoxSession, CredentialProvider, SessionFactory, SyncError, SyncTiming};
use tokio::sync::broadcast;
use vault::MemoryKeyStore;

use crate::common;

pub(crate) const PASSWORD: &str = "correct horse";

/// Hands out sessions on a [`FakeServer`] after checking the password, like a server.
#[derive(Debug, Clone)]
pub(crate) struct FakeConnector {
    pub(crate) server: FakeServer,
    pub(crate) reachable: bool,
}

struct FakeSessions {
    server: FakeServer,
    reachable: bool,
    credentials: Arc<dyn CredentialProvider>,
}

#[async_trait]
impl SessionFactory for FakeSessions {
    async fn connect(&self) -> sync::Result<BoxSession> {
        if !self.reachable {
            return Err(SyncError::Imap(ImapError::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "connection refused",
            ))));
        }
        match self.credentials.credentials().await? {
            ImapAuth::Password { user, password }
                if user == "alice@example.org" && password.expose() == PASSWORD =>
            {
                Ok(Box::new(self.server.connect()))
            }
            ImapAuth::XOAuth2 { user, access_token }
                if (user == "carol@gmail.com" && access_token.expose() == "access-1")
                    || (user == "dana@contoso.com"
                        && access_token.expose().starts_with("access-")) =>
            {
                Ok(Box::new(self.server.connect()))
            }
            _ => Err(SyncError::Imap(ImapError::Auth(
                "[AUTHENTICATIONFAILED] invalid credentials".into(),
            ))),
        }
    }
}

impl ImapConnector for FakeConnector {
    fn sessions(
        &self,
        _server: &ImapServer,
        credentials: Arc<dyn CredentialProvider>,
    ) -> Arc<dyn SessionFactory> {
        Arc::new(FakeSessions {
            server: self.server.clone(),
            reachable: self.reachable,
            credentials,
        })
    }
}

pub(crate) fn timing() -> SyncTiming {
    SyncTiming {
        idle_reissue: Duration::from_secs(25 * 60),
        poll_interval: Duration::from_secs(3600),
        open_folder_poll: Duration::from_secs(3600),
        uid_diff_interval: Duration::from_secs(3600),
        coalesce_window: Duration::from_millis(5),
        reconnect_initial: Duration::from_millis(30),
        reconnect_max: Duration::from_millis(200),
    }
}

struct Setup {
    core: CoreHandle,
    secrets: Arc<MemorySecretStore>,
    server: FakeServer,
    _dir: tempfile::TempDir,
}

async fn open_with(server: &FakeServer, reachable: bool, dir: tempfile::TempDir) -> Setup {
    let keys = Arc::new(MemoryKeyStore::new());
    let secrets = Arc::new(MemorySecretStore::new());
    let engine = SyncEngineConfig::new(secrets.clone())
        .with_connector(Arc::new(FakeConnector {
            server: server.clone(),
            reachable,
        }))
        .with_timing(timing());
    let core = CoreHandle::open(common::config(dir.path(), &keys).with_sync_engine(engine))
        .await
        .unwrap();
    Setup {
        core,
        secrets,
        server: server.clone(),
        _dir: dir,
    }
}

async fn open(server: &FakeServer) -> Setup {
    open_with(server, true, tempfile::tempdir().unwrap()).await
}

pub(crate) fn input(password: &str) -> AddImapAccount {
    AddImapAccount {
        display_name: Some("Work".into()),
        email: "alice@example.org".into(),
        server: ImapServer::new("imap.example.org", ImapSecurity::Tls),
        username: None,
        password: Sensitive::new(password.to_owned()),
        smtp: None,
    }
}

/// A message from 2020: older than the offline policy's 90 days, so its body is only
/// downloaded when it is opened.
fn mail(n: u32, date: &str) -> FakeMessage {
    FakeMessage::new(format!(
        "From: Bob <bob@example.net>\r\nTo: alice@example.org\r\nSubject: Report {n}\r\n\
         Message-ID: <r{n}@example.net>\r\nDate: {date}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\nQuarterly numbers {n}\r\n"
    ))
    .internal_date(time::macros::datetime!(2020-01-06 09:00 UTC))
}

const OLD: &str = "Mon, 6 Jan 2020 09:00:00 +0000";

/// Waits until `check` holds, for at most 10 s.
pub(crate) async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn inbox_count(core: &CoreHandle) -> usize {
    let Some(inbox) = core
        .store()
        .reader()
        .unwrap()
        .folders_with_role(FolderRole::Inbox, None)
        .unwrap()
        .into_iter()
        .next()
    else {
        return 0;
    };
    core.thread_list(&ListQuery::new(ListScope::Folder(inbox.id)))
        .unwrap()
        .keys()
        .len()
}

/// Collects sync states from event batches until `want` shows up.
async fn wait_for_state(events: &mut broadcast::Receiver<CoreEventBatch>, want: &SyncState) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let batch = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("sync state in the events")
            .unwrap();
        if batch.sync_status.iter().any(|s| &s.state == want) {
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn adding_validates_saves_the_password_and_syncs() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail(1, OLD));
    server.deliver("INBOX", mail(2, OLD));
    let s = open(&server).await;
    let mut events = s.core.events();

    // A wrong password is rejected by the server: nothing is saved.
    let wrong = s.core.add_imap_account(input("nope")).await.unwrap_err();
    assert!(
        matches!(
            wrong,
            Error::Account {
                kind: AccountErrorKind::AuthFailed,
                ..
            }
        ),
        "{wrong}"
    );
    assert!(s.core.accounts().unwrap().is_empty());
    assert!(s.secrets.is_empty());
    let bad = AddImapAccount {
        email: "not an address".into(),
        ..input(PASSWORD)
    };
    assert!(matches!(
        s.core.add_imap_account(bad).await,
        Err(Error::Account {
            kind: AccountErrorKind::InvalidInput,
            ..
        })
    ));

    let info = s.core.add_imap_account(input(PASSWORD)).await.unwrap();
    assert_eq!(info.name, "Work");
    assert_eq!(info.kind, AccountKind::Imap);
    assert_eq!(info.email.as_deref(), Some("alice@example.org"));
    assert_eq!(s.core.synced_accounts(), vec![info.id]);
    // The password is in the secret store under the account's UUID, not in the
    // account's config.
    let stored = s
        .secrets
        .load(&SecretKey::new(info.uuid, SecretKind::Password))
        .unwrap()
        .unwrap();
    assert_eq!(stored.expose(), PASSWORD);
    let account = s
        .core
        .store()
        .reader()
        .unwrap()
        .account(info.id)
        .unwrap()
        .unwrap();
    assert!(!account.config_json.contains(PASSWORD));
    // IMAP accounts get an automatic provider profile when a message is verified.
    assert!(!account.config_json.contains("provider_profile"));
    let identities = s
        .core
        .store()
        .reader()
        .unwrap()
        .identities(Some(info.id))
        .unwrap();
    assert_eq!(identities.len(), 1);
    assert!(identities[0].input.is_default);

    // The actor syncs the folders and the mail through ingest, and says so.
    wait_for_state(&mut events, &SyncState::Idle).await;
    wait_for("the inbox", || inbox_count(&s.core) == 2).await;
    let status = s.core.sync_status(info.id).unwrap();
    assert_eq!(status.state, SyncState::Idle);
    assert_eq!(status.text(), "Up to date");
    // Indexed: search finds it.
    s.core.commit_index().await.unwrap();
    let hits = s
        .core
        .search("report", mazemail_core::SearchOpts::default())
        .await
        .unwrap();
    assert_eq!(hits.threads.len(), 2);

    // Adding it again is refused.
    assert!(matches!(
        s.core.add_imap_account(input(PASSWORD)).await,
        Err(Error::Account {
            kind: AccountErrorKind::Duplicate,
            ..
        })
    ));
    s.core.shutdown().await.unwrap();
}

/// "Sync activity": each synced account with its state, the last round, and the work
/// that waits (pending ops).
#[tokio::test(flavor = "multi_thread")]
async fn sync_activity_lists_accounts_with_their_work() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail(1, OLD));
    let s = open(&server).await;
    let mut events = s.core.events();
    let info = s.core.add_imap_account(input(PASSWORD)).await.unwrap();
    wait_for_state(&mut events, &SyncState::Idle).await;
    wait_for("the first round", || {
        s.core
            .sync_activity()
            .unwrap()
            .first()
            .is_some_and(|a| a.last_synced.is_some())
    })
    .await;
    let activity = s.core.sync_activity().unwrap();
    assert_eq!(activity.len(), 1);
    let work = &activity[0];
    assert_eq!(work.account, info.id);
    assert_eq!(work.name, "Work");
    assert_eq!(work.kind, AccountKind::Imap);
    assert_eq!(work.sync.as_ref().map(|s| &s.state), Some(&SyncState::Idle));
    assert!(work.folders.is_empty(), "{work:?}");
    assert_eq!((work.pending_ops, work.stuck_ops, work.outbox), (0, 0, 0));

    // An account whose sync doesn't run, with a change waiting for the server.
    let idle = s
        .core
        .store()
        .write(|w| {
            let id = w.create_account(&store::NewAccount {
                kind: AccountKind::Imap,
                name: "Paused".to_owned(),
                config_json: r#"{"host":"imap.example.net","auth":"gssapi"}"#.to_owned(),
                ordinal: 1,
            })?;
            w.enqueue_op(id, "flags", "{}", "user")?;
            Ok(id)
        })
        .await
        .unwrap();
    let activity = s.core.sync_activity().unwrap();
    let paused = activity.iter().find(|a| a.account == idle).unwrap();
    assert_eq!(paused.sync, None);
    assert_eq!(paused.pending_ops, 1);
    assert!(paused.oldest_op.is_some());
    assert_eq!(paused.last_synced, None);
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_server_is_reported() {
    let server = FakeServer::new();
    let s = open_with(&server, false, tempfile::tempdir().unwrap()).await;
    let err = s.core.add_imap_account(input(PASSWORD)).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::Account {
                kind: AccountErrorKind::Unreachable,
                ..
            }
        ),
        "{err}"
    );
    assert!(err.to_string().contains("imap.example.org:993"), "{err}");
    assert!(s.core.accounts().unwrap().is_empty());
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn testing_a_connection_saves_nothing() {
    let server = FakeServer::new();
    server.add_folder("Projects", None);
    let s = open(&server).await;

    let test = s.core.test_imap_account(input(PASSWORD)).await.unwrap();
    assert!(!test.gmail);
    assert_eq!(test.folders, server.folder_names().len());
    let wrong = s.core.test_imap_account(input("nope")).await.unwrap_err();
    assert!(
        matches!(
            wrong,
            Error::Account {
                kind: AccountErrorKind::AuthFailed,
                ..
            }
        ),
        "{wrong}"
    );
    assert!(s.core.accounts().unwrap().is_empty());
    assert!(s.secrets.is_empty());
    assert!(s.core.synced_accounts().is_empty());

    // A tested account can still be added.
    s.core.add_imap_account(input(PASSWORD)).await.unwrap();
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn new_mail_bodies_and_ops_flow_through_the_engine() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail(1, OLD));
    let s = open(&server).await;
    let info = s.core.add_imap_account(input(PASSWORD)).await.unwrap();
    wait_for("the first message", || inbox_count(&s.core) == 1).await;

    // New mail on the server arrives through IDLE and refreshes the list.
    let mut events = s.core.events();
    s.server.deliver("INBOX", mail(2, OLD));
    wait_for("the second message", || inbox_count(&s.core) == 2).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let batch = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("a thread list change event")
            .unwrap();
        if !batch.threads_changed.is_empty() {
            break;
        }
    }

    // Old mail isn't prefetched (offline policy: recent); opening it fetches the body.
    let reader = s.core.store().reader().unwrap();
    let inbox = reader
        .folders_with_role(FolderRole::Inbox, Some(info.id))
        .unwrap()
        .remove(0);
    let first = reader.message_by_uid(inbox.id, 1).unwrap().unwrap();
    drop(reader);
    let view = s.core.message(first).await.unwrap();
    assert!(matches!(view.body, Body::Pending));
    let mut opened = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while opened.is_none() {
        assert!(tokio::time::Instant::now() < deadline, "body never arrived");
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Body::Ready(body) = s.core.message(first).await.unwrap().body {
            opened = Some(body);
        }
    }

    // A local change is replayed on the server.
    s.core
        .apply(
            MailCommand::SetFlags {
                ids: vec![first],
                add: Flags::SEEN | Flags::FLAGGED,
                remove: Flags::empty(),
            },
            Origin::User,
        )
        .await
        .unwrap();
    wait_for("the flags on the server", || {
        s.server
            .messages("INBOX")
            .iter()
            .any(|m| m.uid == 1 && m.flags.contains(Flags::SEEN | Flags::FLAGGED))
    })
    .await;
    wait_for("the op queue to drain", || {
        s.core
            .store()
            .reader()
            .unwrap()
            .pending_ops(info.id)
            .unwrap()
            .is_empty()
    })
    .await;

    // Archive moves it on the server (into Archive/2020: yearly by default).
    s.core
        .apply(MailCommand::Archive { ids: vec![first] }, Origin::User)
        .await
        .unwrap();
    wait_for("the archived message on the server", || {
        s.server.messages("INBOX").len() == 1 && s.server.messages("Archive/2020").len() == 1
    })
    .await;
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_stops_sync_and_deletes_data_and_secrets() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail(1, OLD));
    let s = open(&server).await;
    let info = s.core.add_imap_account(input(PASSWORD)).await.unwrap();
    wait_for("the first message", || inbox_count(&s.core) == 1).await;
    // Another secret of the account (an SMTP password) goes too.
    AccountSecrets::new(s.secrets.clone(), info.uuid)
        .set_smtp_password(Sensitive::new("smtp".into()))
        .await
        .unwrap();

    s.core.remove_account(info.id).await.unwrap();
    assert!(s.core.accounts().unwrap().is_empty());
    assert!(s.core.synced_accounts().is_empty());
    assert!(s.core.sync_status(info.id).is_none());
    assert!(s.secrets.is_empty(), "no secret of the account is left");
    assert_eq!(inbox_count(&s.core), 0);
    assert!(matches!(
        s.core.remove_account(info.id).await,
        Err(Error::NotFound { .. })
    ));
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn accounts_restart_syncing_when_the_core_reopens() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail(1, OLD));
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let secrets = Arc::new(MemorySecretStore::new());
    let engine = || {
        SyncEngineConfig::new(secrets.clone())
            .with_connector(Arc::new(FakeConnector {
                server: server.clone(),
                reachable: true,
            }))
            .with_timing(timing())
    };
    let core = CoreHandle::open(common::config(dir.path(), &keys).with_sync_engine(engine()))
        .await
        .unwrap();
    let info = core.add_imap_account(input(PASSWORD)).await.unwrap();
    wait_for("the first message", || inbox_count(&core) == 1).await;
    core.shutdown().await.unwrap();

    server.deliver("INBOX", mail(2, OLD));
    let core = CoreHandle::open(common::config(dir.path(), &keys).with_sync_engine(engine()))
        .await
        .unwrap();
    assert_eq!(core.synced_accounts(), vec![info.id]);
    wait_for("mail that arrived while closed", || inbox_count(&core) == 2).await;
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn without_an_engine_accounts_cant_be_added() {
    let t = common::open().await;
    assert!(matches!(
        t.core.add_imap_account(input(PASSWORD)).await,
        Err(Error::NoSyncEngine)
    ));
    t.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn google_sign_in_needs_a_client_id() {
    let server = FakeServer::gmail();
    let s = open(&server).await;
    let err = s
        .core
        .add_oauth_account(AddOAuthAccount {
            provider: ProviderKind::Google,
            email: "carol@gmail.com".into(),
            display_name: None,
            tenant: None,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Account {
                kind: AccountErrorKind::OAuthNotConfigured,
                ..
            }
        ),
        "{err}"
    );
    assert!(err.to_string().contains("auth.google.client_id"), "{err}");
    assert!(s.core.accounts().unwrap().is_empty());
    s.core.shutdown().await.unwrap();
}

/// A browser that consents at once: follows the authorization redirect to the
/// loopback listener.
struct ConsentingBrowser;

impl BrowserOpener for ConsentingBrowser {
    fn open(&self, url: &url::Url) -> std::io::Result<()> {
        let url = url.clone();
        tokio::spawn(async move {
            let http = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let response = http.get(url).send().await.unwrap();
            let location = response.headers()["location"].to_str().unwrap().to_owned();
            http.get(location).send().await.unwrap();
        });
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_sign_in_adds_a_gmail_account() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let provider = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/authorize"))
        .respond_with(|request: &Request| {
            let q: std::collections::HashMap<String, String> =
                request.url.query_pairs().into_owned().collect();
            assert_eq!(q["login_hint"], "carol@gmail.com");
            let location = format!("{}?code=the-code&state={}", q["redirect_uri"], q["state"]);
            ResponseTemplate::new(302).insert_header("Location", location.as_str())
        })
        .mount(&provider)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "expires_in": 3600,
            "token_type": "Bearer",
        })))
        .mount(&provider)
        .await;
    let google = auth::Provider::builtin(ProviderKind::Google)
        .with_endpoints(
            &format!("{}/authorize", provider.uri()),
            &format!("{}/token", provider.uri()),
        )
        .unwrap()
        .with_credentials(auth::ClientCredentials::new(
            "client.apps.googleusercontent.com",
        ));
    let mut registry = auth::ProviderRegistry::new();
    registry.set_provider(google);

    let server = FakeServer::gmail();
    server.deliver("[Gmail]/All Mail", mail(1, OLD));
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let secrets = Arc::new(MemorySecretStore::new());
    let engine = SyncEngineConfig::new(secrets.clone())
        .with_connector(Arc::new(FakeConnector {
            server: server.clone(),
            reachable: true,
        }))
        .with_browser(Arc::new(ConsentingBrowser))
        .with_providers(registry)
        .with_timing(timing());
    let core = CoreHandle::open(common::config(dir.path(), &keys).with_sync_engine(engine))
        .await
        .unwrap();

    let info = core
        .add_oauth_account(AddOAuthAccount {
            provider: ProviderKind::Google,
            email: "carol@gmail.com".into(),
            display_name: Some("Carol".into()),
            tenant: None,
        })
        .await
        .unwrap();
    assert_eq!(info.kind, AccountKind::Gmail);
    assert_eq!(info.auth, Some(AccountAuth::OAuth(ProviderKind::Google)));
    assert_eq!(
        info.server.as_ref().map(|s| s.host.as_str()),
        Some("imap.gmail.com")
    );
    // Only the refresh token is stored; the access token stays in memory.
    let refresh = secrets
        .load(&SecretKey::new(info.uuid, SecretKind::OAuthRefresh))
        .unwrap()
        .unwrap();
    assert_eq!(refresh.expose(), "refresh-1");
    assert_eq!(secrets.len(), 1);
    // The actor logs in with the access token and syncs All Mail.
    wait_for("Gmail to sync", || {
        core.store()
            .reader()
            .unwrap()
            .folders_with_role(FolderRole::All, Some(info.id))
            .unwrap()
            .first()
            .is_some_and(|all| all.total == 1)
    })
    .await;
    core.shutdown().await.unwrap();
}

/// A Microsoft account signs in with the tenant asked for, keeps it in its config, and
/// refreshes its token there after a restart, even though the registry's default
/// tenant (`auth.microsoft.tenant`) is `common`.
#[tokio::test(flavor = "multi_thread")]
async fn microsoft_accounts_refresh_with_the_tenant_they_signed_in_with() {
    use auth::{ClientCredentials, MicrosoftTenant, Provider, ProviderRegistry};
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let provider = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/contoso.com/oauth2/v2.0/authorize"))
        .respond_with(|request: &Request| {
            let q: std::collections::HashMap<String, String> =
                request.url.query_pairs().into_owned().collect();
            let location = format!("{}?code=the-code&state={}", q["redirect_uri"], q["state"]);
            ResponseTemplate::new(302).insert_header("Location", location.as_str())
        })
        .mount(&provider)
        .await;
    Mock::given(method("POST"))
        .and(path("/contoso.com/oauth2/v2.0/token"))
        .respond_with(|request: &Request| {
            let form: std::collections::HashMap<String, String> =
                url::form_urlencoded::parse(&request.body)
                    .into_owned()
                    .collect();
            let n = if form["grant_type"] == "refresh_token" {
                assert_eq!(form["refresh_token"], "refresh-1");
                2
            } else {
                1
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": format!("access-{n}"),
                "refresh_token": format!("refresh-{n}"),
                "expires_in": 3600,
                "token_type": "Bearer",
            }))
        })
        .mount(&provider)
        .await;
    // Anything else (the `common` endpoints) fails the sign-in.
    Mock::given(path_regex("^/common/"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_request",
            "error_description": "AADSTS50194: not a multi-tenant application",
        })))
        .expect(0)
        .mount(&provider)
        .await;
    // The registry's default tenant stays `common`, like the default setting.
    let registry = || {
        let mut registry = ProviderRegistry::new();
        registry.set_provider(
            Provider::builtin(ProviderKind::Microsoft)
                .with_authority(&provider.uri())
                .unwrap()
                .with_credentials(ClientCredentials::new("entra-app")),
        );
        assert_eq!(
            registry.microsoft_tenant(),
            Some(&MicrosoftTenant::common())
        );
        registry
    };

    let server = FakeServer::new();
    server.deliver("INBOX", mail(1, OLD));
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let secrets = Arc::new(MemorySecretStore::new());
    let engine = || {
        SyncEngineConfig::new(secrets.clone())
            .with_connector(Arc::new(FakeConnector {
                server: server.clone(),
                reachable: true,
            }))
            .with_browser(Arc::new(ConsentingBrowser))
            .with_providers(registry())
            .with_timing(timing())
    };
    let core = CoreHandle::open(common::config(dir.path(), &keys).with_sync_engine(engine()))
        .await
        .unwrap();
    let info = core
        .add_oauth_account(AddOAuthAccount {
            provider: ProviderKind::Microsoft,
            email: "dana@contoso.com".into(),
            display_name: None,
            tenant: Some(MicrosoftTenant::parse("Contoso.com").unwrap()),
        })
        .await
        .unwrap();
    assert_eq!(info.kind, AccountKind::M365);
    let account = core.accounts().unwrap().pop().unwrap();
    let config = mazemail_core::AccountConfig::parse(&account.config_json).unwrap();
    assert_eq!(config.oauth_tenant.as_deref(), Some("contoso.com"));
    core.shutdown().await.unwrap();

    // After a restart the access token is gone: the actor refreshes at contoso.com.
    let core = CoreHandle::open(common::config(dir.path(), &keys).with_sync_engine(engine()))
        .await
        .unwrap();
    let refreshed = async {
        loop {
            let requests = provider.received_requests().await.unwrap_or_default();
            if requests.iter().any(|r| {
                r.url.path() == "/contoso.com/oauth2/v2.0/token"
                    && String::from_utf8_lossy(&r.body).contains("grant_type=refresh_token")
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), refreshed)
        .await
        .expect("a refresh at the account's tenant");
    wait_for("the rotated refresh token", || {
        secrets
            .load(&SecretKey::new(info.uuid, SecretKind::OAuthRefresh))
            .unwrap()
            .is_some_and(|token| token.expose() == "refresh-2")
    })
    .await;
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_local_api_adds_lists_syncs_and_removes_accounts() {
    use local_api::registry::args::{AccountAddArgs, AccountListArgs, AccountRemoveArgs, SyncArgs};
    use local_api::{
        Command, DenyAllApprover, EffectiveScope, ErrorCode, ExecutionMode, PeerInfo, ProcessInfo,
        RequestContext, TokenScope,
    };
    use mazemail_core::api::{self, TokenAuthorizer};

    let server = FakeServer::new();
    server.deliver("INBOX", mail(1, OLD));
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    std::fs::write(
        &settings,
        r#"{ "agent": { "accounts": { "Work": "any" } } }"#,
    )
    .unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let secrets = Arc::new(MemorySecretStore::new());
    let engine = SyncEngineConfig::new(secrets.clone())
        .with_connector(Arc::new(FakeConnector {
            server: server.clone(),
            reachable: true,
        }))
        .with_timing(timing());
    let core = CoreHandle::open(
        common::config(&dir.path().join("data"), &keys)
            .with_settings_file(&settings)
            .with_sync_engine(engine),
    )
    .await
    .unwrap();
    let handler = api::services(&core, &TokenAuthorizer::new(), Arc::new(DenyAllApprover)).handler;
    let ctx = || {
        let policy: std::collections::BTreeSet<_> = core
            .accounts()
            .unwrap()
            .iter()
            .filter(|a| a.name == "Work")
            .map(|a| a.id)
            .collect();
        RequestContext::new(
            1,
            Arc::new(PeerInfo {
                uid: 501,
                gid: 20,
                process: ProcessInfo {
                    pid: Some(1),
                    exe: None,
                    parent: None,
                },
            }),
            None,
            EffectiveScope::resolve(&TokenScope::all(), &policy),
            ExecutionMode::Execute,
        )
    };
    let add = |password: &str| {
        Command::AccountAdd(AccountAddArgs {
            email: Sensitive::new("alice@example.org".into()),
            name: Some("Work".into()),
            oauth: None,
            tenant: None,
            host: Some("imap.example.org".into()),
            port: None,
            security: None,
            username: None,
            password: Some(Sensitive::new(password.into())),
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
        })
    };

    let rejected = handler.handle(&ctx(), add("wrong")).await.unwrap_err();
    assert_eq!(rejected.code, ErrorCode::InvalidArgs);
    assert!(
        rejected.message.contains("rejected the login"),
        "{rejected}"
    );
    assert!(!rejected.message.contains("wrong"));

    let added = handler.handle(&ctx(), add(PASSWORD)).await.unwrap();
    let added = added.data.expose().clone();
    assert_eq!(added["name"], "Work");
    assert_eq!(added["server"], "imap.example.org:993");
    assert_eq!(added["auth"], "password");
    let id = added["id"].as_str().unwrap().to_owned();
    wait_for("the first message", || inbox_count(&core) == 1).await;
    wait_for("the account to be idle", || {
        core.accounts_with_status().unwrap()[0]
            .sync
            .as_ref()
            .is_some_and(|s| s.state == SyncState::Idle)
    })
    .await;

    let list = handler
        .handle(&ctx(), Command::AccountList(AccountListArgs {}))
        .await
        .unwrap();
    let list = list.data.expose().clone();
    assert_eq!(list[0]["id"], id.as_str());
    assert_eq!(list[0]["state"], "idle");
    assert_eq!(list[0]["status"], "Up to date");

    let synced = handler
        .handle(&ctx(), Command::Sync(SyncArgs::default()))
        .await
        .unwrap();
    assert_eq!(synced.data.expose()["accounts"][0], id.as_str());

    let account: mazemail_types::AccountId = mazemail_types::PublicId::parse_public(&id).unwrap();
    handler
        .handle(
            &ctx(),
            Command::AccountRemove(AccountRemoveArgs { id: account }),
        )
        .await
        .unwrap();
    assert!(core.accounts().unwrap().is_empty());
    assert!(secrets.is_empty());
    core.shutdown().await.unwrap();
}

/// Every file under `dir` that contains `needle`.
fn files_containing(dir: &std::path::Path, needle: &[u8]) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if std::fs::read(&path)
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle)
            {
                found.push(path);
            }
        }
    }
    found
}

fn encrypted_engine(
    server: &FakeServer,
    legacy: Option<Arc<dyn auth::SecretStore>>,
) -> SyncEngineConfig {
    SyncEngineConfig::encrypted(legacy)
        .with_connector(Arc::new(FakeConnector {
            server: server.clone(),
            reachable: true,
        }))
        .with_timing(timing())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_encrypted_store_keeps_secrets_across_restarts_until_removal() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail(1, OLD));
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let open = || async {
        CoreHandle::open(
            common::config(dir.path(), &keys).with_sync_engine(encrypted_engine(&server, None)),
        )
        .await
        .unwrap()
    };

    let core = open().await;
    let info = core.add_imap_account(input(PASSWORD)).await.unwrap();
    wait_for("the first message", || inbox_count(&core) == 1).await;
    let secrets: Arc<dyn auth::SecretStore> = Arc::new(StoreSecretStore::new(core.store().clone()));
    let password = SecretKey::new(info.uuid, SecretKind::Password);
    assert_eq!(secrets.load(&password).unwrap().unwrap().expose(), PASSWORD);
    AccountSecrets::new(secrets.clone(), info.uuid)
        .set_smtp_password(Sensitive::new("smtp secret".into()))
        .await
        .unwrap();
    // Never in plaintext on disk: database, WAL, blobs, index.
    assert_eq!(
        files_containing(dir.path(), PASSWORD.as_bytes()),
        Vec::<std::path::PathBuf>::new()
    );
    core.shutdown().await.unwrap();
    assert_eq!(
        files_containing(dir.path(), PASSWORD.as_bytes()),
        Vec::<std::path::PathBuf>::new()
    );
    assert_eq!(
        files_containing(dir.path(), b"smtp secret"),
        Vec::<std::path::PathBuf>::new()
    );

    // After a restart the actor logs in with the stored password.
    server.deliver("INBOX", mail(2, OLD));
    let core = open().await;
    assert_eq!(core.synced_accounts(), vec![info.id]);
    wait_for("mail that arrived while closed", || inbox_count(&core) == 2).await;

    // Removing the account removes its secrets.
    core.remove_account(info.id).await.unwrap();
    let secrets = StoreSecretStore::new(core.store().clone());
    for kind in SecretKind::ALL {
        assert_eq!(
            secrets.load(&SecretKey::new(info.uuid, kind)).unwrap(),
            None,
            "{kind}"
        );
    }
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn keychain_secrets_move_into_the_store_when_the_core_opens() {
    let server = FakeServer::new();
    server.deliver("INBOX", mail(1, OLD));
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let open = |legacy: Option<Arc<dyn auth::SecretStore>>| {
        let config =
            common::config(dir.path(), &keys).with_sync_engine(encrypted_engine(&server, legacy));
        async move { CoreHandle::open(config).await.unwrap() }
    };

    // An account as an earlier version left it: the password in the keychain, the
    // account on the list of the v5 migration.
    let core = open(None).await;
    let info = core.add_imap_account(input(PASSWORD)).await.unwrap();
    wait_for("the first message", || inbox_count(&core) == 1).await;
    let store_secrets = AccountSecrets::new(
        Arc::new(StoreSecretStore::new(core.store().clone())),
        info.uuid,
    );
    store_secrets.delete_all().await.unwrap();
    let uuid = info.uuid;
    core.store()
        .write(move |w| w.list_legacy_secrets(uuid))
        .await
        .unwrap();
    core.shutdown().await.unwrap();
    let keychain = Arc::new(MemorySecretStore::new());
    let password = SecretKey::new(info.uuid, SecretKind::Password);
    keychain
        .store(&password, &Sensitive::new(PASSWORD.to_owned()))
        .unwrap();

    // The next start moves it before the actor needs it, and deletes the item.
    server.deliver("INBOX", mail(2, OLD));
    let core = open(Some(keychain.clone())).await;
    assert!(keychain.is_empty(), "the keychain item is deleted");
    let store_secrets = StoreSecretStore::new(core.store().clone());
    assert_eq!(
        store_secrets.load(&password).unwrap().unwrap().expose(),
        PASSWORD
    );
    assert!(
        core.store()
            .reader()
            .unwrap()
            .legacy_secret_accounts()
            .unwrap()
            .is_empty()
    );
    wait_for("mail that arrived while closed", || inbox_count(&core) == 2).await;
    core.shutdown().await.unwrap();

    // Once moved, the keychain is never consulted again for the account.
    let stale = Arc::new(MemorySecretStore::new());
    stale
        .store(&password, &Sensitive::new("stale".to_owned()))
        .unwrap();
    let core = open(Some(stale.clone())).await;
    assert!(stale.contains(&password));
    let store_secrets = StoreSecretStore::new(core.store().clone());
    assert_eq!(
        store_secrets.load(&password).unwrap().unwrap().expose(),
        PASSWORD
    );
    core.shutdown().await.unwrap();
}
