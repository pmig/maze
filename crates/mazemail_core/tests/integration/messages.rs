//! Opening messages: rendering and the reader cache, missing bodies, verification,
//! phishing warnings, remote content.

use std::time::{Duration, Instant};

use mazemail_core::{AuthStatus, Body, FetchPriority, RENDERER_VERSION};
use security::phishing::WarningKind;
use settings::RemoteContentPolicy;
use store::{BodyState, Queries};

use crate::common::{self, Place};

#[tokio::test(flavor = "multi_thread")]
async fn a_message_renders_once_and_opens_from_the_cache() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let (id, thread) = common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 1),
        common::html_message("news@example.com", "Weekly report", 400),
    )
    .await;

    let started = Instant::now();
    let first = t.core.message(id).await.unwrap();
    let first_time = started.elapsed();
    assert_eq!(first.thread, thread);
    assert_eq!(first.subject.as_deref(), Some("Weekly report"));
    let body = first.rendered().expect("body is stored");
    assert!(!body.reader.blocks.is_empty());
    assert!(body.sanitized_html.is_some());
    assert!(first.remote_content.has_remote_resources);
    assert_eq!(first.remote_content.trackers_removed, 1);
    assert_eq!(first.remote_content.policy, RemoteContentPolicy::Ask);
    assert!(first.remote_content.blocked);
    assert!(body.attachments.is_empty());

    let blob = t
        .core
        .store()
        .reader()
        .unwrap()
        .message(id)
        .unwrap()
        .unwrap()
        .blob
        .unwrap();
    assert!(
        t.core
            .store()
            .blobs()
            .cache_get(&blob, "reader", RENDERER_VERSION)
            .is_some(),
        "the rendered body is cached"
    );

    let mut cached_times = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        let again = t.core.message(id).await.unwrap();
        cached_times.push(started.elapsed());
        assert_eq!(again.body, first.body);
    }
    cached_times.sort();
    let cached = cached_times[cached_times.len() / 2];
    eprintln!("open message: first {first_time:?}, cached (median of 5) {cached:?}");
    // CONCEPT §3: a cached message opens in < 30 ms (met even in debug builds).
    assert!(
        cached < Duration::from_millis(30),
        "cached open took {cached:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_body_is_requested_with_priority() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let raw = common::raw_message(
        "later@example.com",
        "anna@example.com",
        "me@work.example",
        "Body comes later",
        "Mon, 6 Oct 2025 10:00:00 +0000",
        "Here it is",
    );
    let (id, _) = common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 1).headers_only(),
        raw.clone(),
    )
    .await;
    let view = t.core.message(id).await.unwrap();
    assert_eq!(view.body, Body::Pending);
    assert_eq!(view.auth, AuthStatus::Pending);
    assert_eq!(view.subject.as_deref(), Some("Body comes later"));
    assert_eq!(
        *t.sync.fetches.lock().unwrap(),
        vec![(id, FetchPriority::Interactive)]
    );

    // Sync delivers the body.
    let staged = t.core.store().blobs().put(&raw).unwrap();
    t.core
        .store()
        .write(move |w| w.set_message_body(id, &staged, BodyState::Full))
        .await
        .unwrap();
    let view = t.core.message(id).await.unwrap();
    let text = view.rendered().unwrap().reader.plain_text();
    assert!(text.contains("Here it is"), "{text:?}");
    assert_eq!(t.sync.fetches.lock().unwrap().len(), 1);

    // A blob file lost in a rollback resets the body and asks again.
    let blob = t
        .core
        .store()
        .reader()
        .unwrap()
        .message(id)
        .unwrap()
        .unwrap()
        .blob
        .unwrap();
    std::fs::remove_file(t.core.store().blobs().path(&blob)).unwrap();
    t.core.store().blobs().cache_clear().unwrap();
    let view = t.core.message(id).await.unwrap();
    assert_eq!(view.body, Body::Pending);
    let detail = t
        .core
        .store()
        .reader()
        .unwrap()
        .message(id)
        .unwrap()
        .unwrap();
    assert_eq!(detail.body_state, BodyState::None);
    assert_eq!(t.sync.fetches.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn sender_verification_runs_lazily_and_is_persisted() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let raw = |id: &str| {
        common::raw_message(
            id,
            "anna@example.com",
            "me@work.example",
            "Unsigned",
            "Mon, 6 Oct 2025 10:00:00 +0000",
            "No DKIM here",
        )
    };
    let (with_uid, _) = common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 1),
        raw("a@example.com"),
    )
    .await;
    let view = t.core.message(with_uid).await.unwrap();
    assert_eq!(view.auth, AuthStatus::Pending);
    // The background check writes `message.auth_json` (and announces the change).
    let json = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let detail = t
                .core
                .store()
                .reader()
                .unwrap()
                .message(with_uid)
                .unwrap()
                .unwrap();
            if let Some(json) = detail.auth_json {
                return json;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("report persisted in the database");
    let report: security::AuthReport = serde_json::from_str(&json).unwrap();
    assert_eq!(report.verdict, security::Verdict::Unverified);
    let view = t.core.message(with_uid).await.unwrap();
    assert!(
        matches!(view.auth, AuthStatus::Ready(ref r) if r.verdict == security::Verdict::Unverified)
    );

    // A message without a UID (a local copy) gets its report in the database too.
    let (local, _) = common::ingest(
        &t.core,
        Place::new(f.work, f.projects, 0).no_uid(),
        raw("b@example.com"),
    )
    .await;
    let status = t.core.verify_auth(local).await.unwrap();
    assert!(matches!(status, AuthStatus::Ready(_)));
    let detail = t
        .core
        .store()
        .reader()
        .unwrap()
        .message(local)
        .unwrap()
        .unwrap();
    assert!(detail.auth_json.is_some());
    let view = t.core.message(local).await.unwrap();
    assert!(matches!(view.auth, AuthStatus::Ready(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn phishing_links_are_flagged() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let raw = "Message-ID: <phish@example.com>\r\nDate: Mon, 6 Oct 2025 10:00:00 +0000\r\n\
               From: Support <support@evil.example>\r\nTo: me@work.example\r\n\
               Subject: Verify your account\r\nMIME-Version: 1.0\r\n\
               Content-Type: text/html; charset=utf-8\r\n\r\n\
               <p>Please log in: <a href=\"https://evil.example/login\">https://www.paypal.com/</a></p>\r\n";
    let (id, _) = common::ingest(
        &t.core,
        Place::new(f.work, f.inbox, 1),
        raw.as_bytes().to_vec(),
    )
    .await;
    let view = t.core.message(id).await.unwrap();
    assert!(
        view.warnings
            .iter()
            .any(|w| matches!(w.kind, WarningKind::LinkTextMismatch { .. })),
        "{:?}",
        view.warnings
    );
    assert!(!view.remote_content.has_remote_resources);
    assert!(!view.remote_content.blocked);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_message_is_not_found() {
    let t = common::open().await;
    let missing = mazemail_types::MessageId(4242);
    assert!(matches!(
        t.core.message(missing).await,
        Err(mazemail_core::Error::NotFound {
            entity: "message",
            ..
        })
    ));
}
