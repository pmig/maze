//! Google contacts through the core: connecting an account's contacts (incremental
//! consent against a mock OAuth provider), the sync into `google:<uuid>` and
//! `google-other:<uuid>` books (a mock People API), completion ranking, photos, the
//! write-through create and edit with conflicts, and offline.

use std::sync::Arc;
use std::time::Duration;

use auth::{BrowserOpener, MemorySecretStore, ProviderKind};
use contacts::Backoff;
use imap::fake::FakeServer;
use mazemail_core::{
    AccountInfo, AddOAuthAccount, ContactBookState, ContactEdit, ContactFields, ContactPhone,
    CoreHandle, Error, Labeled, Name, ServerContactError as ContactError, SyncEngineConfig,
};
use mazemail_types::Address;
use serde_json::json;
use store::Queries as _;
use vault::MemoryKeyStore;
use wiremock::matchers::{method, path, query_param_is_missing};
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

const MAIL_SCOPE: &str = "https://mail.google.com/";
const ALL_SCOPES: &str = "https://mail.google.com/ https://www.googleapis.com/auth/contacts \
                          https://www.googleapis.com/auth/contacts.other.readonly";

/// A Google OAuth provider: a sign-in that asks for contacts gets them.
async fn oauth_provider() -> MockServer {
    let provider = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/authorize"))
        .respond_with(|request: &Request| {
            let q: std::collections::HashMap<String, String> =
                request.url.query_pairs().into_owned().collect();
            let code = if q["scope"].contains("contacts") {
                "code-contacts"
            } else {
                "code-mail"
            };
            let location = format!("{}?code={code}&state={}", q["redirect_uri"], q["state"]);
            ResponseTemplate::new(302).insert_header("Location", location.as_str())
        })
        .mount(&provider)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(|request: &Request| {
            let body = String::from_utf8_lossy(&request.body);
            let scope = if body.contains("code=code-mail") {
                MAIL_SCOPE
            } else {
                ALL_SCOPES
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "access-1",
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

fn person(id: &str, name: &str, emails: &[&str]) -> serde_json::Value {
    json!({
        "resourceName": format!("people/{id}"),
        "etag": format!("etag-{id}"),
        "metadata": {"sources": [{"type": "CONTACT", "id": id, "etag": format!("src-{id}")}]},
        "names": [{"metadata": {"primary": true}, "displayName": name}],
        "emailAddresses": emails.iter().map(|e| json!({"value": e})).collect::<Vec<_>>(),
    })
}

/// A People API with Anna (with a photo) and Bob in My Contacts and Dora in Other
/// contacts.
async fn people_api() -> MockServer {
    let api = MockServer::start().await;
    let mut anna = person("c1", "Anna Example", &["anna@example.com"]);
    anna["photos"] =
        json!([{"metadata": {"primary": true}, "url": format!("{}/photo/anna=s100", api.uri())}]);
    Mock::given(method("GET"))
        .and(path("/v1/people/me/connections"))
        .and(query_param_is_missing("syncToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "connections": [anna, person("c2", "Bob", &["bob@example.net"])],
            "nextSyncToken": "sync-1",
        })))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/people/me/connections"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "nextSyncToken": "sync-2",
        })))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/otherContacts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "otherContacts": [{
                "resourceName": "otherContacts/c9",
                "names": [{"displayName": "Dora"}],
                "emailAddresses": [{"value": "dora@example.org"}],
            }],
            "nextSyncToken": "other-1",
        })))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/photo/anna=s96"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/jpeg")
                .set_body_bytes(b"anna's face".to_vec()),
        )
        .mount(&api)
        .await;
    api
}

struct Setup {
    core: CoreHandle,
    account: AccountInfo,
    _provider: MockServer,
    _dir: tempfile::TempDir,
}

impl Setup {
    fn book(&self) -> String {
        format!("google:{}", self.account.uuid)
    }

    fn other_book(&self) -> String {
        format!("google-other:{}", self.account.uuid)
    }

