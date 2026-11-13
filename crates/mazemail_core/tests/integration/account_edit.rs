//! Editing accounts against the in-memory IMAP server: a new password ends "sign-in
//! required", new servers need the password again (and the saved one never goes
//! anywhere else), a rename takes the account's settings along, the local API's
//! `account set`, and signing a Google account in again.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use auth::{MemorySecretStore, ProviderKind, SecretKey, SecretKind, SecretStore as _};
use imap::fake::{FakeMessage, FakeServer};
use imap::{ImapAuth, ImapError};
use mazemail_core::{
    AccountChanges, AccountErrorKind, AddImapAccount, AddOAuthAccount, CoreHandle, Error,
    ImapConnector, ImapSecurity, ImapServer, ListQuery, SmtpServerSettings, SyncEngineConfig,
    SyncState,
};
use mazemail_types::{AccountId, FolderRole, Sensitive};
use smtp::{Credentials, Mailer, OutboxEnvelope, SmtpConfig, SmtpError};
use store::{ListScope, Queries as _};
use sync::{BoxSession, CredentialProvider, SessionFactory, SyncError};
use vault::MemoryKeyStore;

use crate::accounts::{ConsentingBrowser, timing, wait_for};
use crate::common;

/// A mail provider whose password can change, recording where every login went.
#[derive(Debug, Clone)]
struct Provider {
    imap: FakeServer,
    password: Arc<Mutex<String>>,
    /// `(host, password)` of every IMAP login and SMTP check.
    logins: Arc<Mutex<Vec<(String, String)>>>,
}

impl Provider {
    fn new(imap: FakeServer, password: &str) -> Self {
        Self {
            imap,
            password: Arc::new(Mutex::new(password.to_owned())),
            logins: Arc::default(),
        }
    }

    fn set_password(&self, password: &str) {
        password.clone_into(&mut self.password.lock().unwrap());
    }

    fn logins(&self) -> Vec<(String, String)> {
        self.logins.lock().unwrap().clone()
    }

    /// Records a password login at `host` and says whether it is right.
    fn login(&self, host: &str, password: &str) -> bool {
        self.logins
            .lock()
            .unwrap()
            .push((host.to_owned(), password.to_owned()));
        *self.password.lock().unwrap() == password
    }
}

struct ProviderSessions {
    provider: Provider,
    host: String,
    credentials: Arc<dyn CredentialProvider>,
}

#[async_trait]
impl SessionFactory for ProviderSessions {
    async fn connect(&self) -> sync::Result<BoxSession> {
        let accepted = match self.credentials.credentials().await? {
            ImapAuth::Password { password, .. } => {
                self.provider.login(&self.host, password.expose())
            }
            ImapAuth::XOAuth2 { user, access_token } => {
                user == "carol@gmail.com" && access_token.expose().starts_with("access-")
            }
        };
        if accepted {
            Ok(Box::new(self.provider.imap.connect()))
        } else {
            Err(SyncError::Imap(ImapError::Auth(
                "[AUTHENTICATIONFAILED] invalid credentials".into(),
            )))
        }
    }
}

impl ImapConnector for Provider {
    fn sessions(
        &self,
        server: &ImapServer,
        credentials: Arc<dyn CredentialProvider>,
    ) -> Arc<dyn SessionFactory> {
        Arc::new(ProviderSessions {
            provider: self.clone(),
            host: server.host.clone(),
            credentials,
        })
    }
}

#[async_trait]
impl Mailer for Provider {
    async fn send(
        &self,
        _: &SmtpConfig,
        _: &Credentials,
        _: &OutboxEnvelope,
        _: &[u8],
    ) -> Result<(), SmtpError> {
        Err(SmtpError::AuthFailed {
            message: "not in these tests".into(),
        })
    }

    async fn check(&self, config: &SmtpConfig, credentials: &Credentials) -> Result<(), SmtpError> {
        match credentials {
            Credentials::Password { password, .. }
                if self.login(&config.host, password.expose()) =>
            {
                Ok(())
            }
            _ => Err(SmtpError::AuthFailed {
                message: "535 5.7.8 bad password".into(),
            }),
        }
    }
}

