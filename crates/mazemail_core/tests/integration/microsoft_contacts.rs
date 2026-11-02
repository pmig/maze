//! Microsoft contacts through the core: connecting an account's contacts (incremental
//! consent and a Graph token against a mock Microsoft identity platform), the delta
//! sync into the `microsoft:<uuid>` book (a mock Graph), photos with the account's
//! token and the write-through create and edit.

use std::sync::Arc;
use std::time::Duration;

use auth::{BrowserOpener, ClientCredentials, MemorySecretStore, Provider, ProviderKind};
use contacts::Backoff;
use imap::fake::FakeServer;
use mazemail_core::{
    AccountInfo, AddOAuthAccount, ContactFields, ContactsProvider, CoreHandle, Labeled, Name,
    SyncEngineConfig,
};
use serde_json::json;
use store::Queries as _;
use vault::MemoryKeyStore;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::accounts::{FakeConnector, timing, wait_for};
use crate::common;

/// Follows the authorization redirect to the loopback listener at once.
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

/// The Microsoft identity platform: the mail sign-in gets an Outlook token, a request
/// for Graph's `Contacts.ReadWrite` (the contacts consent or a refresh) a Graph token
/// and one for `Contacts.Read.Shared` a separate shared-contacts token.
async fn identity_platform() -> MockServer {
    let provider = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/common/oauth2/v2.0/authorize"))
        .respond_with(|request: &Request| {
            let q: std::collections::HashMap<String, String> =
                request.url.query_pairs().into_owned().collect();
            let code = if q["scope"].contains("Contacts.Read.Shared") {
                "code-shared"
            } else if q["scope"].contains("Contacts.ReadWrite") {
                "code-graph"
            } else {
                "code-mail"
            };
            let location = format!("{}?code={code}&state={}", q["redirect_uri"], q["state"]);
            ResponseTemplate::new(302).insert_header("Location", location.as_str())
        })
        .mount(&provider)
        .await;
    Mock::given(method("POST"))
        .and(path("/common/oauth2/v2.0/token"))
        .respond_with(|request: &Request| {
            let form: std::collections::HashMap<String, String> =
                url::form_urlencoded::parse(&request.body)
                    .into_owned()
                    .collect();
            let shared = form
                .get("scope")
                .is_some_and(|s| s.contains("Contacts.Read.Shared"))
                || form.get("code").is_some_and(|c| c == "code-shared");
            let graph = form
                .get("scope")
                .is_some_and(|s| s.contains("Contacts.ReadWrite"))
                || form.get("code").is_some_and(|c| c == "code-graph");
            let (access, scope) = if shared {
                (
                    "shared-1",
                    "https://graph.microsoft.com/Contacts.Read.Shared",
                )
            } else if graph {
                ("graph-1", "https://graph.microsoft.com/Contacts.ReadWrite")
            } else {
                (
                    "access-1",
                    "https://outlook.office.com/IMAP.AccessAsUser.All \
                     https://outlook.office.com/SMTP.Send",
                )
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "access_token": access,
                "refresh_token": "refresh-1",
                "expires_in": 3600,
                "token_type": "Bearer",
                "scope": scope,
            }))
        })
        .mount(&provider)
        .await;
    provider
}

fn contact(id: &str, name: &str, email: &str) -> serde_json::Value {
    json!({
        "@odata.etag": format!("W/\"etag-{id}\""),
        "id": id,
        "parentFolderId": "FOLDER1",
        "displayName": name,
        "emailAddresses": [{"name": name, "address": email}],
    })
}

/// A Graph with Anna (with a photo) and Bob in the default Contacts folder. Every
/// request must carry the Graph token.
async fn graph() -> MockServer {
    let api = MockServer::start().await;
    let delta = "/v1.0/me/contactFolders/FOLDER1/contacts/delta";
    let bearer = || header("authorization", "Bearer graph-1");
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts"))
        .and(query_param("$top", "1"))
        .and(bearer())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"value": [{"id": "C1", "parentFolderId": "FOLDER1"}]})),
        )
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path(delta))
        .and(query_param("$deltatoken", "d1"))
        .and(bearer())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [],
            "@odata.deltaLink": format!("{}{delta}?$deltatoken=d1", api.uri()),
        })))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path(delta))
        .and(bearer())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                contact("C1", "Anna Example", "anna@example.com"),
                contact("C2", "Bob", "bob@example.net"),
            ],
            "@odata.deltaLink": format!("{}{delta}?$deltatoken=d1", api.uri()),
        })))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts/C1/photo/$value"))
        .and(bearer())
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"anna's face".to_vec(), "image/jpeg"),
        )
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts/C2/photo/$value"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"error": {"code": "ImageNotFound", "message": "none"}})),
        )
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts/C1"))
        .and(bearer())
        .respond_with(ResponseTemplate::new(200).set_body_json(contact(
            "C1",
            "Anna Example",
            "anna@example.com",
        )))
        .mount(&api)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/contacts/C1"))
        .and(header("if-match", "W/\"etag-C1\""))
        .and(bearer())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "@odata.etag": "W/\"etag-C1b\"",
            "id": "C1",
            "displayName": "Anna B. Example",
            "emailAddresses": [{"address": "anna@example.com"}],
        })))
        .mount(&api)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/contacts"))
        .and(bearer())
        .respond_with(ResponseTemplate::new(201).set_body_json(contact(
            "C3",
            "Erin Example",
            "erin@example.org",
        )))
        .mount(&api)
        .await;
    api
}

