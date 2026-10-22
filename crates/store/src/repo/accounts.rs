//! Accounts, identities, identity rules, SMTP servers and signatures.

use mazemail_types::{AccountId, FolderId, IdentityId};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::{
    Account, Identity, IdentityInput, IdentityRule, IdentityRuleId, IdentityRuleKind, NewAccount,
    Signature, SignatureId, SignatureInput, SmtpServer, SmtpServerId, SmtpServerInput,
};
use crate::{Error, Result, WriteTx};

const ACCOUNT_COLUMNS: &str = "id, kind, name, config_json, sync_state_json, ordinal";

fn account_row(row: &Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: AccountId(row.get(0)?),
        kind: row.get(1)?,
        name: row.get(2)?,
        config_json: row.get(3)?,
        sync_state_json: row.get(4)?,
        ordinal: row.get(5)?,
    })
}

pub(crate) fn accounts(conn: &Connection) -> Result<Vec<Account>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {ACCOUNT_COLUMNS} FROM account ORDER BY ordinal, id"
    ))?;
    Ok(stmt
        .query_map([], account_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn account(conn: &Connection, id: AccountId) -> Result<Option<Account>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {ACCOUNT_COLUMNS} FROM account WHERE id = ?1"
        ))?
        .query_row([id.0], account_row)
        .optional()?)
}

const IDENTITY_COLUMNS: &str = "id, account_id, label, display_name, email, reply_to, \
     signature_id, smtp_id, sent_folder_id, drafts_folder_id, archive_folder_id, \
     archive_granularity, color, match_patterns_json, is_default, ordinal";

fn identity_row(row: &Row<'_>) -> rusqlite::Result<Identity> {
    Ok(Identity {
        id: IdentityId(row.get(0)?),
        account: AccountId(row.get(1)?),
        input: IdentityInput {
            label: row.get(2)?,
            display_name: row.get(3)?,
            email: row.get(4)?,
            reply_to: row.get(5)?,
            signature: row.get::<_, Option<i64>>(6)?.map(SignatureId),
            smtp: row.get::<_, Option<i64>>(7)?.map(SmtpServerId),
            sent_folder: row.get::<_, Option<i64>>(8)?.map(FolderId),
            drafts_folder: row.get::<_, Option<i64>>(9)?.map(FolderId),
            archive_folder: row.get::<_, Option<i64>>(10)?.map(FolderId),
            archive_granularity: row.get(11)?,
            color: row.get(12)?,
            match_patterns_json: row.get(13)?,
            is_default: row.get(14)?,
            ordinal: row.get(15)?,
        },
    })
}

pub(crate) fn identities(conn: &Connection, account: Option<AccountId>) -> Result<Vec<Identity>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {IDENTITY_COLUMNS} FROM identity
         WHERE ?1 IS NULL OR account_id = ?1 ORDER BY account_id, ordinal, id"
    ))?;
    Ok(stmt
        .query_map([account.map(|a| a.0)], identity_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn identity(conn: &Connection, id: IdentityId) -> Result<Option<Identity>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {IDENTITY_COLUMNS} FROM identity WHERE id = ?1"
        ))?
        .query_row([id.0], identity_row)
        .optional()?)
}

