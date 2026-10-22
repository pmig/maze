//! The search query language (CONCEPT §13.2): parsing, the typed AST, its canonical
//! string form and translation for server-side search.
//!
//! ```
//! use search::query::{parse, Query, Term, Text};
//!
//! let parsed = parse("from:anna has:attachment (invoice OR receipt)").unwrap();
//! assert!(parsed.warnings.is_empty());
//! assert_eq!(parsed.query.to_string(), "from:anna has:attachment invoice OR receipt");
//! ```

mod ast;
mod display;
mod parse;
mod server;

pub use ast::{Age, AgeUnit, Mailbox, Query, Size, SizeUnit, Status, Term, Text};
pub use parse::{ParseError, ParseWarning, Parsed, parse};
pub use server::ServerQuery;

#[cfg(test)]
mod tests;