struct Setup {
    core: CoreHandle,
    account: AccountInfo,
    _provider: MockServer,
    dir: tempfile::TempDir,
}

impl Setup {
    /// Writes the account's `shared_contacts` setting and reloads the settings.
    fn share(&self, mailboxes: &[&str]) {
        let settings =
            json!({"accounts": {self.account.name.clone(): {"shared_contacts": mailboxes}}});
        std::fs::write(self.dir.path().join("settings.json"), settings.to_string()).unwrap();
        self.core.reload_settings();
    }

    fn book(&self) -> String {
        format!("microsoft:{}", self.account.uuid)
    }

    fn rows(&self) -> Vec<(String, Option<String>)> {
        let book = self.book();
        let mut rows: Vec<_> = self
            .core
            .store()
            .reader()
            .unwrap()
            .search_contacts("", 100)
            .unwrap()
            .into_iter()
            .filter(|c| c.book == book)
            .map(|c| (c.email, c.display_name))
            .collect();
        rows.sort();
        rows
    }
}

/// A core with a Microsoft account (signed in for mail only) whose contacts come from
/// `graph`.
async fn setup(graph: &str) -> Setup {
    let provider = identity_platform().await;
    let microsoft = Provider::builtin(ProviderKind::Microsoft)
        .with_authority(&provider.uri())
        .unwrap()
        .with_credentials(ClientCredentials::new("entra-app"));
    let mut registry = auth::ProviderRegistry::new();
    registry.set_provider(microsoft);
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let engine = SyncEngineConfig::new(Arc::new(MemorySecretStore::new()))
        .with_connector(Arc::new(FakeConnector {
            server: FakeServer::new(),
            reachable: true,
        }))
        .with_browser(Arc::new(ConsentingBrowser))
        .with_providers(registry)
        .with_timing(timing())
        .with_contacts_api(graph)
        .with_contacts_backoff(Backoff {
            attempts: 1,
            initial: Duration::from_millis(1),
            max: Duration::from_millis(1),
        });
    let settings = dir.path().join("settings.json");
    std::fs::write(&settings, "{}").unwrap();
    let core = CoreHandle::open(
        common::config(&dir.path().join("data"), &keys)
            .with_settings_file(&settings)
            .with_sync_engine(engine),
    )
    .await
    .unwrap();
    let account = core
        .add_oauth_account(AddOAuthAccount {
            provider: ProviderKind::Microsoft,
            // The address and token the fake IMAP server accepts for Microsoft.
            email: "dana@contoso.com".into(),
            display_name: Some("Dana".into()),
            tenant: None,
        })
        .await
        .unwrap();
    Setup {
        core,
        account,
        _provider: provider,
        dir,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn microsoft_contacts_connect_sync_show_photos_and_take_edits() {
    let api = graph().await;
    let s = setup(&api.uri()).await;

    // Before connecting: listed as a Microsoft account without permission.
    let status = s.core.contacts_status().unwrap();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].provider, ContactsProvider::Microsoft);
    assert!(!status[0].connected);

    assert!(s.core.connect_contacts(s.account.id).await.unwrap());
    let core = s.core.clone();
    let book = s.book();
    wait_for("the first contact sync", move || {
        core.store()
            .reader()
            .unwrap()
            .contact_source(&book)
            .unwrap()
            .is_some_and(|source| source.last_sync.is_some())
    })
    .await;
    assert_eq!(
        s.rows(),
        [
            (
                "anna@example.com".to_owned(),
                Some("Anna Example".to_owned())
            ),
            ("bob@example.net".to_owned(), Some("Bob".to_owned())),
        ]
    );
    let status = s.core.contacts_status().unwrap();
    assert!(status[0].connected && status[0].permission);
    assert_eq!(status[0].books.len(), 1);
    assert!(status[0].books[0].editable);
    assert!(s.core.contact_book_editable(&s.book()));

    // Photos come from Graph with the account's token; a contact without one has none.
    let photo = s
        .core
        .contact_photo("anna@example.com".into(), None)
        .await
        .unwrap()
        .expect("Anna's photo");
    assert_eq!(photo.bytes, b"anna's face");
    assert_eq!(
        s.core
            .contact_photo("bob@example.net".into(), None)
            .await
            .unwrap(),
        None
    );

    // Edits go to Graph first, with If-Match; the answer is stored.
    let anna = s
        .core
        .synced_contact_for_email(&s.book(), "anna@example.com")
        .unwrap()
        .unwrap();
    assert_eq!(anna.resource_name, "C1");
    let mut fields = anna.fields.clone();
    fields.name.display = Some("Anna B. Example".into());
    let saved = s
        .core
        .update_contact(s.book(), "C1".into(), fields, anna.etag.clone().unwrap())
        .await
        .unwrap();
    assert_eq!(saved.etag.as_deref(), Some("W/\"etag-C1b\""));
    assert!(s.rows().contains(&(
        "anna@example.com".to_owned(),
        Some("Anna B. Example".to_owned())
    )));

    // New contacts go to the Contacts book.
    let created = s
        .core
        .create_contact(
            s.account.id,
            ContactFields {
                name: Name {
                    given: Some("Erin".into()),
                    family: Some("Example".into()),
                    ..Name::default()
                },
                emails: vec![Labeled::new("erin@example.org")],
                ..ContactFields::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(created.book, s.book());
    assert!(
        s.rows()
            .iter()
            .any(|(email, _)| email == "erin@example.org")
    );

    // Graph only ever saw the Graph token, never the mail token.
    for request in api.received_requests().await.unwrap() {
        if request.url.path().starts_with("/v1.0/") {
            assert_eq!(
                request.headers.get("authorization").unwrap(),
                "Bearer graph-1",
                "{}",
                request.url.path()
            );
        }
    }
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_mailboxes_sync_read_only_with_their_own_token() {
    let api = graph().await;
    let team = "/v1.0/users/team@contoso.com";
    let delta = format!("{team}/contactFolders/TEAMFOLDER/contacts/delta");
    let shared = || header("authorization", "Bearer shared-1");
    Mock::given(method("GET"))
        .and(path(format!("{team}/contacts")))
        .and(shared())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"value": [{"id": "T1", "parentFolderId": "TEAMFOLDER"}]})),
        )
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path(delta.clone()))
        .and(shared())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [contact("T1", "Support Desk", "support@contoso.com")],
            "@odata.deltaLink": format!("{}{delta}?$deltatoken=t1", api.uri()),
        })))
        .mount(&api)
        .await;
    let s = setup(&api.uri()).await;
    s.share(&["Team@Contoso.com"]);
    let shared_book = format!("microsoft-shared:{}:team@contoso.com", s.account.uuid);

    let status = s.core.contacts_status().unwrap();
    assert_eq!(status[0].shared_mailboxes, ["team@contoso.com"]);
    assert!(!status[0].shared_permission);

    // Connecting asks for contacts, then separately for shared contacts.
    assert!(s.core.connect_contacts(s.account.id).await.unwrap());
    let core = s.core.clone();
    let book = shared_book.clone();
    wait_for("the shared contacts", move || {
        core.store()
            .reader()
            .unwrap()
            .contact_source(&book)
            .unwrap()
            .is_some_and(|source| source.last_sync.is_some())
    })
    .await;
    let rows: Vec<String> = s
        .core
        .store()
        .reader()
        .unwrap()
        .search_contacts("", 100)
        .unwrap()
        .into_iter()
        .filter(|c| c.book == shared_book)
        .map(|c| c.email)
        .collect();
    assert_eq!(rows, ["support@contoso.com"]);
    assert!(
        !s.core.contact_book_editable(&shared_book),
        "shared books are read-only"
    );
    let status = s.core.contacts_status().unwrap();
    assert!(status[0].shared_permission);

    // Taken out of the setting, the mailbox's copy goes at the next sync.
    s.share(&[]);
    s.core.sync_contacts_now(Some(s.account.id));
    let core = s.core.clone();
    let book = shared_book.clone();
    wait_for("the shared book to go", move || {
        core.store()
            .reader()
            .unwrap()
            .contact_source(&book)
            .unwrap()
            .is_none()
    })
    .await;
    s.core.shutdown().await.unwrap();
}
