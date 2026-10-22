//! Splitting a [`Query`] into what the index answers and what SQLite answers.
//!
//! The index only holds what never changes about a message (PLAN §4.4, "Mutable
//! fields"): text, addresses, attachment names and text, list id, date, size, account and
//! thread. Folders, flags and tags change all the time, so they live only in SQLite and
//! the caller filters them there. A [`QueryPlan`] says which part goes where.
//!
//! # Semantics
//!
//! The query is first put into negation normal form and then into a conjunction of
//! clauses (CNF), where every maximal sub-expression that touches only one side stays a
//! single opaque literal. Each clause is then one of:
//!
//! - **index only**: ANDed into [`QueryPlan::text`],
//! - **metadata only**: ANDed into [`QueryPlan::metadata`],
//! - **mixed**: a disjunction of index terms and metadata terms, kept as a
//!   [`MixedClause`] whose result is "ids from the index ∪ ids from the metadata".
//!
//! The messages matching the query are exactly
//!
//! ```text
//! I(text) ∩ M(metadata) ∩ ⋂ₖ ( I(mixed[k].index) ∪ M(mixed[k].metadata) )
//! ```
//!
//! where `I(x)` is the set of messages the index matches and `M(x)` the set SQLite
//! matches; a missing `text` means "every message". `from:anna is:unread` becomes
//! `text = from:anna`, `metadata = unread`; `from:anna OR is:starred` becomes one mixed
//! clause. [`IndexSearcher::execute`](crate::IndexSearcher::execute) evaluates a plan in
//! that order, narrowing the candidate set step by step.
//!
//! Distributing OR over AND can blow up; a plan with more than [`MAX_CLAUSES`] clauses is
//! refused with [`PlanError::TooComplex`] rather than evaluated approximately.

use mazemail_types::Flags;
use time::{Date, Duration, OffsetDateTime, UtcOffset};

use crate::query::{Age, AgeUnit, Mailbox, Query, Status, Term, Text};

/// Plans with more clauses than this are refused.
pub const MAX_CLAUSES: usize = 64;

/// Errors from building a plan.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PlanError {
    /// Mixing OR and AND across index and metadata terms produced too many clauses.
    #[error("the query combines OR and AND in too many ways; simplify it")]
    TooComplex,
    /// A date or age falls outside the supported range.
    #[error("a date in the query is out of range")]
    DateOutOfRange,
}

/// The clock and time zone a plan resolves dates against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanContext {
    now: OffsetDateTime,
    offset: UtcOffset,
}

impl PlanContext {
    /// Dates like `before:2026-01-01` mean midnight at `offset`; ages like
    /// `older_than:30d` count back from `now`.
    pub fn new(now: OffsetDateTime, offset: UtcOffset) -> Self {
        Self { now, offset }
    }

    /// The current time, with dates interpreted in UTC.
    pub fn now_utc() -> Self {
        Self::new(OffsetDateTime::now_utc(), UtcOffset::UTC)
    }

    /// The reference time.
    pub fn now(&self) -> OffsetDateTime {
        self.now
    }

    /// The local time zone offset.
    pub fn offset(&self) -> UtcOffset {
        self.offset
    }

    /// Unix seconds of midnight at the start of `date`, local time.
    pub fn start_of_day(&self, date: Date) -> i64 {
        date.midnight().assume_offset(self.offset).unix_timestamp()
    }

    /// The moment `age` before now, counting calendar months and years in local time.
    pub fn ago(&self, age: Age) -> Result<OffsetDateTime, PlanError> {
        let local = self.now.to_offset(self.offset);
        let count = i64::from(age.count);
        let result = match age.unit {
            AgeUnit::Days => local.checked_sub(Duration::days(count)),
            AgeUnit::Weeks => local.checked_sub(Duration::weeks(count)),
            AgeUnit::Months => sub_months(local, count),
            AgeUnit::Years => sub_months(local, count.saturating_mul(12)),
        };
        result.ok_or(PlanError::DateOutOfRange)
    }
}

