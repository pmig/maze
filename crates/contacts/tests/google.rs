//! The People API client against a mock server: paging, sync tokens, deletions,
//! Other contacts, writes with etags, conflicts, token refresh, missing scopes,
//! rate limits and photos.

#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use contacts::{
    AccessTokens, Backoff, BookKind, ContactError, ContactFields, ContactSource, GoogleContacts,
    Labeled, Name, PageUpdate, sync_book,
};
use mazemail_types::Sensitive;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::matchers::{
    body_partial_json, header, method, path, query_param, query_param_is_missing,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Hands out `token-<n>`, where n counts the invalidations.
#[derive(Default)]
struct Tokens {
    invalidated: AtomicUsize,
    missing_scope: bool,
}

#[async_trait]
impl AccessTokens for Tokens {
    async fn token(&self) -> Result<Sensitive<String>, auth::AuthError> {
        if self.missing_scope {
            return Err(auth::AuthError::missing_scope(auth::Capability::Contacts));
        }
        Ok(Sensitive::new(format!(
            "token-{}",
            self.invalidated.load(Ordering::SeqCst)
        )))
    }

    async fn invalidate(&self) {
        self.invalidated.fetch_add(1, Ordering::SeqCst);
    }
}

fn client(server: &MockServer, tokens: Arc<Tokens>) -> GoogleContacts {
    GoogleContacts::new(tokens)
        .unwrap()
        .with_base_url(&server.uri())
        .with_backoff(Backoff {
            attempts: 3,
            initial: Duration::from_millis(5),
            max: Duration::from_millis(20),
        })
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

/// Runs a sync and collects the pages.
async fn sync(
    source: &GoogleContacts,
    book: BookKind,
    token: Option<&str>,
) -> (contacts::SyncSummary, Vec<PageUpdate>) {
    let pages = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&pages);
    let summary = sync_book(source, book, token.map(str::to_owned), move |update| {
        let collected = Arc::clone(&collected);
        async move {
            collected.lock().unwrap().push(update);
            Ok::<(), ContactError>(())
        }
    })
    .await
    .unwrap();
    let pages = pages.lock().unwrap().clone();
    (summary, pages)
}

fn names(update: &PageUpdate) -> Vec<String> {
    update
        .page
        .upserts
        .iter()
        .map(|p| p.fields.name.full().unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn a_full_sync_pages_through_my_contacts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/people/me/connections"))
        .and(query_param("personFields", contacts::google::PERSON_FIELDS))
        .and(query_param("pageSize", "1000"))
        .and(query_param("requestSyncToken", "true"))
        .and(query_param_is_missing("syncToken"))
        .and(query_param_is_missing("pageToken"))
        .and(header("authorization", "Bearer token-0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "connections": [
                person("c1", "Anna Example", &["anna@example.com", "anna@work.example"]),
                person("c2", "Bob", &["bob@example.net"]),
            ],
            "nextPageToken": "page-2",
            "totalPeople": 3,
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/people/me/connections"))
        .and(query_param("pageToken", "page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "connections": [person("c3", "Carol", &[])],
            "nextSyncToken": "sync-1",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let source = client(&server, Arc::default());
    let (summary, pages) = sync(&source, BookKind::Google, None).await;
    assert!(summary.full && !summary.restarted);
    assert_eq!((summary.pages, summary.upserts, summary.deletes), (2, 3, 0));
    assert_eq!(pages.len(), 2);
    assert!(pages[0].full && pages[0].first && !pages[0].last);
    assert!(pages[1].full && !pages[1].first && pages[1].last);
    assert_eq!(names(&pages[0]), ["Anna Example", "Bob"]);
    assert_eq!(
        pages[0].page.upserts[0].fields.email_addresses(),
        ["anna@example.com", "anna@work.example"]
    );
    assert_eq!(pages[0].page.next_sync_token, None);
    assert_eq!(pages[1].page.next_sync_token.as_deref(), Some("sync-1"));
}

#[tokio::test]
async fn an_incremental_sync_brings_changes_and_deletions() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/people/me/connections"))
        .and(query_param("syncToken", "sync-1"))
        .and(query_param("requestSyncToken", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "connections": [
                person("c1", "Anna Renamed", &["anna@example.com"]),
                {"resourceName": "people/c2", "etag": "x", "metadata": {"deleted": true}},
            ],
            "nextSyncToken": "sync-2",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let (summary, pages) = sync(&source, BookKind::Google, Some("sync-1")).await;
    assert!(!summary.full);
    assert_eq!((summary.upserts, summary.deletes), (1, 1));
    assert_eq!(names(&pages[0]), ["Anna Renamed"]);
    assert_eq!(pages[0].page.deletes, ["people/c2"]);
    assert!(pages[0].last && !pages[0].full);
    assert_eq!(pages[0].page.next_sync_token.as_deref(), Some("sync-2"));
}

#[tokio::test]
async fn an_expired_sync_token_means_a_full_sync() {
    for expired in [
        ResponseTemplate::new(400).set_body_json(json!({"error": {
            "code": 400,
            "message": "Sync token is expired. Clear local cache and retry call without the sync token.",
            "status": "FAILED_PRECONDITION",
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "EXPIRED_SYNC_TOKEN"}]
        }})),
        ResponseTemplate::new(410),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/people/me/connections"))
            .and(query_param("syncToken", "old"))
            .respond_with(expired)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/people/me/connections"))
            .and(query_param_is_missing("syncToken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "connections": [person("c1", "Anna", &["anna@example.com"])],
                "nextSyncToken": "fresh",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let source = client(&server, Arc::default());
        let (summary, pages) = sync(&source, BookKind::Google, Some("old")).await;
        assert!(summary.full && summary.restarted);
        assert_eq!(pages.len(), 1);
        assert!(pages[0].full && pages[0].first && pages[0].last);
        assert_eq!(pages[0].page.next_sync_token.as_deref(), Some("fresh"));
    }
}

#[tokio::test]
async fn other_contacts_have_their_own_listing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/otherContacts"))
        .and(query_param(
            "readMask",
            "names,emailAddresses,phoneNumbers,photos,metadata",
        ))
        .and(query_param("requestSyncToken", "true"))
        .and(query_param("pageSize", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "otherContacts": [{
                "resourceName": "otherContacts/c9",
                "etag": "e9",
                "names": [{"displayName": "Dora"}],
                "emailAddresses": [{"value": "dora@example.org"}],
            }],
            "nextSyncToken": "other-1",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let (summary, pages) = sync(&source, BookKind::GoogleOther, None).await;
    assert!(summary.full);
    assert_eq!(pages[0].page.upserts[0].resource_name, "otherContacts/c9");
    assert_eq!(names(&pages[0]), ["Dora"]);
    assert_eq!(pages[0].page.next_sync_token.as_deref(), Some("other-1"));
}

fn fields(name: &str, email: &str) -> ContactFields {
    ContactFields {
        name: Name {
            given: Some(name.to_owned()),
            family: Some("Example".to_owned()),
            ..Name::default()
        },
        emails: vec![Labeled::with_kind(email, "work")],
        phones: vec![Labeled::new("+43 1 234")],
        ..ContactFields::default()
    }
}

#[tokio::test]
async fn creating_a_contact_returns_the_servers_version() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/people:createContact"))
        .and(query_param("personFields", contacts::google::PERSON_FIELDS))
        .and(header("content-type", "application/json"))
        .and(body_partial_json(json!({
            "names": [{"givenName": "Eve", "familyName": "Example"}],
            "emailAddresses": [{"value": "eve@example.com", "type": "work"}],
            "phoneNumbers": [{"value": "+43 1 234"}],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(person(
            "c42",
            "Eve Example",
            &["eve@example.com"],
        )))
        .expect(1)
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let created = source
        .create(&fields("Eve", "eve@example.com"))
        .await
        .unwrap();
    assert_eq!(created.resource_name, "people/c42");
    assert_eq!(created.etag.as_deref(), Some("etag-c42"));
    // Nothing to save is refused before any request.
    assert!(matches!(
        source.create(&ContactFields::default()).await,
        Err(ContactError::Empty)
    ));
}

#[tokio::test]
async fn an_update_sends_the_etag_and_the_contact_source() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/people/c1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(person(
            "c1",
            "Anna",
            &["anna@example.com"],
        )))
        .expect(1)
        .mount(&server)
        .await;
    let mut updated = person("c1", "Anna Example", &["anna@new.example"]);
    updated["etag"] = json!("etag-c1-v2");
    Mock::given(method("PATCH"))
        .and(path("/v1/people/c1:updateContact"))
        .and(query_param(
            "updatePersonFields",
            "names,emailAddresses,phoneNumbers,organizations,addresses,urls,biographies",
        ))
        .and(body_partial_json(json!({
            "resourceName": "people/c1",
            "etag": "etag-c1",
            "metadata": {"sources": [{"type": "CONTACT", "id": "c1", "etag": "src-c1"}]},
            "emailAddresses": [{"value": "anna@new.example"}],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(updated))
        .expect(1)
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let person = source
        .update("people/c1", "etag-c1", &fields("Anna", "anna@new.example"))
        .await
        .unwrap();
    assert_eq!(person.etag.as_deref(), Some("etag-c1-v2"));
    assert_eq!(person.fields.email_addresses(), ["anna@new.example"]);
}

#[tokio::test]
async fn a_changed_contact_is_a_conflict_with_the_current_version() {
    // The etag Maze has is old: no write, the current version comes back.
    let server = MockServer::start().await;
    let mut current = person("c1", "Anna Changed Elsewhere", &["anna@example.com"]);
    current["etag"] = json!("etag-newer");
    Mock::given(method("GET"))
        .and(path("/v1/people/c1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(current))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    match source
        .update("people/c1", "etag-c1", &fields("Anna", "a@example.com"))
        .await
    {
        Err(ContactError::Conflict(person)) => {
            assert_eq!(person.etag.as_deref(), Some("etag-newer"));
            assert_eq!(
                person.fields.name.full().as_deref(),
                Some("Anna Changed Elsewhere")
            );
        }
        other => panic!("expected a conflict, got {other:?}"),
    }

    // The contact changes between the read and the write: the server says
    // FAILED_PRECONDITION, and the version read afterwards comes back.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/people/c1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(person(
            "c1",
            "Anna",
            &["anna@example.com"],
        )))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let mut raced = person("c1", "Anna Raced", &["anna@example.com"]);
    raced["etag"] = json!("etag-raced");
    Mock::given(method("GET"))
        .and(path("/v1/people/c1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(raced))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1/people/c1:updateContact"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "code": 400,
            "message": "Request person.etag is different than the current person.etag. Clear local cache and get the latest person.",
            "status": "FAILED_PRECONDITION",
        }})))
        .expect(1)
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    match source
        .update("people/c1", "etag-c1", &fields("Anna", "a@example.com"))
        .await
    {
        Err(ContactError::Conflict(person)) => {
            assert_eq!(person.etag.as_deref(), Some("etag-raced"));
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn a_rejected_token_is_refreshed_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("authorization", "Bearer token-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "connections": [], "nextSyncToken": "s"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let tokens = Arc::new(Tokens::default());
    let source = client(&server, Arc::clone(&tokens));
    let page = source.list(BookKind::Google, None, None).await.unwrap();
    assert_eq!(page.next_sync_token.as_deref(), Some("s"));
    assert_eq!(tokens.invalidated.load(Ordering::SeqCst), 1);

    // Still rejected after a fresh token: the account has to sign in again.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .expect(2)
        .mount(&server)
        .await;
    let tokens = Arc::new(Tokens::default());
    let source = client(&server, Arc::clone(&tokens));
    let err = source.list(BookKind::Google, None, None).await.unwrap_err();
    assert!(matches!(err, ContactError::Unauthorized), "{err:?}");
    assert!(err.needs_consent());
}

#[tokio::test]
async fn missing_contacts_permission_is_its_own_error() {
    // The token doesn't have the scopes (auth knows it): no request at all.
    let server = MockServer::start().await;
    let source = client(
        &server,
        Arc::new(Tokens {
            missing_scope: true,
            ..Tokens::default()
        }),
    );
    let err = source.list(BookKind::Google, None, None).await.unwrap_err();
    assert!(matches!(err, ContactError::MissingScope), "{err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());

    // Google says so.
    Mock::given(method("GET"))
        .and(path("/v1/otherContacts"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": {
            "code": 403,
            "message": "Request had insufficient authentication scopes.",
            "status": "PERMISSION_DENIED",
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "ACCESS_TOKEN_SCOPE_INSUFFICIENT"}]
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/people/me/connections"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": {
            "code": 403,
            "message": "People API has not been used in project 123 before or it is disabled.",
            "status": "PERMISSION_DENIED",
            "details": [{"reason": "SERVICE_DISABLED"}]
        }})))
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let err = source
        .list(BookKind::GoogleOther, None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, ContactError::MissingScope), "{err:?}");
    let err = source.list(BookKind::Google, None, None).await.unwrap_err();
    assert!(matches!(err, ContactError::ApiDisabled), "{err:?}");
}

#[tokio::test]
async fn rate_limits_and_server_errors_back_off() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"connections": []})))
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    assert!(source.list(BookKind::Google, None, None).await.is_ok());
    assert_eq!(server.received_requests().await.unwrap().len(), 3);

    // A write isn't repeated after a server error (it may have happened).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let err = source
        .create(&fields("Eve", "eve@example.com"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ContactError::Server { status: 500 }),
        "{err:?}"
    );
    assert!(err.is_transient());
}

