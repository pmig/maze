//! Turning an [`IndexExpr`] into a Tantivy query.

use std::ops::Bound;

use tantivy::Term as TantivyTerm;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, EmptyQuery, Occur, PhraseQuery, Query, RangeQuery,
    TermQuery, TermSetQuery,
};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::tokenizer::{Language as StemLanguage, TextAnalyzer};

use super::doc::id_to_u64;
use super::schema::{Fields, neutral, stemmed};
use super::searcher::Scope;
use crate::plan::{IndexExpr, IndexTerm, TextScope};
use crate::query::Text;

/// Which analyzer a field uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Analysis {
    Neutral,
    German,
    English,
}

/// A field a text term searches, with its weight. Neutral fields carry positions and
/// get the higher weight, so exact matches outrank stemmed ones.
#[derive(Clone, Copy, Debug)]
struct Target {
    field: Field,
    boost: f32,
    analysis: Analysis,
}

/// The analyzers, mirroring what the index registered.
#[derive(Clone)]
pub(crate) struct Analyzers {
    neutral: TextAnalyzer,
    german: TextAnalyzer,
    english: TextAnalyzer,
}

impl Analyzers {
    pub(crate) fn new() -> Self {
        Self {
            neutral: neutral(),
            german: stemmed(StemLanguage::German),
            english: stemmed(StemLanguage::English),
        }
    }

    fn tokens(&mut self, analysis: Analysis, text: &str) -> Vec<String> {
        let analyzer = match analysis {
            Analysis::Neutral => &mut self.neutral,
            Analysis::German => &mut self.german,
            Analysis::English => &mut self.english,
        };
        let mut stream = analyzer.token_stream(text);
        let mut tokens = Vec::new();
        while stream.advance() {
            tokens.push(stream.token().text.clone());
        }
        tokens
    }
}

pub(crate) struct Compiler<'a> {
    fields: &'a Fields,
    analyzers: Analyzers,
}

impl<'a> Compiler<'a> {
    pub(crate) fn new(fields: &'a Fields, analyzers: &Analyzers) -> Self {
        Self {
            fields,
            analyzers: analyzers.clone(),
        }
    }

    /// Compiles `expr` and restricts it to `scope`.
    pub(crate) fn compile_scoped(&mut self, expr: &IndexExpr, scope: &Scope) -> Box<dyn Query> {
        let query = self.compile(expr);
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        if let Some(messages) = &scope.messages {
            let terms = messages
                .iter()
                .map(|id| TantivyTerm::from_field_u64(self.fields.id, id_to_u64(id.0)));
            clauses.push((Occur::Must, Box::new(TermSetQuery::new(terms))));
        }
        if let Some(accounts) = &scope.accounts {
            let terms = accounts
                .iter()
                .map(|id| TantivyTerm::from_field_u64(self.fields.account, id_to_u64(id.0)));
            clauses.push((Occur::Must, Box::new(TermSetQuery::new(terms))));
        }
        if clauses.is_empty() {
            return query;
        }
        clauses.insert(0, (Occur::Must, query));
        Box::new(BooleanQuery::new(clauses))
    }

    pub(crate) fn compile(&mut self, expr: &IndexExpr) -> Box<dyn Query> {
        match expr {
            IndexExpr::Term(term) => self.term(term),
            IndexExpr::Not(inner) => exclude(self.compile(inner)),
            IndexExpr::And(items) => {
                if items.is_empty() {
                    return Box::new(AllQuery);
                }
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = items
                    .iter()
                    .map(|item| match item {
                        IndexExpr::Not(inner) => (Occur::MustNot, self.compile(inner)),
                        other => (Occur::Must, self.compile(other)),
                    })
                    .collect();
                if !clauses.iter().any(|(occur, _)| *occur == Occur::Must) {
                    // Tantivy needs a positive clause to subtract from.
                    clauses.push((Occur::Must, Box::new(AllQuery)));
                }
                Box::new(BooleanQuery::new(clauses))
            }
            IndexExpr::Or(items) => {
                if items.is_empty() {
                    return Box::new(EmptyQuery);
                }
                let clauses = items
                    .iter()
                    .map(|item| (Occur::Should, self.compile(item)))
                    .collect();
                Box::new(BooleanQuery::new(clauses))
            }
        }
    }

