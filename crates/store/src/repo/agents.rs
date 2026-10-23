//! API tokens, proposals, the audit log and agent conversations.

use mazemail_types::{BlobId, ThreadId};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::blob::StagedBlob;
use crate::model::{
    AgentThread, AgentThreadId, ApiToken, AuditEntry, AuditId, AuditQuery, NewApiToken,
    NewProposalItem, Proposal, ProposalId, ProposalItem, ProposalItemId, ProposalItemState,
    ProposalState, TokenId,
};
use crate::repo::accounts::ensure_changed;
use crate::{Error, Result, WriteTx};

/// The hash stored for a token secret: BLAKE3 of its text. Tokens are 256-bit random,
/// so a fast unkeyed hash is enough; the database never holds the secret itself.
pub fn token_hash(secret: &str) -> [u8; 32] {
    *blake3::hash(secret.as_bytes()).as_bytes()
}

const TOKEN_COLUMNS: &str =
    "id, hash, profile, scope_json, expires, label, created, last_used, revoked";

fn token_row(row: &Row<'_>) -> rusqlite::Result<ApiToken> {
    Ok(ApiToken {
        id: TokenId(row.get(0)?),
        hash: row.get(1)?,
        profile: row.get(2)?,
        scope_json: row.get(3)?,
        expires: row.get(4)?,
        label: row.get(5)?,
        created: row.get(6)?,
        last_used: row.get(7)?,
        revoked: row.get(8)?,
    })
}

pub(crate) fn api_tokens(conn: &Connection) -> Result<Vec<ApiToken>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {TOKEN_COLUMNS} FROM api_token ORDER BY id"
    ))?;
    Ok(stmt
        .query_map([], token_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// The token whose secret is `secret` (hashed here), valid or not; check
/// [`ApiToken::is_valid_at`].
pub(crate) fn api_token_by_secret(conn: &Connection, secret: &str) -> Result<Option<ApiToken>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {TOKEN_COLUMNS} FROM api_token WHERE hash = ?1"
        ))?
        .query_row([token_hash(secret)], token_row)
        .optional()?)
}

/// The token whose stored hash is `hash`, valid or not. For callers that hash secrets
/// themselves (the local API uses a keyed BLAKE3 derivation, see `local_api::TokenHash`).
pub(crate) fn api_token_by_hash(conn: &Connection, hash: &[u8; 32]) -> Result<Option<ApiToken>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {TOKEN_COLUMNS} FROM api_token WHERE hash = ?1"
        ))?
        .query_row([hash.as_slice()], token_row)
        .optional()?)
}

/// One token by id.
pub(crate) fn api_token(conn: &Connection, id: TokenId) -> Result<Option<ApiToken>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {TOKEN_COLUMNS} FROM api_token WHERE id = ?1"
        ))?
        .query_row([id.0], token_row)
        .optional()?)
}

