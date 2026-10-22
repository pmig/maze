//! Thread list keys (keyset pagination), metadata filters over search candidates, and
//! scope id sets (PLAN §4.3 "Thread list model", §4.4 "Mutable fields").

use std::collections::HashSet;
use std::fmt::Write as _;

use mazemail_types::{AccountId, FolderId, MessageId, TagId};
use rusqlite::types::Value;
use rusqlite::{Connection, ToSql};

use crate::Result;
use crate::model::{ListFilter, ListKey, ListScope, MessageScope, MetadataFilter, Page, SortOrder};
use crate::repo::folders::folders_with_role;
use crate::repo::threads::int_array;

/// Effective message flags over all locations: `\Seen` (bit 0) only if every location
/// has it, every other flag if any location has it.
const EFFECTIVE_FLAGS: &str = "(MIN(ml.flags & 1) | MAX(ml.flags & 2) | MAX(ml.flags & 4) \
     | MAX(ml.flags & 8) | MAX(ml.flags & 16) | MAX(ml.flags & 32) | MAX(ml.flags & 64) \
     | MAX(ml.flags & 128))";

pub(crate) fn thread_list_keys(
    conn: &Connection,
    scope: ListScope,
    sort: SortOrder,
    filter: &ListFilter,
    page: Page,
) -> Result<Vec<ListKey>> {
    if page.limit == 0 {
        return Ok(Vec::new());
    }
    match scope {
        ListScope::Folder(folder) => folder_keys(conn, folder, sort, filter, page),
        ListScope::Account(account) => account_keys(conn, account, sort, filter, page),
        ListScope::Unified(role) => {
            let folders = folders_with_role(conn, role, None)?;
            let mut merged: Vec<ListKey> = Vec::new();
            for folder in folders {
                merged.extend(folder_keys(conn, folder.id, sort, filter, page)?);
            }
            sort_keys(&mut merged, sort);
            // A thread can sit in two folders with the same role; keep its first entry.
            let mut seen = HashSet::with_capacity(merged.len());
            merged.retain(|k| seen.insert(k.thread));
            merged.truncate(page.limit);
            Ok(merged)
        }
    }
}

fn sort_keys(keys: &mut [ListKey], sort: SortOrder) {
    match sort {
        SortOrder::NewestFirst => {
            keys.sort_unstable_by_key(|k| std::cmp::Reverse((k.key, k.thread)))
        }
        SortOrder::OldestFirst => keys.sort_unstable_by_key(|k| (k.key, k.thread)),
    }
}

fn order(sort: SortOrder) -> (&'static str, &'static str) {
    match sort {
        SortOrder::NewestFirst => ("<", "DESC"),
        SortOrder::OldestFirst => (">", "ASC"),
    }
}

fn folder_keys(
    conn: &Connection,
    folder: FolderId,
    sort: SortOrder,
    filter: &ListFilter,
    page: Page,
) -> Result<Vec<ListKey>> {
    let (cmp, dir) = order(sort);
    let index = if filter.unread {
        "folder_thread_unread"
    } else if filter.flagged {
        "folder_thread_flagged"
    } else if filter.has_attachment {
        "folder_thread_attachments"
    } else {
        "folder_thread_list"
    };
    let mut sql = format!(
        "SELECT thread_id, last_date FROM folder_thread INDEXED BY {index} WHERE folder_id = ?1"
    );
    let mut args: Vec<Value> = vec![Value::Integer(folder.0)];
    if let Some(after) = page.after {
        args.push(Value::Integer(after.key));
        args.push(Value::Integer(after.thread.0));
        let _ = write!(sql, " AND (last_date, thread_id) {cmp} (?2, ?3)");
    }
    if filter.unread {
        sql.push_str(" AND unread_count > 0");
    }
    if filter.flagged {
        sql.push_str(" AND flagged > 0");
    }
    if filter.has_attachment {
        sql.push_str(" AND attachment_count > 0");
    }
    if let Some(tag) = filter.tag {
        args.push(Value::Integer(tag.0));
        let _ = write!(
            sql,
            " AND thread_id IN (SELECT m.thread_id FROM message_tag mt
                 CROSS JOIN message m ON m.id = mt.message_id
                 CROSS JOIN message_location ml ON ml.message_id = m.id AND ml.folder_id = ?1
                 WHERE mt.tag_id = ?{})",
            args.len()
        );
    }
    args.push(Value::Integer(page.limit as i64));
    let _ = write!(
        sql,
        " ORDER BY last_date {dir}, thread_id {dir} LIMIT ?{}",
        args.len()
    );
    query_keys(conn, &sql, &args)
}

