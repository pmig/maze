//! The Microsoft Graph client against a mock server: the default folder, delta paging,
//! incremental changes and deletions, expired delta tokens, links that point
//! elsewhere, writes with etags, conflicts, server errors (retries, what gets logged),
//! missing permission and photos.

#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use contacts::{
    AccessTokens, Backoff, BookKind, ContactError, ContactFields, ContactSource, Labeled,
    MicrosoftContacts, Name, PageUpdate, sync_book,
};
use mazemail_types::Sensitive;
use pretty_assertions::assert_eq;
use serde_json::json;
use uuid::Uuid;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

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
            "graph-{}",
            self.invalidated.load(Ordering::SeqCst)
        )))
    }

    async fn invalidate(&self) {
        self.invalidated.fetch_add(1, Ordering::SeqCst);
    }
}

fn account() -> Uuid {
    Uuid::from_u128(0x6f00_0000_0000_4000_8000_0000_0000_0001)
}

fn client(server: &MockServer, tokens: Arc<Tokens>) -> MicrosoftContacts {
    MicrosoftContacts::new(tokens, account())
        .unwrap()
        .with_base_url(&server.uri())
        .with_backoff(Backoff {
            attempts: 3,
            initial: Duration::from_millis(5),
            max: Duration::from_millis(20),
        })
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

async fn mount_folder(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts"))
        .and(query_param("$top", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"value": [{"id": "C1", "parentFolderId": "FOLDER1"}]})),
        )
        .mount(server)
        .await;
}

async fn sync(
    source: &MicrosoftContacts,
    token: Option<&str>,
) -> Result<(contacts::SyncSummary, Vec<PageUpdate>), ContactError> {
    let pages = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&pages);
    let summary = sync_book(
        source,
        BookKind::Microsoft,
        token.map(str::to_owned),
        move |update| {
            let collected = Arc::clone(&collected);
            async move {
                collected.lock().unwrap().push(update);
                Ok::<(), ContactError>(())
            }
        },
    )
    .await?;
    let pages = pages.lock().unwrap().clone();
    Ok((summary, pages))
}

fn names(update: &PageUpdate) -> Vec<String> {
    update
        .page
        .upserts
        .iter()
        .filter_map(|p| p.fields.display_name())
        .collect()
}

#[tokio::test]
async fn a_full_sync_pages_through_the_default_folder() {
    let server = MockServer::start().await;
    mount_folder(&server).await;
    let delta = "/v1.0/me/contactFolders/FOLDER1/contacts/delta";
    Mock::given(method("GET"))
        .and(path(delta))
        .and(query_param("$skiptoken", "p2"))
        .and(header("authorization", "Bearer graph-0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [contact("C3", "Carol", "carol@example.com")],
            "@odata.deltaLink": format!("{}{delta}?$deltatoken=d1", server.uri()),
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(delta))
        .and(header("prefer", "odata.maxpagesize=200"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [contact("C1", "Anna", "anna@example.com"), contact("C2", "Bob", "bob@example.com")],
            "@odata.nextLink": format!("{}{delta}?$skiptoken=p2", server.uri()),
        })))
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let (summary, pages) = sync(&source, None).await.unwrap();
    assert!(summary.full);
    assert_eq!(summary.pages, 2);
    assert_eq!(names(&pages[0]), ["Anna", "Bob"]);
    assert_eq!(names(&pages[1]), ["Carol"]);
    assert!(pages[1].last);
    assert_eq!(
        pages[1].page.next_sync_token.as_deref(),
        Some(format!("{}{delta}?$deltatoken=d1", server.uri()).as_str())
    );
    let anna = &pages[0].page.upserts[0];
    assert_eq!(anna.etag.as_deref(), Some("W/\"etag-C1\""));
    assert_eq!(
        anna.photo_url.as_deref(),
        Some("microsoft-photo:6f000000-0000-4000-8000-000000000001:C1")
    );
}

