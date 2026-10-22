use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::time::Duration;

use mazemail_types::{AccountId, Address, Flags, FolderRole, MessageId, Sensitive, ThreadId};
use pretty_assertions::assert_eq;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use time::macros::{datetime, offset};

use super::*;
use crate::plan::{MessageMetadata, MetadataFilter, PlanContext, QueryPlan, TantivyPart};
use crate::query::{Mailbox, Query, Status, Term, Text, parse};

/// 2026-01-01T00:00:00Z
const BASE: i64 = 1_767_225_600;
const DAY: i64 = 86_400;

fn context() -> PlanContext {
    PlanContext::new(datetime!(2026-06-01 00:00 UTC), offset!(UTC))
}

fn doc(id: i64) -> IndexDoc {
    IndexDoc {
        message_id: MessageId(id),
        thread_id: ThreadId(id),
        account_id: AccountId(1),
        date: BASE + id * DAY,
        size: 10_000,
        from: Vec::new(),
        to: Vec::new(),
        cc: Vec::new(),
        subject: Sensitive::default(),
        body: Sensitive::default(),
        attachments: Vec::new(),
        list_id: None,
        language: None,
    }
}

fn with_body(id: i64, body: &str, language: Option<Language>) -> IndexDoc {
    IndexDoc {
        body: Sensitive::new(body.to_owned()),
        language,
        ..doc(id)
    }
}

fn build(docs: &[IndexDoc]) -> SearchIndex {
    let index = SearchIndex::in_memory().unwrap();
    let mut indexer = index.indexer(&IndexOptions::default()).unwrap();
    for doc in docs {
        indexer.add(doc).unwrap();
    }
    indexer.finish().unwrap();
    index
}

fn part(input: &str) -> TantivyPart {
    let plan = QueryPlan::new(&parse(input).unwrap().query, &context()).unwrap();
    assert!(plan.metadata.is_all() && plan.mixed.is_empty(), "{input}");
    plan.text.unwrap_or_else(TantivyPart::everything)
}

fn ids(index: &SearchIndex, input: &str) -> Vec<i64> {
    let options = SearchOptions {
        limit: 1000,
        ..SearchOptions::default()
    };
    let mut ids: Vec<i64> = index
        .searcher()
        .search(&part(input), &options)
        .unwrap()
        .into_iter()
        .map(|hit| hit.message_id.0)
        .collect();
    ids.sort_unstable();
    ids
}

fn ranked(index: &SearchIndex, input: &str, order: SortOrder) -> Vec<i64> {
    let options = SearchOptions {
        limit: 1000,
        order,
        ..SearchOptions::default()
    };
    index
        .searcher()
        .search(&part(input), &options)
        .unwrap()
        .into_iter()
        .map(|hit| hit.message_id.0)
        .collect()
}

#[test]
fn stemming_matches_inflections_per_language() {
    let index = build(&[
        with_body(1, "Anbei Ihre Rechnung für März", Some(Language::De)),
        with_body(2, "Please find the invoice attached", Some(Language::En)),
        with_body(3, "Offene Rechnungen", None),
        with_body(4, "Running late, invoicing tomorrow", Some(Language::En)),
    ]);
    assert_eq!(ids(&index, "Rechnungen"), [1, 3]);
    assert_eq!(ids(&index, "Rechnung"), [1]);
    assert_eq!(ids(&index, "invoices"), [2, 4]);
    assert_eq!(ids(&index, "invoice"), [2, 4]);
    assert_eq!(ids(&index, "run"), [4]);
    // Phrases are exact: no stemming.
    assert_eq!(ids(&index, "\"invoices\""), Vec::<i64>::new());
}

#[test]
fn exact_matches_rank_above_stemmed_ones() {
    let index = build(&[
        with_body(1, "Die Rechnung ist offen", Some(Language::De)),
        with_body(2, "Die Rechnungen sind offen", Some(Language::De)),
    ]);
    assert_eq!(ranked(&index, "Rechnungen", SortOrder::Relevance), [2, 1]);
    assert_eq!(ranked(&index, "Rechnung", SortOrder::Relevance), [1, 2]);
}

