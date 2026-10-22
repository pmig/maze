//! The query parser (CONCEPT §13.2), built on `winnow`.
//!
//! Grammar, loosest binding first:
//!
//! ```text
//! query       := ws sequence ws EOF
//! sequence    := (disjunction | "AND")*          juxtaposition is AND
//! disjunction := unary ("OR" unary)*
//! unary       := "-" unary | primary
//! primary     := "(" sequence ")" | phrase | operator ":" value | word
//! value       := phrase | bare
//! ```
//!
//! Error messages never echo what the user typed (it is sensitive); they name the
//! operator and carry the byte span to underline.

use std::borrow::Cow;
use std::fmt;
use std::ops::Range;

use time::{Date, Month};
use winnow::LocatingSlice;
use winnow::ascii::{digit1, multispace0};
use winnow::error::{ErrMode, ParserError};
use winnow::stream::{Location, Stream};
use winnow::token::{any, take_while};
use winnow::{ModalResult, Parser as _};

use super::ast::{Age, AgeUnit, Mailbox, Query, Size, SizeUnit, Status, Term, Text};

/// Groups nested deeper than this are rejected instead of risking the stack.
const MAX_DEPTH: usize = 32;

type Input<'i> = LocatingSlice<&'i str>;
type PResult<T> = ModalResult<T, SyntaxError>;

/// A query that couldn't be parsed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message} (at {}..{})", span.start, span.end)]
pub struct ParseError {
    /// A friendly description that doesn't repeat the query text.
    pub message: Cow<'static, str>,
    /// Byte range in the input to underline.
    pub span: Range<usize>,
}

/// Something the parser accepted but the user may not have meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseWarning {
    /// A friendly description that doesn't repeat the query text.
    pub message: Cow<'static, str>,
    /// Byte range in the input to underline.
    pub span: Range<usize>,
}

impl fmt::Display for ParseWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (at {}..{})",
            self.message, self.span.start, self.span.end
        )
    }
}

/// A successfully parsed query and any warnings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    /// The normalized query.
    pub query: Query,
    /// Things worth pointing out, such as unknown operators searched as text.
    pub warnings: Vec<ParseWarning>,
}

/// Parses a search query.
///
/// Unknown operators (`foo:bar`) are searched as free text and reported as a warning.
pub fn parse(input: &str) -> Result<Parsed, ParseError> {
    let mut parser = Parser::default();
    let mut stream = LocatingSlice::new(input);
    match parser.query(&mut stream) {
        Ok(query) => Ok(Parsed {
            query,
            warnings: parser.warnings,
        }),
        Err(err) => {
            let err = match err {
                ErrMode::Backtrack(e) | ErrMode::Cut(e) => e,
                ErrMode::Incomplete(_) => SyntaxError::at(input.len(), "incomplete query"),
            };
            Err(ParseError {
                message: err.message,
                span: err.span,
            })
        }
    }
}

/// The parser's error type: a friendly message plus a span.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SyntaxError {
    message: Cow<'static, str>,
    span: Range<usize>,
}

impl SyntaxError {
    fn new(message: impl Into<Cow<'static, str>>, span: Range<usize>) -> Self {
        Self {
            message: message.into(),
            span,
        }
    }

    fn at(position: usize, message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(message, position..position)
    }

    fn cut<T>(self) -> PResult<T> {
        Err(ErrMode::Cut(self))
    }
}

impl ParserError<Input<'_>> for SyntaxError {
    type Inner = Self;

    fn from_input(input: &Input<'_>) -> Self {
        Self::at(input.current_token_start(), "unexpected input")
    }

    fn into_inner(self) -> Result<Self::Inner, Self> {
        Ok(self)
    }
}

/// The operators the language knows. Names are matched case-insensitively.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operator {
    From,
    To,
    Cc,
    Subject,
    Filename,
    Has,
    Tag,
    Is,
    In,
    Account,
    List,
    Before,
    After,
    OlderThan,
    NewerThan,
    Larger,
    Smaller,
}

