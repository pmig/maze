//! The OAuth flow end to end against a fake authorization and token server
//! (`wiremock`) and a fake browser that follows the redirect to the loopback listener.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers; tests may unwrap (AGENTS.md)"
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use auth::{
    AccountSecrets, AuthError, AuthorizeOptions, BrowserOpener, ClientCredentials,
    MemorySecretStore, OAuthClient, OAuthTokenManager, Provider, ProviderKind, ReauthReason,
    SecretKey, SecretKind, SecretStore, TokenProvider, Uuid,
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mazemail_types::Sensitive;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const ACCOUNT: Uuid = Uuid::from_u128(0x5eed);

fn form(request: &Request) -> HashMap<String, String> {
    url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect()
}

fn query(url: &Url) -> HashMap<String, String> {
    url.query_pairs().into_owned().collect()
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// What the fake authorization endpoint does with a request.
#[derive(Clone, Copy)]
enum Consent {
    /// Redirects back with a code and the right state.
    Grant,
    /// Redirects back with a code and a forged state.
    WrongState,
    /// Redirects back with an OAuth error.
    Error(&'static str, &'static str),
}

struct Fake {
    server: MockServer,
    /// The PKCE challenge from the last authorization request.
    challenge: Arc<Mutex<Option<String>>>,
}

impl Fake {
    async fn start(consent: Consent) -> Self {
        let server = MockServer::start().await;
        let challenge = Arc::new(Mutex::new(None));
        let seen = challenge.clone();
        Mock::given(method("GET"))
            .and(path("/authorize"))
            .respond_with(move |request: &Request| {
                let q = query(&request.url);
                assert_eq!(q["response_type"], "code");
                assert_eq!(q["code_challenge_method"], "S256");
                *seen.lock().unwrap() = Some(q["code_challenge"].clone());
                let redirect = &q["redirect_uri"];
                let location = match consent {
                    Consent::Grant => format!("{redirect}?code=the-code&state={}", q["state"]),
                    Consent::WrongState => format!("{redirect}?code=the-code&state=forged"),
                    Consent::Error(error, description) => {
                        let mut url = Url::parse(redirect).unwrap();
                        url.query_pairs_mut()
                            .append_pair("error", error)
                            .append_pair("error_description", description)
                            .append_pair("state", &q["state"]);
                        url.to_string()
                    }
                };
                ResponseTemplate::new(302).insert_header("Location", location.as_str())
            })
            .mount(&server)
            .await;
        Self { server, challenge }
    }

    fn provider(&self, kind: ProviderKind, credentials: ClientCredentials) -> Provider {
        Provider::builtin(kind)
            .with_endpoints(
                &format!("{}/authorize", self.server.uri()),
                &format!("{}/token", self.server.uri()),
            )
            .unwrap()
            .with_credentials(credentials)
    }

    fn client(&self, kind: ProviderKind, credentials: ClientCredentials) -> OAuthClient {
        OAuthClient::new(self.provider(kind, credentials))
            .unwrap()
            .with_http_client(http())
    }

    /// Mounts a token endpoint that checks the code exchange (incl. PKCE) and answers
    /// with `response`.
    async fn token_exchange(&self, response: ResponseTemplate, expected_calls: u64) {
        let challenge = self.challenge.clone();
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(move |request: &Request| {
                let f = form(request);
                assert_eq!(f["grant_type"], "authorization_code");
                assert_eq!(f["code"], "the-code");
                assert!(f["redirect_uri"].starts_with("http://127.0.0.1:"));
                let verifier = &f["code_verifier"];
                let expected = challenge.lock().unwrap().clone().unwrap();
                assert_eq!(
                    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
                    expected,
                    "the code_verifier must match the challenge"
                );
                response.clone()
            })
            .expect(expected_calls)
            .mount(&self.server)
            .await;
    }
}

/// A browser that follows the redirect chain in the background and reports the page
/// it ends up on.
struct FakeBrowser {
    page: Mutex<Option<oneshot::Sender<String>>>,
}

impl FakeBrowser {
    fn new() -> (Self, oneshot::Receiver<String>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                page: Mutex::new(Some(tx)),
            },
            rx,
        )
    }
}

impl BrowserOpener for FakeBrowser {
    fn open(&self, url: &Url) -> std::io::Result<()> {
        let url = url.clone();
        let page = self.page.lock().unwrap().take();
        tokio::spawn(async move {
            let browser = reqwest::Client::builder().no_proxy().build().unwrap();
            let body = browser.get(url).send().await.unwrap().text().await.unwrap();
            if let Some(page) = page {
                let _ = page.send(body);
            }
        });
        Ok(())
    }
}

