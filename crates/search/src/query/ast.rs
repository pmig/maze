//! The typed query AST produced by [`parse`](super::parse).

use mazemail_types::FolderRole;
use time::Date;

/// A parsed search query.
///
/// Queries hold what the user typed, so they are sensitive: wrap them in
/// [`mazemail_types::Sensitive`] before they get anywhere near a log line.
///
/// `And(vec![])` is the empty query and matches every message (see [`Query::all`]).
/// Implicit juxtaposition is `And`; `OR` binds tighter than juxtaposition, as in Gmail:
/// `a b OR c` means `a AND (b OR c)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    /// Every sub-query must match. Empty means "match everything".
    And(Vec<Query>),
    /// At least one sub-query must match. Never empty in a normalized query.
    Or(Vec<Query>),
    /// The sub-query must not match (`-term`, `-(…)`).
    Not(Box<Query>),
    /// A single search term.
    Term(Term),
}

impl Query {
    /// The empty query, which matches every message.
    pub fn all() -> Self {
        Self::And(Vec::new())
    }

    /// Whether this is the empty query.
    pub fn is_all(&self) -> bool {
        matches!(self, Self::And(items) if items.is_empty())
    }

    /// A conjunction of `items`, normalized.
    pub fn and(items: Vec<Query>) -> Self {
        Self::And(items).normalize()
    }

    /// A disjunction of `items`, normalized.
    pub fn or(items: Vec<Query>) -> Self {
        Self::Or(items).normalize()
    }

    /// A single term.
    pub fn term(term: Term) -> Self {
        Self::Term(term)
    }

    /// Returns the canonical shape of this query: nested `And`/`Or` of the same kind are
    /// flattened, single-item groups are unwrapped, empty `And`s inside an `And` are
    /// dropped, and an `Or` containing a match-everything branch becomes match-everything.
    ///
    /// The parser always returns normalized queries, and the canonical string of a
    /// normalized query parses back to the same query.
    pub fn normalize(self) -> Self {
        match self {
            Self::Term(term) => Self::Term(term),
            Self::Not(inner) => Self::Not(Box::new(inner.normalize())),
            Self::And(items) => {
                let mut flat = Vec::with_capacity(items.len());
                for item in items {
                    match item.normalize() {
                        Self::And(inner) => flat.extend(inner),
                        other => flat.push(other),
                    }
                }
                if flat.len() == 1 {
                    flat.pop().unwrap_or_else(Self::all)
                } else {
                    Self::And(flat)
                }
            }
            Self::Or(items) => {
                let mut flat = Vec::with_capacity(items.len());
                for item in items {
                    match item.normalize() {
                        Self::Or(inner) => flat.extend(inner),
                        all if all.is_all() => return Self::all(),
                        other => flat.push(other),
                    }
                }
                if flat.len() == 1 {
                    flat.pop().unwrap_or_else(Self::all)
                } else {
                    Self::Or(flat)
                }
            }
        }
    }

    /// Calls `visit` for every term in the query, depth first.
    pub fn for_each_term(&self, visit: &mut impl FnMut(&Term)) {
        match self {
            Self::Term(term) => visit(term),
            Self::Not(inner) => inner.for_each_term(visit),
            Self::And(items) | Self::Or(items) => {
                for item in items {
                    item.for_each_term(visit);
                }
            }
        }
    }
}

impl std::ops::Not for Query {
    type Output = Query;

    /// `-query`.
    fn not(self) -> Query {
        Query::Not(Box::new(self))
    }
}

/// Free text or the value of a text operator.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Text {
    /// A bare word. Words that the analyzer splits into several tokens
    /// (`anna@example.com`, `e-mail`) match those tokens next to each other.
    Word(String),
    /// A quoted phrase: its tokens must appear next to each other, in order.
    Phrase(String),
}

impl Text {
    /// The text without quotes.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Word(text) | Self::Phrase(text) => text,
        }
    }

    /// Whether this is a quoted phrase.
    pub fn is_phrase(&self) -> bool {
        matches!(self, Self::Phrase(_))
    }
}

/// One search term: free text or an operator with its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    /// Free text, searched in subject, body, addresses and attachments.
    Text(Text),
    /// `from:` sender name or address.
    From(Text),
    /// `to:` recipient name or address.
    To(Text),
    /// `cc:` copy recipient name or address.
    Cc(Text),
    /// `subject:` text in the subject.
    Subject(Text),
    /// `filename:` text in an attachment name.
    Filename(Text),
    /// `has:attachment`.
    HasAttachment,
    /// `tag:` (alias `label:`) a tag name.
    Tag(String),
    /// `is:unread`, `is:read`, `is:starred`, `is:flagged`.
    Is(Status),
    /// `in:` a folder role or a folder name.
    In(Mailbox),
    /// `account:` an account name.
    Account(String),
    /// `list:` a mailing list id or address.
    List(String),
    /// `before:YYYY-MM-DD`: sent before the start of that day (local time).
    Before(Date),
    /// `after:YYYY-MM-DD`: sent on or after the start of that day (local time).
    After(Date),
    /// `older_than:30d`: sent longer ago than that.
    OlderThan(Age),
    /// `newer_than:2w`: sent within that time.
    NewerThan(Age),
    /// `larger:5M`: larger than that many bytes.
    Larger(Size),
    /// `smaller:200k`: smaller than that many bytes.
    Smaller(Size),
}