fn sub_months(at: OffsetDateTime, months: i64) -> Option<OffsetDateTime> {
    let total = i64::from(at.year()) * 12 + i64::from(u8::from(at.month())) - 1 - months;
    let year = i32::try_from(total.div_euclid(12)).ok()?;
    let month = time::Month::try_from(u8::try_from(total.rem_euclid(12) + 1).ok()?).ok()?;
    let day = at.day().min(month.length(year));
    let date = Date::from_calendar_date(year, month, day).ok()?;
    Some(at.replace_date(date))
}

/// Which text fields a text term searches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextScope {
    /// Subject, body, addresses and attachments.
    Anywhere,
    /// Sender names and addresses.
    From,
    /// Recipient names and addresses.
    To,
    /// Copy recipient names and addresses.
    Cc,
    /// The subject.
    Subject,
    /// Attachment names.
    Filename,
}

/// A term the index can answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexTerm {
    /// Text in the given fields.
    Text {
        /// Where to search.
        scope: TextScope,
        /// What to search for.
        text: Text,
    },
    /// The message has at least one attachment.
    HasAttachment,
    /// The message came through this mailing list (id or address).
    List(String),
    /// The message date is in `[start, end)`, Unix seconds.
    Date {
        /// Inclusive lower bound.
        start: Option<i64>,
        /// Exclusive upper bound.
        end: Option<i64>,
    },
    /// The message size is strictly between the bounds, in bytes.
    Size {
        /// Exclusive lower bound.
        larger_than: Option<u64>,
        /// Exclusive upper bound.
        smaller_than: Option<u64>,
    },
}

/// A boolean expression over [`IndexTerm`]s. `And(vec![])` matches everything,
/// `Or(vec![])` nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexExpr {
    /// All must match.
    And(Vec<IndexExpr>),
    /// At least one must match.
    Or(Vec<IndexExpr>),
    /// Must not match.
    Not(Box<IndexExpr>),
    /// A single term.
    Term(IndexTerm),
}

/// The part of a plan that runs against the Tantivy index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TantivyPart {
    expr: IndexExpr,
}

impl TantivyPart {
    /// Wraps an expression, for callers that build index queries directly (Quick Filter).
    pub fn new(expr: IndexExpr) -> Self {
        Self { expr }
    }

    /// Free text searched everywhere, as typed into a search box: whitespace separates
    /// words that must all match.
    pub fn text(text: &str) -> Self {
        let words = text
            .split_whitespace()
            .map(|word| {
                IndexExpr::Term(IndexTerm::Text {
                    scope: TextScope::Anywhere,
                    text: Text::Word(word.to_owned()),
                })
            })
            .collect();
        Self::new(IndexExpr::And(words))
    }

    /// Matches every indexed message.
    pub fn everything() -> Self {
        Self::new(IndexExpr::And(Vec::new()))
    }

    /// Matches no message.
    pub fn nothing() -> Self {
        Self::new(IndexExpr::Or(Vec::new()))
    }

    /// The expression.
    pub fn expr(&self) -> &IndexExpr {
        &self.expr
    }
}

/// A term SQLite answers: the mutable state of a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataTerm {
    /// The message is in this folder or a folder with this role (any account).
    In(Mailbox),
    /// The message carries this tag.
    Tag(String),
    /// `flag` is set (`set == true`) or not set on the message.
    Flag {
        /// The flag.
        flag: Flags,
        /// Whether it must be set.
        set: bool,
    },
    /// The message belongs to the account with this name.
    Account(String),
}

/// A boolean expression over [`MetadataTerm`]s that the caller evaluates in SQL.
/// `And(vec![])` matches everything, `Or(vec![])` nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataFilter {
    /// All must match.
    And(Vec<MetadataFilter>),
    /// At least one must match.
    Or(Vec<MetadataFilter>),
    /// Must not match.
    Not(Box<MetadataFilter>),
    /// A single term.
    Term(MetadataTerm),
}

/// What [`MetadataFilter::matches`] needs to know about a message.
pub trait MessageMetadata {
    /// Whether the message is in `mailbox` (a folder name or role).
    fn in_mailbox(&self, mailbox: &Mailbox) -> bool;
    /// Whether the message carries `tag`.
    fn has_tag(&self, tag: &str) -> bool;
    /// The message's flags.
    fn flags(&self) -> Flags;
    /// Whether the message belongs to the account named `account`.
    fn in_account(&self, account: &str) -> bool;
}