#[tokio::test]
async fn an_incremental_sync_brings_changes_and_deletions() {
    let server = MockServer::start().await;
    let delta = "/v1.0/me/contactFolders/FOLDER1/contacts/delta";
    Mock::given(method("GET"))
        .and(path(delta))
        .and(query_param("$deltatoken", "d1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                contact("C1", "Anna B.", "anna@example.com"),
                {"id": "C2", "@removed": {"reason": "deleted"}},
            ],
            "@odata.deltaLink": format!("{}{delta}?$deltatoken=d2", server.uri()),
        })))
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let token = format!("{}{delta}?$deltatoken=d1", server.uri());
    let (summary, pages) = sync(&source, Some(&token)).await.unwrap();
    assert!(!summary.full);
    assert_eq!(names(&pages[0]), ["Anna B."]);
    assert_eq!(pages[0].page.deletes, ["C2"]);
    assert!(
        pages[0]
            .page
            .next_sync_token
            .as_deref()
            .unwrap()
            .ends_with("d2")
    );
}

#[tokio::test]
async fn an_expired_delta_token_means_a_full_sync() {
    let server = MockServer::start().await;
    mount_folder(&server).await;
    let delta = "/v1.0/me/contactFolders/FOLDER1/contacts/delta";
    Mock::given(method("GET"))
        .and(path(delta))
        .and(query_param("$deltatoken", "old"))
        .respond_with(
            ResponseTemplate::new(410)
                .set_body_json(json!({"error": {"code": "SyncStateNotFound", "message": "gone"}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(delta))
        .and(query_param("$select", contacts::microsoft::json::SELECT))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [contact("C1", "Anna", "anna@example.com")],
            "@odata.deltaLink": format!("{}{delta}?$deltatoken=new", server.uri()),
        })))
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let token = format!("{}{delta}?$deltatoken=old", server.uri());
    let (summary, pages) = sync(&source, Some(&token)).await.unwrap();
    assert!(summary.restarted && summary.full);
    assert_eq!(names(&pages[0]), ["Anna"]);
}

#[tokio::test]
async fn an_empty_folder_has_nothing_to_track_yet() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": []})))
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let (summary, pages) = sync(&source, None).await.unwrap();
    assert_eq!((summary.pages, summary.upserts), (1, 0));
    assert!(pages[0].last && pages[0].full);
    assert_eq!(pages[0].page.next_sync_token, None);
}

#[tokio::test]
async fn links_that_point_away_from_graph_are_refused() {
    let server = MockServer::start().await;
    mount_folder(&server).await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contactFolders/FOLDER1/contacts/delta"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [],
            "@odata.nextLink": "https://evil.example/v1.0/steal",
        })))
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let err = sync(&source, None).await.unwrap_err();
    assert!(matches!(err, ContactError::InvalidResponse(_)), "{err:?}");
    // A stored token from elsewhere is dropped for a full sync instead of used.
    let err = source
        .list(BookKind::Microsoft, Some("https://evil.example/x"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, ContactError::SyncTokenExpired), "{err:?}");
}

fn fields(given: &str, email: &str) -> ContactFields {
    ContactFields {
        name: Name {
            given: Some(given.into()),
            family: Some("Example".into()),
            ..Name::default()
        },
        emails: vec![Labeled::new(email)],
        phones: vec![Labeled::with_kind("+43 660 1", "mobile")],
        ..ContactFields::default()
    }
}

/// A JSON body without `null` members and empty lists, at any depth.
struct NothingEmpty;

impl Match for NothingEmpty {
    fn matches(&self, request: &Request) -> bool {
        fn empty_anywhere(value: &serde_json::Value) -> bool {
            match value {
                serde_json::Value::Null => true,
                serde_json::Value::Array(items) => {
                    items.is_empty() || items.iter().any(empty_anywhere)
                }
                serde_json::Value::Object(map) => map.values().any(empty_anywhere),
                _ => false,
            }
        }
        serde_json::from_slice(&request.body).is_ok_and(|body| !empty_anywhere(&body))
    }
}

/// What the client logs, at debug and above.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Graph's answer to a request it failed: a 500 with its error and request id.
fn graph_500() -> ResponseTemplate {
    ResponseTemplate::new(500)
        .insert_header("request-id", "graph-request-7")
        .set_body_json(json!({"error": {
            "code": "ErrorInternalServerError",
            "message": "An internal server error occurred. The operation failed.",
        }}))
}

