//! `virtualFolders.dat` (saved searches) and the translation of Thunderbird search
//! terms into Maze's query language (CONCEPT §13.2).
//!
//! Format (`nsMsgAccountManager::LoadVirtualFolders`): `version=1`, then per folder
//! `uri=`, optional `searchFolderFlag=`, `scope=` (folder URIs separated by `|`),
//! `terms=` (a filter condition) and `searchOnline=`.

use serde::Serialize;
use time::{Date, Month};

use super::filters::{BoolOp, Condition, Term, parse_condition};

/// One saved search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VirtualFolder {
    /// The virtual folder's own URI.
    pub uri: String,
    /// Its name (the last URI segment, decoded).
    pub name: String,
    /// The folders it searches.
    pub scope: Vec<String>,
    pub terms: Condition,
    pub search_online: bool,
}

/// Parses `virtualFolders.dat`.
pub fn parse(text: &str) -> Vec<VirtualFolder> {
    let mut out: Vec<VirtualFolder> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(uri) = line.strip_prefix("uri=") {
            out.push(VirtualFolder {
                uri: uri.to_owned(),
                name: folder_name_from_uri(uri),
                scope: Vec::new(),
                terms: Condition::default(),
                search_online: false,
            });
        } else if let Some(current) = out.last_mut() {
            if let Some(scope) = line.strip_prefix("scope=") {
                current.scope = scope
                    .split('|')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
            } else if let Some(terms) = line.strip_prefix("terms=") {
                current.terms = parse_condition(terms);
            } else if let Some(online) = line.strip_prefix("searchOnline=") {
                current.search_online = online == "true";
            }
        }
    }
    out
}

/// The decoded last path segment of a folder URI.
pub fn folder_name_from_uri(uri: &str) -> String {
    let segment = uri.trim_end_matches('/').rsplit('/').next().unwrap_or(uri);
    percent_decode(segment)
}

/// Decodes `%XX` escapes (invalid UTF-8 becomes U+FFFD).
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A Maze query for a Thunderbird condition, plus the terms that have no
/// equivalent (left out of the query, listed in the import report).
pub fn to_query(condition: &Condition) -> (String, Vec<String>) {
    if condition.match_all {
        return (String::new(), Vec::new());
    }
    let mut parts: Vec<(BoolOp, String)> = Vec::new();
    let mut untranslated = Vec::new();
    for term in &condition.terms {
        match translate(term) {
            Some(q) => parts.push((term.boolean, q)),
            None => untranslated.push(format!("{} {}", term.attribute, term.operator)),
        }
    }
    let mut query = String::new();
    for (i, (op, part)) in parts.iter().enumerate() {
        if i > 0 {
            query.push_str(if *op == BoolOp::Or { " OR " } else { " " });
        }
        query.push_str(part);
    }
    (query, untranslated)
}