    /// `(email, name)` of the rows of `book`.
    fn rows(&self, book: &str) -> Vec<(String, Option<String>)> {
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

/// A core with a Gmail account (signed in for mail only) whose contacts come from
/// `people_api`.
async fn setup(people_api: &str) -> Setup {
    let provider = oauth_provider().await;
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
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let engine = SyncEngineConfig::new(Arc::new(MemorySecretStore::new()))
        .with_connector(Arc::new(FakeConnector {
            server: FakeServer::gmail(),
            reachable: true,
        }))
        .with_browser(Arc::new(ConsentingBrowser))
        .with_providers(registry)
        .with_timing(timing())
        .with_contacts_api(people_api)
        .with_contacts_backoff(Backoff {
            attempts: 1,
            initial: Duration::from_millis(1),
            max: Duration::from_millis(1),
        });
    let core = CoreHandle::open(common::config(dir.path(), &keys).with_sync_engine(engine))
        .await
        .unwrap();
    let account = core
        .add_oauth_account(AddOAuthAccount {
            provider: ProviderKind::Google,
            email: "carol@gmail.com".into(),
            display_name: Some("Carol".into()),
            tenant: None,
        })
        .await
        .unwrap();
    Setup {
        core,
        account,
        _provider: provider,
        _dir: dir,
    }
}

/// Connects the account's contacts and waits for the first sync.
async fn connect(s: &Setup) {
    assert!(s.core.connect_google_contacts(s.account.id).await.unwrap());
    let core = s.core.clone();
    let books = [s.book(), s.other_book()];
    wait_for("the first contact sync", move || {
        let reader = core.store().reader().unwrap();
        books.iter().all(|book| {
            reader
                .contact_source(book)
                .unwrap()
                .is_some_and(|source| source.last_sync.is_some())
        })
    })
    .await;
}

fn fields(given: &str, emails: &[&str]) -> ContactFields {
    ContactFields {
        name: Name {
            given: Some(given.to_owned()),
            family: Some("Example".to_owned()),
            ..Name::default()
        },
        emails: emails.iter().map(|e| Labeled::new(*e)).collect(),
        ..ContactFields::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn connected_contacts_sync_and_complete_before_collected_addresses() {
    let api = people_api().await;
    let s = setup(&api.uri()).await;
    // Signed in for mail only: nothing synced, nothing editable.
    let status = s.core.contacts_status().unwrap();
    assert_eq!(status.len(), 1);
    assert!(!status[0].connected && !status[0].permission);
    assert!(!s.core.contact_book_editable(&s.book()));
    // An address you wrote to, often.
    s.core
        .store()
        .write(|w| {
            for _ in 0..5 {
                w.bump_contact("annabel@example.org", Some("Annabel"), 1)?;
            }
            Ok(())
        })
        .await
        .unwrap();

    connect(&s).await;
    assert_eq!(
        s.rows(&s.book()),
        [
            (
                "anna@example.com".to_owned(),
                Some("Anna Example".to_owned())
            ),
            ("bob@example.net".to_owned(), Some("Bob".to_owned())),
        ]
    );
    assert_eq!(
        s.rows(&s.other_book()),
        [("dora@example.org".to_owned(), Some("Dora".to_owned()))]
    );
    // The address book entry comes first, although Annabel was written to more often.
    assert_eq!(
        s.core.complete_addresses("ann", 8).unwrap(),
        [
            Address::new(Some("Anna Example"), "anna@example.com"),
            Address::new(Some("Annabel"), "annabel@example.org"),
        ]
    );
    assert_eq!(
        s.core.complete_addresses("dora", 8).unwrap(),
        [Address::new(Some("Dora"), "dora@example.org")]
    );
    // My Contacts are editable, Other contacts aren't.
    assert!(s.core.contact_book_editable(&s.book()));
    assert!(!s.core.contact_book_editable(&s.other_book()));
    assert!(!s.core.contact_book_editable("collected"));
    let status = s.core.contacts_status().unwrap();
    assert!(status[0].connected && status[0].permission && status[0].sync_enabled);
    assert_eq!(status[0].books.len(), 2);
    assert!(
        status[0]
            .books
            .iter()
            .all(|b| b.state == ContactBookState::Ok)
    );
    // The opt-in is in the account's config.
    let account = s.core.accounts().unwrap().remove(0);
    let config = mazemail_core::AccountConfig::parse(&account.config_json).unwrap();
    assert!(config.contacts_opt_in().connected);

    // Details for the Address Book, from the stored vCard.
    let anna = s
        .core
        .synced_contact_for_email(&s.book(), "ANNA@example.com")
        .unwrap()
        .unwrap();
    assert_eq!(anna.resource_name, "people/c1");
    assert_eq!(anna.etag.as_deref(), Some("etag-c1"));
    assert_eq!(anna.fields.display_name().as_deref(), Some("Anna Example"));
    assert!(anna.editable);
    let vcard = s
        .core
        .store()
        .reader()
        .unwrap()
        .contact_by_email("anna@example.com")
        .unwrap()
        .unwrap()
        .vcard
        .unwrap();
    assert!(
        vcard.starts_with("BEGIN:VCARD\r\nVERSION:4.0\r\n"),
        "{vcard}"
    );

    // The photo was fetched once into the encrypted cache.
    let core = s.core.clone();
    wait_for("the photo to be cached", move || {
        let reader = core.store().reader().unwrap();
        let url = reader
            .contact_photo_url("anna@example.com", None)
            .unwrap()
            .unwrap();
        reader.contact_photo(&url).unwrap().is_some()
    })
    .await;
    let photo = s
        .core
        .contact_photo("anna@example.com".into(), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(photo.bytes, b"anna's face");
    // From the cache: Google was asked once.
    let photo_requests = api
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with("/photo/"))
        .count();
    assert_eq!(photo_requests, 1);
    assert_eq!(
        s.core
            .contact_photo("bob@example.net".into(), None)
            .await
            .unwrap(),
        None
    );

    // Disconnecting deletes the copy.
    s.core.disconnect_contacts(s.account.id).await.unwrap();
    assert!(s.rows(&s.book()).is_empty() && s.rows(&s.other_book()).is_empty());
    assert!(!s.core.contact_book_editable(&s.book()));
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_reach_the_store_only_after_google_accepted_them() {
    let api = people_api().await;
    let s = setup(&api.uri()).await;
    // Not connected yet: no write at all.
    let err = s
        .core
        .create_contact(s.account.id, fields("Eve", &["eve@example.com"]))
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Contacts(ContactError::MissingScope)),
        "{err:?}"
    );
    connect(&s).await;

    // Google fails the edit: nothing changes here.
    Mock::given(method("GET"))
        .and(path("/v1/people/c2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(person(
            "c2",
            "Bob",
            &["bob@example.net"],
        )))
        .mount(&api)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1/people/c2:updateContact"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "code": 400, "message": "Invalid email address.", "status": "INVALID_ARGUMENT"
        }})))
        .up_to_n_times(1)
        .mount(&api)
        .await;
    // The Address Book's Save, on Bob's row.
    let bob = s.core.contact("bob@example.net").unwrap().unwrap();
    assert!(s.core.contact_book_editable(&bob.versions[0].book.id));
    assert!(bob.versions[0].book.editable);
    let edit = ContactEdit {
        book: s.book(),
        contact: Some(bob.versions[0].id),
        name: Some("Robert Example".into()),
        emails: vec!["robert@example.net".into()],
        phones: vec![ContactPhone {
            kind: Some("cell".into()),
            number: "+43 1 234".into(),
        }],
        organization: Some("Builders".into()),
        title: None,
    };
    let err = s.core.save_contact(edit.clone()).await.unwrap_err();
    assert!(
        matches!(
            err,
            mazemail_core::ContactError::Core(Error::Contacts(ContactError::Rejected {
                status: 400,
                ..
            }))
        ),
        "{err:?}"
    );
    assert!(
        s.rows(&s.book())
            .contains(&("bob@example.net".to_owned(), Some("Bob".to_owned())))
    );

    // Google accepts it: the row follows Google's answer.
    let mut robert = person("c2", "Robert Example", &["robert@example.net"]);
    robert["etag"] = json!("etag-c2-v2");
    Mock::given(method("PATCH"))
        .and(path("/v1/people/c2:updateContact"))
        .and(wiremock::matchers::body_partial_json(json!({
            "etag": "etag-c2",
            "names": [{"unstructuredName": "Robert Example"}],
            "emailAddresses": [{"value": "robert@example.net"}],
            "phoneNumbers": [{"value": "+43 1 234", "type": "cell"}],
            "organizations": [{"name": "Builders"}],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(robert))
        .mount(&api)
        .await;
    let saved = s.core.save_contact(edit).await.unwrap();
    assert_eq!(saved.email, "robert@example.net");
    assert_eq!(saved.name.as_deref(), Some("Robert Example"));
    assert_eq!(
        s.rows(&s.book()),
        [
            (
                "anna@example.com".to_owned(),
                Some("Anna Example".to_owned())
            ),
            (
                "robert@example.net".to_owned(),
                Some("Robert Example".to_owned())
            ),
        ]
    );
    let stored = s
        .core
        .synced_contact(&s.book(), "people/c2")
        .unwrap()
        .unwrap();
    assert_eq!(stored.etag.as_deref(), Some("etag-c2-v2"));

    // A new contact, from the Address Book.
    Mock::given(method("POST"))
        .and(path("/v1/people:createContact"))
        .respond_with(ResponseTemplate::new(200).set_body_json(person(
            "c3",
            "Eve Example",
            &["eve@example.com"],
        )))
        .mount(&api)
        .await;
    let created = s
        .core
        .save_contact(ContactEdit {
            book: s.book(),
            contact: None,
            name: Some("Eve Example".into()),
            emails: vec!["eve@example.com".into()],
            phones: Vec::new(),
            organization: None,
            title: None,
        })
        .await
        .unwrap();
    assert_eq!(created.email, "eve@example.com");
    assert!(
        s.rows(&s.book())
            .contains(&("eve@example.com".to_owned(), Some("Eve Example".to_owned())))
    );
    // The core API directly.
    let direct = s
        .core
        .create_contact(s.account.id, fields("Eve", &["eve@example.com"]))
        .await
        .unwrap();
    assert_eq!(direct.resource_name, "people/c3");
    assert_eq!(direct.book, s.book());

    // Anna changed on Google meanwhile: a conflict with the current version, which
    // is stored.
    let mut anna = person("c1", "Anna Changed", &["anna@example.com"]);
    anna["etag"] = json!("etag-c1-v9");
    Mock::given(method("GET"))
        .and(path("/v1/people/c1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anna))
        .mount(&api)
        .await;
    let err = s
        .core
        .update_contact(
            s.book(),
            "people/c1".into(),
            fields("Anna", &["anna@example.com"]),
            "etag-c1".into(),
        )
        .await
        .unwrap_err();
    match err {
        Error::Contacts(ContactError::Conflict(current)) => {
            assert_eq!(current.etag.as_deref(), Some("etag-c1-v9"));
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert!(s.rows(&s.book()).contains(&(
        "anna@example.com".to_owned(),
        Some("Anna Changed".to_owned())
    )));

    // Other contacts are read-only.
    let err = s
        .core
        .update_contact(
            s.other_book(),
            "otherContacts/c9".into(),
            fields("Dora", &["dora@example.org"]),
            "x".into(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Contacts(ContactError::ReadOnly)),
        "{err:?}"
    );
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_saves_fail_without_storing_anything() {
    // Nothing listens on port 1.
    let s = setup("http://127.0.0.1:1").await;
    assert!(s.core.connect_google_contacts(s.account.id).await.unwrap());
    let err = s
        .core
        .create_contact(s.account.id, fields("Eve", &["eve@example.com"]))
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Contacts(ContactError::Offline { .. })),
        "{err:?}"
    );
    assert!(err.to_string().contains("online"), "{err}");
    assert!(s.rows(&s.book()).is_empty());
    // Sync rounds offline leave no error behind.
    s.core.sync_contacts_now(Some(s.account.id));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let status = s.core.contacts_status().unwrap();
    assert!(
        status[0]
            .books
            .iter()
            .all(|b| b.state == ContactBookState::Ok)
    );
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_the_account_removes_its_contacts() {
    let api = people_api().await;
    let s = setup(&api.uri()).await;
    connect(&s).await;
    assert!(!s.rows(&s.book()).is_empty());
    s.core.remove_account(s.account.id).await.unwrap();
    let reader = s.core.store().reader().unwrap();
    assert!(reader.search_contacts("", 100).unwrap().is_empty());
    assert!(reader.server_contacts(&s.book()).unwrap().is_empty());
    drop(reader);
    s.core.shutdown().await.unwrap();
}
