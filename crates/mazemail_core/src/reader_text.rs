//! The reader document (`mazemail_mime::ReaderDoc`) as text: Markdown and plain text
//! for `mazemail show` (CONCEPT §10.2), and the Markdown the message view renders.
//!
//! - [`markdown`] and [`plain_text`]: for agents and terminals. Remote images become
//!   Markdown images (`![alt](url)`) or `[alt: url]`; inline (`cid:`) and `data:`
//!   images become placeholders; quotes keep their nesting; signatures follow a `-- `
//!   line.
//! - [`display_markdown`]: for the GUI's Markdown view. Text is escaped so mail can't
//!   inject Markdown, and remote images are plain links labelled "Image", so the view
//!   never loads remote content (PLAN §4.8).

use mazemail_mime::reader::{Block, ImageSource, Inline, InlineStyle, ListItem, TableRow};
use mazemail_mime::{AttachmentMeta, ReaderDoc};
use mazemail_types::Address;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

/// The two text formats of `show`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextKind {
    Markdown,
    Plain,
}

/// How [`document`] writes text: the format, and whether it's for the GUI's Markdown
/// view (escaped text, no image syntax).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mode {
    kind: TextKind,
    display: bool,
}

impl From<TextKind> for Mode {
    fn from(kind: TextKind) -> Self {
        Self {
            kind,
            display: false,
        }
    }
}

/// A reader document as Markdown (for agents: see the [module docs](self)).
pub fn markdown(doc: &ReaderDoc) -> String {
    document(TextKind::Markdown, doc)
}

/// A reader document as plain text.
pub fn plain_text(doc: &ReaderDoc) -> String {
    document(TextKind::Plain, doc)
}

/// A reader document as Markdown for the GUI: escaped text, remote images as links.
pub fn display_markdown(doc: &ReaderDoc) -> String {
    render(
        Mode {
            kind: TextKind::Markdown,
            display: true,
        },
        doc,
    )
}

/// One message as `show` prints it.
#[derive(Debug)]
pub(crate) struct RenderedMessage<'a> {
    pub(crate) subject: Option<&'a str>,
    pub(crate) from: Option<&'a Address>,
    pub(crate) to: &'a [Address],
    pub(crate) cc: &'a [Address],
    pub(crate) date: OffsetDateTime,
    pub(crate) attachments: &'a [AttachmentMeta],
    /// `None`: the body isn't stored yet or can't be read; `note` says which.
    pub(crate) body: Option<&'a ReaderDoc>,
    pub(crate) note: &'a str,
}

