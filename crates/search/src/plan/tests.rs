use mazemail_types::{Flags, FolderRole};
use pretty_assertions::assert_eq;
use time::macros::{date, datetime, offset};

use super::*;
use crate::query::{Mailbox, parse};

fn context() -> PlanContext {
    PlanContext::new(datetime!(2026-03-15 12:00 +01:00), offset!(+01:00))
}

fn plan(input: &str) -> QueryPlan {
    QueryPlan::new(&parse(input).unwrap().query, &context()).unwrap()
}

fn text(scope: TextScope, word: &str) -> IndexExpr {
    IndexExpr::Term(IndexTerm::Text {
        scope,
        text: Text::Word(word.to_owned()),
    })
}

fn any(word: &str) -> IndexExpr {
    text(TextScope::Anywhere, word)
}

fn flag(flag: Flags, set: bool) -> MetadataFilter {
    MetadataFilter::Term(MetadataTerm::Flag { flag, set })
}

fn tag(name: &str) -> MetadataFilter {
    MetadataFilter::Term(MetadataTerm::Tag(name.to_owned()))
}

#[test]
fn empty_query_constrains_nothing() {
    let p = plan("");
    assert_eq!(p.text, None);
    assert!(p.metadata.is_all());
    assert!(p.mixed.is_empty());
    assert!(!p.uses_index() && !p.uses_metadata());
}

#[test]
fn conjunctions_split_by_side() {
    let p = plan("from:anna invoice is:unread tag:todo");
    assert_eq!(
        p.text,
        Some(TantivyPart::new(IndexExpr::And(vec![
            text(TextScope::From, "anna"),
            any("invoice"),
        ])))
    );
    assert_eq!(
        p.metadata,
        MetadataFilter::And(vec![flag(Flags::SEEN, false), tag("todo")])
    );
    assert!(p.mixed.is_empty());

    let p = plan("is:starred in:inbox");
    assert_eq!(p.text, None);
    assert_eq!(
        p.metadata,
        MetadataFilter::And(vec![
            flag(Flags::FLAGGED, true),
            MetadataFilter::Term(MetadataTerm::In(Mailbox::Role(FolderRole::Inbox))),
        ])
    );
}

#[test]
fn pure_disjunctions_stay_on_their_side() {
    let p = plan("(a OR from:b) (is:read OR tag:x)");
    assert_eq!(
        p.text,
        Some(TantivyPart::new(IndexExpr::Or(vec![
            any("a"),
            text(TextScope::From, "b")
        ])))
    );
    assert_eq!(
        p.metadata,
        MetadataFilter::Or(vec![flag(Flags::SEEN, true), tag("x")])
    );
    assert!(p.mixed.is_empty());
}

#[test]
fn mixed_disjunction_becomes_a_union_clause() {
    let p = plan("from:anna OR is:starred");
    assert_eq!(p.text, None);
    assert!(p.metadata.is_all());
    assert_eq!(
        p.mixed,
        vec![MixedClause {
            index: TantivyPart::new(text(TextScope::From, "anna")),
            metadata: flag(Flags::FLAGGED, true),
        }]
    );
}

#[test]
fn negation_is_pushed_to_the_leaves() {
    // -(A OR M) = -A AND -M: both sides separate again.
    let p = plan("-(from:anna OR is:starred)");
    assert_eq!(
        p.text,
        Some(TantivyPart::new(IndexExpr::Not(Box::new(text(
            TextScope::From,
            "anna"
        )))))
    );
    assert_eq!(p.metadata, flag(Flags::FLAGGED, false));
    assert!(p.mixed.is_empty());

    // -(A AND M) = -A OR -M: one mixed clause.
    let p = plan("-(invoice is:unread)");
    assert_eq!(
        p.mixed,
        vec![MixedClause {
            index: TantivyPart::new(IndexExpr::Not(Box::new(any("invoice")))),
            metadata: flag(Flags::SEEN, true),
        }]
    );

    let p = plan("-tag:x");
    assert_eq!(p.metadata, MetadataFilter::Not(Box::new(tag("x"))));
}