impl Operator {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "from" => Self::From,
            "to" => Self::To,
            "cc" => Self::Cc,
            "subject" => Self::Subject,
            "filename" => Self::Filename,
            "has" => Self::Has,
            "tag" | "label" => Self::Tag,
            "is" => Self::Is,
            "in" => Self::In,
            "account" => Self::Account,
            "list" => Self::List,
            "before" => Self::Before,
            "after" => Self::After,
            "older_than" => Self::OlderThan,
            "newer_than" => Self::NewerThan,
            "larger" => Self::Larger,
            "smaller" => Self::Smaller,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::From => "from:",
            Self::To => "to:",
            Self::Cc => "cc:",
            Self::Subject => "subject:",
            Self::Filename => "filename:",
            Self::Has => "has:",
            Self::Tag => "tag:",
            Self::Is => "is:",
            Self::In => "in:",
            Self::Account => "account:",
            Self::List => "list:",
            Self::Before => "before:",
            Self::After => "after:",
            Self::OlderThan => "older_than:",
            Self::NewerThan => "newer_than:",
            Self::Larger => "larger:",
            Self::Smaller => "smaller:",
        }
    }

    fn example(self) -> &'static str {
        match self {
            Self::From => "from:anna",
            Self::To => "to:anna@example.com",
            Self::Cc => "cc:anna",
            Self::Subject => "subject:\"offsite plan\"",
            Self::Filename => "filename:pdf",
            Self::Has => "has:attachment",
            Self::Tag => "tag:todo",
            Self::Is => "is:unread",
            Self::In => "in:inbox",
            Self::Account => "account:work",
            Self::List => "list:dev@example.com",
            Self::Before => "before:2026-01-31",
            Self::After => "after:2025-10-01",
            Self::OlderThan => "older_than:30d",
            Self::NewerThan => "newer_than:2w",
            Self::Larger => "larger:5M",
            Self::Smaller => "smaller:200k",
        }
    }
}

/// Characters that end a bare word or value.
fn is_delimiter(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '"')
}

