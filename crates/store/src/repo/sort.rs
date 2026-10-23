//! Lists sorted by any column of the message list (CONCEPT §8.2): one `ORDER BY` over
//! the keys of a list.
//!
//! The keyset queries ([`super::lists`]) read a scope in date order straight from an
//! index. Every other order sorts the keys the scope already produced (after the Quick
//! Filter), so the scope, the filters and global search results all sort the same way:
//! the ids go in as an `rarray`, each is joined by primary key to what the column shows,
//! and SQLite sorts the result in one temporary B-tree. No index can serve these orders:
//! they cross `folder_thread` → `thread` → `message`, and the scope decides which rows
//! take part.
//!
//! # What a thread sorts by
//!
//! A thread sorts by what its row shows, so a sorted column reads in order:
//!
//! - **Date**: its newest message in the context folder (else in any folder).
//! - **Star, Unread, Attachments**: whether any of its messages (in the context folder)
//!   is starred, unread, has attachments.
//! - **Subject**: the thread's subject (its first message's), normalized.
//! - **Sender, Recipients, Correspondents, Size, Received**: those of its newest
//!   message, as the row shows them.
//! - **Tags**: the names of every tag on any of its messages, in tag order.
//! - **Account**: the account's name.
//!
//! A message sorts by its own fields; its flags are those of its copy in the context
//! folder, else its effective flags.
//!
//! # Text
//!
//! Text is compared by Unicode code point after lowercasing. Subjects drop reply and
//! forward prefixes (`Re:`, `Fwd:`, `AW:`, `WG:`, …) and list tags
//! ([`mazemail_mime::subject::parse_subject`]); people sort by display name, else
//! address, as the list shows them. Empty text (no subject, no tags) sorts last in both
//! directions.
//! These keys come from SQL functions registered on every connection
//! ([`register_functions`]).
//!
//! Ties are broken by date (newest first), then by id, so the order is total.

use std::fmt::Write as _;
use std::rc::Rc;

use mazemail_types::{Address, FolderId, MessageId, ThreadId};
use rusqlite::functions::{Context, FunctionFlags};
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, ToSql};

use crate::Result;
use crate::model::{ListKey, ListOrder, MessageKey, SortField};
use crate::repo::lists::EFFECTIVE_FLAGS;
use crate::repo::threads::{int_array, text_array};

/// Registers the sort-key functions on a connection:
///
/// - `mz_subject_key(subject)`: the subject without reply/forward prefixes and list
///   tags, lowercased.
/// - `mz_name_key(name, email)`: the display name, else the address, lowercased.
/// - `mz_recipients_key(to_json, cc_json)`: the `To` recipients (else `Cc`) as the list
///   shows them, lowercased.
/// - `mz_fold(text)`: lowercased.
/// - `mz_email_key(email)`: lowercased, `+tag` sub-addressing dropped.
///
/// Each returns `NULL` for empty text, so `NULLS LAST` puts it last.
pub(crate) fn register_functions(conn: &Connection) -> rusqlite::Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8
        | FunctionFlags::SQLITE_DETERMINISTIC
        | FunctionFlags::SQLITE_INNOCUOUS;
    conn.create_scalar_function("mz_subject_key", 1, flags, |ctx| {
        Ok(text(ctx, 0).and_then(|s| fold(&mazemail_mime::subject::parse_subject(s).base)))
    })?;
    conn.create_scalar_function("mz_name_key", 2, flags, |ctx| {
        let name = text(ctx, 0).map(str::trim).filter(|n| !n.is_empty());
        Ok(name.or(text(ctx, 1)).and_then(fold))
    })?;
    conn.create_scalar_function("mz_recipients_key", 2, flags, |ctx| {
        let to = addresses(text(ctx, 0));
        let shown = if to.is_empty() {
            addresses(text(ctx, 1))
        } else {
            to
        };
        let names: Vec<&str> = shown.iter().map(display).collect();
        Ok(fold(&names.join(", ")))
    })?;
    conn.create_scalar_function("mz_fold", 1, flags, |ctx| Ok(text(ctx, 0).and_then(fold)))?;
    conn.create_scalar_function("mz_email_key", 1, flags, |ctx| {
        Ok(text(ctx, 0).and_then(email_key))
    })?;
    Ok(())
}