#[test]
fn diacritics_fold_both_ways() {
    let index = build(&[
        with_body(1, "Viele Grüße aus München", Some(Language::De)),
        with_body(2, "Gruss aus Zurich", None),
    ]);
    assert_eq!(ids(&index, "munchen"), [1]);
    assert_eq!(ids(&index, "MÜNCHEN"), [1]);
    assert_eq!(ids(&index, "grusse"), [1]);
    assert_eq!(ids(&index, "Zürich"), [2]);
}

#[test]
fn addresses_are_searchable_by_part() {
    let index = build(&[
        IndexDoc {
            from: vec![Address::new(
                Some("Anna Schmidt"),
                "anna.schmidt@example.com",
            )],
            to: vec![Address::new(None::<String>, "bob@corp.example")],
            cc: vec![Address::new(Some("Carla"), "carla@example.org")],
            ..doc(1)
        },
        IndexDoc {
            from: vec![Address::new(None::<String>, "bob@corp.example")],
            ..doc(2)
        },
    ]);
    assert_eq!(ids(&index, "from:anna"), [1]);
    assert_eq!(ids(&index, "from:Schmidt"), [1]);
    assert_eq!(ids(&index, "from:example.com"), [1]);
    assert_eq!(ids(&index, "from:anna.schmidt@example.com"), [1]);
    assert_eq!(ids(&index, "from:\"Anna Schmidt\""), [1]);
    assert_eq!(ids(&index, "from:\"Anna Example\""), Vec::<i64>::new());
    assert_eq!(ids(&index, "to:bob"), [1]);
    assert_eq!(ids(&index, "from:bob"), [2]);
    assert_eq!(ids(&index, "cc:carla"), [1]);
    assert_eq!(ids(&index, "bob@corp.example"), [1, 2]);
    assert_eq!(ids(&index, "from:corp.example"), [2]);
}

#[test]
fn phrases_need_adjacent_tokens() {
    let index = build(&[
        with_body(1, "the quick brown fox", None),
        with_body(2, "brown and quick", None),
    ]);
    assert_eq!(ids(&index, "\"quick brown\""), [1]);
    assert_eq!(ids(&index, "quick brown"), [1, 2]);
    assert_eq!(ids(&index, "quick -fox"), [2]);
    assert_eq!(ids(&index, "fox OR and"), [1, 2]);
    assert_eq!(ids(&index, "!!!"), [1, 2], "punctuation doesn't constrain");
}

#[test]
fn operators_on_immutable_fields() {
    let index = build(&[
        IndexDoc {
            subject: Sensitive::new("Offsite planning".to_owned()),
            attachments: vec![Attachment {
                name: Sensitive::new("agenda-2026.pdf".to_owned()),
                text: Some(Sensitive::new("Day one: hiking".to_owned())),
            }],
            list_id: Some("dev.lists.example.com".to_owned()),
            size: 6 * 1024 * 1024,
            ..doc(1)
        },
        IndexDoc {
            subject: Sensitive::new("Lunch".to_owned()),
            body: Sensitive::new("offsite?".to_owned()),
            size: 100 * 1024,
            ..doc(40)
        },
    ]);
    assert_eq!(ids(&index, "subject:offsite"), [1]);
    assert_eq!(ids(&index, "offsite"), [1, 40]);
    assert_eq!(ids(&index, "filename:pdf"), [1]);
    assert_eq!(ids(&index, "filename:agenda"), [1]);
    assert_eq!(ids(&index, "hiking"), [1]);
    assert_eq!(ids(&index, "has:attachment"), [1]);
    assert_eq!(ids(&index, "-has:attachment"), [40]);
    assert_eq!(ids(&index, "list:dev.lists.example.com"), [1]);
    assert_eq!(ids(&index, "list:dev@lists.example.com"), [1]);
    assert_eq!(ids(&index, "larger:5M"), [1]);
    assert_eq!(ids(&index, "smaller:200k"), [40]);
    // doc 1 is dated 2026-01-02, doc 40 2026-02-10.
    assert_eq!(ids(&index, "before:2026-01-03"), [1]);
    assert_eq!(ids(&index, "after:2026-01-03"), [40]);
    assert_eq!(ids(&index, "after:2026-01-02 before:2026-01-03"), [1]);
    assert_eq!(ids(&index, "older_than:4m"), [1]);
    assert_eq!(ids(&index, "newer_than:4m"), [40]);
}