pub(crate) fn identity_rules(conn: &Connection, identity: IdentityId) -> Result<Vec<IdentityRule>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, identity_id, kind, pattern, ordinal FROM identity_rule
         WHERE identity_id = ?1 ORDER BY ordinal, id",
    )?;
    Ok(stmt
        .query_map([identity.0], |row| {
            Ok(IdentityRule {
                id: IdentityRuleId(row.get(0)?),
                identity: IdentityId(row.get(1)?),
                kind: row.get(2)?,
                pattern: row.get(3)?,
                ordinal: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

const SMTP_COLUMNS: &str = "id, name, host, port, security, auth, username, saves_sent";

fn smtp_row(row: &Row<'_>) -> rusqlite::Result<SmtpServer> {
    Ok(SmtpServer {
        id: SmtpServerId(row.get(0)?),
        name: row.get(1)?,
        host: row.get(2)?,
        port: row.get(3)?,
        security: row.get(4)?,
        auth: row.get(5)?,
        username: row.get(6)?,
        saves_sent: row.get(7)?,
    })
}

pub(crate) fn smtp_servers(conn: &Connection) -> Result<Vec<SmtpServer>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {SMTP_COLUMNS} FROM smtp_server ORDER BY id"
    ))?;
    Ok(stmt
        .query_map([], smtp_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn smtp_server(conn: &Connection, id: SmtpServerId) -> Result<Option<SmtpServer>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {SMTP_COLUMNS} FROM smtp_server WHERE id = ?1"
        ))?
        .query_row([id.0], smtp_row)
        .optional()?)
}

const SIGNATURE_COLUMNS: &str = "id, name, body_html, body_text, ordinal, created, updated";

fn signature_row(row: &Row<'_>) -> rusqlite::Result<Signature> {
    Ok(Signature {
        id: SignatureId(row.get(0)?),
        name: row.get(1)?,
        body_html: row.get(2)?,
        body_text: row.get(3)?,
        ordinal: row.get(4)?,
        created: row.get(5)?,
        updated: row.get(6)?,
    })
}

pub(crate) fn signatures(conn: &Connection) -> Result<Vec<Signature>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {SIGNATURE_COLUMNS} FROM signature ORDER BY ordinal, id"
    ))?;
    Ok(stmt
        .query_map([], signature_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn signature(conn: &Connection, id: SignatureId) -> Result<Option<Signature>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {SIGNATURE_COLUMNS} FROM signature WHERE id = ?1"
        ))?
        .query_row([id.0], signature_row)
        .optional()?)
}

pub(crate) fn ensure_changed(changed: usize, entity: &'static str, id: i64) -> Result<()> {
    if changed == 0 {
        Err(Error::not_found(entity, id))
    } else {
        Ok(())
    }
}

