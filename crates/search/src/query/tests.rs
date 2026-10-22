use mazemail_types::FolderRole;
use pretty_assertions::assert_eq;
use proptest::prelude::*;
use time::macros::{date, datetime, offset};

use super::*;
use crate::plan::PlanContext;

fn q(input: &str) -> Query {
    let parsed = parse(input).unwrap_or_else(|err| panic!("{input:?}: {err}"));
    assert!(
        parsed.warnings.is_empty(),
        "{input:?}: {:?}",
        parsed.warnings
    );
    parsed.query
}

fn word(s: &str) -> Query {
    Query::Term(Term::Text(Text::Word(s.to_owned())))
}

fn phrase(s: &str) -> Query {
    Query::Term(Term::Text(Text::Phrase(s.to_owned())))
}

fn term(t: Term) -> Query {
    Query::Term(t)
}

fn err(input: &str) -> ParseError {
    parse(input).expect_err(input)
}

#[test]
fn empty_query_matches_everything() {
    assert!(q("").is_all());
    assert!(q("   \t ").is_all());
    assert_eq!(q("").to_string(), "");
}

#[test]
fn free_text_and_phrases() {
    assert_eq!(q("hello"), word("hello"));
    assert_eq!(
        q("hello world"),
        Query::And(vec![word("hello"), word("world")])
    );
    assert_eq!(q("\"quarterly report\""), phrase("quarterly report"));
    assert_eq!(q(r#""say \"hi\" \\o/""#), phrase(r#"say "hi" \o/"#));
    assert_eq!(q("Grüße"), word("Grüße"));
    assert_eq!(q("e-mail"), word("e-mail"));
}

#[test]
fn negation_or_and_grouping() {
    assert_eq!(q("-spam"), !word("spam"));
    assert_eq!(q("-\"a b\""), !phrase("a b"));
    assert_eq!(q("a OR b"), Query::Or(vec![word("a"), word("b")]));
    // OR binds tighter than juxtaposition, as in Gmail.
    assert_eq!(
        q("a b OR c"),
        Query::And(vec![word("a"), Query::Or(vec![word("b"), word("c")])])
    );
    assert_eq!(
        q("(a b) OR c"),
        Query::Or(vec![Query::And(vec![word("a"), word("b")]), word("c")])
    );
    assert_eq!(
        q("-(a OR b) c"),
        Query::And(vec![!Query::Or(vec![word("a"), word("b")]), word("c")])
    );
    assert_eq!(q("a AND b"), Query::And(vec![word("a"), word("b")]));
    assert_eq!(q("((a))"), word("a"));
    assert_eq!(
        q("a OR b OR c"),
        Query::Or(vec![word("a"), word("b"), word("c")])
    );
    assert_eq!(q("--a"), !!word("a"));
    // A lone dash is text.
    assert_eq!(
        q("a - b"),
        Query::And(vec![word("a"), word("-"), word("b")])
    );
    // Lowercase `or` is a word.
    assert_eq!(
        q("a or b"),
        Query::And(vec![word("a"), word("or"), word("b")])
    );
}

#[test]
fn concept_example_parses() {
    let query = q(concat!(
        r#"from:anna to:me subject:"offsite" has:attachment filename:pdf "#,
        "tag:todo is:unread is:starred in:inbox account:work list:dev@example.com ",
        "before:2026-01-01 after:2025-10-01 older_than:30d larger:5M -from:noreply (a OR b)"
    ));
    let Query::And(items) = &query else {
        panic!("expected AND, got {query:?}")
    };
    assert_eq!(items.len(), 17);
    assert_eq!(items[0], term(Term::From(Text::Word("anna".into()))));
    assert_eq!(
        items[2],
        term(Term::Subject(Text::Phrase("offsite".into())))
    );
    assert_eq!(items[3], term(Term::HasAttachment));
    assert_eq!(items[5], term(Term::Tag("todo".into())));
    assert_eq!(items[6], term(Term::Is(Status::Unread)));
    assert_eq!(items[8], term(Term::In(Mailbox::Role(FolderRole::Inbox))));
    assert_eq!(items[10], term(Term::List("dev@example.com".into())));
    assert_eq!(items[11], term(Term::Before(date!(2026 - 01 - 01))));
    assert_eq!(
        items[13],
        term(Term::OlderThan(Age {
            count: 30,
            unit: AgeUnit::Days
        }))
    );
    assert_eq!(
        items[14],
        term(Term::Larger(Size::new(5, SizeUnit::Mega).unwrap()))
    );
    assert_eq!(items[15], !term(Term::From(Text::Word("noreply".into()))));
}

#[test]
fn operator_values() {
    assert_eq!(q("FROM:Anna"), term(Term::From(Text::Word("Anna".into()))));
    assert_eq!(q("label:work"), term(Term::Tag("work".into())));
    assert_eq!(q("tag:\"to do\""), term(Term::Tag("to do".into())));
    assert_eq!(
        q("in:\"Sent Items\""),
        term(Term::In(Mailbox::Name("Sent Items".into())))
    );
    assert_eq!(
        q("in:Spam"),
        term(Term::In(Mailbox::Role(FolderRole::Junk)))
    );
    assert_eq!(q("is:FLAGGED"), term(Term::Is(Status::Flagged)));
    assert_eq!(
        q("after:2025/10/01"),
        term(Term::After(date!(2025 - 10 - 01)))
    );
    assert_eq!(
        q("smaller:200k"),
        term(Term::Smaller(Size::new(200, SizeUnit::Kilo).unwrap()))
    );
    assert_eq!(
        q("larger:1024"),
        term(Term::Larger(Size::new(1024, SizeUnit::Bytes).unwrap()))
    );
    assert_eq!(
        q("newer_than:2W"),
        term(Term::NewerThan(Age {
            count: 2,
            unit: AgeUnit::Weeks
        }))
    );
    assert_eq!(
        q("(from:anna)").to_string(),
        "from:anna",
        "a value stops at a parenthesis"
    );
}

#[test]
fn unknown_operators_are_text_with_a_warning() {
    let parsed = parse("hello foo:bar").unwrap();
    assert_eq!(
        parsed.query,
        Query::And(vec![word("hello"), word("foo:bar")])
    );
    assert_eq!(parsed.warnings.len(), 1);
    assert_eq!(parsed.warnings[0].span, 6..13);
    // Not an operator name at all: no warning.
    assert!(parse("10:30 a1:b").unwrap().warnings.is_empty());
}

#[test]
fn errors_are_positioned_and_friendly() {
    let e = err("from:");
    assert_eq!(e.span, 0..5);
    assert!(e.message.contains("needs a value"), "{}", e.message);

    let e = err("hello (world");
    assert_eq!(e.span, 6..7);
    assert!(e.message.contains("never closed"));

    let e = err("hello world)");
    assert_eq!(e.span, 11..12);

    let e = err("a \"open");
    assert_eq!(e.span, 2..3);

    let e = err("before:2026-13-01");
    assert_eq!(e.span, 7..17);
    assert!(e.message.contains("date"));

    let e = err("is:urgent");
    assert_eq!(e.span, 3..9);
    assert!(!e.message.contains("urgent"), "errors don't echo input");

    assert_eq!(err("has:pdf").span, 4..7);
    assert_eq!(err("older_than:30x").span, 11..14);
    assert_eq!(err("larger:5T").span, 7..9);
    assert!(err("larger:99999999999999G").message.contains("too large"));
    assert_eq!(err("OR a").span, 0..2);
    assert_eq!(err("a OR").span, 2..4);
    assert_eq!(err("a OR OR b").span, 2..4);
    assert_eq!(err("()").span, 0..2);
    assert_eq!(err("a \"\"").span, 2..4);
    assert_eq!(err("from:\"\"").span, 0..7);
    let deep = format!("{}a{}", "(".repeat(100), ")".repeat(100));
    assert!(err(&deep).message.contains("nested too deeply"));
}

#[test]
fn canonical_strings() {
    let cases = [
        ("a  b", "a b"),
        ("a OR (b c)", "a OR (b c)"),
        ("(a OR b) c", "a OR b c"),
        ("FROM:Anna", "from:Anna"),
        ("label:\"to do\"", "tag:\"to do\""),
        ("in:INBOX", "in:inbox"),
        ("after:2025/1/5", "after:2025-01-05"),
        ("-(a b)", "-(a b)"),
        ("-(a OR b)", "-(a OR b)"),
        ("\"OR\"", "\"OR\""),
        ("\"from:x\"", "\"from:x\""),
        ("from:OR", "from:OR"),
        ("subject:\"a\\\"b\"", "subject:\"a\\\"b\""),
    ];
    for (input, canonical) in cases {
        assert_eq!(q(input).to_string(), canonical, "{input}");
        assert_eq!(q(canonical), q(input), "{input}");
    }
}

fn context() -> PlanContext {
    PlanContext::new(datetime!(2026-03-15 12:00 UTC), offset!(UTC))
}

#[test]
fn imap_search_translation() {
    let s = q("from:anna subject:\"q1 report\" -is:read").to_imap_search(&context());
    assert_eq!(s.query, r#"FROM "anna" SUBJECT "q1 report" NOT SEEN"#);
    assert!(s.exact);

    let s = q("a OR b OR c").to_imap_search(&context());
    assert_eq!(s.query, r#"OR TEXT "a" OR TEXT "b" TEXT "c""#);

    let s = q("(a b) OR is:starred").to_imap_search(&context());
    assert_eq!(s.query, r#"OR (TEXT "a" TEXT "b") FLAGGED"#);

    let s = q("before:2026-01-01 after:2025-10-01 larger:5M smaller:200k tag:todo list:dev")
        .to_imap_search(&context());
    assert_eq!(
        s.query,
        r#"SENTBEFORE 1-Jan-2026 SENTSINCE 1-Oct-2025 LARGER 5242880 SMALLER 204800 KEYWORD todo HEADER List-Id "dev""#
    );
    assert!(s.exact);

    // Untranslatable terms widen the result, never narrow it.
    let s = q("in:inbox has:attachment").to_imap_search(&context());
    assert_eq!(s.query, "ALL");
    assert!(!s.exact);
    let s = q("a -has:attachment").to_imap_search(&context());
    assert_eq!(s.query, r#"TEXT "a""#);
    assert!(!s.exact);
    let s = q("a OR filename:pdf").to_imap_search(&context());
    assert_eq!(s.query, "ALL");

    let s = q("older_than:2w").to_imap_search(&context());
    assert_eq!(s.query, "SENTBEFORE 1-Mar-2026");
    assert!(!s.exact, "day granularity");

    let s = q("Grüße").to_imap_search(&context());
    assert!(s.utf8);
    assert_eq!(q("").to_imap_search(&context()).query, "ALL");
}

#[test]
fn gmail_raw_translation() {
    let s = q("from:anna tag:\"to do\" is:flagged in:junk in:Projects (a OR b) -c")
        .to_gmail_raw(&context());
    assert_eq!(
        s.query,
        r#"from:anna label:"to do" is:starred in:spam label:Projects (a OR b) -c"#
    );
    assert!(s.exact);

    let s = q("after:2026-01-01 older_than:1m larger:5M").to_gmail_raw(&context());
    assert_eq!(s.query, "after:1767225600 before:1771156800 larger:5242880");

    let s = q("account:work in:archive x").to_gmail_raw(&context());
    assert_eq!(s.query, "x");
    assert!(!s.exact);

    let s = q("(a b) OR c").to_gmail_raw(&context());
    assert_eq!(s.query, "(a b) OR c");
}

// --- Property tests -------------------------------------------------------------------

fn arb_word() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9äöüß@._+][a-zA-Z0-9äöüß@._+:-]{0,10}"
        .prop_filter("bare word", |w| display::is_bare_word(w))
}

fn arb_phrase() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 äöü\"\\\\:()-]{1,12}".prop_filter("not blank", |p| !p.trim().is_empty())
}

fn arb_bare_value() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9@._/:-]{1,10}"
}

fn arb_value() -> impl Strategy<Value = String> {
    prop_oneof![arb_bare_value(), arb_phrase()]
}

fn arb_text() -> impl Strategy<Value = Text> {
    prop_oneof![
        arb_bare_value().prop_map(Text::Word),
        arb_phrase().prop_map(Text::Phrase),
    ]
}

fn arb_term() -> impl Strategy<Value = Term> {
    let role = prop::sample::select(vec![
        FolderRole::Inbox,
        FolderRole::Sent,
        FolderRole::Drafts,
        FolderRole::Trash,
        FolderRole::Junk,
        FolderRole::Archive,
        FolderRole::All,
        FolderRole::Flagged,
        FolderRole::Important,
    ]);
    let age_unit = prop::sample::select(vec![
        AgeUnit::Days,
        AgeUnit::Weeks,
        AgeUnit::Months,
        AgeUnit::Years,
    ]);
    let size_unit = prop::sample::select(vec![
        SizeUnit::Bytes,
        SizeUnit::Kilo,
        SizeUnit::Mega,
        SizeUnit::Giga,
    ]);
    let status = prop::sample::select(vec![
        Status::Unread,
        Status::Read,
        Status::Starred,
        Status::Flagged,
    ]);
    let date = (1970i32..2100, 1u8..=12, 1u8..=28).prop_map(|(y, m, d)| {
        time::Date::from_calendar_date(y, time::Month::try_from(m).unwrap(), d).unwrap()
    });
    prop_oneof![
        arb_word().prop_map(|w| Term::Text(Text::Word(w))),
        arb_phrase().prop_map(|p| Term::Text(Text::Phrase(p))),
        arb_text().prop_map(Term::From),
        arb_text().prop_map(Term::To),
        arb_text().prop_map(Term::Cc),
        arb_text().prop_map(Term::Subject),
        arb_text().prop_map(Term::Filename),
        Just(Term::HasAttachment),
        arb_value().prop_map(Term::Tag),
        status.prop_map(Term::Is),
        role.prop_map(|r| Term::In(Mailbox::Role(r))),
        arb_value()
            .prop_filter("not a role", |v| matches!(
                Mailbox::parse(v),
                Mailbox::Name(_)
            ))
            .prop_map(|v| Term::In(Mailbox::Name(v))),
        arb_value().prop_map(Term::Account),
        arb_value().prop_map(Term::List),
        date.clone().prop_map(Term::Before),
        date.prop_map(Term::After),
        (0u32..1000, age_unit.clone())
            .prop_map(|(count, unit)| Term::OlderThan(Age { count, unit })),
        (0u32..1000, age_unit).prop_map(|(count, unit)| Term::NewerThan(Age { count, unit })),
        (0u64..10_000, size_unit.clone()).prop_map(|(v, u)| Term::Larger(Size::new(v, u).unwrap())),
        (0u64..10_000, size_unit).prop_map(|(v, u)| Term::Smaller(Size::new(v, u).unwrap())),
    ]
}

fn arb_query() -> impl Strategy<Value = Query> {
    let leaf = arb_term().prop_map(Query::Term);
    leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 1..4).prop_map(Query::And),
            prop::collection::vec(inner.clone(), 1..4).prop_map(Query::Or),
            inner.prop_map(|q| !q),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// AST → canonical string → AST gives back the (normalized) AST.
    #[test]
    fn ast_round_trips_through_canonical_string(query in arb_query()) {
        let query = query.normalize();
        let text = query.to_string();
        let parsed = parse(&text).map_err(|e| TestCaseError::fail(format!("{text:?}: {e}")))?;
        prop_assert_eq!(&parsed.query, &query, "canonical form {:?}", text);
        // The canonical form is a fixed point.
        prop_assert_eq!(parsed.query.to_string(), text);
    }

    /// The parser never panics, and whatever it accepts round-trips.
    #[test]
    fn arbitrary_input_never_panics(input in "[ a-zA-Z0-9:()\"\\\\_-]{0,40}") {
        if let Ok(parsed) = parse(&input) {
            let canonical = parsed.query.to_string();
            let again = parse(&canonical).map_err(|e| TestCaseError::fail(format!("{canonical:?}: {e}")))?;
            prop_assert_eq!(again.query, parsed.query);
        }
    }

    /// Errors always point inside the input.
    #[test]
    fn error_spans_are_in_bounds(input in "\\PC{0,30}") {
        if let Err(error) = parse(&input) {
            prop_assert!(error.span.start <= error.span.end);
            prop_assert!(error.span.end <= input.len());
        }
    }
}
