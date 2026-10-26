//! Remote content of the original HTML mode (ADR-020): the policy per account, "Load
//! images" for one message, "Always for this sender", and inline parts that are always
//! served.

use std::sync::Arc;

use mazemail_core::CoreHandle;
use mazemail_types::MessageId;
use settings::RemoteContentPolicy;
use vault::MemoryKeyStore;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{self, Place};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake";

/// A message from `from` with a remote image on `server` and an inline image.
fn message(id: &str, from: &str, server: &MockServer) -> Vec<u8> {
    format!(
        "Message-ID: <{id}>\r\nDate: Tue, 14 Oct 2025 09:30:00 +0000\r\n\
         From: {from}\r\nTo: me@work.example\r\nSubject: Sale\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/related; boundary=b\r\n\r\n\
         --b\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
         <p>Sale</p><img src=\"{}/hero.png\" width=\"600\" height=\"200\">\
         <img src=\"cid:logo@shop\" width=\"40\" height=\"40\">\r\n\
         --b\r\nContent-Type: image/png\r\nContent-ID: <logo@shop>\r\n\
         Content-Transfer-Encoding: base64\r\n\r\niVBORw0KGgo=\r\n--b--\r\n",
        server.uri()
    )
    .into_bytes()
}

struct Setup {
    core: CoreHandle,
    work: MessageId,
    gmail: MessageId,
    _dir: tempfile::TempDir,
}

/// Work uses `work_policy`; Gmail the default (`ask`).
async fn setup(work_policy: &str, server: &MockServer) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    std::fs::write(
        &settings,
        format!(r#"{{ "accounts": {{ "Work": {{ "remote_content": "{work_policy}" }} }} }}"#),
    )
    .unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let config = common::config(&dir.path().join("data"), &keys).with_settings_file(&settings);
    let core = CoreHandle::open(config).await.unwrap();
    let f = common::fixture(&core).await;
    let (work, _) = common::ingest(
        &core,
        Place::new(f.work, f.inbox, 1),
        message("w1@shop.example", "Shop <news@shop.example>", server),
    )
    .await;
    let (gmail, _) = common::ingest(
        &core,
        Place::new(f.gmail, f.g_inbox, 2),
        message("g1@shop.example", "Shop <news@shop.example>", server),
    )
    .await;
    Setup {
        core,
        work,
        gmail,
        _dir: dir,
    }
}

async fn image_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/hero.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(PNG),
        )
        .mount(&server)
        .await;
    server
}

fn hero(server: &MockServer) -> String {
    format!("{}/hero.png", server.uri())
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_blocks_until_load_images_or_the_sender_is_allowed() {
    let server = image_server().await;
    let t = setup("ask", &server).await;
    let decision = t.core.remote_content(t.work).await.unwrap();
    assert_eq!(decision.policy, RemoteContentPolicy::Ask);
    assert!(decision.blocked, "the banner shows");
    // Nothing is fetched while blocked.
    assert!(
        t.core
            .remote_resource(t.work, hero(&server), false)
            .await
            .unwrap()
            .is_none()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    // "Load images": this message, this time.
    let image = t
        .core
        .remote_resource(t.work, hero(&server), true)
        .await
        .unwrap()
        .expect("fetched by Maze");
    assert_eq!(image.content_type, "image/png");
    assert_eq!(image.bytes, PNG);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].headers.get("referer").is_none());
    assert!(requests[0].headers.get("cookie").is_none());
    assert!(
        t.core.remote_content(t.work).await.unwrap().blocked,
        "not kept"
    );
    // URLs the message doesn't reference are never fetched.
    assert!(
        t.core
            .remote_resource(t.work, format!("{}/other.png", server.uri()), true)
            .await
            .unwrap()
            .is_none()
    );

    // "Always for this sender" is kept and applies to the sender's other mail.
    t.core
        .allow_remote_content("News@Shop.example".to_owned())
        .await
        .unwrap();
    for id in [t.work, t.gmail] {
        let decision = t.core.remote_content(id).await.unwrap();
        assert!(decision.sender_allowed);
        assert!(!decision.blocked);
        assert!(
            t.core
                .remote_resource(id, hero(&server), false)
                .await
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn never_fetches_even_for_allowed_senders() {
    let server = image_server().await;
    let t = setup("never", &server).await;
    t.core
        .allow_remote_content("news@shop.example".to_owned())
        .await
        .unwrap();
    let work = t.core.remote_content(t.work).await.unwrap();
    assert_eq!(work.policy, RemoteContentPolicy::Never);
    assert!(work.blocked);
    for once in [false, true] {
        assert!(
            t.core
                .remote_resource(t.work, hero(&server), once)
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    // The override is per account: Gmail asks, and the sender is allowed there.
    let gmail = t.core.remote_content(t.gmail).await.unwrap();
    assert_eq!(gmail.policy, RemoteContentPolicy::Ask);
    assert!(!gmail.blocked);
}

#[tokio::test(flavor = "multi_thread")]
async fn always_fetches_and_inline_parts_are_always_served() {
    let server = image_server().await;
    let t = setup("always", &server).await;
    assert!(!t.core.remote_content(t.work).await.unwrap().blocked);
    assert!(
        t.core
            .remote_resource(t.work, hero(&server), false)
            .await
            .unwrap()
            .is_some()
    );
    // Inline parts are part of the message: served under every policy.
    for id in [t.work, t.gmail] {
        let part = t
            .core
            .inline_part(id, "logo@shop".to_owned())
            .await
            .unwrap()
            .expect("the inline image");
        assert_eq!(part.content_type, "image/png");
        assert!(part.bytes.starts_with(b"\x89PNG"));
    }
    assert!(
        t.core
            .inline_part(t.work, "missing@shop".to_owned())
            .await
            .unwrap()
            .is_none()
    );
}
