//! The outbox sender against a real (temporary) store and a fake transport: one send
//! per message, the send log before the outbox row, recovery after a crash at either
//! side of the server's acceptance, retries and permanent failures.

#![allow(clippy::unwrap_used)] // Tests may unwrap (AGENTS.md).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mazemail_types::{IdentityId, Sensitive};
use pretty_assertions::assert_eq;
use smtp::{
    Credentials, Delivery, MAX_SEND_ATTEMPTS, Mailer, Outbox, OutboxEnvelope, Route, Routes,
    Security, SentHook, SmtpConfig, SmtpError,
};
use store::{OutboxId, OutboxItem, OutboxState, Queries as _, Store, StoreConfig, WriteTx};
use vault::{KEY_LEN, KeyId, MasterKey, Vault};

/// A transport that records what it was asked to send and answers as told.
#[derive(Debug, Default)]
struct FakeMailer {
    sent: Mutex<Vec<(OutboxEnvelope, Vec<u8>)>>,
    calls: AtomicUsize,
    fail_with: Mutex<Option<SmtpError>>,
}

impl FakeMailer {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn fail_with(&self, error: Option<SmtpError>) {
        *self.fail_with.lock().unwrap() = error;
    }
}

#[async_trait]
impl Mailer for FakeMailer {
    async fn send(
        &self,
        _config: &SmtpConfig,
        _credentials: &Credentials,
        envelope: &OutboxEnvelope,
        raw: &[u8],
    ) -> Result<(), SmtpError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.fail_with.lock().unwrap().clone() {
            return Err(error);
        }
        self.sent
            .lock()
            .unwrap()
            .push((envelope.clone(), raw.to_vec()));
        Ok(())
    }

    async fn check(&self, _: &SmtpConfig, _: &Credentials) -> Result<(), SmtpError> {
        Ok(())
    }
}

/// Every identity sends through the same server; counts rejected logins.
#[derive(Debug, Default)]
struct OneRoute {
    rejected: AtomicUsize,
}

#[async_trait]
impl Routes for OneRoute {
    async fn route(&self, _identity: Option<IdentityId>) -> Result<Route, SmtpError> {
        Ok(Route {
            config: SmtpConfig::new("smtp.example.org", Security::StartTls),
            credentials: Credentials::Password {
                user: "alice".into(),
                password: Sensitive::new("secret".into()),
            },
        })
    }

    async fn rejected(&self, _identity: Option<IdentityId>, _credentials: &Credentials) {
        self.rejected.fetch_add(1, Ordering::SeqCst);
    }
}

/// Counts deliveries in the database, in the transaction that marks the item sent.
#[derive(Debug, Default)]
struct CountingHook;

impl SentHook for CountingHook {
    fn on_sent(&self, w: &mut WriteTx<'_>, item: &OutboxItem) -> store::Result<()> {
        // A tag row stands in for "queue the Sent copy": it commits with the outbox row.
        w.create_tag(&format!("sent-{}", item.rfc_message_id), "sent", None, 0)?;
        Ok(())
    }
}

fn hook_runs(store: &Store, rfc: &str) -> usize {
    store
        .reader()
        .unwrap()
        .tags()
        .unwrap()
        .iter()
        .filter(|t| t.keyword == format!("sent-{rfc}"))
        .count()
}

struct Setup {
    store: Store,
    mailer: Arc<FakeMailer>,
    outbox: Outbox,
    _dir: tempfile::TempDir,
}

impl Drop for Setup {
    /// Stops the store's writer and checkpoints before the test's runtime goes away, so
    /// no SQLCipher work is left running at process exit.
    fn drop(&mut self) {
        let store = self.store.clone();
        let closed = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.block_on(store.close())
        })
        .join();
        assert!(matches!(closed, Ok(Ok(()))), "closing the store");
    }
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let mut config = StoreConfig::new(dir.path());
    config.gc_interval = None;
    let vault = Vault::from_master_key(MasterKey::from_bytes(KeyId::FIRST, [5; KEY_LEN]));
    let store = Store::open(&config, &vault).unwrap();
    let mailer = Arc::new(FakeMailer::default());
    let outbox = Outbox::new(store.clone(), mailer.clone());
    Setup {
        store,
        mailer,
        outbox,
        _dir: dir,
    }
}

fn raw(rfc: &str) -> Vec<u8> {
    format!("Message-ID: <{rfc}>\r\nSubject: hi\r\n\r\nHello\r\n").into_bytes()
}