#[test]
fn replace_and_delete() {
    let index = build(&[with_body(1, "alpha", None)]);
    let mut indexer = index.indexer(&IndexOptions::default()).unwrap();
    indexer.replace(&with_body(1, "beta", None)).unwrap();
    indexer.commit().unwrap();
    assert_eq!(ids(&index, "alpha"), Vec::<i64>::new());
    assert_eq!(ids(&index, "beta"), [1]);
    assert_eq!(index.num_docs(), 1);
    indexer.delete(MessageId(1)).unwrap();
    indexer.commit().unwrap();
    assert_eq!(ids(&index, "beta"), Vec::<i64>::new());
    assert_eq!(index.num_docs(), 0);
    indexer.add(&with_body(2, "gamma", None)).unwrap();
    indexer.delete_all().unwrap();
    indexer.commit().unwrap();
    assert_eq!(index.num_docs(), 0);
}

#[test]
fn commit_policy_by_count_and_time() {
    let index = SearchIndex::in_memory().unwrap();
    let options = IndexOptions {
        commit: CommitPolicy {
            max_pending: 3,
            max_delay: Duration::from_secs(3600),
        },
        ..IndexOptions::default()
    };
    let mut indexer = index.indexer(&options).unwrap();
    assert_eq!(indexer.commit_due(), None);
    assert!(!indexer.add(&with_body(1, "x", None)).unwrap());
    assert!(indexer.commit_due().is_some());
    assert!(!indexer.add(&with_body(2, "x", None)).unwrap());
    assert_eq!(index.num_docs(), 0, "not visible before the commit");
    assert!(indexer.add(&with_body(3, "x", None)).unwrap());
    assert_eq!(index.num_docs(), 3);
    assert_eq!(indexer.pending(), 0);
    drop(indexer);

    let options = IndexOptions {
        commit: CommitPolicy {
            max_pending: 1000,
            max_delay: Duration::ZERO,
        },
        ..IndexOptions::default()
    };
    let mut indexer = index.indexer(&options).unwrap();
    assert!(indexer.add(&with_body(4, "x", None)).unwrap());
    assert_eq!(index.num_docs(), 4);
    indexer.add(&with_body(5, "x", None)).unwrap();
    indexer.rollback().unwrap_or(());
}

fn corpus_for_ranking() -> SearchIndex {
    let mut docs = Vec::new();
    for id in 1..=6 {
        docs.push(IndexDoc {
            thread_id: ThreadId(if id <= 3 { 10 } else { 20 + id }),
            account_id: AccountId(if id % 2 == 0 { 2 } else { 1 }),
            body: Sensitive::new("quarterly report".repeat(id as usize)),
            ..doc(id)
        });
    }
    build(&docs)
}

#[test]
fn ordering_paging_and_scope() {
    let index = corpus_for_ranking();
    assert_eq!(
        ranked(&index, "report", SortOrder::NewestFirst),
        [6, 5, 4, 3, 2, 1]
    );
    assert_eq!(
        ranked(&index, "report", SortOrder::OldestFirst),
        [1, 2, 3, 4, 5, 6]
    );
    let searcher = index.searcher();
    let page = searcher
        .search(
            &part("report"),
            &SearchOptions {
                limit: 2,
                offset: 2,
                order: SortOrder::NewestFirst,
                scope: Scope::default(),
            },
        )
        .unwrap();
    assert_eq!(
        page.iter().map(|hit| hit.message_id.0).collect::<Vec<_>>(),
        [4, 3]
    );
    assert!(page.iter().all(|hit| hit.score > 0.0));
    assert_eq!(page[0].date, BASE + 4 * DAY);

    let scoped = |scope: Scope| {
        let mut ids: Vec<i64> = searcher
            .search(
                &part("report"),
                &SearchOptions {
                    scope,
                    ..SearchOptions::default()
                },
            )
            .unwrap()
            .into_iter()
            .map(|hit| hit.message_id.0)
            .collect();
        ids.sort_unstable();
        ids
    };
    assert_eq!(
        scoped(Scope {
            messages: Some(vec![MessageId(2), MessageId(5), MessageId(99)]),
            accounts: None
        }),
        [2, 5]
    );
    assert_eq!(
        scoped(Scope {
            messages: None,
            accounts: Some(vec![AccountId(2)])
        }),
        [2, 4, 6]
    );
    assert_eq!(
        scoped(Scope {
            messages: Some(Vec::new()),
            accounts: None
        }),
        Vec::<i64>::new()
    );
    assert_eq!(
        searcher.count(&part("report"), &Scope::default()).unwrap(),
        6
    );
    assert!(
        searcher
            .search(
                &part("report"),
                &SearchOptions {
                    limit: 0,
                    ..SearchOptions::default()
                }
            )
            .unwrap()
            .is_empty()
    );
}