/// The value of `is:`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    /// `is:unread`: `\Seen` is not set.
    Unread,
    /// `is:read`: `\Seen` is set.
    Read,
    /// `is:starred`: `\Flagged` is set (Gmail's name).
    Starred,
    /// `is:flagged`: `\Flagged` is set (Thunderbird's name).
    Flagged,
}

impl Status {
    /// The keyword after `is:`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unread => "unread",
            Self::Read => "read",
            Self::Starred => "starred",
            Self::Flagged => "flagged",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value.to_ascii_lowercase().as_str() {
            "unread" => Self::Unread,
            "read" => Self::Read,
            "starred" => Self::Starred,
            "flagged" => Self::Flagged,
            _ => return None,
        })
    }
}

/// The value of `in:`: a special-use role (`in:inbox`, `in:sent`, …) or a folder name.
///
/// Role names win: `in:sent` always means the role, matched case-insensitively.
/// `in:spam` is accepted as an alias of `in:junk`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Mailbox {
    /// A folder role, resolved per account by the caller.
    Role(FolderRole),
    /// A folder (or Gmail label) name or path.
    Name(String),
}

impl Mailbox {
    pub(crate) fn parse(value: &str) -> Self {
        let lower = value.to_ascii_lowercase();
        match lower.as_str() {
            "spam" => Self::Role(FolderRole::Junk),
            other => FolderRole::parse(other)
                .map(Self::Role)
                .unwrap_or_else(|| Self::Name(value.to_owned())),
        }
    }
}

/// A relative age such as `30d` for `older_than:` and `newer_than:`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Age {
    /// How many units.
    pub count: u32,
    /// The unit.
    pub unit: AgeUnit,
}

/// The unit of an [`Age`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgeUnit {
    /// `d`
    Days,
    /// `w`
    Weeks,
    /// `m`: calendar months.
    Months,
    /// `y`: calendar years.
    Years,
}

impl AgeUnit {
    /// The suffix letter.
    pub fn suffix(self) -> char {
        match self {
            Self::Days => 'd',
            Self::Weeks => 'w',
            Self::Months => 'm',
            Self::Years => 'y',
        }
    }

    pub(crate) fn from_suffix(c: char) -> Option<Self> {
        Some(match c.to_ascii_lowercase() {
            'd' => Self::Days,
            'w' => Self::Weeks,
            'm' => Self::Months,
            'y' => Self::Years,
            _ => return None,
        })
    }
}

/// A message size such as `5M` or `200k` for `larger:` and `smaller:`.
///
/// Units are binary: `k` = 1024 bytes, `M` = 1024², `G` = 1024³.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Size {
    value: u64,
    unit: SizeUnit,
}

impl Size {
    /// A size of `value` units, or `None` if it doesn't fit in 64 bits of bytes.
    pub fn new(value: u64, unit: SizeUnit) -> Option<Self> {
        value.checked_mul(unit.factor())?;
        Some(Self { value, unit })
    }

    /// The number as typed.
    pub fn value(self) -> u64 {
        self.value
    }

    /// The unit as typed.
    pub fn unit(self) -> SizeUnit {
        self.unit
    }

    /// The size in bytes.
    pub fn bytes(self) -> u64 {
        // Checked in `new`.
        self.value.saturating_mul(self.unit.factor())
    }
}

/// The unit of a [`Size`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SizeUnit {
    /// No suffix: bytes.
    Bytes,
    /// `k`: KiB.
    Kilo,
    /// `M`: MiB.
    Mega,
    /// `G`: GiB.
    Giga,
}

impl SizeUnit {
    /// Bytes per unit.
    pub fn factor(self) -> u64 {
        match self {
            Self::Bytes => 1,
            Self::Kilo => 1 << 10,
            Self::Mega => 1 << 20,
            Self::Giga => 1 << 30,
        }
    }

    /// The canonical suffix (empty for bytes).
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Bytes => "",
            Self::Kilo => "k",
            Self::Mega => "M",
            Self::Giga => "G",
        }
    }

    pub(crate) fn from_suffix(c: char) -> Option<Self> {
        Some(match c.to_ascii_lowercase() {
            'k' => Self::Kilo,
            'm' => Self::Mega,
            'g' => Self::Giga,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(s: &str) -> Query {
        Query::Term(Term::Text(Text::Word(s.to_owned())))
    }

    #[test]
    fn normalize_flattens_and_unwraps() {
        let q = Query::And(vec![
            Query::And(vec![word("a"), Query::all()]),
            Query::Or(vec![Query::Or(vec![word("b"), word("c")])]),
        ]);
        assert_eq!(
            q.normalize(),
            Query::And(vec![word("a"), Query::Or(vec![word("b"), word("c")])])
        );
        assert_eq!(Query::And(vec![word("a")]).normalize(), word("a"));
        assert!(
            Query::Or(vec![word("a"), Query::all()])
                .normalize()
                .is_all()
        );
    }

    #[test]
    fn sizes_check_overflow() {
        assert_eq!(
            Size::new(5, SizeUnit::Mega).unwrap().bytes(),
            5 * 1024 * 1024
        );
        assert!(Size::new(u64::MAX, SizeUnit::Kilo).is_none());
    }

    #[test]
    fn mailbox_roles_win() {
        assert_eq!(Mailbox::parse("Inbox"), Mailbox::Role(FolderRole::Inbox));
        assert_eq!(Mailbox::parse("spam"), Mailbox::Role(FolderRole::Junk));
        assert_eq!(
            Mailbox::parse("Projects/Maze"),
            Mailbox::Name("Projects/Maze".to_owned())
        );
    }
}