struct Setup {
    core: CoreHandle,
    provider: Provider,
    secrets: Arc<MemorySecretStore>,
    settings: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

async fn setup_with(
    imap: FakeServer,
    settings_json: &str,
    engine: impl FnOnce(SyncEngineConfig) -> SyncEngineConfig,
) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    std::fs::write(&settings, settings_json).unwrap();
    let provider = Provider::new(imap, "old");
    let keys = Arc::new(MemoryKeyStore::new());
    let secrets = Arc::new(MemorySecretStore::new());
    let config = SyncEngineConfig::new(secrets.clone())
        .with_connector(Arc::new(provider.clone()))
        .with_mailer(Arc::new(provider.clone()))
        .with_timing(timing());
    let core = CoreHandle::open(
        common::config(&dir.path().join("data"), &keys)
            .with_settings_file(&settings)
            .with_sync_engine(engine(config)),
    )
    .await
    .unwrap();
    Setup {
        core,
        provider,
        secrets,
        settings,
        _dir: dir,
    }
}

async fn setup(settings_json: &str) -> Setup {
    let imap = FakeServer::new();
    imap.deliver(
        "INBOX",
        FakeMessage::new(
            "From: Bob <bob@example.net>\r\nTo: alice@example.org\r\nSubject: Hello\r\n\
             Message-ID: <hello@example.net>\r\nDate: Mon, 6 Jan 2020 09:00:00 +0000\r\n\r\nHi\r\n",
        ),
    );
    setup_with(imap, settings_json, |engine| engine).await
}

fn account(name: &str, email: &str) -> AddImapAccount {
    AddImapAccount {
        display_name: Some(name.into()),
        email: email.into(),
        server: ImapServer::new("imap.example.org", ImapSecurity::Tls),
        username: None,
        password: Sensitive::new("old".into()),
        smtp: None,
    }
}

fn password(password: &str) -> AccountChanges {
    AccountChanges {
        password: Some(Sensitive::new(password.into())),
        ..AccountChanges::default()
    }
}

fn kind(error: &Error) -> AccountErrorKind {
    match error {
        Error::Account { kind, .. } => *kind,
        other => panic!("{other}"),
    }
}

fn state(core: &CoreHandle, id: AccountId) -> Option<SyncState> {
    core.sync_status(id).map(|status| status.state)
}

async fn wait_for_state(core: &CoreHandle, id: AccountId, want: SyncState) {
    wait_for(&format!("{want:?}"), || {
        state(core, id) == Some(want.clone())
    })
    .await;
}