fn token_json(access: &str, refresh: Option<&str>, expires_in: u64) -> ResponseTemplate {
    let mut body = serde_json::json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": expires_in,
    });
    if let Some(refresh) = refresh {
        body["refresh_token"] = refresh.into();
    }
    ResponseTemplate::new(200).set_body_json(body)
}

fn refresh_key() -> SecretKey {
    SecretKey::new(ACCOUNT, SecretKind::OAuthRefresh)
}

fn stored_refresh(store: &MemorySecretStore) -> Option<String> {
    store
        .load(&refresh_key())
        .unwrap()
        .map(Sensitive::into_inner)
}

#[tokio::test]
async fn full_loopback_flow_signs_in() {
    let fake = Fake::start(Consent::Grant).await;
    fake.token_exchange(token_json("access-1", Some("refresh-1"), 3600), 1)
        .await;
    let store = Arc::new(MemorySecretStore::new());
    let manager = OAuthTokenManager::new(
        fake.client(
            ProviderKind::Google,
            ClientCredentials::new("cid").with_secret("desktop-secret"),
        ),
        AccountSecrets::new(store.clone(), ACCOUNT),
    );

    let (browser, page) = FakeBrowser::new();
    manager
        .sign_in(
            &browser,
            &AuthorizeOptions::new().login_hint("bob@gmail.com"),
        )
        .await
        .unwrap();

    assert!(
        page.await
            .unwrap()
            .contains("You're signed in to Maze. You can close this tab.")
    );
    assert_eq!(stored_refresh(&store).as_deref(), Some("refresh-1"));
    assert!(manager.is_signed_in().await.unwrap());
    // Served from memory: the token endpoint is called exactly once.
    assert_eq!(manager.access_token().await.unwrap().expose(), "access-1");
    assert_eq!(manager.access_token().await.unwrap().expose(), "access-1");

    let requests = fake.server.received_requests().await.unwrap();
    let token_request = requests.iter().find(|r| r.url.path() == "/token").unwrap();
    let f = form(token_request);
    assert_eq!(f["client_id"], "cid");
    assert_eq!(f["client_secret"], "desktop-secret");
    let auth_request = requests
        .iter()
        .find(|r| r.url.path() == "/authorize")
        .unwrap();
    assert_eq!(query(&auth_request.url)["login_hint"], "bob@gmail.com");
}

#[tokio::test]
async fn public_client_sends_no_secret() {
    let fake = Fake::start(Consent::Grant).await;
    fake.token_exchange(token_json("access-1", Some("refresh-1"), 3600), 1)
        .await;
    let client = fake.client(ProviderKind::Microsoft, ClientCredentials::new("entra-app"));
    let (browser, _page) = FakeBrowser::new();
    let tokens = client
        .authorize(&browser, &AuthorizeOptions::new())
        .await
        .unwrap();
    assert_eq!(tokens.access_token.expose(), "access-1");
    assert_eq!(tokens.refresh_token.unwrap().expose(), "refresh-1");
    let requests = fake.server.received_requests().await.unwrap();
    let token_request = requests.iter().find(|r| r.url.path() == "/token").unwrap();
    assert!(!form(token_request).contains_key("client_secret"));
    let auth_request = requests
        .iter()
        .find(|r| r.url.path() == "/authorize")
        .unwrap();
    assert_eq!(
        query(&auth_request.url)["scope"],
        "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access"
    );
}

#[tokio::test]
async fn state_mismatch_is_rejected() {
    let fake = Fake::start(Consent::WrongState).await;
    fake.token_exchange(token_json("never", None, 3600), 0)
        .await;
    let client = fake.client(ProviderKind::Google, ClientCredentials::new("cid"));
    let (browser, page) = FakeBrowser::new();
    let err = client
        .authorize(&browser, &AuthorizeOptions::new())
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::StateMismatch), "{err:?}");
    let page = page.await.unwrap();
    assert!(page.contains("Maze couldn't sign you in"), "{page}");
}

#[tokio::test]
async fn times_out_without_a_redirect() {
    let fake = Fake::start(Consent::Grant).await;
    let client = fake.client(ProviderKind::Google, ClientCredentials::new("cid"));
    let idle = |_: &Url| Ok(());
    let err = client
        .authorize(
            &idle,
            &AuthorizeOptions::new().timeout(Duration::from_millis(200)),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::Timeout), "{err:?}");
}

#[tokio::test]
async fn browser_failure_is_reported() {
    let fake = Fake::start(Consent::Grant).await;
    let client = fake.client(ProviderKind::Google, ClientCredentials::new("cid"));
    let broken = |_: &Url| Err(std::io::Error::other("no browser"));
    let err = client
        .authorize(&broken, &AuthorizeOptions::new())
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::Browser(_)), "{err:?}");
}