/// Argument `ix` if it is text (anything else has no sort text).
fn text<'a>(ctx: &'a Context<'_>, ix: usize) -> Option<&'a str> {
    match ctx.get_raw(ix) {
        ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok(),
        _ => None,
    }
}

/// Trimmed and lowercased; `None` if that leaves nothing.
fn fold(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_lowercase())
}

/// Addresses as the store keeps them (`to_json`); unreadable JSON is no addresses.
fn addresses(json: Option<&str>) -> Vec<Address> {
    json.and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default()
}

/// How the list shows a person (`mazemail_core::correspondents::display`).
fn display(address: &Address) -> &str {
    address
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&address.email)
}

/// An address as identities are compared (`mazemail_core::correspondents::normalize_email`).
fn email_key(email: &str) -> Option<String> {
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        return None;
    }
    Some(match email.split_once('@') {
        Some((local, domain)) => {
            let local = local.split_once('+').map_or(local, |(base, _)| base);
            format!("{local}@{domain}")
        }
        None => email,
    })
}

/// How a field compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    /// The date itself: date and id in the same direction, like the keyset queries.
    Date,
    /// A number or a truth value.
    Number,
    /// Text that can be empty (`NULL`, last).
    Text,
}

fn kind(field: SortField) -> KeyKind {
    match field {
        SortField::Date => KeyKind::Date,
        SortField::Received
        | SortField::Starred
        | SortField::Unread
        | SortField::Attachments
        | SortField::Size => KeyKind::Number,
        SortField::Subject
        | SortField::Correspondents
        | SortField::Sender
        | SortField::Recipients
        | SortField::Tags
        | SortField::Account => KeyKind::Text,
    }
}

/// Whether the key of `field` needs the message (a thread: its newest message).
fn needs_message(field: SortField) -> bool {
    matches!(
        field,
        SortField::Received
            | SortField::Correspondents
            | SortField::Sender
            | SortField::Recipients
            | SortField::Size
    )
}

/// The correspondents key of message `m`: its recipients if it is mail you sent (it has
/// a copy in a Sent, Drafts or Outbox folder, or its sender is one of the identities in
/// parameter `?3`), else its sender.
const CORRESPONDENTS: &str = "CASE WHEN EXISTS (SELECT 1 FROM message_location ol
                                  CROSS JOIN folder f ON f.id = ol.folder_id
                                WHERE ol.message_id = m.id
                                  AND f.role IN ('sent', 'drafts', 'outbox'))
            OR mz_email_key(m.from_addr) IN rarray(?3)
       THEN mz_recipients_key(m.to_json, m.cc_json)
       ELSE mz_name_key(m.from_name, m.from_addr) END";

/// The `ORDER BY` clause over the columns `sort_key`, `sort_date` and `id`.
fn order_by(sql: &mut String, order: ListOrder, id: &str) {
    let dir = if order.descending { "DESC" } else { "ASC" };
    match kind(order.field) {
        KeyKind::Date => {
            let _ = write!(sql, " ORDER BY sort_date {dir}, {id} {dir}");
        }
        KeyKind::Number | KeyKind::Text => {
            let _ = write!(
                sql,
                " ORDER BY sort_key {dir} NULLS LAST, sort_date DESC, {id} DESC"
            );
        }
    }
}

