//! Translating a [`Query`] for server-side search (CONCEPT §13.3): IMAP `SEARCH`
//! criteria (RFC 9051 §6.4.4) and Gmail's `X-GM-RAW`.
//!
//! Not every term has a server equivalent (`in:` is the selected mailbox on IMAP,
//! `account:` is the server itself, IMAP has no `has:attachment`). Such terms are
//! dropped in a way that keeps the server result a **superset** of the exact answer:
//! the query is put into negation normal form first and each untranslatable literal,
//! negated or not, becomes "match everything". [`ServerQuery::exact`] is `false` when
//! that happened or when a term was approximated (day granularity), so the caller knows
//! to filter the returned messages locally.

use std::fmt::Write as _;

use time::Date;

use super::ast::{Mailbox, Query, Status, Term, Text};
use super::display::{is_bare_value, write_quoted};
use crate::plan::PlanContext;
use mazemail_types::FolderRole;

/// A query translated for a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerQuery {
    /// The criteria. For IMAP this is the part after `SEARCH` (`ALL` when nothing
    /// constrains); for Gmail the `X-GM-RAW` argument before IMAP quoting (empty when
    /// nothing constrains).
    pub query: String,
    /// Whether the server result is exactly the query's result. When `false` it is a
    /// superset and the caller filters locally.
    pub exact: bool,
    /// Whether the criteria contain non-ASCII text. IMAP then needs `CHARSET UTF-8`
    /// (with literals) or `UTF8=ACCEPT`.
    pub utf8: bool,
}

impl Query {
    /// IMAP `SEARCH` criteria for this query. Dates use `SENTBEFORE`/`SENTSINCE`, so they
    /// match the `Date:` header like the local index; tags become `KEYWORD`s verbatim.
    pub fn to_imap_search(&self, context: &PlanContext) -> ServerQuery {
        let mut exact = true;
        let tree = lower(self, false, &mut |term| {
            imap_term(term, context, &mut exact)
        });
        let mut out = String::new();
        write_imap(&mut out, &tree.node, false);
        if out.is_empty() {
            out.push_str("ALL");
        }
        ServerQuery {
            utf8: !out.is_ascii(),
            query: out,
            exact: exact && tree.complete,
        }
    }

    /// The Gmail `X-GM-RAW` search string for this query. Dates become Unix timestamps
    /// (`after:1767225600`), which Gmail evaluates exactly; tags become `label:`.
    pub fn to_gmail_raw(&self, context: &PlanContext) -> ServerQuery {
        let tree = lower(self, false, &mut |term| gmail_term(term, context));
        let mut out = String::new();
        write_gmail(&mut out, &tree.node, false);
        ServerQuery {
            utf8: !out.is_ascii(),
            query: out,
            exact: tree.complete,
        }
    }
}

/// A query in negation normal form with every literal already rendered.
/// `And(vec![])` is "everything".
struct Lowered {
    node: Node,
    /// Whether no literal had to be dropped.
    complete: bool,
}

enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Lit { text: String, negated: bool },
}

fn lower(
    query: &Query,
    negated: bool,
    render: &mut impl FnMut(&Term) -> Option<String>,
) -> Lowered {
    match query {
        Query::Term(term) => match render(term) {
            Some(text) => Lowered {
                node: Node::Lit { text, negated },
                complete: true,
            },
            None => Lowered {
                node: Node::And(Vec::new()),
                complete: false,
            },
        },
        Query::Not(inner) => lower(inner, !negated, render),
        Query::And(items) | Query::Or(items) => {
            let conjunction = matches!(query, Query::And(_)) != negated;
            let mut complete = true;
            let mut nodes = Vec::with_capacity(items.len());
            for item in items {
                let lowered = lower(item, negated, render);
                complete &= lowered.complete;
                match lowered.node {
                    Node::And(inner) if inner.is_empty() => {
                        if !conjunction {
                            // `x OR everything` is everything.
                            return Lowered {
                                node: Node::And(Vec::new()),
                                complete,
                            };
                        }
                    }
                    node => nodes.push(node),
                }
            }
            let node = if conjunction || nodes.len() == 1 {
                if nodes.len() == 1 {
                    nodes.pop().unwrap_or(Node::And(Vec::new()))
                } else {
                    Node::And(nodes)
                }
            } else {
                Node::Or(nodes)
            };
            Lowered { node, complete }
        }
    }
}