impl WriteTx<'_> {
    // ---- accounts ----------------------------------------------------------------

    /// Creates an account.
    pub fn create_account(&mut self, account: &NewAccount) -> Result<AccountId> {
        self.conn
            .prepare_cached(
                "INSERT INTO account (kind, name, config_json, ordinal) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                account.kind,
                account.name,
                account.config_json,
                account.ordinal
            ])?;
        let id = AccountId(self.conn.last_insert_rowid());
        self.changes.accounts.insert(id);
        Ok(id)
    }

    /// Renames an account, replaces its connector config, or moves it.
    pub fn update_account(
        &mut self,
        id: AccountId,
        name: &str,
        config_json: &str,
        ordinal: i64,
    ) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE account SET name = ?2, config_json = ?3, ordinal = ?4 WHERE id = ?1",
            )?
            .execute(params![id.0, name, config_json, ordinal])?;
        ensure_changed(changed, "account", id.0)?;
        self.changes.accounts.insert(id);
        Ok(())
    }

    /// Stores the connector's account-level sync state.
    pub fn set_account_sync_state(&mut self, id: AccountId, json: Option<&str>) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("UPDATE account SET sync_state_json = ?2 WHERE id = ?1")?
            .execute(params![id.0, json])?;
        ensure_changed(changed, "account", id.0)
    }

    /// Deletes an account with its folders, messages, threads, identities, rules and
    /// pending ops. Blob references are released (the blobs are collected later).
    pub fn delete_account(&mut self, id: AccountId) -> Result<()> {
        let messages: Vec<i64> = self
            .conn
            .prepare_cached("SELECT id FROM message WHERE account_id = ?1")?
            .query_map([id.0], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let folders: Vec<i64> = self
            .conn
            .prepare_cached("SELECT id FROM folder WHERE account_id = ?1")?
            .query_map([id.0], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let threads: Vec<i64> = self
            .conn
            .prepare_cached("SELECT id FROM thread WHERE account_id = ?1")?
            .query_map([id.0], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        // Children first, so no cascade has to do the work row by row.
        self.conn.execute(
            "DELETE FROM folder_thread WHERE folder_id IN (SELECT id FROM folder WHERE account_id = ?1)",
            [id.0],
        )?;
        self.conn
            .execute("DELETE FROM message WHERE account_id = ?1", [id.0])?;
        self.conn
            .execute("DELETE FROM thread WHERE account_id = ?1", [id.0])?;
        self.conn
            .execute("DELETE FROM folder WHERE account_id = ?1", [id.0])?;
        let changed = self
            .conn
            .execute("DELETE FROM account WHERE id = ?1", [id.0])?;
        ensure_changed(changed, "account", id.0)?;
        self.changes.accounts.insert(id);
        self.changes
            .messages_removed
            .extend(messages.into_iter().map(mazemail_types::MessageId));
        self.changes
            .folders
            .extend(folders.into_iter().map(FolderId));
        self.changes
            .threads_removed
            .extend(threads.into_iter().map(mazemail_types::ThreadId));
        self.changes.outbox = true;
        Ok(())
    }

    // ---- identities --------------------------------------------------------------

    /// Creates an identity.
    pub fn create_identity(
        &mut self,
        account: AccountId,
        input: &IdentityInput,
    ) -> Result<IdentityId> {
        self.conn
            .prepare_cached(
                "INSERT INTO identity (account_id, label, display_name, email, reply_to,
                   signature_id, smtp_id, sent_folder_id, drafts_folder_id, archive_folder_id,
                   archive_granularity, color, match_patterns_json, is_default, ordinal)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            )?
            .execute(params![
                account.0,
                input.label,
                input.display_name,
                input.email,
                input.reply_to,
                input.signature.map(|s| s.0),
                input.smtp.map(|s| s.0),
                input.sent_folder.map(|f| f.0),
                input.drafts_folder.map(|f| f.0),
                input.archive_folder.map(|f| f.0),
                input.archive_granularity,
                input.color,
                input.match_patterns_json,
                input.is_default,
                input.ordinal,
            ])?;
        let id = IdentityId(self.conn.last_insert_rowid());
        if input.is_default {
            self.make_default_identity(account, id)?;
        }
        self.changes.accounts.insert(account);
        self.changes.settings = true;
        Ok(id)
    }

    /// Replaces an identity's editable fields.
    pub fn update_identity(&mut self, id: IdentityId, input: &IdentityInput) -> Result<()> {
        let account: i64 = self
            .conn
            .prepare_cached("SELECT account_id FROM identity WHERE id = ?1")?
            .query_row([id.0], |row| row.get(0))
            .optional()?
            .ok_or_else(|| Error::not_found("identity", id))?;
        self.conn
            .prepare_cached(
                "UPDATE identity SET label = ?2, display_name = ?3, email = ?4, reply_to = ?5,
                   signature_id = ?6, smtp_id = ?7, sent_folder_id = ?8, drafts_folder_id = ?9,
                   archive_folder_id = ?10, archive_granularity = ?11, color = ?12,
                   match_patterns_json = ?13, is_default = ?14, ordinal = ?15
                 WHERE id = ?1",
            )?
            .execute(params![
                id.0,
                input.label,
                input.display_name,
                input.email,
                input.reply_to,
                input.signature.map(|s| s.0),
                input.smtp.map(|s| s.0),
                input.sent_folder.map(|f| f.0),
                input.drafts_folder.map(|f| f.0),
                input.archive_folder.map(|f| f.0),
                input.archive_granularity,
                input.color,
                input.match_patterns_json,
                input.is_default,
                input.ordinal,
            ])?;
        if input.is_default {
            self.make_default_identity(AccountId(account), id)?;
        }
        self.changes.accounts.insert(AccountId(account));
        self.changes.settings = true;
        Ok(())
    }

    fn make_default_identity(&mut self, account: AccountId, id: IdentityId) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE identity SET is_default = (id = ?2) WHERE account_id = ?1")?
            .execute(params![account.0, id.0])?;
        Ok(())
    }

    /// Deletes an identity and its rules.
    pub fn delete_identity(&mut self, id: IdentityId) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("DELETE FROM identity WHERE id = ?1")?
            .execute([id.0])?;
        ensure_changed(changed, "identity", id.0)?;
        self.changes.settings = true;
        Ok(())
    }

    /// Adds a rule that selects `identity`.
    pub fn add_identity_rule(
        &mut self,
        identity: IdentityId,
        kind: IdentityRuleKind,
        pattern: &str,
        ordinal: i64,
    ) -> Result<IdentityRuleId> {
        self.conn
            .prepare_cached(
                "INSERT INTO identity_rule (identity_id, kind, pattern, ordinal)
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![identity.0, kind, pattern, ordinal])?;
        self.changes.settings = true;
        Ok(IdentityRuleId(self.conn.last_insert_rowid()))
    }

    /// Deletes an identity rule.
    pub fn delete_identity_rule(&mut self, id: IdentityRuleId) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("DELETE FROM identity_rule WHERE id = ?1")?
            .execute([id.0])?;
        ensure_changed(changed, "identity rule", id.0)?;
        self.changes.settings = true;
        Ok(())
    }

    // ---- SMTP servers ------------------------------------------------------------

    /// Creates an SMTP server.
    pub fn create_smtp_server(&mut self, input: &SmtpServerInput) -> Result<SmtpServerId> {
        self.conn
            .prepare_cached(
                "INSERT INTO smtp_server (name, host, port, security, auth, username, saves_sent)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                input.name,
                input.host,
                input.port,
                input.security,
                input.auth,
                input.username,
                input.saves_sent
            ])?;
        self.changes.settings = true;
        Ok(SmtpServerId(self.conn.last_insert_rowid()))
    }

    /// Replaces an SMTP server's fields.
    pub fn update_smtp_server(&mut self, id: SmtpServerId, input: &SmtpServerInput) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE smtp_server SET name = ?2, host = ?3, port = ?4, security = ?5, auth = ?6,
                   username = ?7, saves_sent = ?8 WHERE id = ?1",
            )?
            .execute(params![
                id.0,
                input.name,
                input.host,
                input.port,
                input.security,
                input.auth,
                input.username,
                input.saves_sent
            ])?;
        ensure_changed(changed, "SMTP server", id.0)?;
        self.changes.settings = true;
        Ok(())
    }

    /// Deletes an SMTP server; identities using it lose their server.
    pub fn delete_smtp_server(&mut self, id: SmtpServerId) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("DELETE FROM smtp_server WHERE id = ?1")?
            .execute([id.0])?;
        ensure_changed(changed, "SMTP server", id.0)?;
        self.changes.settings = true;
        Ok(())
    }

    // ---- signatures --------------------------------------------------------------

    /// Creates a signature.
    pub fn create_signature(&mut self, input: &SignatureInput) -> Result<SignatureId> {
        self.conn
            .prepare_cached(
                "INSERT INTO signature (name, body_html, body_text, ordinal, created, updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            )?
            .execute(params![
                input.name,
                input.body_html,
                input.body_text,
                input.ordinal,
                self.now
            ])?;
        self.changes.settings = true;
        Ok(SignatureId(self.conn.last_insert_rowid()))
    }

    /// Replaces a signature's content.
    pub fn update_signature(&mut self, id: SignatureId, input: &SignatureInput) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE signature SET name = ?2, body_html = ?3, body_text = ?4, ordinal = ?5,
                   updated = ?6 WHERE id = ?1",
            )?
            .execute(params![
                id.0,
                input.name,
                input.body_html,
                input.body_text,
                input.ordinal,
                self.now
            ])?;
        ensure_changed(changed, "signature", id.0)?;
        self.changes.settings = true;
        Ok(())
    }

    /// Deletes a signature; identities using it lose their signature.
    pub fn delete_signature(&mut self, id: SignatureId) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("DELETE FROM signature WHERE id = ?1")?
            .execute([id.0])?;
        ensure_changed(changed, "signature", id.0)?;
        self.changes.settings = true;
        Ok(())
    }
}