/// The parameters: the ids, the context folder and (for correspondents) the identities'
/// addresses.
fn params(
    conn: &Connection,
    ids: Rc<Vec<Value>>,
    context: Option<FolderId>,
    field: SortField,
) -> Result<Vec<Box<dyn ToSql>>> {
    let mut params: Vec<Box<dyn ToSql>> = vec![Box::new(ids), Box::new(context.map(|f| f.0))];
    if field == SortField::Correspondents {
        let mut stmt = conn.prepare_cached("SELECT email FROM identity")?;
        let emails: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .filter_map(|email| email_key(email))
            .collect();
        params.push(Box::new(text_array(emails.iter().map(String::as_str))));
    }
    Ok(params)
}

pub(crate) fn sorted_thread_keys(
    conn: &Connection,
    ids: &[ThreadId],
    context: Option<FolderId>,
    order: ListOrder,
) -> Result<Vec<ListKey>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // With a context folder, the thread's aggregates there (as its row shows them).
    let agg = |column: &str| match context {
        Some(_) => format!("COALESCE(ft.{column}, t.{column})"),
        None => format!("t.{column}"),
    };
    let key = match order.field {
        SortField::Date => agg("last_date"),
        SortField::Received => "COALESCE(m.received, m.date, 0)".to_owned(),
        SortField::Starred => format!("{} > 0", agg("flagged")),
        SortField::Unread => format!("{} > 0", agg("unread_count")),
        SortField::Attachments => format!("{} > 0", agg("attachment_count")),
        SortField::Subject => "mz_subject_key(t.subject)".to_owned(),
        SortField::Correspondents => CORRESPONDENTS.to_owned(),
        SortField::Sender => "mz_name_key(m.from_name, m.from_addr)".to_owned(),
        SortField::Recipients => "mz_recipients_key(m.to_json, m.cc_json)".to_owned(),
        SortField::Tags => "(SELECT mz_fold(group_concat(tg.name, ', ' ORDER BY tg.id))
              FROM tag tg WHERE tg.id IN (SELECT mt.tag_id FROM message tm
                CROSS JOIN message_tag mt ON mt.message_id = tm.id
                WHERE tm.thread_id = t.id))"
            .to_owned(),
        SortField::Account => {
            "(SELECT mz_fold(name) FROM account WHERE id = t.account_id)".to_owned()
        }
        SortField::Size => "COALESCE(m.size, 0)".to_owned(),
    };
    let mut sql = format!(
        "SELECT t.id, {} AS sort_date, {key} AS sort_key
         FROM rarray(?1) x
         CROSS JOIN thread t ON t.id = x.value",
        agg("last_date")
    );
    if context.is_some() {
        sql.push_str(" LEFT JOIN folder_thread ft ON ft.folder_id = ?2 AND ft.thread_id = t.id");
    }
    if needs_message(order.field) {
        sql.push_str(" LEFT JOIN message m ON m.id = t.last_message_id");
    }
    // Binds `?2` when no join uses it.
    sql.push_str(" WHERE ?2 IS ?2");
    order_by(&mut sql, order, "t.id");

    let params = params(
        conn,
        int_array(ids.iter().map(|t| t.0)),
        context,
        order.field,
    )?;
    let refs: Vec<&dyn ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let keys = stmt
        .query_map(refs.as_slice(), |row| {
            Ok(ListKey {
                thread: ThreadId(row.get(0)?),
                key: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(keys)
}

pub(crate) fn sorted_message_keys(
    conn: &Connection,
    ids: &[MessageId],
    context: Option<FolderId>,
    order: ListOrder,
) -> Result<Vec<MessageKey>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // Flags of the copy in the context folder, else the effective flags.
    let flags = format!(
        "COALESCE((SELECT flags FROM message_location WHERE folder_id = ?2 AND message_id = m.id),
                  (SELECT {EFFECTIVE_FLAGS} FROM message_location ml WHERE ml.message_id = m.id),
                  0)"
    );
    let key = match order.field {
        SortField::Date => "m.date".to_owned(),
        SortField::Received => "COALESCE(m.received, m.date)".to_owned(),
        SortField::Starred => format!("({flags} & 4) <> 0"),
        SortField::Unread => format!("({flags} & 1) = 0"),
        SortField::Attachments => "m.has_attachments".to_owned(),
        SortField::Subject => "mz_subject_key(m.subject)".to_owned(),
        SortField::Correspondents => CORRESPONDENTS.to_owned(),
        SortField::Sender => "mz_name_key(m.from_name, m.from_addr)".to_owned(),
        SortField::Recipients => "mz_recipients_key(m.to_json, m.cc_json)".to_owned(),
        SortField::Tags => "(SELECT mz_fold(group_concat(tg.name, ', ' ORDER BY tg.id))
              FROM message_tag mt CROSS JOIN tag tg ON tg.id = mt.tag_id
              WHERE mt.message_id = m.id)"
            .to_owned(),
        SortField::Account => {
            "(SELECT mz_fold(name) FROM account WHERE id = m.account_id)".to_owned()
        }
        SortField::Size => "COALESCE(m.size, 0)".to_owned(),
    };
    let mut sql = format!(
        "SELECT m.id, m.date AS sort_date, {key} AS sort_key
         FROM rarray(?1) x
         CROSS JOIN message m ON m.id = x.value
         WHERE ?2 IS ?2" // binds `?2` when the key doesn't use it
    );
    order_by(&mut sql, order, "m.id");

    let params = params(
        conn,
        int_array(ids.iter().map(|m| m.0)),
        context,
        order.field,
    )?;
    let refs: Vec<&dyn ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let keys = stmt
        .query_map(refs.as_slice(), |row| {
            Ok(MessageKey {
                message: MessageId(row.get(0)?),
                key: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        register_functions(&conn).unwrap();
        conn
    }

    fn eval(conn: &Connection, sql: &str) -> Option<String> {
        conn.query_row(&format!("SELECT {sql}"), [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn subject_keys_drop_prefixes_and_case() {
        let c = conn();
        for (subject, key) in [
            ("'Re: AW: Invoice March'", Some("invoice march")),
            ("'Fwd: [dev] WG: Release'", Some("release")),
            ("'Ärger'", Some("ärger")),
            ("'  Re:  '", None),
            ("NULL", None),
        ] {
            assert_eq!(
                eval(&c, &format!("mz_subject_key({subject})")).as_deref(),
                key
            );
        }
    }

    #[test]
    fn name_keys_prefer_the_display_name() {
        let c = conn();
        assert_eq!(
            eval(&c, "mz_name_key(' Anna Example ', 'anna@example.com')").as_deref(),
            Some("anna example")
        );
        assert_eq!(
            eval(&c, "mz_name_key('  ', 'Bob@Example.com')").as_deref(),
            Some("bob@example.com")
        );
        assert_eq!(eval(&c, "mz_name_key(NULL, NULL)"), None);
    }

    #[test]
    fn recipient_keys_follow_the_column() {
        let c = conn();
        let to =
            r#"'[{"name":"Zoe","email":"z@example.com"},{"name":null,"email":"Al@example.com"}]'"#;
        let cc = r#"'[{"name":"Cora","email":"c@example.com"}]'"#;
        assert_eq!(
            eval(&c, &format!("mz_recipients_key({to}, {cc})")).as_deref(),
            Some("zoe, al@example.com")
        );
        assert_eq!(
            eval(&c, &format!("mz_recipients_key(NULL, {cc})")).as_deref(),
            Some("cora"),
            "Cc when there is no To"
        );
        assert_eq!(eval(&c, "mz_recipients_key(NULL, 'not json')"), None);
    }

    #[test]
    fn email_keys_match_identities_like_the_core() {
        let c = conn();
        assert_eq!(
            eval(&c, "mz_email_key('Philip+Lists@Example.com')").as_deref(),
            Some("philip@example.com")
        );
        assert_eq!(eval(&c, "mz_email_key('')"), None);
    }
}