#[tokio::test]
async fn user_cancel_is_access_denied() {
    let fake = Fake::start(Consent::Error("access_denied", "The user cancelled")).await;
    let client = fake.client(ProviderKind::Google, ClientCredentials::new("cid"));
    let (browser, page) = FakeBrowser::new();
    let err = client
        .authorize(&browser, &AuthorizeOptions::new())
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::AccessDenied(_)), "{err:?}");
    assert!(page.await.unwrap().contains("cancelled"));
}

#[tokio::test]
async fn admin_consent_in_the_redirect() {
    let fake = Fake::start(Consent::Error(
        "access_denied",
        "AADSTS90094: The grant requires admin permission.",
    ))
    .await;
    let client = fake.client(ProviderKind::Microsoft, ClientCredentials::new("entra-app"));
    let (browser, page) = FakeBrowser::new();
    let err = client
        .authorize(&browser, &AuthorizeOptions::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, AuthError::AdminConsentRequired { code: Some(90094) }),
        "{err:?}"
    );
    assert!(page.await.unwrap().contains("administrator"));
}

#[tokio::test]
async fn admin_consent_at_the_token_endpoint() {
    let fake = Fake::start(Consent::Grant).await;
    fake.token_exchange(
        ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "AADSTS65001: The user or administrator has not consented to use the application with ID 'x'.",
            "error_codes": [65001],
            "timestamp": "2026-10-22 10:00:00Z",
        })),
        1,
    )
    .await;
    let store = Arc::new(MemorySecretStore::new());
    let manager = OAuthTokenManager::new(
        fake.client(ProviderKind::Microsoft, ClientCredentials::new("entra-app")),
        AccountSecrets::new(store.clone(), ACCOUNT),
    );
    let (browser, page) = FakeBrowser::new();
    let err = manager
        .sign_in(&browser, &AuthorizeOptions::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, AuthError::AdminConsentRequired { code: Some(65001) }),
        "{err:?}"
    );
    assert!(!err.requires_reauth());
    assert!(page.await.unwrap().contains("administrator"));
    assert!(store.is_empty());
}

/// A manager with a stored refresh token and a token endpoint answering refreshes
/// with `responses` in order.
async fn refresh_setup(
    responses: Vec<ResponseTemplate>,
) -> (MockServer, Arc<MemorySecretStore>, OAuthTokenManager) {
    let fake = Fake::start(Consent::Grant).await;
    let responses = Arc::new(Mutex::new(responses.into_iter()));
    let expected = responses.lock().unwrap().len() as u64;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(move |request: &Request| {
            let f = form(request);
            assert_eq!(f["grant_type"], "refresh_token");
            assert!(f["refresh_token"].starts_with("refresh-"));
            assert_eq!(f["client_id"], "entra-app");
            responses
                .lock()
                .unwrap()
                .next()
                .expect("unexpected refresh")
        })
        .expect(expected)
        .mount(&fake.server)
        .await;
    let store = Arc::new(MemorySecretStore::new());
    store
        .store(&refresh_key(), &Sensitive::new("refresh-0".to_owned()))
        .unwrap();
    let manager = OAuthTokenManager::new(
        fake.client(ProviderKind::Microsoft, ClientCredentials::new("entra-app")),
        AccountSecrets::new(store.clone(), ACCOUNT),
    );
    (fake.server, store, manager)
}

#[tokio::test]
async fn refreshes_and_persists_the_rotated_refresh_token() {
    let (_server, store, manager) =
        refresh_setup(vec![token_json("access-1", Some("refresh-1"), 3600)]).await;
    assert_eq!(manager.access_token().await.unwrap().expose(), "access-1");
    assert_eq!(manager.access_token().await.unwrap().expose(), "access-1");
    assert_eq!(stored_refresh(&store).as_deref(), Some("refresh-1"));
}

#[tokio::test]
async fn keeps_the_refresh_token_when_none_is_returned() {
    let (_server, store, manager) = refresh_setup(vec![token_json("access-1", None, 3600)]).await;
    assert_eq!(manager.access_token().await.unwrap().expose(), "access-1");
    assert_eq!(stored_refresh(&store).as_deref(), Some("refresh-0"));
}

#[tokio::test]
async fn refreshes_tokens_that_expire_within_the_margin() {
    // 30 s left is inside the 60 s margin, so every call refreshes.
    let (_server, _store, manager) = refresh_setup(vec![
        token_json("access-1", None, 30),
        token_json("access-2", None, 30),
    ])
    .await;
    assert_eq!(manager.access_token().await.unwrap().expose(), "access-1");
    assert_eq!(manager.access_token().await.unwrap().expose(), "access-2");
}