fn account_keys(
    conn: &Connection,
    account: AccountId,
    sort: SortOrder,
    filter: &ListFilter,
    page: Page,
) -> Result<Vec<ListKey>> {
    let (cmp, dir) = order(sort);
    let index = if filter.unread {
        "thread_list_unread"
    } else if filter.flagged {
        "thread_list_flagged"
    } else if filter.has_attachment {
        "thread_list_attachments"
    } else {
        "thread_list"
    };
    let mut sql = format!(
        "SELECT id, last_date FROM thread INDEXED BY {index}
         WHERE account_id = ?1 AND message_count > 0"
    );
    let mut args: Vec<Value> = vec![Value::Integer(account.0)];
    if let Some(after) = page.after {
        args.push(Value::Integer(after.key));
        args.push(Value::Integer(after.thread.0));
        let _ = write!(sql, " AND (last_date, id) {cmp} (?2, ?3)");
    }
    if filter.unread {
        sql.push_str(" AND unread_count > 0");
    }
    if filter.flagged {
        sql.push_str(" AND flagged > 0");
    }
    if filter.has_attachment {
        sql.push_str(" AND attachment_count > 0");
    }
    if let Some(tag) = filter.tag {
        args.push(Value::Integer(tag.0));
        let _ = write!(
            sql,
            " AND id IN (SELECT m.thread_id FROM message_tag mt
                 CROSS JOIN message m ON m.id = mt.message_id
                 WHERE mt.tag_id = ?{} AND m.account_id = ?1 AND m.detached_at IS NULL)",
            args.len()
        );
    }
    args.push(Value::Integer(page.limit as i64));
    let _ = write!(
        sql,
        " ORDER BY last_date {dir}, id {dir} LIMIT ?{}",
        args.len()
    );
    query_keys(conn, &sql, &args)
}

fn query_keys(conn: &Connection, sql: &str, args: &[Value]) -> Result<Vec<ListKey>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let params: Vec<&dyn ToSql> = args.iter().map(|v| v as &dyn ToSql).collect();
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(ListKey {
            thread: mazemail_types::ThreadId(row.get(0)?),
            key: row.get(1)?,
        });
    }
    Ok(out)
}

/// Runs entirely on the `location_message (message_id, flags)` index and the
/// `message_tag` primary key, both compact, so 10k random candidates touch few pages:
/// every location of a message is in its account, so account and role conditions become
/// folder-id sets, and a message without locations (detached) never matches.
pub(crate) fn filter_message_ids(
    conn: &Connection,
    candidates: &[MessageId],
    filter: &MetadataFilter,
) -> Result<Vec<MessageId>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut params: Vec<Box<dyn ToSql>> = vec![Box::new(int_array(candidates.iter().map(|m| m.0)))];
    let mut sql = String::from(
        // CROSS JOIN: drive from the candidates, never from a scan.
        "SELECT ml.message_id FROM rarray(?1) x
         CROSS JOIN message_location ml INDEXED BY location_message ON ml.message_id = x.value
         GROUP BY ml.message_id HAVING 1",
    );
    // Pushes a parameter and returns its placeholder.
    let mut bind = |value: Box<dyn ToSql>| {
        params.push(value);
        format!("?{}", params.len())
    };

    if let Some(folders) = &filter.folders {
        let p = bind(Box::new(int_array(folders.iter().map(|f| f.0))));
        let _ = write!(sql, " AND max(ml.folder_id IN rarray({p}))");
    }
    if let Some(account) = filter.account {
        let p = bind(Box::new(account.0));
        let _ = write!(
            sql,
            " AND max(ml.folder_id IN (SELECT id FROM folder WHERE account_id = {p}))"
        );
    }
    if let Some(role) = filter.in_role {
        let p = bind(Box::new(role.as_str()));
        let _ = write!(
            sql,
            " AND max(ml.folder_id IN (SELECT id FROM folder WHERE role = {p}))"
        );
    }
    if !filter.flags_set.is_empty() || !filter.flags_unset.is_empty() {
        let set = bind(Box::new(i64::from(filter.flags_set.bits())));
        let unset = bind(Box::new(i64::from(filter.flags_unset.bits())));
        let _ = write!(
            sql,
            " AND ({EFFECTIVE_FLAGS} & {set}) = {set} AND ({EFFECTIVE_FLAGS} & {unset}) = 0"
        );
    }
    if !filter.tags.is_empty() {
        let tags: HashSet<TagId> = filter.tags.iter().copied().collect();
        let count = tags.len();
        let p = bind(Box::new(int_array(tags.into_iter().map(|t| t.0))));
        let _ = write!(
            sql,
            " AND (SELECT count(*) FROM message_tag mt
                 WHERE mt.message_id = ml.message_id AND mt.tag_id IN rarray({p})) = {count}"
        );
    }

    let mut stmt = conn.prepare_cached(&sql)?;
    let refs: Vec<&dyn ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let passing: HashSet<i64> = stmt
        .query_map(refs.as_slice(), |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(candidates
        .iter()
        .copied()
        .filter(|m| passing.contains(&m.0))
        .collect())
}

