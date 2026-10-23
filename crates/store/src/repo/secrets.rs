//! Account secrets (ADR-017): IMAP passwords, OAuth refresh tokens and SMTP
//! passwords, in the encrypted database instead of separate OS keychain items.
//!
//! The store doesn't know what the secrets are: `kind` is the `auth` crate's name for
//! it (`password`, `oauth-refresh`, `smtp-password`). Values come back in
//! [`Zeroizing`] strings, so the copies the store hands out are wiped when dropped.
//! Errors never carry a secret (SQLite's messages don't include bound values).

use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{Error, Result, WriteTx};

fn text(account: Uuid) -> String {
    account.hyphenated().to_string()
}

/// The `kind` secret of `account`, if there is one.
pub(crate) fn account_secret(
    conn: &Connection,
    account: Uuid,
    kind: &str,
) -> Result<Option<Zeroizing<String>>> {
    Ok(conn
        .prepare_cached("SELECT secret FROM account_secret WHERE account_uuid = ?1 AND kind = ?2")?
        .query_row(params![text(account), kind], |row| {
            row.get::<_, String>(0).map(Zeroizing::new)
        })
        .optional()?)
}

/// Accounts whose secrets may still be in the OS keychain (created before schema v5
/// and not moved yet).
pub(crate) fn legacy_secret_accounts(conn: &Connection) -> Result<Vec<Uuid>> {
    let mut stmt = conn
        .prepare_cached("SELECT account_uuid FROM legacy_secret_account ORDER BY account_uuid")?;
    let uuids: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    uuids
        .iter()
        .map(|uuid| Uuid::parse_str(uuid).map_err(|_| Error::Invalid("invalid account uuid")))
        .collect()
}

impl WriteTx<'_> {
    /// Stores the `kind` secret of `account`, replacing any existing one.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] if there is no account with this UUID.
    pub fn set_account_secret(&mut self, account: Uuid, kind: &str, secret: &str) -> Result<()> {
        let exists: Option<i64> = self
            .conn
            .prepare_cached("SELECT 1 FROM account WHERE uuid = ?1")?
            .query_row([text(account)], |row| row.get(0))
            .optional()?;
        if exists.is_none() {
            return Err(Error::Invalid("no account with this uuid"));
        }
        self.conn
            .prepare_cached(
                "INSERT INTO account_secret (account_uuid, kind, secret, updated)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (account_uuid, kind)
                 DO UPDATE SET secret = excluded.secret, updated = excluded.updated",
            )?
            .execute(params![text(account), kind, secret, self.now])?;
        Ok(())
    }

    /// Deletes the `kind` secret of `account`; succeeds if there is none.
    pub fn delete_account_secret(&mut self, account: Uuid, kind: &str) -> Result<()> {
        self.conn
            .prepare_cached("DELETE FROM account_secret WHERE account_uuid = ?1 AND kind = ?2")?
            .execute(params![text(account), kind])?;
        Ok(())
    }

    /// Puts `account` on the list of accounts whose secrets may still be in the OS
    /// keychain. The v5 migration does this for every account that existed before it;
    /// tests use it to stage a move.
    pub fn list_legacy_secrets(&mut self, account: Uuid) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO legacy_secret_account (account_uuid) VALUES (?1)",
            )?
            .execute([text(account)])?;
        Ok(())
    }

    /// Takes `account` off the list of accounts whose secrets may still be in the OS
    /// keychain ([`Queries::legacy_secret_accounts`](crate::Queries::legacy_secret_accounts)).
    pub fn finish_legacy_secrets(&mut self, account: Uuid) -> Result<()> {
        self.conn
            .prepare_cached("DELETE FROM legacy_secret_account WHERE account_uuid = ?1")?
            .execute([text(account)])?;
        Ok(())
    }
}
