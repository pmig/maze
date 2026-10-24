//! Local drafts (PLAN §4.10): what the composer autosaves while you write.

use mazemail_types::compose::{Draft, DraftAttachment, DraftOrigin};
use mazemail_types::{Address, DraftId, IdentityId};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::StoredDraft;
use crate::repo::accounts::ensure_changed;
use crate::{Result, WriteTx};

const DRAFT_COLUMNS: &str = "id, identity_id, to_json, cc_json, bcc_json, subject, body, \
     in_reply_to, references_json, attachments_json, origin_json, created, updated, name, \
     support_maze";

fn json<T: serde::de::DeserializeOwned>(row: &Row<'_>, ix: usize) -> rusqlite::Result<T> {
    let text: String = row.get(ix)?;
    serde_json::from_str(&text).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(ix, rusqlite::types::Type::Text, err.into())
    })
}

fn draft_row(row: &Row<'_>) -> rusqlite::Result<StoredDraft> {
    let origin: Option<String> = row.get(10)?;
    let origin = origin
        .map(|text| serde_json::from_str::<DraftOrigin>(&text))
        .transpose()
        .map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, err.into())
        })?;
    Ok(StoredDraft {
        id: DraftId(row.get(0)?),
        draft: Draft {
            identity: row.get::<_, Option<i64>>(1)?.map(IdentityId),
            to: json::<Vec<Address>>(row, 2)?,
            cc: json::<Vec<Address>>(row, 3)?,
            bcc: json::<Vec<Address>>(row, 4)?,
            subject: row.get(5)?,
            body_markdown: row.get(6)?,
            in_reply_to: row.get(7)?,
            references: json::<Vec<String>>(row, 8)?,
            attachments: json::<Vec<DraftAttachment>>(row, 9)?,
            origin,
            name: row.get(13)?,
            support_maze: row.get(14)?,
        },
        created: row.get(11)?,
        updated: row.get(12)?,
    })
}

pub(crate) fn draft(conn: &Connection, id: DraftId) -> Result<Option<StoredDraft>> {
    Ok(conn
        .prepare_cached(&format!("SELECT {DRAFT_COLUMNS} FROM draft WHERE id = ?1"))?
        .query_row([id.0], draft_row)
        .optional()?)
}

pub(crate) fn drafts(conn: &Connection) -> Result<Vec<StoredDraft>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {DRAFT_COLUMNS} FROM draft ORDER BY updated DESC, id DESC"
    ))?;
    Ok(stmt
        .query_map([], draft_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// The draft's columns 2..=11 as SQL values.
struct Columns {
    to: String,
    cc: String,
    bcc: String,
    references: String,
    attachments: String,
    origin: Option<String>,
}

impl Columns {
    fn of(draft: &Draft) -> Result<Self> {
        Ok(Self {
            to: serde_json::to_string(&draft.to)?,
            cc: serde_json::to_string(&draft.cc)?,
            bcc: serde_json::to_string(&draft.bcc)?,
            references: serde_json::to_string(&draft.references)?,
            attachments: serde_json::to_string(&draft.attachments)?,
            origin: draft
                .origin
                .map(|o| serde_json::to_string(&o))
                .transpose()?,
        })
    }
}

impl WriteTx<'_> {
    /// Saves a new draft.
    pub fn create_draft(&mut self, draft: &Draft) -> Result<DraftId> {
        let c = Columns::of(draft)?;
        self.conn
            .prepare_cached(
                "INSERT INTO draft (identity_id, to_json, cc_json, bcc_json, subject, body,
                   in_reply_to, references_json, attachments_json, origin_json, created, updated,
                   name, support_maze)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12, ?13)",
            )?
            .execute(params![
                draft.identity.map(|i| i.0),
                c.to,
                c.cc,
                c.bcc,
                draft.subject,
                draft.body_markdown,
                draft.in_reply_to,
                c.references,
                c.attachments,
                c.origin,
                self.now,
                draft.name,
                draft.support_maze
            ])?;
        self.changes.drafts = true;
        Ok(DraftId(self.conn.last_insert_rowid()))
    }

    /// Replaces a draft's content.
    pub fn update_draft(&mut self, id: DraftId, draft: &Draft) -> Result<()> {
        let c = Columns::of(draft)?;
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE draft SET identity_id = ?2, to_json = ?3, cc_json = ?4, bcc_json = ?5,
                   subject = ?6, body = ?7, in_reply_to = ?8, references_json = ?9,
                   attachments_json = ?10, origin_json = ?11, updated = ?12, name = ?13,
                   support_maze = ?14
                 WHERE id = ?1",
            )?
            .execute(params![
                id.0,
                draft.identity.map(|i| i.0),
                c.to,
                c.cc,
                c.bcc,
                draft.subject,
                draft.body_markdown,
                draft.in_reply_to,
                c.references,
                c.attachments,
                c.origin,
                self.now,
                draft.name,
                draft.support_maze
            ])?;
        ensure_changed(changed, "draft", id.0)?;
        self.changes.drafts = true;
        Ok(())
    }

    /// Deletes a draft (sent or discarded).
    pub fn delete_draft(&mut self, id: DraftId) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached("DELETE FROM draft WHERE id = ?1")?
            .execute([id.0])?;
        ensure_changed(changed, "draft", id.0)?;
        self.changes.drafts = true;
        Ok(())
    }
}