#[tokio::test]
async fn a_create_isnt_repeated_after_a_server_error_and_the_error_is_logged() {
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let server = MockServer::start().await;
    // A 500 may come after the contact was created: sending it again could create it
    // twice, so the client gives up (sync tries later; the user sees the error).
    Mock::given(method("POST"))
        .and(path("/v1.0/me/contacts"))
        .and(header("return-client-request-id", "true"))
        .respond_with(graph_500())
        .expect(1)
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let result = source.create(&fields("Anna", "anna@example.com")).await;
    assert!(
        matches!(result, Err(ContactError::Server { status: 500 })),
        "{result:?}"
    );
    let sent = &server.received_requests().await.unwrap()[0];
    let client_request_id = sent
        .headers
        .get("client-request-id")
        .unwrap()
        .to_str()
        .unwrap();
    let log = logs.text();
    for expected in [
        "contacts request failed",
        "Microsoft Graph",
        "POST",
        "v1.0/me/contacts",
        "500",
        "ErrorInternalServerError",
        "An internal server error occurred",
        "graph-request-7",
        client_request_id,
    ] {
        assert!(
            log.contains(expected),
            "{expected:?} missing from the log:\n{log}"
        );
    }
    assert!(
        !log.contains("anna@example.com"),
        "contact data in the log:\n{log}"
    );
}

#[tokio::test]
async fn an_update_retries_a_server_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts/C1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(contact(
            "C1",
            "Anna",
            "anna@example.com",
        )))
        .mount(&server)
        .await;
    // The first PATCH fails, the second goes through: If-Match keeps a repeat from
    // applying twice, so an update may try again.
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/contacts/C1"))
        .respond_with(graph_500())
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/contacts/C1"))
        .and(header("if-match", "W/\"etag-C1\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(contact(
            "C1",
            "Anna Example",
            "anna@example.com",
        )))
        .expect(1)
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let updated = source
        .update("C1", "W/\"etag-C1\"", &fields("Anna", "anna@example.com"))
        .await
        .unwrap();
    assert_eq!(updated.resource_name, "C1");
}

#[tokio::test]
async fn creating_a_contact_returns_the_servers_version() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/contacts"))
        .and(body_partial_json(json!({
            "givenName": "Anna",
            "surname": "Example",
            "displayName": "Anna Example",
            "emailAddresses": [{"address": "anna@example.com"}],
            "mobilePhone": "+43 660 1",
        })))
        .and(NothingEmpty)
        .respond_with(ResponseTemplate::new(201).set_body_json(contact(
            "NEW1",
            "Anna Example",
            "anna@example.com",
        )))
        .expect(1)
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let person = source
        .create(&fields("Anna", "anna@example.com"))
        .await
        .unwrap();
    assert_eq!(person.resource_name, "NEW1");
    assert!(matches!(
        source.create(&ContactFields::default()).await,
        Err(ContactError::Empty)
    ));
}