    fn term(&mut self, term: &IndexTerm) -> Box<dyn Query> {
        let fields = self.fields;
        match term {
            IndexTerm::Text { scope, text } => {
                let targets = targets(fields, *scope);
                self.text(&targets, text)
            }
            IndexTerm::HasAttachment => Box::new(TermQuery::new(
                TantivyTerm::from_field_bool(fields.has_attachment, true),
                IndexRecordOption::Basic,
            )),
            IndexTerm::List(list) => {
                let target = Target {
                    field: fields.list_id,
                    boost: 1.0,
                    analysis: Analysis::Neutral,
                };
                self.text(&[target], &Text::Word(list.clone()))
            }
            IndexTerm::Date { start, end } => {
                let lower = start.map_or(Bound::Unbounded, |start| {
                    Bound::Included(TantivyTerm::from_field_i64(fields.date, start))
                });
                let upper = end.map_or(Bound::Unbounded, |end| {
                    Bound::Excluded(TantivyTerm::from_field_i64(fields.date, end))
                });
                range(lower, upper)
            }
            IndexTerm::Size {
                larger_than,
                smaller_than,
            } => {
                let lower = larger_than.map_or(Bound::Unbounded, |size| {
                    Bound::Excluded(TantivyTerm::from_field_u64(fields.size, size))
                });
                let upper = smaller_than.map_or(Bound::Unbounded, |size| {
                    Bound::Excluded(TantivyTerm::from_field_u64(fields.size, size))
                });
                range(lower, upper)
            }
        }
    }

    /// A text term across `targets`: any field may match, scores add up.
    ///
    /// A word that analyzes to several tokens (`anna@example.com`) must match them next
    /// to each other in positional fields and all of them in the stemmed fields. A quoted
    /// phrase searches only the positional (neutral) fields. Text without any token
    /// (punctuation) doesn't constrain the result.
    fn text(&mut self, targets: &[Target], text: &Text) -> Box<dyn Query> {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let mut had_tokens = false;
        for target in targets {
            let positional = target.analysis == Analysis::Neutral;
            if text.is_phrase() && !positional {
                continue;
            }
            let tokens = self.analyzers.tokens(target.analysis, text.as_str());
            if tokens.is_empty() {
                continue;
            }
            had_tokens = true;
            let mut terms: Vec<TantivyTerm> = tokens
                .iter()
                .map(|token| TantivyTerm::from_field_text(target.field, token))
                .collect();
            let query: Box<dyn Query> = if terms.len() == 1 {
                let term = terms
                    .pop()
                    .unwrap_or_else(|| TantivyTerm::from_field_text(target.field, ""));
                Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs))
            } else if positional {
                Box::new(PhraseQuery::new(terms))
            } else {
                Box::new(BooleanQuery::new(
                    terms
                        .into_iter()
                        .map(|term| {
                            let query: Box<dyn Query> =
                                Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs));
                            (Occur::Must, query)
                        })
                        .collect(),
                ))
            };
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(query, target.boost)),
            ));
        }
        if !had_tokens {
            return Box::new(AllQuery);
        }
        Box::new(BooleanQuery::new(clauses))
    }
}

fn exclude(query: Box<dyn Query>) -> Box<dyn Query> {
    Box::new(BooleanQuery::new(vec![
        (Occur::Must, Box::new(AllQuery) as Box<dyn Query>),
        (Occur::MustNot, query),
    ]))
}

fn range(lower: Bound<TantivyTerm>, upper: Bound<TantivyTerm>) -> Box<dyn Query> {
    if matches!((&lower, &upper), (Bound::Unbounded, Bound::Unbounded)) {
        Box::new(AllQuery)
    } else {
        Box::new(RangeQuery::new(lower, upper))
    }
}

fn targets(fields: &Fields, scope: TextScope) -> Vec<Target> {
    let neutral = |field, boost| Target {
        field,
        boost,
        analysis: Analysis::Neutral,
    };
    let german = |field, boost| Target {
        field,
        boost,
        analysis: Analysis::German,
    };
    let english = |field, boost| Target {
        field,
        boost,
        analysis: Analysis::English,
    };
    match scope {
        TextScope::Anywhere => vec![
            neutral(fields.subject, 4.0),
            german(fields.subject_de, 2.0),
            english(fields.subject_en, 2.0),
            neutral(fields.body, 2.0),
            german(fields.body_de, 1.0),
            english(fields.body_en, 1.0),
            neutral(fields.from, 3.0),
            neutral(fields.to, 2.0),
            neutral(fields.cc, 1.5),
            neutral(fields.attachment_names, 2.0),
            neutral(fields.attachment_text, 1.0),
        ],
        TextScope::From => vec![neutral(fields.from, 1.0)],
        TextScope::To => vec![neutral(fields.to, 1.0)],
        TextScope::Cc => vec![neutral(fields.cc, 1.0)],
        TextScope::Subject => vec![
            neutral(fields.subject, 2.0),
            german(fields.subject_de, 1.0),
            english(fields.subject_en, 1.0),
        ],
        TextScope::Filename => vec![neutral(fields.attachment_names, 1.0)],
    }
}