async fn enqueue(store: &Store, rfc: &str) -> OutboxId {
    let blob = store.blobs().put(&raw(rfc)).unwrap();
    let rfc = rfc.to_owned();
    store
        .write(move |w| {
            w.outbox_enqueue(
                None,
                &blob,
                &rfc,
                0,
                Some(&OutboxEnvelope {
                    from: "alice@example.org".into(),
                    to: vec!["bob@example.org".into(), "bcc@example.net".into()],
                }),
            )
        })
        .await
        .unwrap()
}

fn item(store: &Store, id: OutboxId) -> OutboxItem {
    store.reader().unwrap().outbox_item(id).unwrap().unwrap()
}

fn hook() -> Arc<dyn SentHook> {
    Arc::new(CountingHook)
}

#[tokio::test]
async fn a_message_is_sent_once_and_recorded() {
    let s = setup();
    let id = enqueue(&s.store, "one@example.org").await;
    let routes = OneRoute::default();

    let delivery = s.outbox.send(id, &routes, hook()).await.unwrap();
    assert_eq!(delivery, Delivery::Sent);
    assert_eq!(s.mailer.calls(), 1);
    let sent = s.mailer.sent.lock().unwrap().clone();
    assert_eq!(sent[0].0.to.len(), 2);
    assert_eq!(sent[0].1, raw("one@example.org"));
    let receipt = s
        .store
        .sendlog()
        .was_sent("one@example.org")
        .unwrap()
        .unwrap();
    assert_eq!(receipt.outbox, Some(id));
    let row = item(&s.store, id);
    assert_eq!((row.state, row.attempts), (OutboxState::Sent, 1));
    assert_eq!(hook_runs(&s.store, "one@example.org"), 1);

    // Again: nothing is sent, the hook doesn't run twice.
    assert_eq!(
        s.outbox.send(id, &routes, hook()).await.unwrap(),
        Delivery::Skipped
    );
    assert!(s.outbox.send_due(&routes, hook()).await.unwrap().is_empty());
    assert_eq!(s.mailer.calls(), 1);
    assert_eq!(hook_runs(&s.store, "one@example.org"), 1);
}

