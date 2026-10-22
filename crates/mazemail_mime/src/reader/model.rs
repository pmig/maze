//! The reader document model.

use std::fmt;

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

/// A message body simplified for native rendering (CONCEPT §8.4, reader mode).
///
/// Built from sanitized HTML with [`from_html`](super::from_html) or from plain text
/// with [`from_plain_text`](super::from_plain_text). Serializable, so the store can
/// cache it next to the sanitized HTML.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReaderDoc {
    /// What the document was built from. Renderers may pick a monospace font for
    /// plain text, as Thunderbird can.
    pub source: DocSource,
    /// Top-level blocks in reading order.
    pub blocks: Vec<Block>,
}

impl fmt::Debug for ReaderDoc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReaderDoc")
            .field("source", &self.source)
            .field(
                "blocks",
                &format_args!("<{} blocks redacted>", self.blocks.len()),
            )
            .finish()
    }
}

/// The body part a [`ReaderDoc`] was built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocSource {
    /// Built from a `text/html` part.
    #[default]
    Html,
    /// Built from a `text/plain` part.
    PlainText,
}

/// A block-level element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// A paragraph of inline runs. Line breaks are `\n` inside run text.
    Paragraph { content: Vec<Inline> },
    /// A heading, `level` 1 to 6.
    Heading { level: u8, content: Vec<Inline> },
    /// A list. Items hold blocks, so lists nest.
    List {
        ordered: bool,
        /// First number of an ordered list, when it isn't 1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<u32>,
        items: Vec<ListItem>,
    },
    /// Quoted text. `depth` is the quote level, 1 for a quote at the top level and
    /// one more for every enclosing quote, so renderers can color levels and fold
    /// without walking up the tree.
    Quote { depth: u8, blocks: Vec<Block> },
    /// Preformatted text; whitespace is significant.
    Preformatted { text: String },
    /// A horizontal rule.
    Rule,
    /// A data table (layout tables are flattened into blocks).
    Table { rows: Vec<TableRow> },
    /// An image on its own line.
    Image(Image),
    /// A signature, collapsed by default in the message view.
    Signature { blocks: Vec<Block> },
}

/// One item of a [`Block::List`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ListItem {
    pub blocks: Vec<Block>,
}

/// One row of a [`Block::Table`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TableRow {
    pub cells: Vec<TableCell>,
}

/// One cell of a [`TableRow`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TableCell {
    /// A header cell (`<th>`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub header: bool,
    pub content: Vec<Inline>,
}

/// An image reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub source: ImageSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    /// Width hint in CSS pixels, from the `width` attribute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Height hint in CSS pixels, from the `height` attribute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Target of an enclosing link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// Where an image comes from. Reader mode resolves these in-process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ImageSource {
    /// An inline MIME part, by Content-ID (without angle brackets).
    Cid(String),
    /// A remote `http(s)` URL, loaded only when remote content is allowed.
    Remote(String),
    /// A `data:image/...` URL.
    Data(String),
}

bitflags! {
    /// Character formatting of an [`Inline`] run.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    pub struct InlineStyle: u8 {
        const BOLD = 1;
        const ITALIC = 1 << 1;
        const UNDERLINE = 1 << 2;
        const STRIKE = 1 << 3;
        const CODE = 1 << 4;
    }
}

/// A run of text with one formatting and an optional link target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inline {
    pub text: String,
    #[serde(default, skip_serializing_if = "InlineStyle::is_empty")]
    pub style: InlineStyle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

impl Inline {
    /// An unformatted run without link.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: InlineStyle::empty(),
            link: None,
        }
    }
}