fn write_imap(out: &mut String, node: &Node, nested: bool) {
    match node {
        Node::Lit { text, negated } => {
            if *negated {
                out.push_str("NOT ");
            }
            out.push_str(text);
        }
        Node::And(items) if items.is_empty() => {
            if nested {
                out.push_str("ALL");
            }
        }
        Node::And(items) => {
            let group = nested && items.len() > 1;
            if group {
                out.push('(');
            }
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                write_imap(out, item, true);
            }
            if group {
                out.push(')');
            }
        }
        Node::Or(items) => {
            // IMAP's OR is binary and prefix: `OR a OR b c` is a OR (b OR c).
            for (i, item) in items.iter().enumerate() {
                if i + 1 < items.len() {
                    out.push_str("OR ");
                }
                write_imap(out, item, true);
                if i + 1 < items.len() {
                    out.push(' ');
                }
            }
        }
    }
}

fn write_gmail(out: &mut String, node: &Node, nested: bool) {
    let (items, separator) = match node {
        Node::Lit { text, negated } => {
            if *negated {
                out.push('-');
            }
            out.push_str(text);
            return;
        }
        Node::And(items) => (items, " "),
        Node::Or(items) => (items, " OR "),
    };
    let group = nested && items.len() > 1;
    if group {
        out.push('(');
    }
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(separator);
        }
        write_gmail(out, item, true);
    }
    if group {
        out.push(')');
    }
}

/// An IMAP quoted string. CR and LF can't appear in one, so they become spaces.
fn imap_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\r' | '\n' => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// IMAP date: `1-Jan-2026`.
fn imap_date(date: Date) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS[usize::from(u8::from(date.month())) - 1];
    format!("{}-{}-{}", date.day(), month, date.year())
}

/// Whether `text` is a valid IMAP atom (RFC 9051 `atom`), for keywords.
fn is_imap_atom(text: &str) -> bool {
    !text.is_empty()
        && text.bytes().all(|b| {
            b.is_ascii_graphic()
                && !matches!(b, b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']')
        })
}

fn imap_term(term: &Term, context: &PlanContext, exact: &mut bool) -> Option<String> {
    Some(match term {
        Term::Text(text) => format!("TEXT {}", imap_string(text.as_str())),
        Term::From(text) => format!("FROM {}", imap_string(text.as_str())),
        Term::To(text) => format!("TO {}", imap_string(text.as_str())),
        Term::Cc(text) => format!("CC {}", imap_string(text.as_str())),
        Term::Subject(text) => format!("SUBJECT {}", imap_string(text.as_str())),
        Term::List(list) => format!("HEADER List-Id {}", imap_string(list)),
        Term::Tag(tag) if is_imap_atom(tag) => format!("KEYWORD {tag}"),
        Term::Is(Status::Unread) => "UNSEEN".to_owned(),
        Term::Is(Status::Read) => "SEEN".to_owned(),
        Term::Is(Status::Starred | Status::Flagged) => "FLAGGED".to_owned(),
        Term::Before(date) => format!("SENTBEFORE {}", imap_date(*date)),
        Term::After(date) => format!("SENTSINCE {}", imap_date(*date)),
        Term::OlderThan(age) => {
            // IMAP compares whole days; the caller refines by time.
            *exact = false;
            let day = context.ago(*age).ok()?.to_offset(context.offset()).date();
            format!("SENTBEFORE {}", imap_date(day))
        }
        Term::NewerThan(age) => {
            *exact = false;
            let day = context.ago(*age).ok()?.to_offset(context.offset()).date();
            format!("SENTSINCE {}", imap_date(day))
        }
        Term::Larger(size) => format!("LARGER {}", size.bytes()),
        Term::Smaller(size) => format!("SMALLER {}", size.bytes()),
        // No IMAP equivalent: the mailbox is chosen by SELECT, the account by the
        // connection, and attachments aren't searchable.
        Term::Tag(_) | Term::Filename(_) | Term::HasAttachment | Term::In(_) | Term::Account(_) => {
            return None;
        }
    })
}