#[test]
fn or_distributes_over_mixed_and() {
    // (a AND unread) OR b = (a OR b) AND (unread OR b).
    let p = plan("(a is:unread) OR b");
    assert_eq!(
        p.text,
        Some(TantivyPart::new(IndexExpr::Or(vec![any("a"), any("b")])))
    );
    assert_eq!(
        p.mixed,
        vec![MixedClause {
            index: TantivyPart::new(any("b")),
            metadata: flag(Flags::SEEN, false),
        }]
    );
}

#[test]
fn too_many_clauses_are_refused() {
    let query = (0..7)
        .map(|i| format!("(w{i} is:unread)"))
        .collect::<Vec<_>>()
        .join(" OR ");
    assert_eq!(
        QueryPlan::new(&parse(&query).unwrap().query, &context()),
        Err(PlanError::TooComplex)
    );
}

#[test]
fn dates_resolve_against_the_context() {
    let p = plan("after:2026-01-01 before:2026-02-01");
    // Midnight at +01:00.
    let jan = date!(2026 - 01 - 01)
        .midnight()
        .assume_offset(offset!(+01:00));
    let feb = date!(2026 - 02 - 01)
        .midnight()
        .assume_offset(offset!(+01:00));
    assert_eq!(
        p.text,
        Some(TantivyPart::new(IndexExpr::And(vec![
            IndexExpr::Term(IndexTerm::Date {
                start: Some(jan.unix_timestamp()),
                end: None
            }),
            IndexExpr::Term(IndexTerm::Date {
                start: None,
                end: Some(feb.unix_timestamp())
            }),
        ])))
    );

    let p = plan("older_than:1m");
    let month_ago = datetime!(2026-02-15 12:00 +01:00).unix_timestamp();
    assert_eq!(
        p.text,
        Some(TantivyPart::new(IndexExpr::Term(IndexTerm::Date {
            start: None,
            end: Some(month_ago)
        })))
    );

    let p = plan("newer_than:1y larger:1k smaller:2M");
    let Some(TantivyPart {
        expr: IndexExpr::And(items),
    }) = p.text
    else {
        panic!("expected a conjunction")
    };
    assert_eq!(
        items[0],
        IndexExpr::Term(IndexTerm::Date {
            start: Some(datetime!(2025-03-15 12:00 +01:00).unix_timestamp()),
            end: None
        })
    );
    assert_eq!(
        items[1],
        IndexExpr::Term(IndexTerm::Size {
            larger_than: Some(1024),
            smaller_than: None
        })
    );
}

#[test]
fn calendar_months_clamp_the_day() {
    let context = PlanContext::new(datetime!(2026-03-31 08:00 UTC), offset!(UTC));
    let age = crate::query::Age {
        count: 1,
        unit: crate::query::AgeUnit::Months,
    };
    assert_eq!(context.ago(age).unwrap(), datetime!(2026-02-28 08:00 UTC));
    let age = crate::query::Age {
        count: 14,
        unit: crate::query::AgeUnit::Months,
    };
    assert_eq!(context.ago(age).unwrap(), datetime!(2025-01-31 08:00 UTC));
}

struct Row {
    flags: Flags,
    tags: Vec<&'static str>,
    folder: Mailbox,
    account: &'static str,
}

impl MessageMetadata for Row {
    fn in_mailbox(&self, mailbox: &Mailbox) -> bool {
        &self.folder == mailbox
    }
    fn has_tag(&self, tag: &str) -> bool {
        self.tags.contains(&tag)
    }
    fn flags(&self) -> Flags {
        self.flags
    }
    fn in_account(&self, account: &str) -> bool {
        self.account == account
    }
}

#[test]
fn metadata_filter_reference_evaluation() {
    let row = Row {
        flags: Flags::FLAGGED,
        tags: vec!["todo"],
        folder: Mailbox::Role(FolderRole::Inbox),
        account: "work",
    };
    let matches = |input: &str| plan(input).metadata.matches(&row);
    assert!(matches(
        "is:unread is:starred tag:todo in:inbox account:work"
    ));
    assert!(!matches("is:read"));
    assert!(matches("-tag:later (in:sent OR in:inbox)"));
    assert!(!matches("account:home"));
}