impl MetadataFilter {
    /// Matches every message.
    pub fn all() -> Self {
        Self::And(Vec::new())
    }

    /// Whether this filter matches every message.
    pub fn is_all(&self) -> bool {
        matches!(self, Self::And(items) if items.is_empty())
    }

    /// Reference evaluation against one message, for in-memory rows (Quick Filter) and
    /// tests. The store evaluates the same tree in SQL.
    pub fn matches(&self, message: &impl MessageMetadata) -> bool {
        match self {
            Self::And(items) => items.iter().all(|item| item.matches(message)),
            Self::Or(items) => items.iter().any(|item| item.matches(message)),
            Self::Not(inner) => !inner.matches(message),
            Self::Term(MetadataTerm::In(mailbox)) => message.in_mailbox(mailbox),
            Self::Term(MetadataTerm::Tag(tag)) => message.has_tag(tag),
            Self::Term(MetadataTerm::Flag { flag, set }) => message.flags().contains(*flag) == *set,
            Self::Term(MetadataTerm::Account(account)) => message.in_account(account),
        }
    }
}

/// A clause that ORs index terms with metadata terms: a message matches if the index
/// matches `index` **or** SQLite matches `metadata`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MixedClause {
    /// The index side of the disjunction.
    pub index: TantivyPart,
    /// The metadata side of the disjunction.
    pub metadata: MetadataFilter,
}

/// A query split into index and metadata work. See the [module docs](self) for the
/// exact semantics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryPlan {
    /// What the index must match; `None` means the index doesn't constrain the result.
    pub text: Option<TantivyPart>,
    /// What SQLite must match; [`MetadataFilter::all`] when nothing.
    pub metadata: MetadataFilter,
    /// Disjunctions across both sides, all of which must hold.
    pub mixed: Vec<MixedClause>,
}

impl QueryPlan {
    /// Plans `query`, resolving dates and ages against `context`.
    pub fn new(query: &Query, context: &PlanContext) -> Result<Self, PlanError> {
        let nnf = to_nnf(query, false, context)?;
        let clauses = cnf(&nnf)?;
        let mut text = Vec::new();
        let mut metadata = Vec::new();
        let mut mixed = Vec::new();
        for clause in clauses {
            match (clause.index.is_empty(), clause.meta.is_empty()) {
                (true, true) => return Ok(Self::nothing()),
                (false, true) => text.push(index_disjunction(clause.index)),
                (true, false) => metadata.push(meta_disjunction(clause.meta)),
                (false, false) => mixed.push(MixedClause {
                    index: TantivyPart::new(index_disjunction(clause.index)),
                    metadata: meta_disjunction(clause.meta),
                }),
            }
        }
        Ok(Self {
            text: (!text.is_empty()).then(|| TantivyPart::new(index_conjunction(text))),
            metadata: meta_conjunction(metadata),
            mixed,
        })
    }

    /// A plan that matches no message.
    pub fn nothing() -> Self {
        Self {
            text: Some(TantivyPart::nothing()),
            metadata: MetadataFilter::all(),
            mixed: Vec::new(),
        }
    }

    /// Whether the plan needs the index at all.
    pub fn uses_index(&self) -> bool {
        self.text.is_some() || !self.mixed.is_empty()
    }

    /// Whether the plan needs SQLite at all.
    pub fn uses_metadata(&self) -> bool {
        !self.metadata.is_all() || !self.mixed.is_empty()
    }
}

#[derive(Clone, Debug)]
enum Atom {
    Index(IndexTerm),
    Meta(MetadataTerm),
}