fn is_operator_name_char(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// A raw operator value: what was written and where.
struct RawValue {
    text: String,
    quoted: bool,
    span: Range<usize>,
}

#[derive(Default)]
struct Parser {
    warnings: Vec<ParseWarning>,
    depth: usize,
}

impl Parser {
    fn query(&mut self, input: &mut Input<'_>) -> PResult<Query> {
        let query = self.sequence(input)?;
        multispace0.parse_next(input)?;
        if let Some(c) = input.peek_token() {
            let at = input.current_token_start();
            let message = if c == ')' {
                "this `)` has no matching `(`"
            } else {
                "unexpected input"
            };
            return SyntaxError::new(message, at..at + c.len_utf8()).cut();
        }
        Ok(query)
    }

    /// Terms separated by whitespace (implicit AND), up to `)` or the end.
    fn sequence(&mut self, input: &mut Input<'_>) -> PResult<Query> {
        let mut items = Vec::new();
        loop {
            multispace0.parse_next(input)?;
            match input.peek_token() {
                None | Some(')') => break,
                Some(_) => {}
            }
            let start = input.current_token_start();
            if keyword(input, "AND") {
                if items.is_empty() {
                    return SyntaxError::new(
                        "AND needs a search term on its left",
                        start..start + 3,
                    )
                    .cut();
                }
                multispace0.parse_next(input)?;
                if matches!(input.peek_token(), None | Some(')')) {
                    return SyntaxError::new(
                        "AND needs a search term on its right",
                        start..start + 3,
                    )
                    .cut();
                }
                continue;
            }
            if keyword(input, "OR") {
                return SyntaxError::new("OR needs a search term on its left", start..start + 2)
                    .cut();
            }
            items.push(self.disjunction(input)?);
        }
        Ok(Query::and(items))
    }

    /// `unary (OR unary)*`.
    fn disjunction(&mut self, input: &mut Input<'_>) -> PResult<Query> {
        let mut alternatives = vec![self.unary(input)?];
        loop {
            let checkpoint = input.checkpoint();
            multispace0.parse_next(input)?;
            let start = input.current_token_start();
            if !keyword(input, "OR") {
                input.reset(&checkpoint);
                break;
            }
            multispace0.parse_next(input)?;
            if matches!(input.peek_token(), None | Some(')')) {
                return SyntaxError::new("OR needs a search term on its right", start..start + 2)
                    .cut();
            }
            if peek_keyword(input, "OR") || peek_keyword(input, "AND") {
                return SyntaxError::new("OR needs a search term on its right", start..start + 2)
                    .cut();
            }
            alternatives.push(self.unary(input)?);
        }
        Ok(Query::or(alternatives))
    }

    /// `-unary | primary`.
    fn unary(&mut self, input: &mut Input<'_>) -> PResult<Query> {
        let start = input.current_token_start();
        if input.peek_token() == Some('-') {
            let checkpoint = input.checkpoint();
            '-'.parse_next(input)?;
            match input.peek_token() {
                Some(c) if !c.is_whitespace() && c != ')' => {
                    self.enter(start)?;
                    let inner = self.unary(input);
                    self.depth -= 1;
                    return Ok(!inner?);
                }
                _ => {
                    // A lone `-`: search for it as text, like any other punctuation.
                    input.reset(&checkpoint);
                }
            }
        }
        self.primary(input)
    }

    fn primary(&mut self, input: &mut Input<'_>) -> PResult<Query> {
        let start = input.current_token_start();
        match input.peek_token() {
            Some('(') => {
                '('.parse_next(input)?;
                self.enter(start)?;
                let inner = self.sequence(input);
                self.depth -= 1;
                let inner = inner?;
                multispace0.parse_next(input)?;
                if input.peek_token() != Some(')') {
                    return SyntaxError::new("this `(` is never closed", start..start + 1).cut();
                }
                ')'.parse_next(input)?;
                if inner.is_all() {
                    return SyntaxError::new(
                        "empty parentheses",
                        start..input.previous_token_end(),
                    )
                    .cut();
                }
                Ok(inner)
            }
            Some('"') => {
                let (text, span) = phrase(input)?;
                if text.is_empty() {
                    return SyntaxError::new("empty quotes", span).cut();
                }
                Ok(Query::Term(Term::Text(Text::Phrase(text))))
            }
            _ => self.word_or_operator(input),
        }
    }

    fn word_or_operator(&mut self, input: &mut Input<'_>) -> PResult<Query> {
        let start = input.current_token_start();
        let checkpoint = input.checkpoint();
        let name: PResult<&str> = take_while(1.., is_operator_name_char).parse_next(input);
        if let Ok(name) = name
            && input.peek_token() == Some(':')
        {
            if let Some(operator) = Operator::from_name(name) {
                ':'.parse_next(input)?;
                let colon_end = input.current_token_start();
                return self.operator_value(input, operator, start..colon_end);
            }
            input.reset(&checkpoint);
            let word = bare(input)?;
            self.warnings.push(ParseWarning {
                message: "unknown operator, searching for it as text".into(),
                span: start..input.previous_token_end(),
            });
            return Ok(Query::Term(Term::Text(Text::Word(word.to_owned()))));
        }
        input.reset(&checkpoint);
        let word = bare(input)?;
        Ok(Query::Term(Term::Text(Text::Word(word.to_owned()))))
    }

    fn operator_value(
        &mut self,
        input: &mut Input<'_>,
        operator: Operator,
        operator_span: Range<usize>,
    ) -> PResult<Query> {
        let value = match input.peek_token() {
            Some('"') => {
                let (text, span) = phrase(input)?;
                RawValue {
                    text,
                    quoted: true,
                    span,
                }
            }
            Some(c) if !is_delimiter(c) => {
                let start = input.current_token_start();
                let text = bare(input)?.to_owned();
                RawValue {
                    text,
                    quoted: false,
                    span: start..input.previous_token_end(),
                }
            }
            _ => {
                return SyntaxError::new(
                    format!(
                        "`{}` needs a value, for example {}",
                        operator.name(),
                        operator.example()
                    ),
                    operator_span,
                )
                .cut();
            }
        };
        if value.text.trim().is_empty() {
            return SyntaxError::new(
                format!(
                    "`{}` needs a value, for example {}",
                    operator.name(),
                    operator.example()
                ),
                operator_span.start..value.span.end,
            )
            .cut();
        }
        let term = match operator {
            Operator::From => Term::From(value.into_text()),
            Operator::To => Term::To(value.into_text()),
            Operator::Cc => Term::Cc(value.into_text()),
            Operator::Subject => Term::Subject(value.into_text()),
            Operator::Filename => Term::Filename(value.into_text()),
            Operator::Has => {
                if value.text.eq_ignore_ascii_case("attachment") {
                    Term::HasAttachment
                } else {
                    return value.invalid("`has:` supports only has:attachment").cut();
                }
            }
            Operator::Tag => Term::Tag(value.text),
            Operator::Is => match Status::parse(&value.text) {
                Some(status) => Term::Is(status),
                None => {
                    return value
                        .invalid("`is:` supports unread, read, starred and flagged")
                        .cut();
                }
            },
            Operator::In => Term::In(Mailbox::parse(&value.text)),
            Operator::Account => Term::Account(value.text),
            Operator::List => Term::List(value.text),
            Operator::Before | Operator::After => match parse_date(&value.text) {
                Some(date) if operator == Operator::Before => Term::Before(date),
                Some(date) => Term::After(date),
                None => {
                    return value
                        .invalid(format!(
                            "`{}` expects a date like {}",
                            operator.name(),
                            operator.example()
                        ))
                        .cut();
                }
            },
            Operator::OlderThan | Operator::NewerThan => match parse_age(&value.text) {
                Some(age) if operator == Operator::OlderThan => Term::OlderThan(age),
                Some(age) => Term::NewerThan(age),
                None => {
                    return value
                        .invalid(format!(
                            "`{}` expects an age like 30d, 2w, 6m or 1y",
                            operator.name()
                        ))
                        .cut();
                }
            },
            Operator::Larger | Operator::Smaller => match parse_size(&value.text) {
                Ok(size) if operator == Operator::Larger => Term::Larger(size),
                Ok(size) => Term::Smaller(size),
                Err(SizeError::Syntax) => {
                    return value
                        .invalid(format!(
                            "`{}` expects a size like 200k or 5M",
                            operator.name()
                        ))
                        .cut();
                }
                Err(SizeError::TooLarge) => {
                    return value.invalid("this size is too large").cut();
                }
            },
        };
        Ok(Query::Term(term))
    }

    fn enter(&mut self, at: usize) -> PResult<()> {
        if self.depth >= MAX_DEPTH {
            return SyntaxError::new("the query is nested too deeply", at..at + 1).cut();
        }
        self.depth += 1;
        Ok(())
    }
}

impl RawValue {
    fn into_text(self) -> Text {
        if self.quoted {
            Text::Phrase(self.text)
        } else {
            Text::Word(self.text)
        }
    }

    fn invalid(&self, message: impl Into<Cow<'static, str>>) -> SyntaxError {
        SyntaxError::new(message, self.span.clone())
    }
}

/// A bare word: everything up to whitespace, a parenthesis or a quote.
fn bare<'i>(input: &mut Input<'i>) -> PResult<&'i str> {
    take_while(1.., |c: char| !is_delimiter(c)).parse_next(input)
}