#[tokio::test]
async fn invalidate_forces_a_refresh() {
    let (_server, _store, manager) = refresh_setup(vec![
        token_json("access-1", None, 3600),
        token_json("access-2", None, 3600),
    ])
    .await;
    assert_eq!(manager.access_token().await.unwrap().expose(), "access-1");
    manager.invalidate().await;
    assert_eq!(manager.access_token().await.unwrap().expose(), "access-2");
}

#[tokio::test]
async fn concurrent_callers_share_one_refresh() {
    let (_server, _store, manager) = refresh_setup(vec![token_json("access-1", None, 3600)]).await;
    let manager = Arc::new(manager);
    let calls: Vec<_> = (0..8)
        .map(|_| {
            let manager = manager.clone();
            tokio::spawn(async move { manager.access_token().await })
        })
        .collect();
    for call in calls {
        assert_eq!(call.await.unwrap().unwrap().expose(), "access-1");
    }
}

#[tokio::test]
async fn invalid_grant_requires_sign_in_and_drops_the_refresh_token() {
    let (_server, store, manager) =
        refresh_setup(vec![
        ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "AADSTS70008: The refresh token has expired due to inactivity.",
            "error_codes": [70008],
        })),
    ])
        .await;
    let err = manager.access_token().await.unwrap_err();
    assert!(
        matches!(
            err,
            AuthError::ReauthRequired {
                reason: ReauthReason::TokenRejected
            }
        ),
        "{err:?}"
    );
    assert!(err.requires_reauth());
    assert_eq!(stored_refresh(&store), None);

    // Fails fast without another request (the mock expects exactly one).
    let err = manager.access_token().await.unwrap_err();
    assert!(matches!(
        err,
        AuthError::ReauthRequired {
            reason: ReauthReason::NotSignedIn
        }
    ));
}

#[tokio::test]
async fn transient_failure_keeps_a_still_valid_token() {
    let (_server, store, manager) = refresh_setup(vec![
        token_json("access-1", None, 3600),
        ResponseTemplate::new(503),
    ])
    .await;
    // A margin longer than the lifetime makes every call try to refresh.
    let manager = manager.with_refresh_margin(Duration::from_secs(7200));
    assert_eq!(manager.access_token().await.unwrap().expose(), "access-1");
    assert_eq!(manager.access_token().await.unwrap().expose(), "access-1");
    assert_eq!(stored_refresh(&store).as_deref(), Some("refresh-0"));
}

#[tokio::test]
async fn server_errors_are_transient() {
    let (_server, store, manager) = refresh_setup(vec![ResponseTemplate::new(503)]).await;
    let err = manager.access_token().await.unwrap_err();
    assert!(matches!(err, AuthError::Http { status: 503 }), "{err:?}");
    assert!(err.is_transient());
    assert_eq!(stored_refresh(&store).as_deref(), Some("refresh-0"));
}

#[tokio::test]
async fn network_errors_are_transient() {
    // A port nobody listens on (wiremock pools its servers, so a dropped one still answers).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let provider = Provider::builtin(ProviderKind::Microsoft)
        .with_endpoints(&format!("{uri}/authorize"), &format!("{uri}/token"))
        .unwrap()
        .with_credentials(ClientCredentials::new("entra-app"));
    let client = OAuthClient::new(provider).unwrap().with_http_client(http());
    let err = client
        .refresh(&Sensitive::new("refresh-0".to_owned()))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::Network { .. }), "{err:?}");
    assert!(err.is_transient());
}

#[tokio::test]
async fn not_signed_in_requires_sign_in() {
    let fake = Fake::start(Consent::Grant).await;
    let manager = OAuthTokenManager::new(
        fake.client(ProviderKind::Google, ClientCredentials::new("cid")),
        AccountSecrets::new(Arc::new(MemorySecretStore::new()), ACCOUNT),
    );
    let err = manager.access_token().await.unwrap_err();
    assert!(matches!(
        err,
        AuthError::ReauthRequired {
            reason: ReauthReason::NotSignedIn
        }
    ));
}

#[tokio::test]
async fn sign_out_forgets_everything() {
    let (_server, store, manager) = refresh_setup(vec![token_json("access-1", None, 3600)]).await;
    assert_eq!(manager.access_token().await.unwrap().expose(), "access-1");
    manager.sign_out().await.unwrap();
    assert!(store.is_empty());
    assert!(!manager.is_signed_in().await.unwrap());
    assert!(manager.access_token().await.unwrap_err().requires_reauth());
}