/// A query in negation normal form: negation only on literals.
#[derive(Clone, Debug)]
enum Nnf {
    And(Vec<Nnf>),
    Or(Vec<Nnf>),
    Lit { atom: Atom, negated: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    /// No literals at all: a constant.
    Const,
    Index,
    Meta,
    Mixed,
}

impl Side {
    fn join(self, other: Side) -> Side {
        match (self, other) {
            (Side::Const, side) | (side, Side::Const) => side,
            (a, b) if a == b => a,
            _ => Side::Mixed,
        }
    }
}

impl Nnf {
    fn side(&self) -> Side {
        match self {
            Nnf::Lit {
                atom: Atom::Index(_),
                ..
            } => Side::Index,
            Nnf::Lit {
                atom: Atom::Meta(_),
                ..
            } => Side::Meta,
            Nnf::And(items) | Nnf::Or(items) => items
                .iter()
                .fold(Side::Const, |side, item| side.join(item.side())),
        }
    }
}

fn to_nnf(query: &Query, negated: bool, context: &PlanContext) -> Result<Nnf, PlanError> {
    Ok(match query {
        Query::Term(term) => Nnf::Lit {
            atom: atom(term, context)?,
            negated,
        },
        Query::Not(inner) => to_nnf(inner, !negated, context)?,
        Query::And(items) | Query::Or(items) => {
            let items = items
                .iter()
                .map(|item| to_nnf(item, negated, context))
                .collect::<Result<Vec<_>, _>>()?;
            // De Morgan: a negated AND is an OR of negations and vice versa.
            if matches!(query, Query::And(_)) != negated {
                Nnf::And(items)
            } else {
                Nnf::Or(items)
            }
        }
    })
}

fn atom(term: &Term, context: &PlanContext) -> Result<Atom, PlanError> {
    let text = |scope: TextScope, text: &Text| {
        Atom::Index(IndexTerm::Text {
            scope,
            text: text.clone(),
        })
    };
    Ok(match term {
        Term::Text(t) => text(TextScope::Anywhere, t),
        Term::From(t) => text(TextScope::From, t),
        Term::To(t) => text(TextScope::To, t),
        Term::Cc(t) => text(TextScope::Cc, t),
        Term::Subject(t) => text(TextScope::Subject, t),
        Term::Filename(t) => text(TextScope::Filename, t),
        Term::HasAttachment => Atom::Index(IndexTerm::HasAttachment),
        Term::List(list) => Atom::Index(IndexTerm::List(list.clone())),
        Term::Before(date) => Atom::Index(IndexTerm::Date {
            start: None,
            end: Some(context.start_of_day(*date)),
        }),
        Term::After(date) => Atom::Index(IndexTerm::Date {
            start: Some(context.start_of_day(*date)),
            end: None,
        }),
        Term::OlderThan(age) => Atom::Index(IndexTerm::Date {
            start: None,
            end: Some(context.ago(*age)?.unix_timestamp()),
        }),
        Term::NewerThan(age) => Atom::Index(IndexTerm::Date {
            start: Some(context.ago(*age)?.unix_timestamp()),
            end: None,
        }),
        Term::Larger(size) => Atom::Index(IndexTerm::Size {
            larger_than: Some(size.bytes()),
            smaller_than: None,
        }),
        Term::Smaller(size) => Atom::Index(IndexTerm::Size {
            larger_than: None,
            smaller_than: Some(size.bytes()),
        }),
        Term::Tag(tag) => Atom::Meta(MetadataTerm::Tag(tag.clone())),
        Term::In(mailbox) => Atom::Meta(MetadataTerm::In(mailbox.clone())),
        Term::Account(account) => Atom::Meta(MetadataTerm::Account(account.clone())),
        Term::Is(status) => {
            let (flag, set) = match status {
                Status::Unread => (Flags::SEEN, false),
                Status::Read => (Flags::SEEN, true),
                Status::Starred | Status::Flagged => (Flags::FLAGGED, true),
            };
            Atom::Meta(MetadataTerm::Flag { flag, set })
        }
    })
}

/// One CNF clause: a disjunction of pure index sub-expressions and pure metadata
/// sub-expressions.
#[derive(Clone, Debug, Default)]
struct Clause {
    index: Vec<Nnf>,
    meta: Vec<Nnf>,
}

fn cnf(nnf: &Nnf) -> Result<Vec<Clause>, PlanError> {
    match (nnf.side(), nnf) {
        (Side::Index, _) => Ok(vec![Clause {
            index: vec![nnf.clone()],
            meta: Vec::new(),
        }]),
        (Side::Meta, _) => Ok(vec![Clause {
            index: Vec::new(),
            meta: vec![nnf.clone()],
        }]),
        (_, Nnf::And(items)) => {
            let mut clauses = Vec::new();
            for item in items {
                clauses.extend(cnf(item)?);
                if clauses.len() > MAX_CLAUSES {
                    return Err(PlanError::TooComplex);
                }
            }
            Ok(clauses)
        }
        (_, Nnf::Or(items)) => {
            // The empty disjunction is false: one empty clause.
            let mut product = vec![Clause::default()];
            for item in items {
                let item_clauses = cnf(item)?;
                if product.len().saturating_mul(item_clauses.len()) > MAX_CLAUSES {
                    return Err(PlanError::TooComplex);
                }
                let mut next = Vec::with_capacity(product.len() * item_clauses.len());
                for left in &product {
                    for right in &item_clauses {
                        let mut merged = left.clone();
                        merged.index.extend(right.index.iter().cloned());
                        merged.meta.extend(right.meta.iter().cloned());
                        next.push(merged);
                    }
                }
                product = next;
            }
            Ok(product)
        }
        // A literal always has a side.
        (_, Nnf::Lit { .. }) => Ok(Vec::new()),
    }
}

fn index_expr(nnf: Nnf) -> IndexExpr {
    match nnf {
        Nnf::And(items) => IndexExpr::And(items.into_iter().map(index_expr).collect()),
        Nnf::Or(items) => IndexExpr::Or(items.into_iter().map(index_expr).collect()),
        Nnf::Lit {
            atom: Atom::Index(term),
            negated,
        } => {
            if negated {
                IndexExpr::Not(Box::new(IndexExpr::Term(term)))
            } else {
                IndexExpr::Term(term)
            }
        }
        // Pure index sub-expressions have no metadata literals.
        Nnf::Lit { .. } => IndexExpr::And(Vec::new()),
    }
}

fn meta_filter(nnf: Nnf) -> MetadataFilter {
    match nnf {
        Nnf::And(items) => MetadataFilter::And(items.into_iter().map(meta_filter).collect()),
        Nnf::Or(items) => MetadataFilter::Or(items.into_iter().map(meta_filter).collect()),
        Nnf::Lit {
            atom: Atom::Meta(MetadataTerm::Flag { flag, set }),
            negated,
        } => MetadataFilter::Term(MetadataTerm::Flag {
            flag,
            set: set != negated,
        }),
        Nnf::Lit {
            atom: Atom::Meta(term),
            negated,
        } => {
            if negated {
                MetadataFilter::Not(Box::new(MetadataFilter::Term(term)))
            } else {
                MetadataFilter::Term(term)
            }
        }
        // Pure metadata sub-expressions have no index literals.
        Nnf::Lit { .. } => MetadataFilter::all(),
    }
}

fn index_disjunction(items: Vec<Nnf>) -> IndexExpr {
    let mut items: Vec<IndexExpr> = items.into_iter().map(index_expr).collect();
    if items.len() == 1 {
        items.pop().unwrap_or_else(|| IndexExpr::Or(Vec::new()))
    } else {
        IndexExpr::Or(items)
    }
}

fn index_conjunction(mut items: Vec<IndexExpr>) -> IndexExpr {
    if items.len() == 1 {
        items.pop().unwrap_or_else(|| IndexExpr::And(Vec::new()))
    } else {
        IndexExpr::And(items)
    }
}

fn meta_disjunction(items: Vec<Nnf>) -> MetadataFilter {
    let mut items: Vec<MetadataFilter> = items.into_iter().map(meta_filter).collect();
    if items.len() == 1 {
        items
            .pop()
            .unwrap_or_else(|| MetadataFilter::Or(Vec::new()))
    } else {
        MetadataFilter::Or(items)
    }
}

fn meta_conjunction(mut items: Vec<MetadataFilter>) -> MetadataFilter {
    if items.len() == 1 {
        items.pop().unwrap_or_else(MetadataFilter::all)
    } else {
        MetadataFilter::And(items)
    }
}

#[cfg(test)]
mod tests;
