//! The outbox sender (PLAN §4.5): takes queued items, submits them through their
//! identity's server and records every acceptance in the send log before the outbox
//! row, so a crash between the server's `250` and the outbox update never sends a
//! message twice.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use mazemail_types::IdentityId;
use store::{OutboxId, OutboxItem, OutboxState, Queries as _, Store, UnixTime, WriteTx};

use crate::config::{Credentials, SmtpConfig};
use crate::error::SmtpError;
use crate::mailer::Mailer;

/// After this many attempts a temporarily failing item is marked failed.
pub const MAX_SEND_ATTEMPTS: u32 = 5;

/// Where and how one identity sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The submission server.
    pub config: SmtpConfig,
    /// The login.
    pub credentials: Credentials,
}

/// Resolves an identity's [`Route`] (its SMTP server and the account's credentials).
#[async_trait]
pub trait Routes: Send + Sync {
    /// The route for `identity` (`None`: an item queued without one).
    async fn route(&self, identity: Option<IdentityId>) -> Result<Route, SmtpError>;

    /// The server rejected `credentials` (an expired OAuth token, say): drop cached
    /// tokens so the next attempt gets a fresh one.
    async fn rejected(&self, _identity: Option<IdentityId>, _credentials: &Credentials) {}
}

/// What has to happen exactly once after a delivery (queue the Sent copy's `APPEND`,
/// flag the replied-to message). Runs inside the transaction that marks the outbox row
/// sent.
pub trait SentHook: Send + Sync {
    /// Called with the delivered item.
    fn on_sent(&self, w: &mut WriteTx<'_>, item: &OutboxItem) -> store::Result<()>;
}

/// A [`SentHook`] that does nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHook;

impl SentHook for NoHook {
    fn on_sent(&self, _: &mut WriteTx<'_>, _: &OutboxItem) -> store::Result<()> {
        Ok(())
    }
}

/// The outcome of one attempt at an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// The server accepted it; the send log and the outbox say so.
    Sent,
    /// The send log already had it (an earlier attempt was accepted before a crash):
    /// marked sent without sending again.
    AlreadySent,
    /// A temporary failure; the item is queued again for `next_attempt`.
    Retrying {
        /// Why.
        error: SmtpError,
        /// When it is due again.
        next_attempt: UnixTime,
    },
    /// A permanent failure (or too many attempts); the item is marked failed.
    Failed(SmtpError),
    /// The item wasn't queued (another sender has it, or it is sent, failed, frozen or
    /// gone).
    Skipped,
}

/// What [`Outbox::recover`] found after a restart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recovery {
    /// Items interrupted while sending that the send log knows: marked sent.
    pub marked_sent: usize,
    /// Items interrupted before the server accepted them: queued again.
    pub requeued: usize,
}

/// Errors of the outbox itself (the store), as opposed to delivery failures, which are
/// [`Delivery`] outcomes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OutboxError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] store::Error),
    /// A blocking task panicked.
    #[error("a background task failed")]
    Task,
}

/// The outbox sender. See the [module docs](self).
#[derive(Clone)]
pub struct Outbox {
    store: Store,
    mailer: Arc<dyn Mailer>,
}

impl fmt::Debug for Outbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outbox")
            .field("mailer", &self.mailer)
            .finish_non_exhaustive()
    }
}

impl Outbox {
    /// An outbox over `store` that submits through `mailer`.
    pub fn new(store: Store, mailer: Arc<dyn Mailer>) -> Self {
        Self { store, mailer }
    }

    /// The transport.
    pub fn mailer(&self) -> &Arc<dyn Mailer> {
        &self.mailer
    }
}

