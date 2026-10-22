//! Folders, Gmail label folders, sync state and counts.

use std::collections::HashMap;

use mazemail_types::{AccountId, FolderId, FolderRole, ThreadId};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::{Folder, FolderNode, FolderPatch, FolderSyncState, NewFolder};
use crate::repo::accounts::ensure_changed;
use crate::{Error, Result, WriteTx};

const FOLDER_COLUMNS: &str = "id, account_id, parent_id, remote_name, display_name, role, \
     is_label, uidvalidity, uidnext, highestmodseq, sync_policy, unread, total, ordinal, favorite";

fn role_from_sql(role: Option<&str>) -> Option<FolderRole> {
    role.and_then(FolderRole::parse)
}

fn folder_row(row: &Row<'_>) -> rusqlite::Result<Folder> {
    Ok(Folder {
        id: FolderId(row.get(0)?),
        account: AccountId(row.get(1)?),
        parent: row.get::<_, Option<i64>>(2)?.map(FolderId),
        remote_name: row.get(3)?,
        display_name: row.get(4)?,
        role: role_from_sql(row.get_ref(5)?.as_str_or_null()?),
        is_label: row.get(6)?,
        sync: FolderSyncState {
            uidvalidity: row.get(7)?,
            uidnext: row.get(8)?,
            highestmodseq: row.get::<_, Option<i64>>(9)?.map(|v| v as u64),
        },
        sync_policy: row.get(10)?,
        unread: row.get::<_, i64>(11)?.max(0) as u64,
        total: row.get::<_, i64>(12)?.max(0) as u64,
        ordinal: row.get(13)?,
        favorite: row.get(14)?,
    })
}

pub(crate) fn folders(conn: &Connection, account: Option<AccountId>) -> Result<Vec<Folder>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {FOLDER_COLUMNS} FROM folder WHERE ?1 IS NULL OR account_id = ?1
         ORDER BY account_id, ordinal, display_name, id"
    ))?;
    Ok(stmt
        .query_map([account.map(|a| a.0)], folder_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn folder(conn: &Connection, id: FolderId) -> Result<Option<Folder>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {FOLDER_COLUMNS} FROM folder WHERE id = ?1"
        ))?
        .query_row([id.0], folder_row)
        .optional()?)
}

pub(crate) fn folder_by_remote_name(
    conn: &Connection,
    account: AccountId,
    remote_name: &str,
) -> Result<Option<Folder>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {FOLDER_COLUMNS} FROM folder WHERE account_id = ?1 AND remote_name = ?2"
        ))?
        .query_row(params![account.0, remote_name], folder_row)
        .optional()?)
}

