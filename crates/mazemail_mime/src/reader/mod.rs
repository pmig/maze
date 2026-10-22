//! Reader mode: a small document model for native rendering (PLAN §4.8).
//!
//! HTML goes through the sanitizer first, then layout tables are flattened and the
//! remaining structure is mapped to [`Block`]s with flat [`Inline`] runs. Plain text
//! gets quote levels, link detection, signature folding and format=flowed decoding.

mod html;
mod model;
mod text;

pub use model::{
    Block, DocSource, Image, ImageSource, Inline, InlineStyle, ListItem, ReaderDoc, TableCell,
    TableRow,
};
pub use text::{TextFormat, from_plain_text};

pub(crate) use html::dom_to_reader;

use crate::html::{SanitizeOptions, sanitize_to_dom};

/// Convert message HTML into a reader document.
///
/// The HTML is sanitized with the default [`SanitizeOptions`] first (tracking pixels
/// are dropped), so raw message HTML is fine here. Use
/// [`html::prepare`](crate::html::prepare) to get the sanitized HTML and the reader
/// document from a single parse.
pub fn from_html(html: &str) -> ReaderDoc {
    let (dom, _) = sanitize_to_dom(html, &SanitizeOptions::default());
    dom_to_reader(&dom)
}
