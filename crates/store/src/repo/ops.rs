//! The pending-op queue and the outbox (PLAN §4.5).

use mazemail_types::{AccountId, BlobId, IdentityId};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::blob::StagedBlob;
use crate::model::{
    OpId, OpState, OutboxEnvelope, OutboxId, OutboxItem, OutboxState, PendingOp, UnixTime,
};
use crate::repo::accounts::ensure_changed;
use crate::sendlog::SendLog;
use crate::{Error, Result, WriteTx};

/// After this many failed attempts an op is [`OpState::Stuck`] (PLAN §4.5).
pub const MAX_OP_ATTEMPTS: u32 = 5;

/// Exponential backoff for the `attempts`-th retry: 30 s doubling, capped at one hour,
/// before jitter (the caller adds jitter).
pub fn retry_backoff_secs(attempts: u32) -> i64 {
    let exponent = attempts.saturating_sub(1).min(7);
    (30_i64 << exponent).min(3600)
}

const OP_COLUMNS: &str = "id, account_id, kind, payload_json, origin, state, attempts, next_attempt, last_error, created";

fn op_row(row: &Row<'_>) -> rusqlite::Result<PendingOp> {
    Ok(PendingOp {
        id: OpId(row.get(0)?),
        account: AccountId(row.get(1)?),
        kind: row.get(2)?,
        payload_json: row.get(3)?,
        origin: row.get(4)?,
        state: row.get(5)?,
        attempts: row.get(6)?,
        next_attempt: row.get(7)?,
        last_error: row.get(8)?,
        created: row.get(9)?,
    })
}

/// The account's ops in replay order (oldest first), stuck ones included.
pub(crate) fn pending_ops(conn: &Connection, account: AccountId) -> Result<Vec<PendingOp>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {OP_COLUMNS} FROM pending_op WHERE account_id = ?1 ORDER BY id"
    ))?;
    Ok(stmt
        .query_map([account.0], op_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// The next op to replay for the account: the oldest queued one, if it is due at `now`.
/// Ops are replayed strictly in order, so a later op never overtakes a waiting one; a
/// stuck op blocks nothing (it waits for Retry or Discard).
pub(crate) fn next_pending_op(
    conn: &Connection,
    account: AccountId,
    now: UnixTime,
) -> Result<Option<PendingOp>> {
    let op = conn
        .prepare_cached(&format!(
            "SELECT {OP_COLUMNS} FROM pending_op WHERE account_id = ?1 AND state = 'queued'
             ORDER BY id LIMIT 1"
        ))?
        .query_row([account.0], op_row)
        .optional()?;
    Ok(op.filter(|op| op.next_attempt <= now))
}

/// Accounts with queued ops.
pub(crate) fn accounts_with_pending_ops(conn: &Connection) -> Result<Vec<AccountId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT account_id FROM pending_op WHERE state = 'queued' ORDER BY account_id",
    )?;
    Ok(stmt
        .query_map([], |row| row.get(0).map(AccountId))?
        .collect::<rusqlite::Result<_>>()?)
}

const OUTBOX_COLUMNS: &str = "id, identity_id, blob_id, rfc_message_id, send_at, state, attempts, \
     last_error, created, envelope_json";

fn outbox_row(row: &Row<'_>) -> rusqlite::Result<OutboxItem> {
    let envelope = row
        .get::<_, Option<String>>(9)?
        .map(|text| serde_json::from_str::<OutboxEnvelope>(&text))
        .transpose()
        .map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, err.into())
        })?;
    Ok(OutboxItem {
        id: OutboxId(row.get(0)?),
        identity: row.get::<_, Option<i64>>(1)?.map(IdentityId),
        blob: row.get::<_, Option<[u8; 32]>>(2)?.map(BlobId),
        rfc_message_id: row.get(3)?,
        send_at: row.get(4)?,
        state: row.get(5)?,
        attempts: row.get(6)?,
        last_error: row.get(7)?,
        created: row.get(8)?,
        envelope,
    })
}

pub(crate) fn outbox(conn: &Connection) -> Result<Vec<OutboxItem>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {OUTBOX_COLUMNS} FROM outbox ORDER BY send_at, id"
    ))?;
    Ok(stmt
        .query_map([], outbox_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn outbox_item(conn: &Connection, id: OutboxId) -> Result<Option<OutboxItem>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {OUTBOX_COLUMNS} FROM outbox WHERE id = ?1"
        ))?
        .query_row([id.0], outbox_row)
        .optional()?)
}

/// Queued items whose send time has come.
pub(crate) fn outbox_due(conn: &Connection, now: UnixTime) -> Result<Vec<OutboxItem>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {OUTBOX_COLUMNS} FROM outbox WHERE state = 'queued' AND send_at <= ?1
         ORDER BY send_at, id"
    ))?;
    Ok(stmt
        .query_map([now], outbox_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Outbox items in `state`, oldest first.
pub(crate) fn outbox_in_state(conn: &Connection, state: OutboxState) -> Result<Vec<OutboxItem>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {OUTBOX_COLUMNS} FROM outbox WHERE state = ?1 ORDER BY send_at, id"
    ))?;
    Ok(stmt
        .query_map([state], outbox_row)?
        .collect::<rusqlite::Result<_>>()?)
}