fn addresses(list: &[Address]) -> String {
    list.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn size(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000. && unit + 1 < UNITS.len() {
        value /= 1000.;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Renders one message: headers, then the body.
pub(crate) fn message(kind: TextKind, msg: &RenderedMessage<'_>) -> String {
    let mut out = String::new();
    let subject = msg.subject.unwrap_or("(no subject)");
    let date = msg
        .date
        .format(&Rfc2822)
        .unwrap_or_else(|_| msg.date.unix_timestamp().to_string());
    let mut headers: Vec<(&str, String)> = Vec::new();
    if let Some(from) = msg.from {
        headers.push(("From", from.to_string()));
    }
    if !msg.to.is_empty() {
        headers.push(("To", addresses(msg.to)));
    }
    if !msg.cc.is_empty() {
        headers.push(("Cc", addresses(msg.cc)));
    }
    headers.push(("Date", date));
    let files: Vec<String> = msg
        .attachments
        .iter()
        .filter(|a| a.counts_as_attachment())
        .map(|a| {
            format!(
                "{} ({})",
                a.filename.as_deref().unwrap_or("unnamed"),
                size(a.size)
            )
        })
        .collect();
    if !files.is_empty() {
        headers.push(("Attachments", files.join(", ")));
    }
    match kind {
        TextKind::Markdown => {
            out.push_str("## ");
            out.push_str(subject);
            out.push_str("\n\n");
            for (name, value) in &headers {
                out.push_str(&format!("**{name}:** {value}  \n"));
            }
        }
        TextKind::Plain => {
            out.push_str(&format!("Subject: {subject}\n"));
            for (name, value) in &headers {
                out.push_str(&format!("{name}: {value}\n"));
            }
        }
    }
    out.push('\n');
    match msg.body {
        Some(doc) => out.push_str(&document(kind, doc)),
        None => {
            out.push_str(msg.note);
            out.push('\n');
        }
    }
    out
}

/// Joins messages of a thread under its subject, oldest first.
pub(crate) fn thread(kind: TextKind, subject: Option<&str>, messages: &[String]) -> String {
    let subject = subject.unwrap_or("(no subject)");
    let mut out = match kind {
        TextKind::Markdown => format!("# {subject}\n\n"),
        TextKind::Plain => format!("{subject}\n{}\n\n", "=".repeat(subject.chars().count())),
    };
    let separator = match kind {
        TextKind::Markdown => "\n---\n\n",
        TextKind::Plain => "\n----------------------------------------\n\n",
    };
    out.push_str(&messages.join(separator));
    out
}

/// Renders a reader document.
pub(crate) fn document(kind: TextKind, doc: &ReaderDoc) -> String {
    render(kind.into(), doc)
}

fn render(mode: Mode, doc: &ReaderDoc) -> String {
    let mut out = String::new();
    blocks(mode, &doc.blocks, &mut out);
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    out.push('\n');
    out
}

fn blocks(mode: Mode, list: &[Block], out: &mut String) {
    for block in list {
        self::block(mode, block, out);
    }
}

fn block(mode: Mode, block: &Block, out: &mut String) {
    let kind = mode.kind;
    match block {
        Block::Paragraph { content } => {
            out.push_str(&inlines(mode, content));
            out.push_str("\n\n");
        }
        Block::Heading { level, content } => {
            let text = inlines(mode, content);
            match kind {
                TextKind::Markdown => {
                    out.push_str(&"#".repeat(usize::from((*level).clamp(1, 6))));
                    out.push(' ');
                    out.push_str(&text);
                }
                TextKind::Plain => out.push_str(&text),
            }
            out.push_str("\n\n");
        }
        Block::List {
            ordered,
            start,
            items,
        } => {
            list(mode, *ordered, start.unwrap_or(1), items, out);
            out.push('\n');
        }
        Block::Quote { blocks: inner, .. } => {
            let mut quoted = String::new();
            blocks(mode, inner, &mut quoted);
            for line in quoted.trim_end().lines() {
                if line.is_empty() {
                    out.push_str(">\n");
                } else {
                    out.push_str("> ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
            out.push('\n');
        }
        Block::Preformatted { text } => {
            if kind == TextKind::Markdown {
                out.push_str("```\n");
                out.push_str(text.trim_end_matches('\n'));
                out.push_str("\n```\n\n");
            } else {
                out.push_str(text.trim_end_matches('\n'));
                out.push_str("\n\n");
            }
        }
        Block::Rule => {
            out.push_str(if kind == TextKind::Markdown {
                "***\n\n"
            } else {
                "--------\n\n"
            });
        }
        Block::Table { rows } => {
            table(mode, rows, out);
            out.push('\n');
        }
        Block::Image(image) => {
            let alt = image.alt.as_deref().unwrap_or("image");
            match (&image.source, kind) {
                (ImageSource::Remote(url), TextKind::Markdown) if mode.display => {
                    // A link, never an image: the view must not load remote content.
                    let url = url.replace(['<', '>', '\n'], "");
                    out.push_str(&format!("[Image: {}](<{url}>)", escape_text(alt)));
                }
                (ImageSource::Cid(_) | ImageSource::Data(_), _) if mode.display => {
                    out.push_str(&format!("\\[{}\\]", escape_text(alt)));
                }
                (ImageSource::Remote(url), TextKind::Markdown) => {
                    out.push_str(&format!("![{}]({url})", escape_label(alt)));
                }
                (ImageSource::Remote(url), TextKind::Plain) => {
                    out.push_str(&format!("[{alt}: {url}]"));
                }
                (ImageSource::Cid(_) | ImageSource::Data(_), _) => {
                    out.push_str(&format!("[{alt}]"));
                }
            }
            out.push_str("\n\n");
        }
        Block::Signature { blocks: inner } => {
            out.push_str("-- \n");
            let mut signature = String::new();
            blocks(mode, inner, &mut signature);
            // In Markdown a bare line break needs two trailing spaces.
            if kind == TextKind::Markdown {
                out.push_str(&signature.trim_end().replace('\n', "  \n"));
                out.push_str("\n\n");
            } else {
                out.push_str(&signature);
            }
        }
    }
}

fn list(mode: Mode, ordered: bool, start: u32, items: &[ListItem], out: &mut String) {
    for (ix, item) in items.iter().enumerate() {
        let marker = if ordered {
            format!(
                "{}. ",
                start.saturating_add(u32::try_from(ix).unwrap_or(u32::MAX))
            )
        } else {
            "- ".to_owned()
        };
        let mut content = String::new();
        blocks(mode, &item.blocks, &mut content);
        let indent = " ".repeat(marker.len());
        for (line_ix, line) in content.trim_end().lines().enumerate() {
            if line_ix == 0 {
                out.push_str(&marker);
            } else if !line.is_empty() {
                out.push_str(&indent);
            }
            out.push_str(line);
            out.push('\n');
        }
    }
}

fn table(mode: Mode, rows: &[TableRow], out: &mut String) {
    let kind = mode.kind;
    let cell = |content: &[Inline]| {
        let text = inlines(mode, content).replace('\n', " ");
        // Display text has its pipes escaped already.
        if mode.display {
            text
        } else {
            text.replace('|', "\\|")
        }
    };
    for (ix, row) in rows.iter().enumerate() {
        let cells: Vec<String> = row.cells.iter().map(|c| cell(&c.content)).collect();
        match kind {
            TextKind::Markdown => {
                out.push_str("| ");
                out.push_str(&cells.join(" | "));
                out.push_str(" |\n");
                if ix == 0 {
                    out.push('|');
                    out.push_str(&" --- |".repeat(cells.len().max(1)));
                    out.push('\n');
                }
            }
            TextKind::Plain => {
                out.push_str(&cells.join("\t"));
                out.push('\n');
            }
        }
    }
}

fn escape_label(text: &str) -> String {
    text.replace('[', "\\[").replace(']', "\\]")
}

/// Backslash-escapes everything Markdown (CommonMark plus tables and strikethrough)
/// could read as syntax, so mail text shows as written.
fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '`'
                | '*'
                | '_'
                | '{'
                | '}'
                | '['
                | ']'
                | '<'
                | '>'
                | '('
                | ')'
                | '#'
                | '+'
                | '-'
                | '.'
                | '!'
                | '|'
                | '~'
                | '&'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn inlines(mode: Mode, content: &[Inline]) -> String {
    let kind = mode.kind;
    let mut out = String::new();
    for run in content {
        if run.text.is_empty() {
            continue;
        }
        let text = match kind {
            TextKind::Plain => run.text.clone(),
            TextKind::Markdown => styled(run, mode.display),
        };
        match (&run.link, kind) {
            // Display text is escaped already; the destination in angle brackets may
            // hold parentheses and spaces.
            (Some(url), TextKind::Markdown) if mode.display => {
                let url = url.replace(['<', '>', '\n'], "");
                out.push_str(&format!("[{text}](<{url}>)"));
            }
            (Some(url), TextKind::Markdown) => {
                out.push_str(&format!("[{}]({url})", escape_label(&text)));
            }
            (Some(url), TextKind::Plain) if url.trim_start_matches("mailto:") != run.text => {
                out.push_str(&format!("{text} <{url}>"));
            }
            _ => out.push_str(&text),
        }
    }
    match kind {
        // A line break inside a paragraph is a hard break.
        TextKind::Markdown => out.replace('\n', "  \n"),
        TextKind::Plain => out,
    }
}

/// Markdown emphasis around a run, keeping surrounding whitespace outside the markers
/// (`** bold**` isn't bold). `escape` escapes the text first (display mode).
fn styled(run: &Inline, escape: bool) -> String {
    let style = run.style;
    let code = style.contains(InlineStyle::CODE);
    // Code spans show their content literally, so it isn't escaped.
    let text = if escape && !code {
        escape_text(&run.text)
    } else {
        run.text.clone()
    };
    if style.is_empty() || text.trim().is_empty() {
        return text;
    }
    let trimmed = text.trim();
    let start = text.len() - text.trim_start().len();
    let leading = &text[..start];
    let trailing = &text[start + trimmed.len()..];
    let mut inner = trimmed.to_owned();
    if code {
        inner = format!("`{inner}`");
    }
    if style.contains(InlineStyle::STRIKE) {
        inner = format!("~~{inner}~~");
    }
    if style.contains(InlineStyle::ITALIC) {
        inner = format!("*{inner}*");
    }
    if style.contains(InlineStyle::BOLD) {
        inner = format!("**{inner}**");
    }
    format!("{leading}{inner}{trailing}")
}

#[cfg(test)]
mod tests {
    use mazemail_mime::reader::{TextFormat, from_html, from_plain_text};
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn html_becomes_markdown() {
        let doc = from_html(
            "<h2>Plan</h2><p>Hello <b>Anna</b>, see <a href=\"https://example.com/x\">the \
             doc</a>.</p><ul><li>one</li><li>two</li></ul><blockquote><p>quoted</p>\
             </blockquote><img src=\"https://cdn.example.com/a.png\" alt=\"logo\">",
        );
        assert_eq!(
            document(TextKind::Markdown, &doc),
            "## Plan\n\nHello **Anna**, see [the doc](https://example.com/x).\n\n- one\n- two\n\n\
             > quoted\n\n![logo](https://cdn.example.com/a.png)\n"
        );
        assert_eq!(
            document(TextKind::Plain, &doc),
            "Plan\n\nHello Anna, see the doc <https://example.com/x>.\n\n- one\n- two\n\n\
             > quoted\n\n[logo: https://cdn.example.com/a.png]\n"
        );
    }

    #[test]
    fn display_markdown_escapes_and_never_embeds_images() {
        let doc = from_html(
            "<p>Price: *50%* off [today] &lt;b&gt;</p><p><a href=\"https://example.com/a_(b)\">\
             the_link</a></p><img src=\"https://tracker.example.net/p.gif\" alt=\"pixel\">",
        );
        let md = display_markdown(&doc);
        assert!(md.contains(r"Price: \*50%\* off \[today\] \<b\>"), "{md}");
        assert!(
            md.contains(r"[the\_link](<https://example.com/a_(b)>)"),
            "{md}"
        );
        assert!(
            md.contains("[Image: pixel](<https://tracker.example.net/p.gif>)"),
            "{md}"
        );
        assert!(!md.contains("!["), "{md}");
        // The agent formats stay as they were.
        assert!(markdown(&doc).contains("![pixel](https://tracker.example.net/p.gif)"));
        assert!(plain_text(&doc).starts_with("Price: *50%* off [today] <b>"));
    }

    #[test]
    fn plain_text_keeps_quotes() {
        let doc = from_plain_text("Hi,\n\n> earlier\n> text\n\nBye", TextFormat::default());
        let md = document(TextKind::Markdown, &doc);
        assert!(md.contains("> earlier"), "{md}");
        assert!(md.ends_with("Bye\n"), "{md}");
    }

    #[test]
    fn message_has_headers_and_attachments() {
        let from = Address::new(Some("Anna"), "anna@example.com");
        let to = [Address::new(None::<String>, "me@example.com")];
        let doc = from_plain_text("Body", TextFormat::default());
        let attachment = AttachmentMeta {
            part_path: "2".into(),
            filename: Some("plan.pdf".into()),
            content_type: "application/pdf".into(),
            size: 1_200_000,
            content_id: None,
            disposition: mazemail_mime::Disposition::Attachment,
        };
        let msg = RenderedMessage {
            subject: Some("Offsite"),
            from: Some(&from),
            to: &to,
            cc: &[],
            date: OffsetDateTime::UNIX_EPOCH,
            attachments: std::slice::from_ref(&attachment),
            body: Some(&doc),
            note: "",
        };
        assert_eq!(
            message(TextKind::Markdown, &msg),
            "## Offsite\n\n**From:** Anna <anna@example.com>  \n**To:** me@example.com  \n\
             **Date:** Thu, 01 Jan 1970 00:00:00 +0000  \n**Attachments:** plan.pdf (1.2 MB)  \n\
             \nBody\n"
        );
        let text = message(TextKind::Plain, &msg);
        assert!(text.starts_with("Subject: Offsite\nFrom: Anna <anna@example.com>\n"));
        let joined = thread(TextKind::Markdown, Some("Offsite"), &[text.clone(), text]);
        assert!(joined.starts_with("# Offsite\n\n"));
        assert!(joined.contains("\n---\n\n"));
    }
}