fn inbox_threads(core: &CoreHandle, id: AccountId) -> usize {
    let Some(inbox) = core
        .store()
        .reader()
        .unwrap()
        .folders_with_role(FolderRole::Inbox, Some(id))
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

fn saved_password(s: &Setup, id: AccountId) -> String {
    let uuid = s
        .core
        .store()
        .reader()
        .unwrap()
        .account(id)
        .unwrap()
        .unwrap()
        .uuid;
    s.secrets
        .load(&SecretKey::new(uuid, SecretKind::Password))
        .unwrap()
        .unwrap()
        .expose()
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_password_ends_sign_in_required_and_keeps_the_mail() {
    let s = setup("{}").await;
    let id = s
        .core
        .add_imap_account(account("Work", "alice@example.org"))
        .await
        .unwrap()
        .id;
    wait_for_state(&s.core, id, SyncState::Idle).await;
    wait_for("the inbox", || inbox_threads(&s.core, id) == 1).await;

    // The password changes at the provider: the next login fails and sync waits for
    // the user instead of retrying.
    s.provider.set_password("new");
    s.core.network_changed();
    wait_for_state(&s.core, id, SyncState::AuthRequired).await;

    // A wrong password is rejected by the server and changes nothing.
    let wrong = s
        .core
        .update_account(id, password("nope"))
        .await
        .unwrap_err();
    assert_eq!(kind(&wrong), AccountErrorKind::AuthFailed, "{wrong}");
    assert_eq!(saved_password(&s, id), "old");

    // The right one is saved and sync starts again, with the mail it had.
    let info = s.core.update_account(id, password("new")).await.unwrap();
    assert_eq!(info.id, id);
    assert_eq!(saved_password(&s, id), "new");
    wait_for_state(&s.core, id, SyncState::Idle).await;
    assert_eq!(inbox_threads(&s.core, id), 1);
    let details = s.core.account_details(id).await.unwrap();
    assert!(details.has_password);
    assert_eq!(details.info.name, "Work");
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn new_servers_need_the_password_and_the_saved_one_stays_put() {
    let s = setup("{}").await;
    let id = s
        .core
        .add_imap_account(account("Work", "alice@example.org"))
        .await
        .unwrap()
        .id;
    wait_for_state(&s.core, id, SyncState::Idle).await;
    let elsewhere = |logins: Vec<(String, String)>| {
        logins
            .into_iter()
            .filter(|(host, _)| host != "imap.example.org" && host != "smtp.example.org")
            .collect::<Vec<_>>()
    };

    // Without the password nothing connects anywhere new: the saved one stays with the
    // servers it was saved for.
    let moved = AccountChanges {
        server: Some(ImapServer {
            host: "mail.example.org".into(),
            port: 143,
            security: ImapSecurity::Starttls,
        }),
        username: Some("alice".into()),
        smtp: Some(SmtpServerSettings::new(
            "mail.example.org",
            ImapSecurity::Tls,
        )),
        ..AccountChanges::default()
    };
    let refused = s.core.update_account(id, moved.clone()).await.unwrap_err();
    assert_eq!(kind(&refused), AccountErrorKind::InvalidInput);
    assert!(
        refused.to_string().contains("enter the password again"),
        "{refused}"
    );
    let refused = s
        .core
        .test_account_changes(id, moved.clone())
        .await
        .unwrap_err();
    assert_eq!(kind(&refused), AccountErrorKind::InvalidInput);
    for smtp_only in [
        AccountChanges {
            smtp: moved.smtp.clone(),
            ..AccountChanges::default()
        },
        AccountChanges {
            username: moved.username.clone(),
            ..AccountChanges::default()
        },
    ] {
        assert!(s.core.update_account(id, smtp_only).await.is_err());
    }
    assert_eq!(elsewhere(s.provider.logins()), vec![]);

    // Test Connection logs in to both new servers with the typed password and saves
    // nothing.
    let with_password = AccountChanges {
        password: Some(Sensitive::new("old".into())),
        ..moved
    };
    let test = s
        .core
        .test_account_changes(id, with_password.clone())
        .await
        .unwrap();
    assert!(test.imap.folders > 0);
    assert_eq!(test.smtp.unwrap().host, "mail.example.org");
    let details = s.core.account_details(id).await.unwrap();
    assert_eq!(details.info.server.unwrap().host, "imap.example.org");

    // Saving moves the account and its SMTP server, login name included.
    let info = s.core.update_account(id, with_password).await.unwrap();
    assert_eq!(
        info.server,
        Some(ImapServer {
            host: "mail.example.org".into(),
            port: 143,
            security: ImapSecurity::Starttls,
        })
    );
    let details = s.core.account_details(id).await.unwrap();
    assert_eq!(details.username, "alice");
    assert_eq!(
        details.smtp,
        Some(SmtpServerSettings::new(
            "mail.example.org",
            ImapSecurity::Tls
        ))
    );
    let reader = s.core.store().reader().unwrap();
    let smtp = reader.identities(Some(id)).unwrap()[0].input.smtp.unwrap();
    assert_eq!(
        reader
            .smtp_server(smtp)
            .unwrap()
            .unwrap()
            .username
            .as_deref(),
        Some("alice")
    );
    drop(reader);
    let before = s.provider.logins().len();
    wait_for_state(&s.core, id, SyncState::Idle).await;
    // Sync runs against the new server now.
    let after: Vec<_> = s.provider.logins()[before..].to_vec();
    assert!(
        after.iter().all(|(host, _)| host == "mail.example.org"),
        "{after:?}"
    );

    // Only the password: the servers it already logs in to.
    s.provider.set_password("newer");
    s.core.update_account(id, password("newer")).await.unwrap();
    assert_eq!(saved_password(&s, id), "newer");
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn renaming_moves_the_settings_and_refuses_taken_names() {
    let s = setup(
        r#"{
  // Work things.
  "accounts": { "Work": { "copy_sent_to_inbox": true } },
  "agent": { "accounts": { "Work": "any", "Job": "off" } },
  "folder_pane": { "favorites": [{ "account": "Work", "folder": "INBOX" }] }
}
"#,
    )
    .await;
    let work = s
        .core
        .add_imap_account(account("Work", "alice@example.org"))
        .await
        .unwrap()
        .id;
    s.core
        .add_imap_account(account("Home", "bob@example.org"))
        .await
        .unwrap();
    let rename = |name: &str| AccountChanges {
        name: Some(name.into()),
        ..AccountChanges::default()
    };

    let taken = s
        .core
        .update_account(work, rename("Home"))
        .await
        .unwrap_err();
    assert_eq!(kind(&taken), AccountErrorKind::Duplicate, "{taken}");
    // settings.json has settings for both names in one place: nothing changes.
    let before = std::fs::read_to_string(&s.settings).unwrap();
    let both = s
        .core
        .update_account(work, rename("Job"))
        .await
        .unwrap_err();
    assert_eq!(kind(&both), AccountErrorKind::InvalidInput);
    assert!(both.to_string().contains("agent.accounts"), "{both}");
    assert_eq!(std::fs::read_to_string(&s.settings).unwrap(), before);
    assert_eq!(
        s.core.account_details(work).await.unwrap().info.name,
        "Work"
    );

    std::fs::write(&s.settings, before.replace(r#", "Job": "off""#, "")).unwrap();
    let info = s.core.update_account(work, rename(" Job ")).await.unwrap();
    assert_eq!(info.name, "Job");
    // The settings went along (comments and all) and apply at once.
    let file = std::fs::read_to_string(&s.settings).unwrap();
    assert!(file.contains("// Work things."), "{file}");
    let settings = s.core.settings();
    assert!(settings.settings.copy_sent_to_inbox("Job"));
    assert_eq!(
        settings.settings.agent.access_for("Job"),
        settings::AgentAccess::Any
    );
    assert_eq!(settings.settings.folder_pane.favorites[0].account, "Job");
    // The SMTP server that carried the account's name, too.
    let reader = s.core.store().reader().unwrap();
    let smtp = reader.identities(Some(work)).unwrap()[0]
        .input
        .smtp
        .unwrap();
    assert_eq!(
        reader.smtp_server(smtp).unwrap().unwrap().name.as_deref(),
        Some("Job")
    );
    drop(reader);
    // It still syncs, without logging in again for a new name.
    assert!(s.core.synced_accounts().contains(&work));
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_local_api_changes_accounts() {
    use local_api::registry::args::AccountSetArgs;
    use local_api::{
        Command, DenyAllApprover, EffectiveScope, ErrorCode, ExecutionMode, PeerInfo, ProcessInfo,
        RequestContext, TokenScope,
    };
    use mazemail_core::api::{self, TokenAuthorizer};
    use mazemail_types::PublicId as _;

    let s = setup(r#"{ "agent": { "accounts": { "Work": "any" } } }"#).await;
    let id = s
        .core
        .add_imap_account(account("Work", "alice@example.org"))
        .await
        .unwrap()
        .id;
    let handler = api::services(
        &s.core,
        &TokenAuthorizer::new(),
        Arc::new(DenyAllApprover),
        None,
    )
    .handler;
    let ctx = |agents: &[AccountId]| {
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
            EffectiveScope::resolve(&TokenScope::all(), &agents.iter().copied().collect()),
            ExecutionMode::Execute,
        )
    };
    let set = |changes: serde_json::Value| {
        let mut args = serde_json::json!({ "id": id.to_public() });
        args.as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        Command::AccountSet(serde_json::from_value::<AccountSetArgs>(args).unwrap())
    };

    // Outside the token's scope the account doesn't exist.
    let hidden = handler
        .handle(&ctx(&[]), set(serde_json::json!({ "name": "Job" })))
        .await
        .unwrap_err();
    assert_eq!(hidden.code, ErrorCode::NotFound);

    let renamed = handler
        .handle(&ctx(&[id]), set(serde_json::json!({ "name": "Job" })))
        .await
        .unwrap();
    assert_eq!(renamed.data.expose()["name"], "Job");

    // The core enforces the password rule even for a request the wire check didn't see.
    let moved = handler
        .handle(&ctx(&[id]), set(serde_json::json!({ "port": 1993 })))
        .await
        .unwrap_err();
    assert_eq!(moved.code, ErrorCode::InvalidArgs);
    assert!(moved.message.contains("password again"), "{moved}");
    let wrong = handler
        .handle(
            &ctx(&[id]),
            set(serde_json::json!({ "security": "starttls", "password": "hunter2" })),
        )
        .await
        .unwrap_err();
    assert_eq!(wrong.code, ErrorCode::InvalidArgs);
    assert!(wrong.message.contains("rejected the login"), "{wrong}");
    assert!(!wrong.message.contains("hunter2"));
    // A new security without a port takes its usual one.
    let starttls = handler
        .handle(
            &ctx(&[id]),
            set(serde_json::json!({ "security": "starttls", "password": "old" })),
        )
        .await
        .unwrap();
    assert_eq!(starttls.data.expose()["server"], "imap.example.org:143");
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn google_accounts_sign_in_again() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let oauth = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/authorize"))
        .respond_with(|request: &Request| {
            let q: std::collections::HashMap<String, String> =
                request.url.query_pairs().into_owned().collect();
            assert_eq!(q["login_hint"], "carol@gmail.com");
            let location = format!("{}?code=the-code&state={}", q["redirect_uri"], q["state"]);
            ResponseTemplate::new(302).insert_header("Location", location.as_str())
        })
        .mount(&oauth)
        .await;
    let sign_ins = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&sign_ins);
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(move |_: &Request| {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": format!("access-{n}"),
                "refresh_token": format!("refresh-{n}"),
                "expires_in": 3600,
                "token_type": "Bearer",
            }))
        })
        .mount(&oauth)
        .await;
    let google = auth::Provider::builtin(ProviderKind::Google)
        .with_endpoints(
            &format!("{}/authorize", oauth.uri()),
            &format!("{}/token", oauth.uri()),
        )
        .unwrap()
        .with_credentials(auth::ClientCredentials::new(
            "client.apps.googleusercontent.com",
        ));
    let mut registry = auth::ProviderRegistry::new();
    registry.set_provider(google);
    let s = setup_with(FakeServer::gmail(), "{}", |engine| {
        engine
            .with_browser(Arc::new(ConsentingBrowser))
            .with_providers(registry)
    })
    .await;

    let gmail = s
        .core
        .add_oauth_account(AddOAuthAccount {
            provider: ProviderKind::Google,
            email: "carol@gmail.com".into(),
            display_name: Some("Carol".into()),
            tenant: None,
        })
        .await
        .unwrap();
    let refresh = |s: &Setup| {
        s.secrets
            .load(&SecretKey::new(gmail.uuid, SecretKind::OAuthRefresh))
            .unwrap()
            .unwrap()
            .expose()
            .clone()
    };
    assert_eq!(refresh(&s), "refresh-1");

    // Its servers are fixed and it has no password; a new name is fine.
    let password_change = s
        .core
        .update_account(gmail.id, password("x"))
        .await
        .unwrap_err();
    assert!(
        password_change.to_string().contains("signs in with Google"),
        "{password_change}"
    );
    let renamed = AccountChanges {
        name: Some("Personal".into()),
        ..AccountChanges::default()
    };
    assert_eq!(
        s.core.update_account(gmail.id, renamed).await.unwrap().name,
        "Personal"
    );

    // Restarting sync for the new name refreshes the token once, in the background.
    wait_for_state(&s.core, gmail.id, SyncState::Idle).await;
    let before = sign_ins.load(Ordering::SeqCst);
    let again = s.core.sign_in_again(gmail.id, None).await.unwrap();
    assert_eq!(again.id, gmail.id);
    assert_eq!(again.name, "Personal");
    let after = sign_ins.load(Ordering::SeqCst);
    assert_eq!(after, before + 1, "one browser sign-in");
    assert_eq!(refresh(&s), format!("refresh-{after}"));
    wait_for_state(&s.core, gmail.id, SyncState::Idle).await;

    // A password account can't sign in in the browser.
    let work = s
        .core
        .add_imap_account(account("Work", "alice@example.org"))
        .await
        .unwrap()
        .id;
    let error = s.core.sign_in_again(work, None).await.unwrap_err();
    assert_eq!(kind(&error), AccountErrorKind::InvalidInput);
    s.core.shutdown().await.unwrap();
}