fn quote(value: &str) -> String {
    if value.is_empty() || value.contains(|c: char| c.is_whitespace() || c == '"' || c == ':') {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

fn negated(op: &str) -> Option<bool> {
    match op {
        "contains" | "is" | "begins with" | "ends with" | "matches" => Some(false),
        "doesn't contain" | "isn't" | "doesn't match" => Some(true),
        _ => None,
    }
}

fn field(prefix: &str, op: &str, value: &str) -> Option<String> {
    let neg = negated(op)?;
    Some(format!(
        "{}{prefix}{}",
        if neg { "-" } else { "" },
        quote(value)
    ))
}

/// Thunderbird search dates: `02-Jan-2024`.
fn parse_tb_date(value: &str) -> Option<String> {
    let mut parts = value.trim().split('-');
    let day: u8 = parts.next()?.parse().ok()?;
    let month = parts.next()?.to_ascii_lowercase();
    let year: i32 = parts.next()?.parse().ok()?;
    let months = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let number = months.iter().position(|m| month.starts_with(m))? as u8 + 1;
    let date = Date::from_calendar_date(year, Month::try_from(number).ok()?, day).ok()?;
    Some(format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    ))
}

fn translate(term: &Term) -> Option<String> {
    let op = term.operator.as_str();
    let value = term.value.as_str();
    if term.custom_header {
        return term
            .attribute
            .eq_ignore_ascii_case("list-id")
            .then(|| field("list:", op, value))
            .flatten();
    }
    match term.attribute.as_str() {
        "subject" => field("subject:", op, value),
        "from" => field("from:", op, value),
        "to" => field("to:", op, value),
        "cc" => field("cc:", op, value),
        "to or cc" => {
            let neg = negated(op)?;
            let v = quote(value);
            Some(if neg {
                format!("-to:{v} -cc:{v}")
            } else {
                format!("(to:{v} OR cc:{v})")
            })
        }
        "all addresses" => {
            let neg = negated(op)?;
            let v = quote(value);
            Some(if neg {
                format!("-from:{v} -to:{v} -cc:{v}")
            } else {
                format!("(from:{v} OR to:{v} OR cc:{v})")
            })
        }
        "body" => field("", op, value),
        "tag" => field("tag:", op, &value.to_lowercase()),
        "status" => {
            let neg = negated(op)?;
            let positive = match value.to_ascii_lowercase().as_str() {
                "read" => "is:read",
                "flagged" => "is:starred",
                "replied" => "is:answered",
                "forwarded" => "is:forwarded",
                "new" => "is:unread",
                _ => return None,
            };
            Some(match (positive, neg) {
                ("is:read", true) => "is:unread".to_owned(),
                ("is:unread", true) => "is:read".to_owned(),
                (p, true) => format!("-{p}"),
                (p, false) => p.to_owned(),
            })
        }
        "has attachment status" => {
            let neg = negated(op)? ^ (value == "false" || value == "0");
            Some(
                if neg {
                    "-has:attachment"
                } else {
                    "has:attachment"
                }
                .to_owned(),
            )
        }
        "date" => {
            let date = parse_tb_date(value)?;
            match op {
                "is before" => Some(format!("before:{date}")),
                "is after" => Some(format!("after:{date}")),
                "is" => Some(format!("after:{date} before:{date}")),
                _ => None,
            }
        }
        "age in days" => {
            let days: u32 = value.trim().parse().ok()?;
            match op {
                "is greater than" => Some(format!("older_than:{days}d")),
                "is less than" => Some(format!("newer_than:{days}d")),
                _ => None,
            }
        }
        "size" => {
            let kb: u64 = value.trim().parse().ok()?;
            match op {
                "is greater than" => Some(format!("larger:{kb}K")),
                "is less than" => Some(format!("smaller:{kb}K")),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_and_translates() {
        let folders = parse(
            "version=1\n\
uri=mailbox://nobody@Local%20Folders/Unread%20Work\n\
scope=imap://me%40example.com@imap.example.com/INBOX|mailbox://nobody@Local%20Folders/Archive\n\
terms=AND (status,isn't,read) AND (from,contains,boss@example.com) AND (subject,doesn't contain,\"weekly report\")\n\
searchOnline=false\n\
uri=imap://me%40example.com@imap.example.com/Big\n\
scope=imap://me%40example.com@imap.example.com/INBOX\n\
terms=OR (size,is greater than,5000) OR (has attachment status,is,true) OR (junk status,is,2)\n\
searchOnline=true\n",
        );
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[0].name, "Unread Work");
        assert_eq!(folders[0].scope.len(), 2);
        let (q, missing) = to_query(&folders[0].terms);
        assert_eq!(
            q,
            "is:unread from:boss@example.com -subject:\"weekly report\""
        );
        assert!(missing.is_empty());
        let (q, missing) = to_query(&folders[1].terms);
        assert_eq!(q, "larger:5000K OR has:attachment");
        assert_eq!(missing, vec!["junk status is"]);
        assert!(folders[1].search_online);
    }

    #[test]
    fn dates_and_ages() {
        let c = parse_condition(
            "AND (date,is before,02-Jan-2024) AND (age in days,is greater than,30)",
        );
        assert_eq!(to_query(&c).0, "before:2024-01-02 older_than:30d");
        assert_eq!(to_query(&parse_condition("ALL")).0, "");
        assert_eq!(percent_decode("a%20b%C3%A4%zz"), "a bä%zz");
    }
}
