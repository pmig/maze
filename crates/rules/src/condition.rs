//! Conditions: which messages a rule matches.

use std::fmt;

use mazemail_types::Address;
use serde_json::{Value, json};

use crate::thunderbird;

/// What a rule looks at in a message.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Field {
    /// The sender (name or address).
    From,
    /// A `To` recipient.
    To,
    /// A `Cc` recipient.
    Cc,
    /// A `To` or `Cc` recipient.
    ToOrCc,
    /// The sender or any recipient.
    AnyAddress,
    /// The subject.
    Subject,
    /// The message text: the preview Maze keeps of it (the first few hundred
    /// characters), once the body has been downloaded.
    Body,
    /// The mailing list (`List-Id`).
    ListId,
    /// Whether the message has attachments; the value is `true` or `false`.
    HasAttachment,
    /// Something Maze doesn't evaluate (kept as written).
    Other(String),
}

impl Field {
    /// The stored name.
    pub fn as_str(&self) -> &str {
        match self {
            Self::From => "from",
            Self::To => "to",
            Self::Cc => "cc",
            Self::ToOrCc => "to_or_cc",
            Self::AnyAddress => "any_address",
            Self::Subject => "subject",
            Self::Body => "body",
            Self::ListId => "list_id",
            Self::HasAttachment => "has_attachment",
            Self::Other(name) => name,
        }
    }

    /// The field with this stored name.
    pub fn parse(name: &str) -> Self {
        match name {
            "from" => Self::From,
            "to" => Self::To,
            "cc" => Self::Cc,
            "to_or_cc" => Self::ToOrCc,
            "any_address" => Self::AnyAddress,
            "subject" => Self::Subject,
            "body" => Self::Body,
            "list_id" => Self::ListId,
            "has_attachment" => Self::HasAttachment,
            other => Self::Other(other.to_owned()),
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::ToOrCc => "to or cc",
            Self::AnyAddress => "any address",
            Self::ListId => "list-id",
            Self::HasAttachment => "has attachment",
            other => other.as_str(),
        }
    }
}

/// How a term compares. All comparisons ignore case.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Op {
    /// The text contains the value.
    Contains,
    /// The text doesn't contain the value.
    NotContains,
    /// The text is the value (for addresses: the address or the name).
    Is,
    /// The text isn't the value.
    IsNot,
    /// The text begins with the value.
    BeginsWith,
    /// The text ends with the value.
    EndsWith,
    /// Something Maze doesn't evaluate (kept as written).
    Other(String),
}

impl Op {
    /// The stored name.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Contains => "contains",
            Self::NotContains => "not_contains",
            Self::Is => "is",
            Self::IsNot => "is_not",
            Self::BeginsWith => "begins_with",
            Self::EndsWith => "ends_with",
            Self::Other(name) => name,
        }
    }

    /// The operator with this stored name.
    pub fn parse(name: &str) -> Self {
        match name {
            "contains" => Self::Contains,
            "not_contains" => Self::NotContains,
            "is" => Self::Is,
            "is_not" => Self::IsNot,
            "begins_with" => Self::BeginsWith,
            "ends_with" => Self::EndsWith,
            other => Self::Other(other.to_owned()),
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::NotContains => "doesn't contain",
            Self::IsNot => "isn't",
            Self::BeginsWith => "begins with",
            Self::EndsWith => "ends with",
            other => other.as_str(),
        }
    }

    /// The negation, for `Not` in a search query.
    pub fn negated(&self) -> Option<Self> {
        Some(match self {
            Self::Contains => Self::NotContains,
            Self::NotContains => Self::Contains,
            Self::Is => Self::IsNot,
            Self::IsNot => Self::Is,
            _ => return None,
        })
    }
}

/// How the terms combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Match {
    /// Every term matches.
    All,
    /// At least one term matches.
    Any,
    /// Every message (no terms; Thunderbird's "Match all messages").
    AllMessages,
}

impl Match {
    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Any => "any",
            Self::AllMessages => "all_messages",
        }
    }
}

/// One comparison.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Term {
    /// What is compared.
    pub field: Field,
    /// How.
    pub op: Op,
    /// With what.
    pub value: String,
}