pub(crate) fn message_ids_in_scope(
    conn: &Connection,
    scope: &MessageScope,
) -> Result<Vec<MessageId>> {
    let collect = |sql: &str, param: &dyn ToSql| -> Result<Vec<MessageId>> {
        let mut stmt = conn.prepare_cached(sql)?;
        Ok(stmt
            .query_map([param], |row| row.get(0).map(MessageId))?
            .collect::<rusqlite::Result<_>>()?)
    };
    match scope {
        MessageScope::Folder(folder) => collect(
            "SELECT message_id FROM message_location WHERE folder_id = ?1 ORDER BY message_id",
            &folder.0,
        ),
        MessageScope::Folders(folders) => collect(
            "SELECT DISTINCT message_id FROM message_location
             WHERE folder_id IN rarray(?1) ORDER BY message_id",
            &int_array(folders.iter().map(|f| f.0)),
        ),
        MessageScope::Role(role) => collect(
            "SELECT DISTINCT message_id FROM message_location
             WHERE folder_id IN (SELECT id FROM folder WHERE role = ?1) ORDER BY message_id",
            &role.as_str(),
        ),
        MessageScope::Account(account) => collect(
            "SELECT id FROM message WHERE account_id = ?1 AND detached_at IS NULL ORDER BY id",
            &account.0,
        ),
        MessageScope::Thread(thread) => collect(
            "SELECT id FROM message WHERE thread_id = ?1 AND detached_at IS NULL ORDER BY id",
            &thread.0,
        ),
    }
}

/// How many of `ids` are in each folder (search facets over mutable fields).
pub(crate) fn folder_counts(conn: &Connection, ids: &[MessageId]) -> Result<Vec<(FolderId, u64)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ml.folder_id, count(*) FROM rarray(?1) x
         CROSS JOIN message_location ml ON ml.message_id = x.value
         GROUP BY ml.folder_id ORDER BY count(*) DESC, ml.folder_id",
    )?;
    Ok(stmt
        .query_map([int_array(ids.iter().map(|m| m.0))], |row| {
            Ok((FolderId(row.get(0)?), row.get::<_, i64>(1)?.max(0) as u64))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// How many of `ids` carry each tag.
pub(crate) fn tag_counts(conn: &Connection, ids: &[MessageId]) -> Result<Vec<(TagId, u64)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT mt.tag_id, count(*) FROM rarray(?1) x
         CROSS JOIN message_tag mt ON mt.message_id = x.value
         GROUP BY mt.tag_id ORDER BY count(*) DESC, mt.tag_id",
    )?;
    Ok(stmt
        .query_map([int_array(ids.iter().map(|m| m.0))], |row| {
            Ok((TagId(row.get(0)?), row.get::<_, i64>(1)?.max(0) as u64))
        })?
        .collect::<rusqlite::Result<_>>()?)
}
