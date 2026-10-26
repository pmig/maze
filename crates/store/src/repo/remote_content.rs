//! Senders allowed to load remote content ("Always for this sender", schema v11).

use rusqlite::{Connection, OptionalExtension, params};

use crate::model::UnixTime;
use crate::{Error, Result, WriteTx};

fn normalize(email: &str) -> Option<String> {
    let email = email.trim().to_lowercase();
    (!email.is_empty() && email.contains('@')).then_some(email)
}

pub(crate) fn remote_content_allowed(conn: &Connection, email: &str) -> Result<bool> {
    let Some(email) = normalize(email) else {
        return Ok(false);
    };
    Ok(conn
        .prepare_cached("SELECT 1 FROM remote_content_sender WHERE email = ?1")?
        .query_row([email], |_| Ok(()))
        .optional()?
        .is_some())
}

impl WriteTx<'_> {
    /// Lets messages from `email` load remote content without asking (idempotent).
    pub fn allow_remote_content(&mut self, email: &str, now: UnixTime) -> Result<()> {
        let email = normalize(email).ok_or(Error::Invalid("not an email address"))?;
        self.conn
            .prepare_cached(
                "INSERT INTO remote_content_sender (email, added) VALUES (?1, ?2)
                 ON CONFLICT (email) DO NOTHING",
            )?
            .execute(params![email, now])?;
        Ok(())
    }

    /// Asks again before loading remote content for messages from `email`.
    pub fn forget_remote_content(&mut self, email: &str) -> Result<()> {
        if let Some(email) = normalize(email) {
            self.conn
                .prepare_cached("DELETE FROM remote_content_sender WHERE email = ?1")?
                .execute([email])?;
        }
        Ok(())
    }
}