fn proposal_items(conn: &Connection, proposal: ProposalId) -> Result<Vec<ProposalItem>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, command_json, reason, state FROM proposal_item WHERE proposal_id = ?1
         ORDER BY id",
    )?;
    Ok(stmt
        .query_map([proposal.0], |row| {
            Ok(ProposalItem {
                id: ProposalItemId(row.get(0)?),
                command_json: row.get(1)?,
                reason: row.get(2)?,
                state: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

const PROPOSAL_COLUMNS: &str = "id, token_id, title, state, created, decided";

fn proposal_row(row: &Row<'_>) -> rusqlite::Result<Proposal> {
    Ok(Proposal {
        id: ProposalId(row.get(0)?),
        token: row.get::<_, Option<i64>>(1)?.map(TokenId),
        title: row.get(2)?,
        state: row.get(3)?,
        created: row.get(4)?,
        decided: row.get(5)?,
        items: Vec::new(),
    })
}

pub(crate) fn proposal(conn: &Connection, id: ProposalId) -> Result<Option<Proposal>> {
    let found = conn
        .prepare_cached(&format!(
            "SELECT {PROPOSAL_COLUMNS} FROM proposal WHERE id = ?1"
        ))?
        .query_row([id.0], proposal_row)
        .optional()?;
    let Some(mut proposal) = found else {
        return Ok(None);
    };
    proposal.items = proposal_items(conn, id)?;
    Ok(Some(proposal))
}

/// Proposals, newest first; `state` filters.
pub(crate) fn proposals(conn: &Connection, state: Option<ProposalState>) -> Result<Vec<Proposal>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {PROPOSAL_COLUMNS} FROM proposal WHERE ?1 IS NULL OR state = ?1
         ORDER BY created DESC, id DESC"
    ))?;
    let mut out: Vec<Proposal> = stmt
        .query_map([state.map(ProposalState::as_str)], proposal_row)?
        .collect::<rusqlite::Result<_>>()?;
    for proposal in &mut out {
        proposal.items = proposal_items(conn, proposal.id)?;
    }
    Ok(out)
}

pub(crate) fn audit_log(conn: &Connection, query: &AuditQuery) -> Result<Vec<AuditEntry>> {
    let limit = if query.limit == 0 { 100 } else { query.limit };
    let (origin_exact, origin_prefix) = match query.origin.as_deref() {
        Some(origin) if origin.ends_with(':') => (None, Some(format!("{origin}%"))),
        Some(origin) => (Some(origin.to_owned()), None),
        None => (None, None),
    };
    let mut stmt = conn.prepare_cached(
        "SELECT id, at, origin, action, detail_json FROM audit_log
         WHERE (?1 IS NULL OR at >= ?1) AND (?2 IS NULL OR id < ?2)
           AND (?3 IS NULL OR origin = ?3) AND (?4 IS NULL OR origin LIKE ?4)
         ORDER BY id DESC LIMIT ?5",
    )?;
    Ok(stmt
        .query_map(
            params![
                query.since,
                query.before_id.map(|a| a.0),
                origin_exact,
                origin_prefix,
                limit as i64
            ],
            |row| {
                Ok(AuditEntry {
                    id: AuditId(row.get(0)?),
                    at: row.get(1)?,
                    origin: row.get(2)?,
                    action: row.get(3)?,
                    detail_json: row.get(4)?,
                })
            },
        )?
        .collect::<rusqlite::Result<_>>()?)
}

const AGENT_THREAD_COLUMNS: &str =
    "id, agent_id, title, mail_thread_id, acp_session_id, transcript_blob, created, updated";

fn agent_thread_row(row: &Row<'_>) -> rusqlite::Result<AgentThread> {
    Ok(AgentThread {
        id: AgentThreadId(row.get(0)?),
        agent_id: row.get(1)?,
        title: row.get(2)?,
        mail_thread: row.get::<_, Option<i64>>(3)?.map(ThreadId),
        acp_session_id: row.get(4)?,
        transcript: row.get::<_, Option<[u8; 32]>>(5)?.map(BlobId),
        created: row.get(6)?,
        updated: row.get(7)?,
    })
}

pub(crate) fn agent_threads(conn: &Connection) -> Result<Vec<AgentThread>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {AGENT_THREAD_COLUMNS} FROM agent_thread ORDER BY updated DESC, id DESC"
    ))?;
    Ok(stmt
        .query_map([], agent_thread_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn agent_thread(conn: &Connection, id: AgentThreadId) -> Result<Option<AgentThread>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {AGENT_THREAD_COLUMNS} FROM agent_thread WHERE id = ?1"
        ))?
        .query_row([id.0], agent_thread_row)
        .optional()?)
}