fn gmail_text(out: &mut String, text: &Text) {
    match text {
        Text::Word(word) if is_bare_value(word) => out.push_str(word),
        other => {
            // Writing to a String can't fail.
            let _ = write_quoted(out, other.as_str());
        }
    }
}

fn gmail_value(operator: &str, value: &str) -> String {
    let mut out = String::from(operator);
    if is_bare_value(value) {
        out.push_str(value);
    } else {
        let _ = write_quoted(&mut out, value);
    }
    out
}

fn gmail_term(term: &Term, context: &PlanContext) -> Option<String> {
    let mut out = String::new();
    let text_operator = |name: &str, text: &Text| {
        let mut out = String::from(name);
        gmail_text(&mut out, text);
        out
    };
    match term {
        Term::Text(text) => {
            if text.as_str().starts_with('-') && !text.is_phrase() {
                let _ = write_quoted(&mut out, text.as_str());
            } else {
                gmail_text(&mut out, text);
            }
        }
        Term::From(text) => out = text_operator("from:", text),
        Term::To(text) => out = text_operator("to:", text),
        Term::Cc(text) => out = text_operator("cc:", text),
        Term::Subject(text) => out = text_operator("subject:", text),
        Term::Filename(text) => out = text_operator("filename:", text),
        Term::HasAttachment => out.push_str("has:attachment"),
        Term::Tag(tag) => out = gmail_value("label:", tag),
        Term::Is(Status::Unread) => out.push_str("is:unread"),
        Term::Is(Status::Read) => out.push_str("is:read"),
        Term::Is(Status::Starred | Status::Flagged) => out.push_str("is:starred"),
        Term::In(Mailbox::Role(role)) => match role {
            FolderRole::Inbox => out.push_str("in:inbox"),
            FolderRole::Sent => out.push_str("in:sent"),
            FolderRole::Drafts => out.push_str("in:drafts"),
            FolderRole::Trash => out.push_str("in:trash"),
            FolderRole::Junk => out.push_str("in:spam"),
            FolderRole::Flagged => out.push_str("is:starred"),
            FolderRole::Important => out.push_str("is:important"),
            // Gmail has no archive folder, and All Mail is what X-GM-RAW searches anyway.
            FolderRole::Archive | FolderRole::All => return None,
        },
        Term::In(Mailbox::Name(name)) => out = gmail_value("label:", name),
        Term::Account(_) => return None,
        Term::List(list) => out = gmail_value("list:", list),
        Term::Before(date) => {
            let _ = write!(out, "before:{}", context.start_of_day(*date));
        }
        Term::After(date) => {
            let _ = write!(out, "after:{}", context.start_of_day(*date));
        }
        Term::OlderThan(age) => {
            let _ = write!(out, "before:{}", context.ago(*age).ok()?.unix_timestamp());
        }
        Term::NewerThan(age) => {
            let _ = write!(out, "after:{}", context.ago(*age).ok()?.unix_timestamp());
        }
        Term::Larger(size) => {
            let _ = write!(out, "larger:{}", size.bytes());
        }
        Term::Smaller(size) => {
            let _ = write!(out, "smaller:{}", size.bytes());
        }
    }
    Some(out)
}