/// A double-quoted string with `\"` and `\\` escapes. Returns the unescaped content and
/// the span including the quotes.
fn phrase(input: &mut Input<'_>) -> PResult<(String, Range<usize>)> {
    let start = input.current_token_start();
    '"'.parse_next(input)?;
    let mut text = String::new();
    loop {
        let c: PResult<char> = any.parse_next(input);
        match c {
            Ok('"') => break,
            Ok('\\') => match input.peek_token() {
                Some(next @ ('"' | '\\')) => {
                    any::<_, ErrMode<SyntaxError>>.parse_next(input)?;
                    text.push(next);
                }
                _ => text.push('\\'),
            },
            Ok(c) => text.push(c),
            Err(_) => {
                return SyntaxError::new("this quote is never closed", start..start + 1).cut();
            }
        }
    }
    Ok((text, start..input.previous_token_end()))
}

/// Consumes `word` if it stands alone (followed by whitespace, a parenthesis or the end).
fn keyword(input: &mut Input<'_>, word: &str) -> bool {
    if peek_keyword(input, word) {
        let _ = input.next_slice(word.len());
        true
    } else {
        false
    }
}

fn peek_keyword(input: &Input<'_>, word: &str) -> bool {
    let rest: &str = input;
    rest.strip_prefix(word).is_some_and(|after| {
        after
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '(' || c == ')')
    })
}

/// `YYYY-MM-DD` or `YYYY/MM/DD`.
fn parse_date(text: &str) -> Option<Date> {
    fn number(input: &mut &str) -> ModalResult<u32> {
        digit1.try_map(str::parse::<u32>).parse_next(input)
    }
    let mut input = text;
    let year = number(&mut input).ok()?;
    let separator = input.chars().next().filter(|c| matches!(c, '-' | '/'))?;
    input = &input[1..];
    let month = number(&mut input).ok()?;
    input = input.strip_prefix(separator)?;
    let day = number(&mut input).ok()?;
    if !input.is_empty() || !(1000..=9999).contains(&year) {
        return None;
    }
    let month = Month::try_from(u8::try_from(month).ok()?).ok()?;
    Date::from_calendar_date(i32::try_from(year).ok()?, month, u8::try_from(day).ok()?).ok()
}

/// `<n>d`, `<n>w`, `<n>m` or `<n>y`.
fn parse_age(text: &str) -> Option<Age> {
    let mut input = text;
    let digits: &str = digit1::<_, ErrMode<winnow::error::ContextError>>
        .parse_next(&mut input)
        .ok()?;
    let count = digits.parse::<u32>().ok()?;
    let mut chars = input.chars();
    let unit = AgeUnit::from_suffix(chars.next()?)?;
    if chars.next().is_some() {
        return None;
    }
    Some(Age { count, unit })
}

enum SizeError {
    Syntax,
    TooLarge,
}

/// `<n>` bytes or `<n>k`, `<n>M`, `<n>G` (binary units).
fn parse_size(text: &str) -> Result<Size, SizeError> {
    let mut input = text;
    let digits: &str = digit1::<_, ErrMode<winnow::error::ContextError>>
        .parse_next(&mut input)
        .map_err(|_| SizeError::Syntax)?;
    let value = digits.parse::<u64>().map_err(|_| SizeError::TooLarge)?;
    let mut chars = input.chars();
    let unit = match chars.next() {
        None => SizeUnit::Bytes,
        Some(c) => SizeUnit::from_suffix(c).ok_or(SizeError::Syntax)?,
    };
    if chars.next().is_some() {
        return Err(SizeError::Syntax);
    }
    Size::new(value, unit).ok_or(SizeError::TooLarge)
}