/// Now, in Unix seconds.
fn unix_now() -> UnixTime {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

impl Outbox {
    /// Connects and logs in through `route` without sending anything ("Test
    /// Connection").
    pub async fn check(&self, route: &Route) -> Result<(), SmtpError> {
        self.mailer.check(&route.config, &route.credentials).await
    }

    /// After a restart: items left in `sending` (Maze died mid-send) are marked sent if
    /// the send log has their Message-ID (running `hook`), else queued again.
    pub async fn recover(&self, hook: Arc<dyn SentHook>) -> Result<Recovery, OutboxError> {
        let interrupted = self.store.reader()?.outbox_in_state(OutboxState::Sending)?;
        let mut recovery = Recovery::default();
        for item in interrupted {
            if self
                .store
                .sendlog()
                .was_sent(&item.rfc_message_id)?
                .is_some()
            {
                self.mark_sent(item, Arc::clone(&hook)).await?;
                recovery.marked_sent += 1;
            } else {
                let id = item.id;
                self.store
                    .write(move |w| {
                        w.outbox_set_state(
                            id,
                            OutboxState::Queued,
                            Some("interrupted while sending"),
                        )
                    })
                    .await?;
                recovery.requeued += 1;
            }
        }
        if recovery != Recovery::default() {
            tracing::info!(
                marked_sent = recovery.marked_sent,
                requeued = recovery.requeued,
                "outbox recovered after a restart"
            );
        }
        Ok(recovery)
    }

    /// One attempt at item `id`: claim it, check the send log, submit, record.
    pub async fn send(
        &self,
        id: OutboxId,
        routes: &dyn Routes,
        hook: Arc<dyn SentHook>,
    ) -> Result<Delivery, OutboxError> {
        let Some(item) = self.store.reader()?.outbox_item(id)? else {
            return Ok(Delivery::Skipped);
        };
        // Only queued items; one interrupted while `sending` is `recover`'s.
        if item.state != OutboxState::Queued
            || !self.store.write(move |w| w.outbox_claim(id)).await?
        {
            return Ok(Delivery::Skipped);
        }
        // Accepted before (and the outbox didn't learn of it, e.g. after a rollback):
        // never again.
        if self
            .store
            .sendlog()
            .was_sent(&item.rfc_message_id)?
            .is_some()
        {
            self.mark_sent(item, hook).await?;
            tracing::info!(outbox = %id, "already in the send log; not sent again");
            return Ok(Delivery::AlreadySent);
        }
        let attempts = item.attempts.saturating_add(1);
        match self.attempt(&item, routes).await? {
            Ok(()) => {
                self.mark_sent(item, hook).await?;
                tracing::info!(outbox = %id, attempts, "message sent");
                Ok(Delivery::Sent)
            }
            Err(error) => self.failed(id, attempts, error).await,
        }
    }

    /// Submits the item; `Ok(Err(_))` is a delivery failure, `Err(_)` a store failure.
    async fn attempt(
        &self,
        item: &OutboxItem,
        routes: &dyn Routes,
    ) -> Result<Result<(), SmtpError>, OutboxError> {
        let Some(envelope) = item.envelope.clone() else {
            return Ok(Err(SmtpError::Invalid(
                "it was queued without an envelope".to_owned(),
            )));
        };
        let Some(blob) = item.blob else {
            return Ok(Err(SmtpError::Invalid(
                "its stored message is missing".to_owned(),
            )));
        };
        let store = self.store.clone();
        let raw = match tokio::task::spawn_blocking(move || store.blobs().read(&blob)).await {
            Ok(Ok(raw)) => raw,
            Ok(Err(err)) => {
                tracing::error!(%err, outbox = %item.id, "reading an outbox message");
                return Ok(Err(SmtpError::Invalid(
                    "its stored message can't be read".to_owned(),
                )));
            }
            Err(_) => return Err(OutboxError::Task),
        };
        let route = match routes.route(item.identity).await {
            Ok(route) => route,
            Err(err) => return Ok(Err(err)),
        };
        let result = self
            .mailer
            .send(&route.config, &route.credentials, &envelope, &raw)
            .await;
        if let Err(err) = &result
            && err.is_auth()
        {
            routes.rejected(item.identity, &route.credentials).await;
        }
        Ok(result)
    }

    /// Records a delivery: send log first, then the outbox row and `hook` in one
    /// transaction.
    async fn mark_sent(&self, item: OutboxItem, hook: Arc<dyn SentHook>) -> store::Result<()> {
        let (id, rfc, identity) = (item.id, item.rfc_message_id.clone(), item.identity);
        self.store
            .record_sent_with(id, &rfc, identity, move |w| hook.on_sent(w, &item))
            .await?;
        Ok(())
    }

    async fn failed(
        &self,
        id: OutboxId,
        attempts: u32,
        error: SmtpError,
    ) -> Result<Delivery, OutboxError> {
        let text = error.to_string();
        if error.is_permanent() || attempts >= MAX_SEND_ATTEMPTS {
            tracing::warn!(outbox = %id, attempts, kind = error.kind(), "sending failed");
            self.store
                .write(move |w| w.outbox_set_state(id, OutboxState::Failed, Some(&text)))
                .await?;
            return Ok(Delivery::Failed(error));
        }
        let next_attempt = unix_now() + store::retry_backoff_secs(attempts);
        tracing::info!(outbox = %id, attempts, kind = error.kind(), "sending failed; trying again later");
        self.store
            .write(move |w| w.outbox_retry_later(id, next_attempt, &text))
            .await?;
        Ok(Delivery::Retrying {
            error,
            next_attempt,
        })
    }

    /// Sends every item that is due now, oldest first.
    pub async fn send_due(
        &self,
        routes: &dyn Routes,
        hook: Arc<dyn SentHook>,
    ) -> Result<Vec<(OutboxId, Delivery)>, OutboxError> {
        let due = self.store.reader()?.outbox_due(unix_now())?;
        let mut out = Vec::with_capacity(due.len());
        for item in due {
            let delivery = self.send(item.id, routes, Arc::clone(&hook)).await?;
            out.push((item.id, delivery));
        }
        Ok(out)
    }

    /// Removes sent items older than `age` whose blob no pending op needs any more.
    pub async fn prune(&self, age: std::time::Duration) -> Result<usize, OutboxError> {
        let before = unix_now().saturating_sub(i64::try_from(age.as_secs()).unwrap_or(i64::MAX));
        Ok(self
            .store
            .write(move |w| w.outbox_prune_sent(before))
            .await?)
    }
}