pub(crate) fn folders_with_role(
    conn: &Connection,
    role: FolderRole,
    account: Option<AccountId>,
) -> Result<Vec<Folder>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {FOLDER_COLUMNS} FROM folder WHERE role = ?1 AND (?2 IS NULL OR account_id = ?2)
         ORDER BY account_id, id"
    ))?;
    Ok(stmt
        .query_map(params![role.as_str(), account.map(|a| a.0)], folder_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Builds the folder tree of an account (or of all accounts), children sorted by
/// ordinal and name.
pub(crate) fn folder_tree(
    conn: &Connection,
    account: Option<AccountId>,
) -> Result<Vec<FolderNode>> {
    let all = folders(conn, account)?;
    let mut children: HashMap<Option<FolderId>, Vec<Folder>> = HashMap::new();
    let ids: std::collections::HashSet<FolderId> = all.iter().map(|f| f.id).collect();
    for folder in all {
        // A parent outside the result (or dangling) makes the folder a root.
        let parent = folder.parent.filter(|p| ids.contains(p));
        children.entry(parent).or_default().push(folder);
    }
    fn build(
        parent: Option<FolderId>,
        children: &mut HashMap<Option<FolderId>, Vec<Folder>>,
        depth: usize,
    ) -> Vec<FolderNode> {
        let Some(folders) = children.remove(&parent) else {
            return Vec::new();
        };
        folders
            .into_iter()
            .map(|folder| {
                // Depth bound guards against a parent cycle written by a buggy caller.
                let kids = if depth < 64 {
                    build(Some(folder.id), children, depth + 1)
                } else {
                    Vec::new()
                };
                FolderNode {
                    folder,
                    children: kids,
                }
            })
            .collect()
    }
    Ok(build(None, &mut children, 0))
}

impl WriteTx<'_> {
    /// Creates a folder (or a Gmail label folder).
    pub fn create_folder(&mut self, folder: &NewFolder) -> Result<FolderId> {
        if let Some(parent) = folder.parent {
            self.check_folder_account(parent, folder.account)?;
        }
        self.conn
            .prepare_cached(
                "INSERT INTO folder (account_id, parent_id, remote_name, display_name, role,
                   is_label, sync_policy, ordinal)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                folder.account.0,
                folder.parent.map(|p| p.0),
                folder.remote_name,
                folder.display_name,
                folder.role.map(FolderRole::as_str),
                folder.is_label,
                folder.sync_policy,
                folder.ordinal,
            ])?;
        let id = FolderId(self.conn.last_insert_rowid());
        self.changes.folders.insert(id);
        self.changes.accounts.insert(folder.account);
        Ok(id)
    }

    /// Changes a folder's editable attributes.
    pub fn update_folder(&mut self, id: FolderId, patch: &FolderPatch) -> Result<()> {
        let current = folder(self.conn, id)?.ok_or_else(|| Error::not_found("folder", id))?;
        if let Some(Some(parent)) = patch.parent {
            self.check_folder_account(parent, current.account)?;
            if self.is_descendant(parent, id)? {
                return Err(Error::Invalid("a folder can't move into its own subtree"));
            }
        }
        let parent = patch.parent.unwrap_or(current.parent);
        let remote_name = patch.remote_name.clone().unwrap_or(current.remote_name);
        let display_name = patch.display_name.clone().unwrap_or(current.display_name);
        let role = patch.role.unwrap_or(current.role);
        let sync_policy = patch.sync_policy.clone().unwrap_or(current.sync_policy);
        self.conn
            .prepare_cached(
                "UPDATE folder SET parent_id = ?2, remote_name = ?3, display_name = ?4,
                   role = ?5, sync_policy = ?6, ordinal = ?7, favorite = ?8 WHERE id = ?1",
            )?
            .execute(params![
                id.0,
                parent.map(|p| p.0),
                remote_name,
                display_name,
                role.map(FolderRole::as_str),
                sync_policy,
                patch.ordinal.unwrap_or(current.ordinal),
                patch.favorite.unwrap_or(current.favorite),
            ])?;
        self.changes.folders.insert(id);
        self.changes.accounts.insert(current.account);
        Ok(())
    }

    fn check_folder_account(&self, folder: FolderId, account: AccountId) -> Result<()> {
        let owner: Option<i64> = self
            .conn
            .prepare_cached("SELECT account_id FROM folder WHERE id = ?1")?
            .query_row([folder.0], |row| row.get(0))
            .optional()?;
        match owner {
            None => Err(Error::not_found("folder", folder)),
            Some(owner) if owner != account.0 => {
                Err(Error::Invalid("folder belongs to another account"))
            }
            Some(_) => Ok(()),
        }
    }

    /// True if `candidate` is `root` or inside its subtree.
    fn is_descendant(&self, candidate: FolderId, root: FolderId) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .prepare_cached(
                "WITH RECURSIVE sub(id) AS (
                   SELECT ?1 UNION SELECT f.id FROM folder f JOIN sub ON f.parent_id = sub.id)
                 SELECT id FROM sub WHERE id = ?2",
            )?
            .query_row(params![root.0, candidate.0], |row| row.get(0))
            .optional()?;
        Ok(found.is_some())
    }

    /// Stores the IMAP sync state of a folder.
    pub fn set_folder_sync_state(&mut self, id: FolderId, state: FolderSyncState) -> Result<()> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE folder SET uidvalidity = ?2, uidnext = ?3, highestmodseq = ?4 WHERE id = ?1",
            )?
            .execute(params![
                id.0,
                state.uidvalidity,
                state.uidnext,
                state.highestmodseq.map(|v| v as i64)
            ])?;
        ensure_changed(changed, "folder", id.0)
    }

    /// The server changed the folder's UIDVALIDITY: drops all of its locations and
    /// stores the new value (UIDNEXT and HIGHESTMODSEQ are cleared). Messages left
    /// without any location are detached, not deleted, so their bodies stay and a
    /// re-sync reattaches them by Message-ID.
    pub fn reset_folder(&mut self, id: FolderId, uidvalidity: u32) -> Result<usize> {
        let messages = self.folder_message_ids(id)?;
        let removed = self.remove_locations_of(id, &messages)?;
        self.conn
            .prepare_cached(
                "UPDATE folder SET uidvalidity = ?2, uidnext = NULL, highestmodseq = NULL
                 WHERE id = ?1",
            )?
            .execute(params![id.0, uidvalidity])?;
        self.changes.folders.insert(id);
        Ok(removed)
    }

    /// Deletes a folder and its subfolders. Messages that were only there are detached.
    pub fn delete_folder(&mut self, id: FolderId) -> Result<()> {
        let account: i64 = self
            .conn
            .prepare_cached("SELECT account_id FROM folder WHERE id = ?1")?
            .query_row([id.0], |row| row.get(0))
            .optional()?
            .ok_or_else(|| Error::not_found("folder", id))?;
        let subtree: Vec<i64> = self
            .conn
            .prepare_cached(
                "WITH RECURSIVE sub(id) AS (
                   SELECT ?1 UNION SELECT f.id FROM folder f JOIN sub ON f.parent_id = sub.id)
                 SELECT id FROM sub",
            )?
            .query_map([id.0], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for folder in &subtree {
            let folder = FolderId(*folder);
            let messages = self.folder_message_ids(folder)?;
            self.remove_locations_of(folder, &messages)?;
            self.changes.folders.insert(folder);
        }
        self.conn
            .prepare_cached("DELETE FROM folder WHERE id = ?1")?
            .execute([id.0])?;
        self.changes.accounts.insert(AccountId(account));
        Ok(())
    }

    fn folder_message_ids(&self, folder: FolderId) -> Result<Vec<i64>> {
        Ok(self
            .conn
            .prepare_cached("SELECT message_id FROM message_location WHERE folder_id = ?1")?
            .query_map([folder.0], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Removes the locations of `messages` in `folder` in bulk, detaching messages left
    /// without a location and marking their threads dirty.
    fn remove_locations_of(&mut self, folder: FolderId, messages: &[i64]) -> Result<usize> {
        // The folder's list rows go away wholesale; the thread flush recomputes the
        // rest (and finds no rows left for this folder).
        let threads: Vec<i64> = self
            .conn
            .prepare_cached("SELECT thread_id FROM folder_thread WHERE folder_id = ?1")?
            .query_map([folder.0], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let removed = self
            .conn
            .prepare_cached("DELETE FROM message_location WHERE folder_id = ?1")?
            .execute([folder.0])?;
        self.conn
            .prepare_cached("DELETE FROM folder_thread WHERE folder_id = ?1")?
            .execute([folder.0])?;
        self.conn
            .prepare_cached("UPDATE folder SET unread = 0, total = 0 WHERE id = ?1")?
            .execute([folder.0])?;
        let entry = self.changes.folder_threads.entry(folder).or_default();
        entry.extend(threads.iter().map(|t| ThreadId(*t)));
        self.dirty_threads.extend(threads.into_iter().map(ThreadId));
        let mut detach = self.conn.prepare_cached(
            "UPDATE message SET detached_at = ?2 WHERE id = ?1 AND detached_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM message_location WHERE message_id = ?1)",
        )?;
        let mut retag = Vec::new();
        for message in messages {
            detach.execute(params![message, self.now])?;
            retag.push(mazemail_types::MessageId(*message));
        }
        drop(detach);
        for message in retag {
            self.sync_message_tags(message)?;
            self.changes.messages.insert(message);
        }
        Ok(removed)
    }
}