#[tokio::test]
async fn an_update_sends_if_match_and_a_changed_contact_is_a_conflict() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts/C1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(contact(
            "C1",
            "Anna",
            "anna@example.com",
        )))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/contacts/C1"))
        .and(header("if-match", "W/\"etag-C1\""))
        .and(body_partial_json(
            json!({"givenName": "Anna", "personalNotes": null}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "@odata.etag": "W/\"etag-C1b\"",
            "id": "C1",
            "displayName": "Anna Example",
            "emailAddresses": [{"address": "anna@example.com"}],
        })))
        .expect(1)
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let updated = source
        .update("C1", "W/\"etag-C1\"", &fields("Anna", "anna@example.com"))
        .await
        .unwrap();
    assert_eq!(updated.etag.as_deref(), Some("W/\"etag-C1b\""));
    // Read at an older version: the current one comes back as a conflict, unwritten.
    match source
        .update("C1", "W/\"older\"", &fields("Anna", "anna@example.com"))
        .await
    {
        Err(ContactError::Conflict(current)) => {
            assert_eq!(current.etag.as_deref(), Some("W/\"etag-C1\""));
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert!(matches!(
        source
            .update("../me", "x", &fields("A", "a@example.com"))
            .await,
        Err(ContactError::NotFound)
    ));
}

#[tokio::test]
async fn graph_s_412_is_a_conflict_too() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts/C1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(contact(
            "C1",
            "Anna",
            "anna@example.com",
        )))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/contacts/C1"))
        .respond_with(ResponseTemplate::new(412).set_body_json(
            json!({"error": {"code": "ErrorIrresolvableConflict", "message": "changed"}}),
        ))
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let result = source
        .update("C1", "W/\"etag-C1\"", &fields("Anna", "anna@example.com"))
        .await;
    assert!(
        matches!(result, Err(ContactError::Conflict(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn missing_contacts_permission_is_its_own_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts"))
        .respond_with(ResponseTemplate::new(403).set_body_json(
            json!({"error": {"code": "ErrorAccessDenied", "message": "Access is denied."}}),
        ))
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let err = sync(&source, None).await.unwrap_err();
    assert!(matches!(err, ContactError::MissingScope), "{err:?}");
    assert!(err.needs_consent());
    // No contacts in the grant at all: the token manager says so before any request.
    let source = client(
        &server,
        Arc::new(Tokens {
            missing_scope: true,
            ..Tokens::default()
        }),
    );
    let err = sync(&source, None).await.unwrap_err();
    assert!(matches!(err, ContactError::MissingScope), "{err:?}");
}

#[tokio::test]
async fn a_rejected_token_is_refreshed_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts"))
        .and(header("authorization", "Bearer graph-0"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts"))
        .and(header("authorization", "Bearer graph-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": []})))
        .mount(&server)
        .await;
    let tokens = Arc::new(Tokens::default());
    let source = client(&server, Arc::clone(&tokens));
    sync(&source, None).await.unwrap();
    assert_eq!(tokens.invalidated.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn photos_need_the_token_and_the_right_account() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts/C1/photo/$value"))
        .and(header("authorization", "Bearer graph-0"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"\xff\xd8jpeg".to_vec(), "image/jpeg"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/contacts/C2/photo/$value"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"error": {"code": "ImageNotFound", "message": "none"}})),
        )
        .mount(&server)
        .await;
    let source = client(&server, Arc::default());
    let prefix = "microsoft-photo:6f000000-0000-4000-8000-000000000001";
    let photo = source
        .photo(&format!("{prefix}:C1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(photo.content_type, "image/jpeg");
    assert_eq!(source.photo(&format!("{prefix}:C2")).await.unwrap(), None);
    // Another account's photo, or not a Microsoft photo at all: nothing is fetched.
    let other = "microsoft-photo:6f000000-0000-4000-8000-000000000002:C1";
    assert_eq!(source.photo(other).await.unwrap(), None);
    assert_eq!(
        source.photo("https://example.com/p.jpg").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn a_shared_mailbox_is_listed_read_only_without_photos() {
    let server = MockServer::start().await;
    let delta = "/v1.0/users/team@contoso.com/contactFolders/TEAMFOLDER/contacts/delta";
    Mock::given(method("GET"))
        .and(path("/v1.0/users/team@contoso.com/contacts"))
        .and(query_param("$top", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"value": [{"id": "T1", "parentFolderId": "TEAMFOLDER"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(delta))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [contact("T1", "Support Desk", "support@contoso.com")],
            "@odata.deltaLink": format!("{}{delta}?$deltatoken=t1", server.uri()),
        })))
        .mount(&server)
        .await;
    let source = client(&server, Arc::default())
        .with_mailbox(" Team@Contoso.com ")
        .unwrap();
    let page = source
        .list(BookKind::MicrosoftShared, None, None)
        .await
        .unwrap();
    assert_eq!(page.upserts.len(), 1);
    assert_eq!(
        page.upserts[0].photo_url, None,
        "no photos for shared contacts"
    );
    assert!(page.next_sync_token.unwrap().ends_with("t1"));
    // Only the shared book, and nothing can be written to it.
    assert!(matches!(
        source.list(BookKind::Microsoft, None, None).await,
        Err(ContactError::ReadOnly)
    ));
    assert!(matches!(
        source.create(&fields("Anna", "anna@example.com")).await,
        Err(ContactError::ReadOnly)
    ));
    assert!(matches!(
        source
            .update("T1", "W/\"etag-T1\"", &fields("Anna", "a@example.com"))
            .await,
        Err(ContactError::ReadOnly)
    ));
    assert!(
        client(&server, Arc::default())
            .with_mailbox("../me")
            .is_err()
    );
}