#[test]
fn hits_group_by_thread() {
    let index = corpus_for_ranking();
    let threads = index
        .searcher()
        .search_threads(
            &part("report"),
            &SearchOptions {
                order: SortOrder::NewestFirst,
                ..SearchOptions::default()
            },
        )
        .unwrap();
    let summary: Vec<(i64, i64, usize)> = threads
        .iter()
        .map(|t| (t.thread_id.0, t.best.message_id.0, t.matches))
        .collect();
    assert_eq!(summary, [(26, 6, 1), (25, 5, 1), (24, 4, 1), (10, 3, 3)]);
    assert_eq!(threads[3].latest_date, BASE + 3 * DAY);

    let page = index
        .searcher()
        .search_threads(
            &part("report"),
            &SearchOptions {
                order: SortOrder::NewestFirst,
                limit: 1,
                offset: 3,
                ..SearchOptions::default()
            },
        )
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].thread_id, ThreadId(10));
}

#[test]
fn facets_over_the_result_set() {
    let anna = Address::new(Some("Anna"), "Anna@Example.com");
    let bob = Address::new(None::<String>, "bob@example.com");
    let carla = Address::new(None::<String>, "carla@example.com");
    let mut docs = Vec::new();
    for id in 0..10 {
        docs.push(IndexDoc {
            from: vec![if id < 7 { anna.clone() } else { bob.clone() }],
            to: vec![carla.clone()],
            cc: if id < 2 {
                vec![bob.clone()]
            } else {
                Vec::new()
            },
            body: Sensitive::new("budget".to_owned()),
            attachments: if id < 3 {
                vec![Attachment::default()]
            } else {
                Vec::new()
            },
            date: BASE + id * 10 * DAY,
            ..doc(id)
        });
    }
    docs.push(with_body(100, "unrelated", None));
    let index = build(&docs);
    let facets = index
        .searcher()
        .facets(&part("budget"), &FacetOptions::default())
        .unwrap();
    assert_eq!(facets.total, 10);
    assert_eq!(facets.counted, 10);
    assert_eq!(
        facets.senders,
        [
            ("anna@example.com".to_owned(), 7),
            ("bob@example.com".to_owned(), 3)
        ]
    );
    assert_eq!(
        facets.recipients,
        [
            ("carla@example.com".to_owned(), 10),
            ("bob@example.com".to_owned(), 2)
        ]
    );
    assert_eq!((facets.with_attachment, facets.without_attachment), (3, 7));
    assert_eq!(facets.histogram.iter().map(|(_, n)| n).sum::<u64>(), 10);
    assert!(facets.histogram.windows(2).all(|w| w[0].0 < w[1].0));

    let capped = index
        .searcher()
        .facets(
            &part("budget"),
            &FacetOptions {
                window: 4,
                people: 1,
                ..FacetOptions::default()
            },
        )
        .unwrap();
    assert_eq!((capped.total, capped.counted), (10, 4));
    assert_eq!(capped.senders.len(), 1);

    let none = index
        .searcher()
        .facets(&part("nothingmatches"), &FacetOptions::default())
        .unwrap();
    assert_eq!(none, Facets::default());
}

#[test]
fn snippets_highlight_exact_and_stemmed_matches() {
    let body = "Hallo Anna, bitte zahlen Sie die Rechnung bis Freitag. Danke!";
    let index = build(&[IndexDoc {
        subject: Sensitive::new("Termin am Freitag".to_owned()),
        ..with_body(1, body, Some(Language::De))
    }]);
    let highlighter = index
        .searcher()
        .highlighter(&part("Rechnungen"), 150)
        .unwrap();
    let snippet = highlighter.body(body, Some(Language::De));
    assert_eq!(snippet.highlighted.len(), 1);
    let range = snippet.highlighted[0].clone();
    assert_eq!(&snippet.fragment[range], "Rechnung");
    assert!(
        highlighter.body(body, None).is_empty(),
        "neutral needs the exact word"
    );

    let highlighter = index
        .searcher()
        .highlighter(&part("anna freitag"), 150)
        .unwrap();
    assert_eq!(highlighter.body(body, None).highlighted.len(), 2);
    let subject = highlighter.subject("Termin am Freitag", None);
    assert_eq!(subject.fragment, "Termin am Freitag");
    assert_eq!(subject.highlighted, vec![(10..17)]);
    // Without a match, the subject comes back whole and the body as a plain prefix.
    let highlighter = index.searcher().highlighter(&part("nomatch"), 20).unwrap();
    let subject = highlighter.subject("Termin am Freitag", None);
    assert_eq!(subject.fragment, "Termin am Freitag");
    assert!(subject.is_empty());
    let snippet = highlighter.body("Grüße, Anna. Bitte die Rechnung bezahlen.", None);
    assert!(snippet.is_empty());
    assert!(snippet.fragment.chars().count() <= 20);
    assert!(snippet.fragment.starts_with("Grüße"));
}