impl Term {
    /// A term.
    pub fn new(field: Field, op: Op, value: impl Into<String>) -> Self {
        Self {
            field,
            op,
            value: value.into(),
        }
    }

    /// Whether Maze evaluates this term.
    pub fn is_supported(&self) -> bool {
        let field = !matches!(self.field, Field::Other(_));
        let op = match self.field {
            Field::HasAttachment => matches!(self.op, Op::Is | Op::IsNot),
            _ => !matches!(self.op, Op::Other(_)),
        };
        field && op
    }

    /// Whether `message` matches. Unsupported terms never match.
    pub fn matches(&self, message: &Message<'_>) -> bool {
        if !self.is_supported() {
            return false;
        }
        let needle = self.value.to_lowercase();
        match &self.field {
            Field::HasAttachment => {
                let wanted = !matches!(needle.trim(), "false" | "no" | "0");
                (message.has_attachment == wanted) == (self.op == Op::Is)
            }
            Field::Subject => self.compare_text(message.subject, &needle),
            Field::Body => self.compare_text(message.body, &needle),
            Field::ListId => {
                // `List-Id: Dev list <dev.example.org>`: the id, or the whole header.
                let whole = message.list_id.unwrap_or_default();
                let id = whole
                    .rsplit_once('<')
                    .and_then(|(_, rest)| rest.split_once('>'))
                    .map(|(id, _)| id);
                let positive = self.positive();
                let hit = positive.compare(&whole.to_lowercase(), &needle)
                    || id.is_some_and(|id| positive.compare(&id.to_lowercase(), &needle));
                hit != self.is_negative()
            }
            Field::From | Field::To | Field::Cc | Field::ToOrCc | Field::AnyAddress => {
                let addresses: Vec<&Address> = match &self.field {
                    Field::From => message.from.iter().copied().collect(),
                    Field::To => message.to.iter().collect(),
                    Field::Cc => message.cc.iter().collect(),
                    Field::ToOrCc => message.to.iter().chain(message.cc).collect(),
                    _ => message
                        .from
                        .iter()
                        .copied()
                        .chain(message.to)
                        .chain(message.cc)
                        .collect(),
                };
                let positive = self.positive();
                let hit = addresses.iter().any(|address| {
                    address_texts(address)
                        .iter()
                        .any(|text| positive.compare(text, &needle))
                });
                hit != self.is_negative()
            }
            Field::Other(_) => false,
        }
    }

    fn compare_text(&self, text: Option<&str>, needle: &str) -> bool {
        let text = text.unwrap_or_default().to_lowercase();
        self.positive().compare(&text, needle) != self.is_negative()
    }

    fn is_negative(&self) -> bool {
        matches!(self.op, Op::NotContains | Op::IsNot)
    }

    /// The operator without its negation.
    fn positive(&self) -> Positive {
        match self.op {
            Op::Contains | Op::NotContains => Positive::Contains,
            Op::Is | Op::IsNot => Positive::Is,
            Op::BeginsWith => Positive::BeginsWith,
            _ => Positive::EndsWith,
        }
    }

    fn to_json(&self) -> Value {
        json!({ "field": self.field.as_str(), "op": self.op.as_str(), "value": self.value })
    }
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.field == Field::HasAttachment && self.is_supported() {
            let wanted = !matches!(self.value.trim(), "false" | "no" | "0");
            return f.write_str(if wanted == (self.op == Op::Is) {
                "has attachments"
            } else {
                "has no attachments"
            });
        }
        write!(
            f,
            "{} {} {:?}",
            self.field.label(),
            self.op.label(),
            self.value
        )
    }
}

#[derive(Clone, Copy)]
enum Positive {
    Contains,
    Is,
    BeginsWith,
    EndsWith,
}

impl Positive {
    fn compare(self, text: &str, needle: &str) -> bool {
        match self {
            Self::Contains => text.contains(needle),
            Self::Is => text == needle,
            Self::BeginsWith => text.starts_with(needle),
            Self::EndsWith => text.ends_with(needle),
        }
    }
}