impl WriteTx<'_> {
    // ---- tokens ------------------------------------------------------------------

    /// Stores a new token (its hash only; see [`token_hash`]).
    pub fn create_api_token(&mut self, token: &NewApiToken) -> Result<TokenId> {
        self.conn
            .prepare_cached(
                "INSERT INTO api_token (hash, profile, scope_json, expires, label, created)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                token.hash,
                token.profile,
                token.scope_json,
                token.expires,
                token.label,
                self.now
            ])?;
        self.changes.settings = true;
        Ok(TokenId(self.conn.last_insert_rowid()))
    }

    /// Records a use of the token.
    pub fn touch_api_token(&mut self, id: TokenId) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("UPDATE api_token SET last_used = ?2 WHERE id = ?1")?
            .execute(params![id.0, self.now])?;
        ensure_changed(changed, "API token", id.0)
    }

    /// Revokes a token (it stays listed, marked revoked).
    pub fn revoke_api_token(&mut self, id: TokenId) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("UPDATE api_token SET revoked = COALESCE(revoked, ?2) WHERE id = ?1")?
            .execute(params![id.0, self.now])?;
        ensure_changed(changed, "API token", id.0)?;
        self.changes.settings = true;
        Ok(())
    }

    /// Deletes revoked and expired tokens older than `before`.
    pub fn prune_api_tokens(&mut self, before: i64) -> Result<usize> {
        Ok(self
            .conn
            .prepare_cached(
                "DELETE FROM api_token WHERE (revoked IS NOT NULL AND revoked < ?1)
                   OR (expires IS NOT NULL AND expires < ?1)",
            )?
            .execute([before])?)
    }

    // ---- proposals ---------------------------------------------------------------

    /// Stores a proposal with its items (all pending).
    pub fn create_proposal(
        &mut self,
        token: Option<TokenId>,
        title: &str,
        items: &[NewProposalItem],
    ) -> Result<ProposalId> {
        if items.is_empty() {
            return Err(Error::Invalid("a proposal needs at least one item"));
        }
        self.conn
            .prepare_cached("INSERT INTO proposal (token_id, title, created) VALUES (?1, ?2, ?3)")?
            .execute(params![token.map(|t| t.0), title, self.now])?;
        let id = ProposalId(self.conn.last_insert_rowid());
        let mut insert = self.conn.prepare_cached(
            "INSERT INTO proposal_item (proposal_id, command_json, reason) VALUES (?1, ?2, ?3)",
        )?;
        for item in items {
            insert.execute(params![id.0, item.command_json, item.reason])?;
        }
        self.changes.proposals = true;
        Ok(id)
    }

    /// Sets one item's state.
    pub fn set_proposal_item_state(
        &mut self,
        item: ProposalItemId,
        state: ProposalItemState,
    ) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("UPDATE proposal_item SET state = ?2 WHERE id = ?1")?
            .execute(params![item.0, state])?;
        ensure_changed(changed, "proposal item", item.0)?;
        self.changes.proposals = true;
        Ok(())
    }

    /// Sets a proposal's state; any state but `Pending` records the decision time.
    pub fn set_proposal_state(&mut self, id: ProposalId, state: ProposalState) -> Result<()> {
        let decided = (state != ProposalState::Pending).then_some(self.now);
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE proposal SET state = ?2, decided = COALESCE(decided, ?3) WHERE id = ?1",
            )?
            .execute(params![id.0, state, decided])?;
        ensure_changed(changed, "proposal", id.0)?;
        self.changes.proposals = true;
        Ok(())
    }

    // ---- audit log ---------------------------------------------------------------

    /// Appends an audit entry. `detail_json` must not hold message content beyond ids
    /// (it is shown in the activity log).
    pub fn audit(
        &mut self,
        origin: &str,
        action: &str,
        detail_json: Option<&str>,
    ) -> Result<AuditId> {
        self.conn
            .prepare_cached(
                "INSERT INTO audit_log (at, origin, action, detail_json) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![self.now, origin, action, detail_json])?;
        Ok(AuditId(self.conn.last_insert_rowid()))
    }

    /// Deletes audit entries older than `before`.
    pub fn prune_audit_log(&mut self, before: i64) -> Result<usize> {
        Ok(self
            .conn
            .prepare_cached("DELETE FROM audit_log WHERE at < ?1")?
            .execute([before])?)
    }

    // ---- agent conversations -----------------------------------------------------

    /// Starts an agent conversation.
    pub fn create_agent_thread(
        &mut self,
        agent_id: &str,
        title: Option<&str>,
        mail_thread: Option<ThreadId>,
    ) -> Result<AgentThreadId> {
        self.conn
            .prepare_cached(
                "INSERT INTO agent_thread (agent_id, title, mail_thread_id, created, updated)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
            )?
            .execute(params![agent_id, title, mail_thread.map(|t| t.0), self.now])?;
        Ok(AgentThreadId(self.conn.last_insert_rowid()))
    }

    /// Updates title and ACP session id.
    pub fn update_agent_thread(
        &mut self,
        id: AgentThreadId,
        title: Option<&str>,
        acp_session_id: Option<&str>,
    ) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE agent_thread SET title = ?2, acp_session_id = ?3, updated = ?4 WHERE id = ?1",
            )?
            .execute(params![id.0, title, acp_session_id, self.now])?;
        ensure_changed(changed, "agent thread", id.0)
    }

    /// Replaces the stored transcript (an encrypted blob; the old one loses a
    /// reference).
    pub fn set_agent_transcript(
        &mut self,
        id: AgentThreadId,
        transcript: &StagedBlob,
    ) -> Result<()> {
        self.register_blob(transcript)?;
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE agent_thread SET transcript_blob = ?2, updated = ?3 WHERE id = ?1",
            )?
            .execute(params![id.0, &transcript.id().0, self.now])?;
        ensure_changed(changed, "agent thread", id.0)
    }

    /// Deletes an agent conversation (its transcript loses a reference).
    pub fn delete_agent_thread(&mut self, id: AgentThreadId) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("DELETE FROM agent_thread WHERE id = ?1")?
            .execute([id.0])?;
        ensure_changed(changed, "agent thread", id.0)
    }
}