#[test]
fn schema_mismatch_is_reported() {
    let directory = tantivy::directory::RamDirectory::create();
    let mut builder = tantivy::schema::Schema::builder();
    builder.add_text_field("other", tantivy::schema::TEXT);
    tantivy::Index::create(directory.clone(), builder.build(), Default::default()).unwrap();
    assert!(matches!(
        SearchIndex::open(directory),
        Err(crate::Error::SchemaMismatch)
    ));
}

#[test]
fn reopening_keeps_the_index() {
    let directory = tantivy::directory::RamDirectory::create();
    {
        let index = SearchIndex::open(directory.clone()).unwrap();
        let mut indexer = index.indexer(&IndexOptions::default()).unwrap();
        indexer.add(&with_body(1, "persisted", None)).unwrap();
        indexer.finish().unwrap();
    }
    let index = SearchIndex::open(directory).unwrap();
    assert_eq!(ids(&index, "persisted"), [1]);
}

// --- Plan execution against a brute-force evaluation ----------------------------------

const WORDS: [&str; 5] = ["apple", "banana", "cherry", "damson", "elder"];
const SENDERS: [&str; 2] = ["anna", "bob"];
const TAGS: [&str; 2] = ["todo", "later"];

#[derive(Clone, Debug)]
struct Row {
    flags: Flags,
    tags: Vec<&'static str>,
    folder: FolderRole,
}

impl MessageMetadata for Row {
    fn in_mailbox(&self, mailbox: &Mailbox) -> bool {
        *mailbox == Mailbox::Role(self.folder)
    }
    fn has_tag(&self, tag: &str) -> bool {
        self.tags.contains(&tag)
    }
    fn flags(&self) -> Flags {
        self.flags
    }
    fn in_account(&self, account: &str) -> bool {
        account == "work"
    }
}

struct Rows(BTreeMap<MessageId, Row>);

impl MetadataSource for Rows {
    type Error = Infallible;

    fn matching(
        &self,
        filter: &MetadataFilter,
        within: Option<&BTreeSet<MessageId>>,
    ) -> Result<BTreeSet<MessageId>, Infallible> {
        Ok(self
            .0
            .iter()
            .filter(|(id, row)| {
                within.is_none_or(|within| within.contains(id)) && filter.matches(*row)
            })
            .map(|(id, _)| *id)
            .collect())
    }
}

fn semantic_corpus() -> (SearchIndex, Rows) {
    let mut docs = Vec::new();
    let mut rows = BTreeMap::new();
    for id in 0..40i64 {
        let n = id as usize;
        let words: Vec<&str> = WORDS
            .iter()
            .enumerate()
            .filter(|(i, _)| (n >> i) & 1 == 1 || (n * 7 + i).is_multiple_of(5))
            .map(|(_, w)| *w)
            .collect();
        docs.push(IndexDoc {
            from: vec![Address::new(
                None::<String>,
                format!("{}@example.com", SENDERS[n % 2]),
            )],
            body: Sensitive::new(words.join(" ")),
            ..doc(id)
        });
        let mut flags = Flags::empty();
        if n.is_multiple_of(3) {
            flags |= Flags::SEEN;
        }
        if n % 4 == 1 {
            flags |= Flags::FLAGGED;
        }
        rows.insert(
            MessageId(id),
            Row {
                flags,
                tags: TAGS
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| (n + i).is_multiple_of(3))
                    .map(|(_, t)| *t)
                    .collect(),
                folder: if n % 5 < 3 {
                    FolderRole::Inbox
                } else {
                    FolderRole::Archive
                },
            },
        );
    }
    (build(&docs), Rows(rows))
}