/// The texts an address term compares with, lowercase: the address, the name and
/// `Name <address>`.
fn address_texts(address: &Address) -> Vec<String> {
    let email = address.email.to_lowercase();
    match address
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        Some(name) => {
            let name = name.to_lowercase();
            let full = format!("{name} <{email}>");
            vec![email, name, full]
        }
        None => vec![email],
    }
}

/// When a rule matches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Condition {
    /// How the terms combine.
    pub matching: Match,
    /// The terms (none for [`Match::AllMessages`]).
    pub terms: Vec<Term>,
}

impl Condition {
    /// Every term must match.
    pub fn all(terms: Vec<Term>) -> Self {
        Self {
            matching: Match::All,
            terms,
        }
    }

    /// Any term may match.
    pub fn any(terms: Vec<Term>) -> Self {
        Self {
            matching: Match::Any,
            terms,
        }
    }

    /// Whether `message` matches. `all` or `any` without terms matches nothing.
    pub fn matches(&self, message: &Message<'_>) -> bool {
        match self.matching {
            Match::AllMessages => true,
            _ if self.terms.is_empty() => false,
            Match::All => self.terms.iter().all(|t| t.matches(message)),
            Match::Any => self.terms.iter().any(|t| t.matches(message)),
        }
    }

    /// Whether a term looks at the message text.
    pub fn needs_body(&self) -> bool {
        self.matching != Match::AllMessages && self.terms.iter().any(|t| t.field == Field::Body)
    }

    /// Reads the stored JSON: the canonical format, or the one the Thunderbird import
    /// wrote before the rules engine (`"match": "terms"` with Thunderbird's terms).
    pub(crate) fn from_json(value: &Value) -> Result<Self, String> {
        let object = value.as_object().ok_or("expected an object")?;
        let matching = object
            .get("match")
            .and_then(Value::as_str)
            .ok_or("`match` is missing")?;
        let terms: &[Value] = object
            .get("terms")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice);
        match matching {
            "all_messages" => Ok(Self {
                matching: Match::AllMessages,
                terms: Vec::new(),
            }),
            "all" | "any" => {
                let terms = terms.iter().map(term_from_json).collect::<Result<_, _>>()?;
                Ok(Self {
                    matching: if matching == "all" {
                        Match::All
                    } else {
                        Match::Any
                    },
                    terms,
                })
            }
            "terms" => thunderbird::legacy_condition(terms),
            other => Err(format!("unknown match `{other}`")),
        }
    }

    pub(crate) fn to_json(&self) -> Value {
        let terms: Vec<Value> = self.terms.iter().map(Term::to_json).collect();
        json!({ "match": self.matching.as_str(), "terms": terms })
    }
}

fn term_from_json(value: &Value) -> Result<Term, String> {
    let text = |key: &str| value.get(key).and_then(Value::as_str);
    let field = text("field").ok_or("a term has no `field`")?;
    let op = text("op").ok_or("a term has no `op`")?;
    Ok(Term::new(
        Field::parse(field),
        Op::parse(op),
        text("value").unwrap_or_default(),
    ))
}

impl fmt::Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let joiner = match self.matching {
            Match::AllMessages => return f.write_str("every message"),
            _ if self.terms.is_empty() => return f.write_str("no message"),
            Match::All => " and ",
            Match::Any => " or ",
        };
        let terms: Vec<String> = self.terms.iter().map(ToString::to_string).collect();
        f.write_str(&terms.join(joiner))
    }
}