impl WriteTx<'_> {
    // ---- pending ops ---------------------------------------------------------------

    /// Persists an op before its optimistic local apply. `origin` is the mutation's
    /// origin as text (`user`, `rule:<id>`, `agent:<token id>`, `cli`).
    pub fn enqueue_op(
        &mut self,
        account: AccountId,
        kind: &str,
        payload_json: &str,
        origin: &str,
    ) -> Result<OpId> {
        self.insert_op(account, kind, payload_json, origin, None)
    }

    /// [`WriteTx::enqueue_op`] for an op that uploads a message nothing else references
    /// (an imported message's `append`): the op holds a reference to the blob until it
    /// is removed, so garbage collection keeps it.
    pub fn enqueue_op_with_blob(
        &mut self,
        account: AccountId,
        kind: &str,
        payload_json: &str,
        origin: &str,
        blob: &StagedBlob,
    ) -> Result<OpId> {
        self.register_blob(blob)?;
        self.insert_op(account, kind, payload_json, origin, Some(blob.id()))
    }

    fn insert_op(
        &mut self,
        account: AccountId,
        kind: &str,
        payload_json: &str,
        origin: &str,
        blob: Option<BlobId>,
    ) -> Result<OpId> {
        self.conn
            .prepare_cached(
                "INSERT INTO pending_op (account_id, kind, payload_json, origin, next_attempt,
                   created, blob_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            )?
            .execute(params![
                account.0,
                kind,
                payload_json,
                origin,
                self.now,
                blob.map(|b| b.0)
            ])?;
        self.changes.accounts.insert(account);
        Ok(OpId(self.conn.last_insert_rowid()))
    }

    /// The op succeeded on the server; removes it.
    pub fn op_done(&mut self, id: OpId) -> Result<()> {
        let account = self.op_account(id)?;
        self.conn
            .prepare_cached("DELETE FROM pending_op WHERE id = ?1")?
            .execute([id.0])?;
        self.changes.accounts.insert(account);
        Ok(())
    }

    /// The op failed: counts the attempt and schedules the next one after
    /// [`retry_backoff_secs`] (plus `jitter_secs`). After [`MAX_OP_ATTEMPTS`] it
    /// becomes [`OpState::Stuck`]. Returns the new state.
    pub fn op_failed(&mut self, id: OpId, error: &str, jitter_secs: i64) -> Result<OpState> {
        let account = self.op_account(id)?;
        let attempts: u32 = self
            .conn
            .prepare_cached("SELECT attempts FROM pending_op WHERE id = ?1")?
            .query_row([id.0], |row| row.get(0))?;
        let attempts = attempts.saturating_add(1);
        let state = if attempts >= MAX_OP_ATTEMPTS {
            OpState::Stuck
        } else {
            OpState::Queued
        };
        let next = self.now + retry_backoff_secs(attempts) + jitter_secs.max(0);
        self.conn
            .prepare_cached(
                "UPDATE pending_op SET attempts = ?2, state = ?3, next_attempt = ?4,
                   last_error = ?5 WHERE id = ?1",
            )?
            .execute(params![id.0, attempts, state, next, error])?;
        self.changes.accounts.insert(account);
        Ok(state)
    }

    /// Retry (from the activity log): queues the op again, due now.
    pub fn op_retry(&mut self, id: OpId) -> Result<()> {
        let account = self.op_account(id)?;
        self.conn
            .prepare_cached(
                "UPDATE pending_op SET state = 'queued', attempts = 0, next_attempt = ?2
                 WHERE id = ?1",
            )?
            .execute(params![id.0, self.now])?;
        self.changes.accounts.insert(account);
        Ok(())
    }

    /// Discard (from the activity log): drops the op. The caller re-syncs the affected
    /// folder so the local optimistic change is undone.
    pub fn op_discard(&mut self, id: OpId) -> Result<()> {
        self.op_done(id)
    }

    /// Replaces an op's payload (for example after COPYUID remapped UIDs that later
    /// ops refer to).
    pub fn op_update_payload(&mut self, id: OpId, payload_json: &str) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("UPDATE pending_op SET payload_json = ?2 WHERE id = ?1")?
            .execute(params![id.0, payload_json])?;
        ensure_changed(changed, "pending op", id.0)
    }

    fn op_account(&self, id: OpId) -> Result<AccountId> {
        self.conn
            .prepare_cached("SELECT account_id FROM pending_op WHERE id = ?1")?
            .query_row([id.0], |row| row.get(0).map(AccountId))
            .optional()?
            .ok_or_else(|| Error::not_found("pending op", id))
    }

    // ---- outbox ----------------------------------------------------------------------

    /// Queues a built message (its raw blob) for sending at `send_at` (after the undo
    /// delay). `envelope` is the SMTP envelope (Bcc recipients included).
    pub fn outbox_enqueue(
        &mut self,
        identity: Option<IdentityId>,
        blob: &StagedBlob,
        rfc_message_id: &str,
        send_at: UnixTime,
        envelope: Option<&OutboxEnvelope>,
    ) -> Result<OutboxId> {
        self.register_blob(blob)?;
        let envelope = envelope.map(serde_json::to_string).transpose()?;
        self.conn
            .prepare_cached(
                "INSERT INTO outbox (identity_id, blob_id, rfc_message_id, send_at, created,
                   envelope_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                identity.map(|i| i.0),
                &blob.id().0,
                rfc_message_id,
                send_at,
                self.now,
                envelope
            ])?;
        self.changes.outbox = true;
        Ok(OutboxId(self.conn.last_insert_rowid()))
    }

    /// Takes a queued item for sending: `queued` becomes `sending` (counting an
    /// attempt). Returns `false` if the item isn't queued (someone else took it, or it
    /// was sent, removed or frozen meanwhile), so two senders never send one item.
    pub fn outbox_claim(&mut self, id: OutboxId) -> Result<bool> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE outbox SET state = 'sending', attempts = attempts + 1
                 WHERE id = ?1 AND state = 'queued'",
            )?
            .execute([id.0])?;
        if changed > 0 {
            self.changes.outbox = true;
        }
        Ok(changed > 0)
    }

    /// A send attempt failed temporarily: back to `queued`, due again at `send_at`,
    /// with the error for the activity log.
    pub fn outbox_retry_later(
        &mut self,
        id: OutboxId,
        send_at: UnixTime,
        error: &str,
    ) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE outbox SET state = 'queued', send_at = ?2, last_error = ?3
                 WHERE id = ?1 AND state != 'sent'",
            )?
            .execute(params![id.0, send_at, error])?;
        ensure_changed(changed, "unsent outbox item", id.0)?;
        self.changes.outbox = true;
        Ok(())
    }

    /// Removes sent items created before `before` whose blob no pending op still needs
    /// (the Sent copy's `APPEND` reads it). Their blobs lose a reference and are
    /// collected. Returns how many were removed.
    pub fn outbox_prune_sent(&mut self, before: UnixTime) -> Result<usize> {
        let removed = self
            .conn
            .prepare_cached(
                "DELETE FROM outbox WHERE state = 'sent' AND created < ?1
                   AND (blob_id IS NULL OR NOT EXISTS (
                     SELECT 1 FROM pending_op
                     WHERE instr(payload_json, lower(hex(outbox.blob_id))) > 0))",
            )?
            .execute([before])?;
        if removed > 0 {
            self.changes.outbox = true;
        }
        Ok(removed)
    }

    /// Sets an item's state (and error text); `Sending` counts an attempt.
    pub fn outbox_set_state(
        &mut self,
        id: OutboxId,
        state: OutboxState,
        error: Option<&str>,
    ) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE outbox SET state = ?2, last_error = ?3,
                   attempts = attempts + (?2 = 'sending') WHERE id = ?1",
            )?
            .execute(params![id.0, state, error])?;
        ensure_changed(changed, "outbox item", id.0)?;
        self.changes.outbox = true;
        Ok(())
    }

    /// Marks an item sent. Use [`Store::record_sent`](crate::Store::record_sent), which
    /// writes the send log first.
    pub fn outbox_mark_sent(&mut self, id: OutboxId) -> Result<()> {
        self.outbox_set_state(id, OutboxState::Sent, None)
    }

    /// Reschedules a queued, failed or frozen item (Send again, or undo-send edits).
    pub fn outbox_reschedule(&mut self, id: OutboxId, send_at: UnixTime) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE outbox SET state = 'queued', send_at = ?2, last_error = NULL
                 WHERE id = ?1 AND state != 'sent'",
            )?
            .execute(params![id.0, send_at])?;
        ensure_changed(changed, "unsent outbox item", id.0)?;
        self.changes.outbox = true;
        Ok(())
    }

    /// Removes an item (undo send, or cleanup after the Sent copy exists). Its blob
    /// loses a reference.
    pub fn outbox_remove(&mut self, id: OutboxId) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("DELETE FROM outbox WHERE id = ?1")?
            .execute([id.0])?;
        ensure_changed(changed, "outbox item", id.0)?;
        self.changes.outbox = true;
        Ok(())
    }

    /// After a rollback (PLAN §4.15): every unsent item found in the send log is marked
    /// sent, every other one is frozen until you press Send again. Returns
    /// `(marked_sent, frozen)`.
    pub fn outbox_reconcile_after_rollback(&mut self, sendlog: &SendLog) -> Result<(usize, usize)> {
        let items: Vec<(i64, String)> = self
            .conn
            .prepare_cached("SELECT id, rfc_message_id FROM outbox WHERE state != 'sent'")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let (mut sent, mut frozen) = (0, 0);
        for (id, rfc_message_id) in items {
            let state = if sendlog.was_sent(&rfc_message_id)?.is_some() {
                sent += 1;
                OutboxState::Sent
            } else {
                frozen += 1;
                OutboxState::Frozen
            };
            self.conn
                .prepare_cached("UPDATE outbox SET state = ?2 WHERE id = ?1")?
                .execute(params![id, state])?;
        }
        self.changes.outbox = true;
        Ok((sent, frozen))
    }
}
