//! Gravatar for contacts without a photo (ADR-032): asked only when `contacts.gravatar`
//! is `contacts` and only for contact addresses, remembered (a missing picture too) and
//! never shown while it's off. Against a mock Gravatar.

use std::path::PathBuf;
use std::sync::Arc;

use auth::MemorySecretStore;
use imap::fake::FakeServer;
use mazemail_core::{CoreHandle, SyncEngineConfig};
use vault::MemoryKeyStore;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::accounts::{FakeConnector, timing};
use crate::common;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n not really, but an image to the core";

struct Setup {
    core: CoreHandle,
    gravatar: MockServer,
    settings: PathBuf,
    _dir: tempfile::TempDir,
}

impl Setup {
    /// The requests Gravatar got so far.
    async fn asked(&self) -> usize {
        self.gravatar.received_requests().await.unwrap().len()
    }

    fn set_gravatar(&self, policy: &str) {
        let json = format!(r#"{{ "contacts": {{ "gravatar": "{policy}" }} }}"#);
        std::fs::write(&self.settings, json).unwrap();
        self.core.reload_settings();
    }

    async fn photo(&self, email: &str) -> Option<Vec<u8>> {
        self.core
            .contact_photo(email.to_owned(), None)
            .await
            .unwrap()
            .map(|photo| photo.bytes)
    }
}

/// A core whose Gravatar has pictures for anna@example.com and carol@example.com,
/// with Anna and Bob among the addresses you wrote to.
async fn setup() -> Setup {
    let gravatar = MockServer::start().await;
    for email in ["anna@example.com", "carol@example.com"] {
        let url = contacts::gravatar_url("", email);
        let at = url.split('?').next().unwrap().to_owned();
        Mock::given(method("GET"))
            .and(path(at))
            .respond_with(ResponseTemplate::new(200).set_body_raw(PNG, "image/png"))
            .mount(&gravatar)
            .await;
    }
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&gravatar)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let engine = SyncEngineConfig::new(Arc::new(MemorySecretStore::new()))
        .with_connector(Arc::new(FakeConnector {
            server: FakeServer::new(),
            reachable: true,
        }))
        .with_timing(timing())
        .with_gravatar(gravatar.uri());
    let settings = dir.path().join("settings.json");
    std::fs::write(&settings, "{}").unwrap();
    let config = common::config(&dir.path().join("data"), &keys)
        .with_sync_engine(engine)
        .with_settings_file(settings.clone());
    let core = CoreHandle::open(config).await.unwrap();
    core.store()
        .write(|w| {
            w.bump_contact("anna@example.com", Some("Anna"), 1_000)?;
            w.bump_contact("bob@example.net", Some("Bob"), 2_000)
        })
        .await
        .unwrap();
    Setup {
        core,
        gravatar,
        settings,
        _dir: dir,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn gravatar_is_asked_only_when_on_and_only_for_contacts() {
    let s = setup().await;

    // Off by default: Gravatar isn't asked.
    assert_eq!(s.photo("anna@example.com").await, None);
    assert_eq!(s.asked().await, 0);

    // On: Anna's picture, asked once and then from the cache.
    s.set_gravatar("contacts");
    assert_eq!(s.photo("Anna@Example.com").await.as_deref(), Some(PNG));
    assert_eq!(s.photo("anna@example.com").await.as_deref(), Some(PNG));
    assert_eq!(s.asked().await, 1);

    // Bob has none: asked once, and that is remembered too.
    assert_eq!(s.photo("bob@example.net").await, None);
    assert_eq!(s.photo("bob@example.net").await, None);
    assert_eq!(s.asked().await, 2);

    // Not a contact: never asked.
    assert_eq!(s.photo("stranger@example.org").await, None);
    assert_eq!(s.asked().await, 2);

    // Off again: what Gravatar gave isn't shown any more.
    s.set_gravatar("off");
    assert_eq!(s.photo("anna@example.com").await, None);
    assert_eq!(s.asked().await, 2);

    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_synced_contact_without_a_photo_gets_the_gravatar() {
    let s = setup().await;
    // Carol is synced from Google with a photo URL, but there is no photo behind it
    // (as for most Microsoft contacts, whose photos are all references).
    let fixture = common::fixture(&s.core).await;
    let gmail = s
        .core
        .accounts()
        .unwrap()
        .into_iter()
        .find(|a| a.id == fixture.gmail)
        .unwrap();
    let book = format!("google:{}", gmail.uuid);
    s.core
        .store()
        .write(move |w| {
            w.ensure_contact_source(&book, gmail.id, "google")?;
            let carol = store::ServerContact {
                resource_name: "people/c1".to_owned(),
                etag: None,
                display_name: Some("Carol".to_owned()),
                emails: vec!["carol@example.com".to_owned()],
                vcard: "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Carol\r\nEND:VCARD\r\n".to_owned(),
                photo_url: Some("https://photos.example/carol".to_owned()),
            };
            w.upsert_server_contact(&book, &carol, 1)?;
            w.record_contact_photo("https://photos.example/carol", false)
        })
        .await
        .unwrap();

    assert_eq!(s.photo("carol@example.com").await, None, "off by default");
    s.set_gravatar("contacts");
    assert_eq!(s.photo("carol@example.com").await.as_deref(), Some(PNG));
    s.core.shutdown().await.unwrap();
}