/// What a rule sees of a message.
#[derive(Debug, Clone, Copy, Default)]
pub struct Message<'a> {
    /// The sender.
    pub from: Option<&'a Address>,
    /// `To` recipients.
    pub to: &'a [Address],
    /// `Cc` recipients.
    pub cc: &'a [Address],
    /// The subject.
    pub subject: Option<&'a str>,
    /// The text preview, if the body has been downloaded.
    pub body: Option<&'a str>,
    /// The `List-Id` header.
    pub list_id: Option<&'a str>,
    /// Whether the message has attachments.
    pub has_attachment: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(name: Option<&str>, email: &str) -> Address {
        Address {
            name: name.map(str::to_owned),
            email: email.to_owned(),
        }
    }

    fn term(field: Field, op: Op, value: &str) -> Term {
        Term::new(field, op, value)
    }

    #[test]
    fn address_terms_compare_name_address_and_both() {
        let anna = address(Some("Anna Berg"), "Anna@Example.com");
        let bob = address(None, "bob@example.net");
        let to = [bob];
        let message = Message {
            from: Some(&anna),
            to: &to,
            ..Message::default()
        };
        assert!(term(Field::From, Op::Contains, "anna").matches(&message));
        assert!(term(Field::From, Op::Is, "anna@example.com").matches(&message));
        assert!(term(Field::From, Op::Is, "anna berg").matches(&message));
        assert!(term(Field::From, Op::BeginsWith, "Anna Berg <").matches(&message));
        assert!(term(Field::From, Op::EndsWith, "example.com").matches(&message));
        assert!(!term(Field::From, Op::Is, "anna").matches(&message));
        assert!(term(Field::From, Op::IsNot, "anna").matches(&message));
        assert!(!term(Field::From, Op::NotContains, "berg").matches(&message));
        assert!(term(Field::To, Op::Contains, "bob").matches(&message));
        assert!(!term(Field::Cc, Op::Contains, "bob").matches(&message));
        assert!(term(Field::Cc, Op::NotContains, "bob").matches(&message));
        assert!(term(Field::ToOrCc, Op::Contains, "bob").matches(&message));
        assert!(term(Field::AnyAddress, Op::Contains, "anna").matches(&message));
        assert!(!term(Field::ToOrCc, Op::Contains, "anna").matches(&message));
    }

    #[test]
    fn text_list_and_attachment_terms() {
        let message = Message {
            subject: Some("[dev] Release 1.2"),
            body: Some("The release notes are attached."),
            list_id: Some("Dev list <dev.example.org>"),
            has_attachment: true,
            ..Message::default()
        };
        assert!(term(Field::Subject, Op::BeginsWith, "[DEV]").matches(&message));
        assert!(term(Field::Subject, Op::EndsWith, "1.2").matches(&message));
        assert!(!term(Field::Subject, Op::Is, "release").matches(&message));
        assert!(term(Field::Body, Op::Contains, "release notes").matches(&message));
        assert!(term(Field::ListId, Op::Is, "dev.example.org").matches(&message));
        assert!(term(Field::ListId, Op::Contains, "dev list").matches(&message));
        assert!(!term(Field::ListId, Op::IsNot, "dev.example.org").matches(&message));
        assert!(term(Field::HasAttachment, Op::Is, "true").matches(&message));
        assert!(!term(Field::HasAttachment, Op::Is, "false").matches(&message));
        assert!(term(Field::HasAttachment, Op::IsNot, "false").matches(&message));
        assert!(!term(Field::HasAttachment, Op::Contains, "true").matches(&message));
        // Missing text is empty.
        let bare = Message::default();
        assert!(!term(Field::Subject, Op::Contains, "x").matches(&bare));
        assert!(term(Field::Subject, Op::NotContains, "x").matches(&bare));
        assert!(!term(Field::Other("size".into()), Op::Is, "1").matches(&bare));
    }

    #[test]
    fn all_any_and_every_message() {
        let message = Message {
            subject: Some("Invoice"),
            ..Message::default()
        };
        let hit = term(Field::Subject, Op::Contains, "invoice");
        let miss = term(Field::Subject, Op::Contains, "receipt");
        assert!(!Condition::all(vec![hit.clone(), miss.clone()]).matches(&message));
        assert!(Condition::any(vec![hit.clone(), miss]).matches(&message));
        assert!(!Condition::all(Vec::new()).matches(&message));
        assert!(!Condition::any(Vec::new()).matches(&message));
        let every = Condition {
            matching: Match::AllMessages,
            terms: Vec::new(),
        };
        assert!(every.matches(&message));
        assert_eq!(every.to_string(), "every message");
        assert_eq!(
            Condition::all(vec![hit, term(Field::HasAttachment, Op::Is, "false")]).to_string(),
            "subject contains \"invoice\" and has no attachments"
        );
    }
}