fn arb_term() -> impl Strategy<Value = Term> {
    prop_oneof![
        prop::sample::select(WORDS.to_vec()).prop_map(|w| Term::Text(Text::Word(w.to_owned()))),
        prop::sample::select(SENDERS.to_vec()).prop_map(|s| Term::From(Text::Word(s.to_owned()))),
        prop::sample::select(vec![Status::Unread, Status::Read, Status::Starred])
            .prop_map(Term::Is),
        prop::sample::select(TAGS.to_vec()).prop_map(|t| Term::Tag(t.to_owned())),
        prop::sample::select(vec![FolderRole::Inbox, FolderRole::Archive])
            .prop_map(|r| Term::In(Mailbox::Role(r))),
    ]
}

fn arb_query() -> impl Strategy<Value = Query> {
    arb_term()
        .prop_map(Query::Term)
        .prop_recursive(3, 16, 3, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 1..4).prop_map(Query::And),
                prop::collection::vec(inner.clone(), 1..4).prop_map(Query::Or),
                inner.prop_map(|q| !q),
            ]
        })
}

/// Evaluates `query` by set algebra over the whole corpus, asking the index or the rows
/// about one term at a time.
fn brute_force(
    query: &Query,
    searcher: &IndexSearcher,
    rows: &Rows,
    universe: &BTreeSet<MessageId>,
) -> BTreeSet<MessageId> {
    match query {
        Query::Term(term) => {
            let plan = QueryPlan::new(&Query::Term(term.clone()), &context()).unwrap();
            searcher
                .execute(&plan, rows, &ExecuteOptions::default())
                .unwrap()
                .hits
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        }
        Query::Not(inner) => universe
            .difference(&brute_force(inner, searcher, rows, universe))
            .copied()
            .collect(),
        Query::And(items) => items.iter().fold(universe.clone(), |acc, item| {
            acc.intersection(&brute_force(item, searcher, rows, universe))
                .copied()
                .collect()
        }),
        Query::Or(items) => items.iter().fold(BTreeSet::new(), |acc, item| {
            acc.union(&brute_force(item, searcher, rows, universe))
                .copied()
                .collect()
        }),
    }
}

#[test]
fn plan_execution_matches_brute_force_evaluation() {
    let (index, rows) = semantic_corpus();
    let searcher = index.searcher();
    let universe: BTreeSet<MessageId> = rows.0.keys().copied().collect();
    let mut runner = TestRunner::new(Config::with_cases(300));
    runner
        .run(&arb_query(), |query| {
            let query = query.normalize();
            let plan = match QueryPlan::new(&query, &context()) {
                Ok(plan) => plan,
                Err(_) => return Ok(()),
            };
            let executed = searcher
                .execute(&plan, &rows, &ExecuteOptions::default())
                .unwrap();
            prop_assert!(!executed.truncated);
            let got: BTreeSet<MessageId> = executed.hits.iter().map(|(id, _)| *id).collect();
            let expected = brute_force(&query, &searcher, &rows, &universe);
            prop_assert_eq!(got, expected, "query {} plan {:?}", query, plan);
            Ok(())
        })
        .unwrap();
}

#[test]
fn execute_scores_text_hits_and_reports_truncation() {
    let (index, rows) = semantic_corpus();
    let searcher = index.searcher();
    let plan = QueryPlan::new(&parse("apple OR is:starred").unwrap().query, &context()).unwrap();
    let executed = searcher
        .execute(&plan, &rows, &ExecuteOptions::default())
        .unwrap();
    // Scored (index) hits come first.
    let first_unscored = executed
        .hits
        .iter()
        .position(|(_, score)| score.is_none())
        .unwrap_or(executed.hits.len());
    assert!(
        executed.hits[..first_unscored]
            .iter()
            .all(|(_, s)| s.is_some())
    );
    assert!(
        executed.hits[first_unscored..]
            .iter()
            .all(|(_, s)| s.is_none())
    );

    let plan = QueryPlan::new(&parse("apple").unwrap().query, &context()).unwrap();
    let executed = searcher
        .execute(&plan, &rows, &ExecuteOptions { cap: 3 })
        .unwrap();
    assert_eq!(executed.hits.len(), 3);
    assert!(executed.truncated);
}