impl ReaderDoc {
    /// Whether the document has no blocks.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// All text, quotes and signatures included, blocks separated by blank lines.
    /// This is what the search index gets.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            block_text(block, &mut out, TextMode::Everything);
        }
        out.trim_end().to_owned()
    }

    /// A short preview: the first `max_chars` characters of the message's own text,
    /// whitespace collapsed, with quotes, the attribution line before a quote,
    /// signatures and images left out.
    pub fn snippet(&self, max_chars: usize) -> String {
        let mut text = String::new();
        let mut blocks = self.blocks.iter().peekable();
        while let Some(block) = blocks.next() {
            if is_attribution(block) && matches!(blocks.peek(), Some(Block::Quote { .. })) {
                continue;
            }
            block_text(block, &mut text, TextMode::OwnText);
            if text.len() > max_chars.saturating_mul(4).max(64) {
                break;
            }
        }
        collapse_and_truncate(&text, max_chars)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextMode {
    Everything,
    OwnText,
}

fn block_text(block: &Block, out: &mut String, mode: TextMode) {
    match block {
        Block::Paragraph { content } | Block::Heading { content, .. } => {
            push_inlines(content, out);
            out.push_str("\n\n");
        }
        Block::List { items, .. } => {
            for item in items {
                for block in &item.blocks {
                    block_text(block, out, mode);
                }
            }
        }
        Block::Quote { blocks, .. } | Block::Signature { blocks } => {
            if mode == TextMode::Everything {
                for block in blocks {
                    block_text(block, out, mode);
                }
            }
        }
        Block::Preformatted { text } => {
            out.push_str(text);
            out.push_str("\n\n");
        }
        Block::Table { rows } => {
            for row in rows {
                for cell in &row.cells {
                    push_inlines(&cell.content, out);
                    out.push('\t');
                }
                out.push('\n');
            }
            out.push('\n');
        }
        Block::Image(image) => {
            if mode == TextMode::Everything
                && let Some(alt) = &image.alt
            {
                out.push_str(alt);
                out.push_str("\n\n");
            }
        }
        Block::Rule => {}
    }
}

fn push_inlines(content: &[Inline], out: &mut String) {
    for inline in content {
        out.push_str(&inline.text);
    }
}

/// "On Mon, Anna wrote:", "Am 1. März schrieb Anna:", "Le … a écrit :".
fn is_attribution(block: &Block) -> bool {
    let Block::Paragraph { content } = block else {
        return false;
    };
    let text: String = content.iter().map(|i| i.text.as_str()).collect();
    let text = text.trim();
    text.ends_with(':') && text.chars().count() < 300
}

/// Collapse all whitespace runs into single spaces and cut at `max_chars`.
fn collapse_and_truncate(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(max_chars.min(text.len()) + 4);
    let mut count = 0;
    for word in text.split_whitespace() {
        if count > 0 {
            if count >= max_chars {
                break;
            }
            out.push(' ');
            count += 1;
        }
        for c in word.chars() {
            if count >= max_chars {
                break;
            }
            out.push(c);
            count += 1;
        }
    }
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(text: &str) -> Block {
        Block::Paragraph {
            content: vec![Inline::plain(text)],
        }
    }

    #[test]
    fn snippet_skips_quotes_attribution_and_signature() {
        let doc = ReaderDoc {
            source: DocSource::PlainText,
            blocks: vec![
                para("Sounds  good,\nsee you\tthere."),
                para("On Monday Anna wrote:"),
                Block::Quote {
                    depth: 1,
                    blocks: vec![para("Lunch?")],
                },
                Block::Signature {
                    blocks: vec![para("Bob")],
                },
            ],
        };
        assert_eq!(doc.snippet(200), "Sounds good, see you there.");
        assert_eq!(doc.snippet(10), "Sounds goo");
        assert!(doc.plain_text().contains("Lunch?"));
        assert!(doc.plain_text().contains("Bob"));
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(collapse_and_truncate("äöü ß", 4), "äöü");
        assert_eq!(collapse_and_truncate("   ", 4), "");
        assert_eq!(collapse_and_truncate("ab cd", 3), "ab");
    }

    #[test]
    fn serializes_compactly() {
        let block = Block::Paragraph {
            content: vec![Inline {
                text: "x".into(),
                style: InlineStyle::BOLD | InlineStyle::ITALIC,
                link: None,
            }],
        };
        assert_eq!(
            serde_json::to_string(&block).unwrap(),
            r#"{"type":"paragraph","content":[{"text":"x","style":"BOLD | ITALIC"}]}"#
        );
    }

    #[test]
    fn debug_redacts_content() {
        let doc = ReaderDoc {
            source: DocSource::Html,
            blocks: vec![para("secret")],
        };
        assert!(!format!("{doc:?}").contains("secret"));
    }
}
