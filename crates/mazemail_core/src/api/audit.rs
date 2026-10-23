//! `local_api`'s [`AuditSink`] on the store's encrypted `audit_log`.
//!
//! [`AuditSink::record`] runs on the request path and must not block, so events go
//! into a channel; a task on the core runtime writes whatever has queued up in one
//! store job. Each request becomes one entry:
//!
//! - `origin`: `agent:<token row id>` when the request carried a token the
//!   [`TokenCache`] knows (the same origin [`CoreHandle::apply`] writes for the
//!   commands it runs), else `api`.
//! - `action`: `api:<command>` (`api:search`, `api:<invalid>`, `api:<connect>`).
//! - `detail_json`: request id, public token id, peer pid and executable, outcome,
//!   whether the user approved it, and up to 100 affected ids. Never mail content.
//!
//! [`CoreHandle::apply`]: crate::CoreHandle::apply

use std::fmt;
use std::sync::Arc;

use local_api::{AuditEvent, AuditSink};
use tokio::sync::mpsc;

use super::tokens::TokenCache;
use crate::handle::CoreHandle;

/// Affected ids written per entry; the total is recorded as well.
const LISTED_IDS: usize = 100;

/// Events written per store job at most.
const BATCH: usize = 256;

/// Writes audit events into the store. See the [module docs](self).
pub struct StoreAuditSink {
    tx: mpsc::UnboundedSender<AuditEvent>,
}

impl fmt::Debug for StoreAuditSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreAuditSink").finish_non_exhaustive()
    }
}

impl StoreAuditSink {
    /// Starts the writer task on the core runtime. It stops when the sink is dropped
    /// (after writing what is queued) or the core shuts down.
    pub fn new(core: &CoreHandle, cache: Arc<TokenCache>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        core.runtime().spawn(write_events(core.clone(), cache, rx));
        Self { tx }
    }
}

impl AuditSink for StoreAuditSink {
    fn record(&self, event: AuditEvent) {
        if self.tx.send(event).is_err() {
            tracing::warn!("the audit writer has stopped; an API request was not audited");
        }
    }
}

/// One `audit_log` row.
struct Entry {
    origin: String,
    action: String,
    detail: String,
}

fn entry(event: &AuditEvent, cache: &TokenCache) -> Entry {
    let origin = event
        .token
        .as_ref()
        .and_then(|token| cache.store_id(token))
        .map_or_else(|| "api".to_owned(), |id| format!("agent:{}", id.0));
    let listed: Vec<&str> = event
        .affected
        .iter()
        .take(LISTED_IDS)
        .map(String::as_str)
        .collect();
    let detail = serde_json::json!({
        "request": event.request_id,
        "token": event.token.as_ref().map(|t| t.as_str()),
        "pid": event.pid,
        "exe": event.exe.as_ref().map(|exe| exe.display().to_string()),
        "outcome": event.outcome,
        "approved": event.approved,
        "affected": listed,
        "affected_total": event.affected.len(),
        "at": event.at.unix_timestamp(),
    });
    Entry {
        origin,
        action: format!("api:{}", event.cmd),
        detail: detail.to_string(),
    }
}

async fn write_events(
    core: CoreHandle,
    cache: Arc<TokenCache>,
    mut rx: mpsc::UnboundedReceiver<AuditEvent>,
) {
    let mut events = Vec::with_capacity(BATCH);
    while rx.recv_many(&mut events, BATCH).await > 0 {
        let entries: Vec<Entry> = events.drain(..).map(|e| entry(&e, &cache)).collect();
        let count = entries.len();
        let written = core
            .store()
            .write_weighted(count, move |w| {
                for entry in &entries {
                    w.audit(&entry.origin, &entry.action, Some(&entry.detail))?;
                }
                Ok(())
            })
            .await;
        if let Err(error) = written {
            tracing::error!(%error, count, "writing API audit entries failed");
            if matches!(error, store::Error::Closed) {
                break;
            }
        }
    }
}
