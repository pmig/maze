//! The canonical string form of a [`Query`].
//!
//! For every query the parser returns, `parse(&query.to_string())` gives back the same
//! query (property-tested). Operators print in lowercase, dates as `YYYY-MM-DD`, and
//! values are quoted only when they have to be.

use std::fmt::{self, Write as _};

use super::ast::{Mailbox, Query, Term, Text};
use super::parse::Operator;

impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_query(f, self)
    }
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_term(f, self)
    }
}

fn write_query(f: &mut fmt::Formatter<'_>, query: &Query) -> fmt::Result {
    match query {
        Query::Term(term) => write_term(f, term),
        Query::And(items) => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    f.write_char(' ')?;
                }
                // OR binds tighter than juxtaposition, so only nested ANDs need parentheses.
                write_grouped(f, item, matches!(item, Query::And(_)))?;
            }
            Ok(())
        }
        Query::Or(items) => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    f.write_str(" OR ")?;
                }
                write_grouped(f, item, matches!(item, Query::And(_) | Query::Or(_)))?;
            }
            Ok(())
        }
        Query::Not(inner) => {
            f.write_char('-')?;
            write_grouped(f, inner, matches!(**inner, Query::And(_) | Query::Or(_)))
        }
    }
}

fn write_grouped(f: &mut fmt::Formatter<'_>, query: &Query, group: bool) -> fmt::Result {
    if group {
        f.write_char('(')?;
        write_query(f, query)?;
        f.write_char(')')
    } else {
        write_query(f, query)
    }
}

fn write_term(f: &mut fmt::Formatter<'_>, term: &Term) -> fmt::Result {
    match term {
        Term::Text(Text::Word(word)) if is_bare_word(word) => f.write_str(word),
        Term::Text(text) => write_quoted(f, text.as_str()),
        Term::From(text) => write_text_operator(f, "from", text),
        Term::To(text) => write_text_operator(f, "to", text),
        Term::Cc(text) => write_text_operator(f, "cc", text),
        Term::Subject(text) => write_text_operator(f, "subject", text),
        Term::Filename(text) => write_text_operator(f, "filename", text),
        Term::HasAttachment => f.write_str("has:attachment"),
        Term::Tag(tag) => write_value_operator(f, "tag", tag),
        Term::Is(status) => write!(f, "is:{}", status.as_str()),
        Term::In(Mailbox::Role(role)) => write!(f, "in:{}", role.as_str()),
        Term::In(Mailbox::Name(name)) => write_value_operator(f, "in", name),
        Term::Account(account) => write_value_operator(f, "account", account),
        Term::List(list) => write_value_operator(f, "list", list),
        Term::Before(date) => write!(f, "before:{}", format_date(*date)),
        Term::After(date) => write!(f, "after:{}", format_date(*date)),
        Term::OlderThan(age) => write!(f, "older_than:{}{}", age.count, age.unit.suffix()),
        Term::NewerThan(age) => write!(f, "newer_than:{}{}", age.count, age.unit.suffix()),
        Term::Larger(size) => write!(f, "larger:{}{}", size.value(), size.unit().suffix()),
        Term::Smaller(size) => write!(f, "smaller:{}{}", size.value(), size.unit().suffix()),
    }
}

fn write_text_operator(f: &mut fmt::Formatter<'_>, name: &str, text: &Text) -> fmt::Result {
    f.write_str(name)?;
    f.write_char(':')?;
    match text {
        Text::Word(word) if is_bare_value(word) => f.write_str(word),
        other => write_quoted(f, other.as_str()),
    }
}

fn write_value_operator(f: &mut fmt::Formatter<'_>, name: &str, value: &str) -> fmt::Result {
    f.write_str(name)?;
    f.write_char(':')?;
    if is_bare_value(value) {
        f.write_str(value)
    } else {
        write_quoted(f, value)
    }
}

pub(crate) fn write_quoted(f: &mut impl fmt::Write, text: &str) -> fmt::Result {
    f.write_char('"')?;
    for c in text.chars() {
        if matches!(c, '"' | '\\') {
            f.write_char('\\')?;
        }
        f.write_char(c)?;
    }
    f.write_char('"')
}

pub(crate) fn format_date(date: time::Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// Whether `value` can follow `operator:` without quotes.
pub(crate) fn is_bare_value(value: &str) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '"'))
}

/// Whether free text `word` reads back as the same word without quotes.
pub(crate) fn is_bare_word(word: &str) -> bool {
    if word == "-" {
        // A lone dash is text; the parser only negates when a term follows directly.
        return true;
    }
    if !is_bare_value(word) || word.starts_with('-') || word == "OR" || word == "AND" {
        return false;
    }
    // `from:x` written bare would parse as the operator.
    match word.split_once(':') {
        Some((name, _)) => {
            !(name.chars().all(|c| c.is_ascii_alphabetic() || c == '_')
                && !name.is_empty()
                && Operator::from_name(name).is_some())
        }
        None => true,
    }
}
