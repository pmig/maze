//! The send log, `mail/sendlog.db` (PLAN §4.3, §4.5, §4.15).
//!
//! A separate SQLCipher file with one row per message the SMTP server accepted. It's
//! never restored from a backup, so after a rollback the outbox can check which items
//! already went out and never sends them twice.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mazemail_types::IdentityId;
use rusqlite::{Connection, OptionalExtension, params};
use vault::SecretString;

use crate::Result;
use crate::conn::{self, Access};
use crate::migrate;
use crate::model::{OutboxId, SentReceipt};
use crate::writer::unix_now;

/// The send log. Writes are rare and synchronous (`synchronous = FULL`): a receipt is
/// on disk before [`SendLog::record`] returns.
pub struct SendLog {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for SendLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendLog").finish_non_exhaustive()
    }
}

impl SendLog {
    pub(crate) fn open(path: &Path, key: &SecretString) -> Result<Self> {
        let mut conn = conn::open(path, key, Access::ReadWrite, 512)?;
        conn.execute_batch("PRAGMA synchronous = FULL;")?;
        migrate::migrate_sendlog(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records that the server accepted `rfc_message_id`. Recording the same id again
    /// keeps the first receipt.
    pub fn record(
        &self,
        rfc_message_id: &str,
        identity: Option<IdentityId>,
        outbox: Option<OutboxId>,
    ) -> Result<SentReceipt> {
        let conn = self.conn();
        conn.prepare_cached(
            "INSERT INTO sent (rfc_message_id, sent_at, identity_id, outbox_id)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT (rfc_message_id) DO NOTHING",
        )?
        .execute(params![
            rfc_message_id,
            unix_now(),
            identity.map(|i| i.0),
            outbox.map(|o| o.0)
        ])?;
        lookup(&conn, rfc_message_id)?.ok_or(crate::Error::Invalid("send log row vanished"))
    }

    /// The receipt for `rfc_message_id`, if it was sent.
    pub fn was_sent(&self, rfc_message_id: &str) -> Result<Option<SentReceipt>> {
        lookup(&self.conn(), rfc_message_id)
    }

    /// Checkpoints the WAL.
    pub(crate) fn close(&self) -> Result<()> {
        self.conn()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }
}

fn lookup(conn: &Connection, rfc_message_id: &str) -> Result<Option<SentReceipt>> {
    Ok(conn
        .prepare_cached(
            "SELECT rfc_message_id, sent_at, identity_id, outbox_id FROM sent
             WHERE rfc_message_id = ?1",
        )?
        .query_row([rfc_message_id], |row| {
            Ok(SentReceipt {
                rfc_message_id: row.get(0)?,
                sent_at: row.get(1)?,
                identity: row.get::<_, Option<i64>>(2)?.map(IdentityId),
                outbox: row.get::<_, Option<i64>>(3)?.map(OutboxId),
            })
        })
        .optional()?)
}