#[tokio::test]
async fn a_crash_after_the_send_log_never_sends_twice() {
    let s = setup();
    let id = enqueue(&s.store, "crash@example.org").await;
    // The attempt: claimed, the server accepted it, the send log has it ... and Maze
    // died before the outbox row was updated.
    assert!(s.store.write(move |w| w.outbox_claim(id)).await.unwrap());
    s.store
        .sendlog()
        .record("crash@example.org", None, Some(id))
        .unwrap();
    assert_eq!(item(&s.store, id).state, OutboxState::Sending);

    // Next start.
    let outbox = Outbox::new(s.store.clone(), s.mailer.clone());
    let recovery = outbox.recover(hook()).await.unwrap();
    assert_eq!((recovery.marked_sent, recovery.requeued), (1, 0));
    assert_eq!(item(&s.store, id).state, OutboxState::Sent);
    assert_eq!(hook_runs(&s.store, "crash@example.org"), 1);
    assert!(
        outbox
            .send_due(&OneRoute::default(), hook())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(s.mailer.calls(), 0);
}

#[tokio::test]
async fn a_queued_item_found_in_the_send_log_is_marked_sent() {
    // After a rollback restored an older database, say.
    let s = setup();
    let id = enqueue(&s.store, "old@example.org").await;
    s.store
        .sendlog()
        .record("old@example.org", None, Some(id))
        .unwrap();
    let delivery = s
        .outbox
        .send(id, &OneRoute::default(), hook())
        .await
        .unwrap();
    assert_eq!(delivery, Delivery::AlreadySent);
    assert_eq!(s.mailer.calls(), 0);
    assert_eq!(item(&s.store, id).state, OutboxState::Sent);
    assert_eq!(hook_runs(&s.store, "old@example.org"), 1);
}

#[tokio::test]
async fn a_crash_before_acceptance_sends_exactly_once_after_restart() {
    let s = setup();
    let id = enqueue(&s.store, "early@example.org").await;
    assert!(s.store.write(move |w| w.outbox_claim(id)).await.unwrap());

    let recovery = s.outbox.recover(hook()).await.unwrap();
    assert_eq!((recovery.marked_sent, recovery.requeued), (0, 1));
    let row = item(&s.store, id);
    assert_eq!(row.state, OutboxState::Queued);

    let routes = OneRoute::default();
    let sent = s.outbox.send_due(&routes, hook()).await.unwrap();
    assert_eq!(sent, vec![(id, Delivery::Sent)]);
    assert!(s.outbox.send_due(&routes, hook()).await.unwrap().is_empty());
    assert_eq!(s.mailer.calls(), 1);
}

#[tokio::test]
async fn temporary_failures_retry_later_and_give_up_eventually() {
    let s = setup();
    let id = enqueue(&s.store, "flaky@example.org").await;
    let routes = OneRoute::default();
    s.mailer.fail_with(Some(SmtpError::Connection {
        message: "connection refused".into(),
    }));

    let delivery = s.outbox.send(id, &routes, hook()).await.unwrap();
    let Delivery::Retrying { next_attempt, .. } = delivery else {
        panic!("not retrying: {delivery:?}");
    };
    let row = item(&s.store, id);
    assert_eq!((row.state, row.attempts), (OutboxState::Queued, 1));
    assert_eq!(row.send_at, next_attempt);
    assert!(row.last_error.unwrap().contains("connection refused"));
    // Not due yet.
    assert!(s.outbox.send_due(&routes, hook()).await.unwrap().is_empty());

    // Further attempts (made due by hand) until the limit.
    for attempt in 2..=MAX_SEND_ATTEMPTS {
        s.store
            .write(move |w| w.outbox_reschedule(id, 0))
            .await
            .unwrap();
        let delivery = s.outbox.send(id, &routes, hook()).await.unwrap();
        if attempt < MAX_SEND_ATTEMPTS {
            assert!(
                matches!(delivery, Delivery::Retrying { .. }),
                "{delivery:?}"
            );
        } else {
            assert!(matches!(delivery, Delivery::Failed(_)), "{delivery:?}");
        }
    }
    let row = item(&s.store, id);
    assert_eq!(
        (row.state, row.attempts),
        (OutboxState::Failed, MAX_SEND_ATTEMPTS)
    );
    assert!(
        s.store
            .sendlog()
            .was_sent("flaky@example.org")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn permanent_failures_are_not_retried() {
    let s = setup();
    let id = enqueue(&s.store, "denied@example.org").await;
    let routes = OneRoute::default();
    s.mailer.fail_with(Some(SmtpError::from_reply(
        554,
        "5.2.252 SendAsDenied; not allowed to send as you@distr.example",
    )));
    let delivery = s.outbox.send(id, &routes, hook()).await.unwrap();
    assert!(matches!(
        delivery,
        Delivery::Failed(SmtpError::SendAsDenied { .. })
    ));
    let row = item(&s.store, id);
    assert_eq!(row.state, OutboxState::Failed);
    assert!(row.last_error.unwrap().contains("Send As"));
    assert_eq!(hook_runs(&s.store, "denied@example.org"), 0);

    // A refused login also tells the routes (to drop a cached OAuth token).
    let id = enqueue(&s.store, "auth@example.org").await;
    s.mailer.fail_with(Some(SmtpError::AuthFailed {
        message: "535 5.7.8 rejected".into(),
    }));
    let delivery = s.outbox.send(id, &routes, hook()).await.unwrap();
    assert!(matches!(
        delivery,
        Delivery::Failed(SmtpError::AuthFailed { .. })
    ));
    assert_eq!(routes.rejected.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn items_without_an_envelope_fail_and_sent_items_are_pruned() {
    let s = setup();
    let blob = s.store.blobs().put(&raw("bare@example.org")).unwrap();
    let id = s
        .store
        .write(move |w| w.outbox_enqueue(None, &blob, "bare@example.org", 0, None))
        .await
        .unwrap();
    let delivery = s
        .outbox
        .send(id, &OneRoute::default(), hook())
        .await
        .unwrap();
    assert!(matches!(delivery, Delivery::Failed(SmtpError::Invalid(_))));
    assert_eq!(s.mailer.calls(), 0);

    let id = enqueue(&s.store, "prune@example.org").await;
    s.outbox
        .send(id, &OneRoute::default(), hook())
        .await
        .unwrap();
    // Too recent for a one-hour cutoff, gone with a zero one.
    assert_eq!(
        s.outbox
            .prune(std::time::Duration::from_secs(3600))
            .await
            .unwrap(),
        0
    );
    // `created` is in whole seconds; wait so it is strictly older than "now".
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(s.outbox.prune(std::time::Duration::ZERO).await.unwrap(), 1);
    assert!(s.store.reader().unwrap().outbox_item(id).unwrap().is_none());
}