#[tokio::test]
async fn offline_is_reported_as_such() {
    // Nothing listens on port 1 (connection refused).
    let source = GoogleContacts::new(Arc::new(Tokens::default()))
        .unwrap()
        .with_base_url("http://127.0.0.1:1")
        .with_backoff(Backoff {
            attempts: 1,
            initial: Duration::from_millis(1),
            max: Duration::from_millis(1),
        });
    let err = source
        .create(&fields("Eve", "eve@example.com"))
        .await
        .unwrap_err();
    assert!(matches!(err, ContactError::Offline { .. }), "{err:?}");
}

#[tokio::test]
async fn photos_are_fetched_at_96_pixels_without_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/photo/anna=s96"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/jpeg")
                .set_body_bytes(b"jpeg bytes".to_vec()),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/photo/gone=s96"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/photo/html=s96"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/html"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/people/me/connections"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "connections": [{
                "resourceName": "people/c1",
                "emailAddresses": [{"value": "anna@example.com"}],
                "photos": [{"metadata": {"primary": true}, "url": format!("{}/photo/anna=s100", server.uri())}],
            }],
        })))
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let page = source.list(BookKind::Google, None, None).await.unwrap();
    let url = page.upserts[0].photo_url.clone().unwrap();
    assert!(url.ends_with("/photo/anna=s96"), "{url}");
    let photo = source.photo(&url).await.unwrap().unwrap();
    assert_eq!(photo.content_type, "image/jpeg");
    assert_eq!(photo.bytes, b"jpeg bytes");
    let requests = server.received_requests().await.unwrap();
    let photo_request = requests
        .iter()
        .find(|r| r.url.path().starts_with("/photo/"))
        .unwrap();
    assert!(!photo_request.headers.contains_key("authorization"));
    assert!(!photo_request.headers.contains_key("cookie"));
    assert!(!photo_request.headers.contains_key("referer"));

    let gone = format!("{}/photo/gone=s96", server.uri());
    assert_eq!(source.photo(&gone).await.unwrap(), None);
    let html = format!("{}/photo/html=s96", server.uri());
    assert_eq!(source.photo(&html).await.unwrap(), None);
    assert_eq!(source.photo("file:///etc/passwd").await.unwrap(), None);
}
