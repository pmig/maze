//! Outgoing mail (PLAN §4.10): Markdown to HTML, MIME building, and the helpers for
//! replies and forwards (subject prefixes, `References`, quoting).
//!
//! A message is `multipart/alternative` with the Markdown source as `text/plain` and
//! the rendered HTML (`pulldown-cmark` plus a small mail stylesheet) as `text/html`;
//! with attachments both go into `multipart/mixed`. Bcc recipients are never written
//! into the headers (they go into the SMTP envelope only).
//!
//! Raw HTML in the Markdown is shown as text, not passed through: a quoted
//! `<script>` stays text, and what you see in the editor is what the recipient gets.
//! Single line breaks are kept (as `<br>`), the way people write mail.
//!
//! # Footer and "Sent with Maze"
//!
//! The configured footer (`compose.footer`, Markdown) and the "Sent with Maze" line
//! ([`MessageFooter`]) are added when the message is built, never to the body being
//! edited. They follow your text and signature; in a reply or forward they go above
//! the quoted original ([`quote_start`]), the way people sign a top-posted reply, so
//! the quote doesn't end up between your words and your footer. Text: a blank line,
//! the footer, a blank line, `Sent with Maze (https://mazemail.dev)`. HTML: the
//! footer rendered from Markdown and a small muted line linking "Maze".

use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address as BuilderAddress;
use mail_builder::headers::date::Date;
use mail_builder::headers::message_id::MessageId;
use mail_builder::headers::text::Text;
use mazemail_types::Address;
use mazemail_types::compose::{MAZE_WEBSITE, SENT_WITH_MAZE};
use pulldown_cmark::{Event, Options, Parser};

use crate::subject::parse_subject;

/// The most `References` an outgoing message carries: the first (the thread root)
/// plus the latest ones, as RFC 5322 §3.6.4 suggests for long threads.
pub const MAX_REFERENCES: usize = 20;

/// The stylesheet of the HTML part: readable defaults that mail clients keep.
const MAIL_CSS: &str = "body{font-family:-apple-system,BlinkMacSystemFont,\"Segoe UI\",\
Helvetica,Arial,sans-serif;font-size:14px;line-height:1.5;color:#1f2328;}\
p{margin:0 0 1em 0;}\
blockquote{margin:0 0 1em 0;padding:0 0 0 1em;border-left:3px solid #d0d7de;color:#57606a;}\
pre{background:#f6f8fa;padding:8px 12px;border-radius:6px;overflow:auto;}\
code{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:13px;}\
table{border-collapse:collapse;}th,td{border:1px solid #d0d7de;padding:4px 8px;}\
a{color:#0969da;}";

/// The line that starts a forwarded message in the body of a forward.
pub const FORWARD_HEADER: &str = "-------- Forwarded Message --------";

/// What is added below the body of an outgoing message (see the [module docs](self)).
/// `Debug` leaves out the footer's text.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct MessageFooter {
    /// The configured footer, Markdown; empty for none.
    pub markdown: String,
    /// Add "Sent with Maze (https://mazemail.dev)".
    pub sent_with_maze: bool,
    /// The message is a reply or forward: the footer goes above the quoted original.
    pub above_quote: bool,
}

impl std::fmt::Debug for MessageFooter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageFooter")
            .field("footer_len", &self.markdown.len())
            .field("sent_with_maze", &self.sent_with_maze)
            .field("above_quote", &self.above_quote)
            .finish()
    }
}

impl MessageFooter {
    fn is_empty(&self) -> bool {
        self.markdown.trim().is_empty() && !self.sent_with_maze
    }
}

/// A file attached to an outgoing message.
#[derive(Clone, PartialEq, Eq)]
pub struct OutgoingAttachment {
    /// The file name the recipient sees.
    pub name: String,
    /// The MIME type (see [`content_type_for`]).
    pub content_type: String,
    /// The content.
    pub data: Vec<u8>,
}

impl std::fmt::Debug for OutgoingAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutgoingAttachment")
            .field("content_type", &self.content_type)
            .field("size", &self.data.len())
            .finish_non_exhaustive()
    }
}

/// Everything an outgoing message is built from. `Debug` shows no content.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct OutgoingMessage {
    /// The sender (the identity).
    pub from: Option<Address>,
    /// Where replies should go, if not to `from`.
    pub reply_to: Option<Address>,
    /// Recipients.
    pub to: Vec<Address>,
    /// Copy recipients.
    pub cc: Vec<Address>,
    /// The subject.
    pub subject: String,
    /// The body, Markdown.
    pub body_markdown: String,
    /// `In-Reply-To`, without angle brackets.
    pub in_reply_to: Option<String>,
    /// `References`, oldest first, without angle brackets.
    pub references: Vec<String>,
    /// The `Message-ID`, without angle brackets ([`new_message_id`]).
    pub message_id: String,
    /// The `Date`, Unix seconds.
    pub date: i64,
    /// Attached files.
    pub attachments: Vec<OutgoingAttachment>,
    /// The `User-Agent` header, if any.
    pub user_agent: Option<String>,
    /// The footer and the "Sent with Maze" line, added below the body.
    pub footer: MessageFooter,
}

impl std::fmt::Debug for OutgoingMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutgoingMessage")
            .field("to", &self.to.len())
            .field("cc", &self.cc.len())
            .field("attachments", &self.attachments.len())
            .field("date", &self.date)
            .finish_non_exhaustive()
    }
}

/// Why a message couldn't be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ComposeError {
    /// No sender.
    #[error("the message has no sender")]
    NoSender,
    /// No Message-ID.
    #[error("the message has no Message-ID")]
    NoMessageId,
    /// Writing the MIME structure failed.
    #[error("building the message failed: {0}")]
    Build(String),
}

/// A new `Message-ID` (without angle brackets) for a message sent from `domain`:
/// `<random>@<domain>`, the random part from `random` (16 bytes from the caller's
/// random source, since this crate does no IO), the domain the sender's.
pub fn new_message_id(random: &[u8; 16], domain: &str) -> String {
    let local: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let domain = domain
        .trim()
        .trim_matches(['<', '>', '[', ']'])
        .to_ascii_lowercase();
    let domain = if domain.is_empty() || domain.contains(char::is_whitespace) {
        "mazemail.invalid".to_owned()
    } else {
        domain
    };
    format!("{local}@{domain}")
}

fn builder_address(address: &Address) -> BuilderAddress<'static> {
    BuilderAddress::new_address(
        address.name.clone().filter(|n| !n.trim().is_empty()),
        address.email.clone(),
    )
}

fn builder_list(addresses: &[Address]) -> BuilderAddress<'static> {
    BuilderAddress::new_list(addresses.iter().map(builder_address).collect())
}

/// Builds the raw RFC 5322 message: `multipart/alternative` (text/plain Markdown +
/// text/html), inside `multipart/mixed` when there are attachments.
pub fn build_message(message: &OutgoingMessage) -> Result<Vec<u8>, ComposeError> {
    let from = message.from.as_ref().ok_or(ComposeError::NoSender)?;
    if message.message_id.trim().is_empty() {
        return Err(ComposeError::NoMessageId);
    }
    let mut builder = MessageBuilder::new()
        .from(builder_address(from))
        .subject(message.subject.clone())
        .message_id(message.message_id.clone())
        .date(Date::new(message.date));
    if !message.to.is_empty() {
        builder = builder.to(builder_list(&message.to));
    }
    if !message.cc.is_empty() {
        builder = builder.cc(builder_list(&message.cc));
    }
    if let Some(reply_to) = &message.reply_to {
        builder = builder.reply_to(builder_address(reply_to));
    }
    if let Some(parent) = message.in_reply_to.as_deref().filter(|p| !p.is_empty()) {
        builder = builder.in_reply_to(parent.to_owned());
    }
    if !message.references.is_empty() {
        builder = builder.references(MessageId::new_list(message.references.iter().cloned()));
    }
    if let Some(agent) = &message.user_agent {
        builder = builder.header("User-Agent", Text::new(agent.clone()));
    }
    let (text, html) = body_parts(&message.body_markdown, &message.footer);
    builder = builder.text_body(text).html_body(html);
    for attachment in &message.attachments {
        builder = builder.attachment(
            attachment.content_type.clone(),
            attachment.name.clone(),
            attachment.data.clone(),
        );
    }
    builder
        .write_to_vec()
        .map_err(|err| ComposeError::Build(err.to_string()))
}

/// The text/plain and text/html bodies: the Markdown and its rendering, with the
/// footer between your text and a quoted original.
pub fn body_parts(body_markdown: &str, footer: &MessageFooter) -> (String, String) {
    if footer.is_empty() {
        return (
            normalize_newlines(body_markdown),
            markdown_to_html(body_markdown),
        );
    }
    let body = body_markdown.replace("\r\n", "\n");
    let split = if footer.above_quote {
        quote_start(&body).unwrap_or(body.len())
    } else {
        body.len()
    };
    let (own, quoted) = body.split_at(split);
    let own = own.trim_end();
    let quoted = quoted.trim_end();
    let footer_text = footer.markdown.replace("\r\n", "\n");
    let footer_text = footer_text.trim();

    let mut blocks: Vec<&str> = Vec::with_capacity(4);
    if !own.trim().is_empty() {
        blocks.push(own);
    }
    if !footer_text.is_empty() {
        blocks.push(footer_text);
    }
    if footer.sent_with_maze {
        blocks.push(SENT_WITH_MAZE);
    }
    if !quoted.is_empty() {
        blocks.push(quoted);
    }
    let text = normalize_newlines(&blocks.join("\n\n"));

    let mut html = String::new();
    if !own.trim().is_empty() {
        html.push_str(&markdown_fragment(own));
    }
    if !footer_text.is_empty() {
        html.push_str("<div class=\"footer\">\n");
        html.push_str(&markdown_fragment(footer_text));
        html.push_str("</div>\n");
    }
    if footer.sent_with_maze {
        html.push_str(&format!(
            "<p class=\"sent-with\" style=\"color:#8c959f;font-size:12px;\">Sent with \
             <a href=\"{MAZE_WEBSITE}\" style=\"color:#8c959f;\">Maze</a></p>\n"
        ));
    }
    if !quoted.is_empty() {
        html.push_str(&markdown_fragment(quoted));
    }
    (text, html_document(&html))
}

/// Where the quoted original of a reply or forward starts in its body (a byte offset
/// at the start of a line), or `None` if the body doesn't end with one:
///
/// - a forward's [`FORWARD_HEADER`] line;
/// - the attribution line (`On …, … wrote:`, any line ending in `:`) before the
///   trailing `>` quote, or the quote itself without one.
///
/// Answers written between quoted parts end with your text, not a quote: then there
/// is nothing to go above, and the footer goes at the end.
pub fn quote_start(body: &str) -> Option<usize> {
    let mut lines = Vec::new();
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        lines.push((offset, line.trim_end_matches(['\n', '\r'])));
        offset += line.len();
    }
    if let Some((start, _)) = lines.iter().find(|(_, line)| line.trim() == FORWARD_HEADER) {
        return Some(*start);
    }
    // The trailing run of quoted and blank lines, and its first quoted line.
    let mut first_quoted = None;
    for (ix, (_, line)) in lines.iter().enumerate().rev() {
        let line = line.trim_start();
        if line.starts_with('>') {
            first_quoted = Some(ix);
        } else if !line.is_empty() {
            break;
        }
    }
    let first_quoted = first_quoted?;
    let start = match first_quoted.checked_sub(1).map(|ix| (ix, lines[ix].1)) {
        Some((ix, line)) if line.trim_end().ends_with(':') => ix,
        _ => first_quoted,
    };
    Some(lines[start].0)
}

fn normalize_newlines(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    if text.ends_with('\n') {
        text
    } else {
        format!("{text}\n")
    }
}

/// Renders the body Markdown as the HTML part: a complete document with the mail
/// stylesheet. Raw HTML in the source is escaped; single line breaks are kept.
pub fn markdown_to_html(markdown: &str) -> String {
    html_document(&markdown_fragment(markdown))
}

/// The body HTML of `markdown`, without the document around it.
fn markdown_fragment(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    let events = Parser::new_ext(markdown, options).map(|event| match event {
        Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
        Event::SoftBreak => Event::HardBreak,
        other => other,
    });
    let mut body = String::with_capacity(markdown.len() * 3 / 2 + 64);
    pulldown_cmark::html::push_html(&mut body, events);
    body
}

/// A complete HTML document around `body`, with the mail stylesheet.
fn html_document(body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width\">\
         <style>{MAIL_CSS}</style></head>\n<body>\n{body}</body></html>\n"
    )
}

/// The subject of a reply: `Re: ` before the original's, unless it is a reply already
/// (in any of the common languages: `Re:`, `AW:`, `SV:`, …). An empty subject stays
/// `Re: `.
pub fn reply_subject(original: &str) -> String {
    let original = original.trim();
    if parse_subject(original).is_reply && !starts_with_forward(original) {
        original.to_owned()
    } else {
        format!("Re: {original}")
    }
}

/// The subject of a forward: `Fwd: ` before the original's, unless it is one already.
pub fn forward_subject(original: &str) -> String {
    let original = original.trim();
    if starts_with_forward(original) {
        original.to_owned()
    } else {
        format!("Fwd: {original}")
    }
}

/// Whether the subject's first prefix is a forward prefix (`Fwd:`, `WG:`, `TR:`, …).
fn starts_with_forward(subject: &str) -> bool {
    let Some((word, _)) = subject.split_once([':', '：']) else {
        return false;
    };
    let word = word.trim();
    if word.is_empty() || word.chars().count() > 8 || word.contains(char::is_whitespace) {
        return false;
    }
    let info = parse_subject(&format!("{word}: x"));
    info.is_forward && !info.is_reply
}

/// The `References` of a reply (RFC 5322 §3.6.4): the original's references (or its
/// `In-Reply-To` if it has none) followed by its Message-ID, without duplicates, at
/// most [`MAX_REFERENCES`] (the root and the newest).
pub fn reply_references(
    original_references: &[String],
    original_in_reply_to: Option<&str>,
    original_message_id: Option<&str>,
) -> Vec<String> {
    let clean = |id: &str| id.trim().trim_matches(['<', '>']).to_owned();
    let mut out: Vec<String> = Vec::new();
    let mut push = |id: String| {
        if !id.is_empty() && !out.contains(&id) {
            out.push(id);
        }
    };
    if original_references.is_empty() {
        if let Some(parent) = original_in_reply_to {
            push(clean(parent));
        }
    } else {
        for id in original_references {
            push(clean(id));
        }
    }
    if let Some(id) = original_message_id {
        push(clean(id));
    }
    if out.len() > MAX_REFERENCES {
        let tail = out.split_off(out.len() - (MAX_REFERENCES - 1));
        out.truncate(1);
        out.extend(tail);
    }
    out
}

/// Quotes text as Markdown `>` blocks, line by line (`>` alone for empty lines, `>>`
/// for lines that were quoted already).
pub fn quote(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    let mut out = String::with_capacity(text.len() + text.len() / 8 + 8);
    for line in text.trim_end_matches('\n').split('\n') {
        let line = line.trim_end();
        if line.is_empty() {
            out.push('>');
        } else if line.starts_with('>') {
            out.push('>');
            out.push_str(line);
        } else {
            out.push_str("> ");
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Converts the HTML of a message being replied to or forwarded into Markdown
/// (`htmd`): lossy, but keeps paragraphs, emphasis, lists, links and quotes. Scripts,
/// styles and images are dropped (an image in the quote would load from the sender's
/// server in the recipient's client).
pub fn html_to_markdown(html: &str) -> String {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec![
            "script", "style", "head", "title", "meta", "link", "img", "picture", "video", "audio",
            "iframe", "object", "svg", "form",
        ])
        .build();
    // htmd only fails on IO errors of its in-memory parser; the caller then falls back
    // to the text part.
    converter
        .convert(html)
        .map(|markdown| tidy_markdown(&markdown))
        .unwrap_or_default()
}

/// Trims trailing spaces and runs of blank lines htmd leaves behind (table layouts,
/// spacer cells), and non-breaking spaces newsletters use for padding.
fn tidy_markdown(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut blank = 0;
    for line in markdown.lines() {
        let line = line.replace('\u{a0}', " ");
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
            out.push('\n');
        } else {
            blank = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_matches('\n').to_owned()
}

/// The MIME type for an attachment's file name, by extension
/// (`application/octet-stream` if unknown).
pub fn content_type_for(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "svg" => "image/svg+xml",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "ics" => "text/calendar",
        "vcf" => "text/vcard",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "eml" => "message/rfc822",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use mail_parser::{MessageParser, MimeHeaders};
    use pretty_assertions::assert_eq;

    use super::*;

    fn sample() -> OutgoingMessage {
        OutgoingMessage {
            from: Some(Address::new(Some("Philip · distr"), "you@distr.example")),
            reply_to: Some(Address::new(None::<String>, "support@distr.example")),
            to: vec![Address::new(Some("Anna Example"), "anna@example.com")],
            cc: vec![Address::new(None::<String>, "team@example.com")],
            subject: "Re: Q3 offsite – Grüße".into(),
            body_markdown: "Hi Anna,\nsee **below**.\n\n<script>alert(1)</script>\n\n> quoted\n"
                .into(),
            in_reply_to: Some("orig@example.com".into()),
            references: vec!["root@example.com".into(), "orig@example.com".into()],
            message_id: new_message_id(&[0xab; 16], "distr.example"),
            date: 1_792_663_200,
            attachments: Vec::new(),
            user_agent: Some("Maze 0.1".into()),
            footer: MessageFooter::default(),
        }
    }

    fn parts(raw: &[u8]) -> (String, String) {
        let parsed = MessageParser::default().parse(raw).unwrap();
        (
            parsed.body_text(0).unwrap().replace("\r\n", "\n"),
            parsed.body_html(0).unwrap().into_owned(),
        )
    }

    const SIGNED: &str = "Hi Anna,\n\nsee you.\n\n-- \nPhilip\n";

    #[test]
    fn no_footer_and_no_maze_line_when_off() {
        let mut message = sample();
        message.body_markdown = SIGNED.into();
        let (text, html) = parts(&build_message(&message).unwrap());
        assert_eq!(text, SIGNED);
        assert!(!text.contains("Sent with Maze") && !html.contains("Sent with"));
        assert!(!html.contains("mazemail.dev"));
    }

    #[test]
    fn footer_and_maze_line_follow_the_signature_in_both_parts() {
        let mut message = sample();
        message.body_markdown = SIGNED.into();
        message.footer = MessageFooter {
            markdown: "Acme Inc. · **Vienna**\n".into(),
            sent_with_maze: true,
            above_quote: false,
        };
        let (text, html) = parts(&build_message(&message).unwrap());
        assert_eq!(
            text,
            "Hi Anna,\n\nsee you.\n\n-- \nPhilip\n\nAcme Inc. · **Vienna**\n\n\
             Sent with Maze (https://mazemail.dev)\n"
        );
        let signature = html.find("Philip").unwrap();
        let footer = html.find("<strong>Vienna</strong>").unwrap();
        let maze = html
            .find("Sent with <a href=\"https://mazemail.dev\"")
            .unwrap();
        assert!(signature < footer && footer < maze, "{html}");
        assert!(html.contains(">Maze</a></p>"), "{html}");
        // No tracking parameters.
        assert!(!html.contains("mazemail.dev?") && !html.contains("utm_"));

        // Only the footer.
        message.footer.sent_with_maze = false;
        let (text, html) = parts(&build_message(&message).unwrap());
        assert!(
            text.ends_with("-- \nPhilip\n\nAcme Inc. · **Vienna**\n"),
            "{text}"
        );
        assert!(!text.contains("Sent with") && !html.contains("Sent with"));

        // Only the Maze line.
        message.footer.markdown.clear();
        message.footer.sent_with_maze = true;
        let (text, html) = parts(&build_message(&message).unwrap());
        assert!(
            text.ends_with("-- \nPhilip\n\nSent with Maze (https://mazemail.dev)\n"),
            "{text}"
        );
        assert!(!html.contains("class=\"footer\""));
        assert!(html.contains("class=\"sent-with\""));
    }

    #[test]
    fn in_replies_the_footer_goes_above_the_quote() {
        let body = "Sounds good!\n\n-- \nPhilip\n\n\
                    On Mon, 3 Nov 2025 at 09:00, Anna <anna@example.com> wrote:\n\
                    > Shall we meet?\n>\n> Anna\n";
        let footer = MessageFooter {
            markdown: "Acme Inc.".into(),
            sent_with_maze: true,
            above_quote: true,
        };
        let (text, html) = body_parts(body, &footer);
        assert_eq!(
            text,
            "Sounds good!\n\n-- \nPhilip\n\nAcme Inc.\n\n\
             Sent with Maze (https://mazemail.dev)\n\n\
             On Mon, 3 Nov 2025 at 09:00, Anna <anna@example.com> wrote:\n\
             > Shall we meet?\n>\n> Anna\n"
        );
        let maze = html.find("Sent with").unwrap();
        let attribution = html.find("wrote:").unwrap();
        let quote = html.find("<blockquote>").unwrap();
        assert!(html.find("Acme Inc.").unwrap() < maze);
        assert!(maze < attribution && attribution < quote, "{html}");

        // A forward: above the forwarded message.
        let forward = format!("FYI\n\n{FORWARD_HEADER}\nSubject: Report\n\nThe report.\n");
        let (text, _) = body_parts(&forward, &footer);
        assert!(
            text.starts_with(
                "FYI\n\nAcme Inc.\n\nSent with Maze (https://mazemail.dev)\n\n\
                 -------- Forwarded Message --------\n"
            ),
            "{text}"
        );

        // Answers between the quoted parts: the footer goes at the end.
        let inline = "> Shall we meet?\nYes.\n> Where?\nAt the office.\n";
        let (text, _) = body_parts(inline, &footer);
        assert!(
            text.ends_with(
                "At the office.\n\nAcme Inc.\n\nSent with Maze (https://mazemail.dev)\n"
            ),
            "{text}"
        );

        // A new message ending in a quote keeps its quote above the footer.
        let new = MessageFooter {
            above_quote: false,
            ..footer
        };
        let (text, _) = body_parts("As they say:\n> less is more\n", &new);
        assert!(
            text.starts_with("As they say:\n> less is more\n\nAcme"),
            "{text}"
        );
    }

    #[test]
    fn quote_starts() {
        assert_eq!(quote_start("Hi\n"), None);
        assert_eq!(quote_start(""), None);
        assert_eq!(quote_start("Hi\n\nAnna wrote:\n> x\n\n"), Some(4));
        assert_eq!(quote_start("Hi\n> x\n"), Some(3));
        assert_eq!(quote_start("> x\n"), Some(0));
        assert_eq!(quote_start(&format!("a\n{FORWARD_HEADER}\n> x\n")), Some(2));
        // An empty reply: the footer comes first, the quote after it.
        let (text, _) = body_parts(
            "\n\nAnna wrote:\n> x\n",
            &MessageFooter {
                markdown: String::new(),
                sent_with_maze: true,
                above_quote: true,
            },
        );
        assert_eq!(
            text,
            "Sent with Maze (https://mazemail.dev)\n\nAnna wrote:\n> x\n"
        );
    }

    #[test]
    fn builds_multipart_alternative_with_the_right_headers() {
        let raw = build_message(&sample()).unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(!text.to_ascii_lowercase().contains("bcc:"));
        let parsed = MessageParser::default().parse(&raw).unwrap();
        assert_eq!(
            parsed.message_id(),
            Some("abababababababababababababababab@distr.example")
        );
        assert_eq!(parsed.in_reply_to().as_text(), Some("orig@example.com"));
        assert_eq!(
            parsed.references().as_text_list().unwrap(),
            vec!["root@example.com", "orig@example.com"]
        );
        assert_eq!(parsed.subject(), Some("Re: Q3 offsite – Grüße"));
        let from = parsed.from().unwrap().first().unwrap();
        assert_eq!(from.address(), Some("you@distr.example"));
        assert_eq!(from.name(), Some("Philip · distr"));
        assert_eq!(
            parsed.reply_to().unwrap().first().unwrap().address(),
            Some("support@distr.example")
        );
        assert_eq!(
            parsed.to().unwrap().first().unwrap().address(),
            Some("anna@example.com")
        );
        assert_eq!(
            parsed.cc().unwrap().first().unwrap().address(),
            Some("team@example.com")
        );
        let root = parsed.root_part();
        let content_type = root.content_type().unwrap();
        assert_eq!(content_type.ctype(), "multipart");
        assert_eq!(content_type.subtype(), Some("alternative"));
        assert_eq!(parsed.text_body_count(), 1);
        assert_eq!(parsed.html_body_count(), 1);
        let plain = parsed.body_text(0).unwrap();
        assert!(plain.contains("see **below**."), "{plain}");
        let html = parsed.body_html(0).unwrap();
        assert!(html.contains("<strong>below</strong>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;"),
            "raw HTML is escaped: {html}"
        );
        assert!(!html.contains("<script>"));
        assert!(html.contains("<blockquote>"));
        assert!(text.contains("User-Agent: Maze 0.1"));
    }

    #[test]
    fn attachments_make_it_mixed() {
        let mut message = sample();
        message.attachments.push(OutgoingAttachment {
            name: "plan v3.pdf".into(),
            content_type: content_type_for("plan v3.pdf").into(),
            data: b"%PDF-1.7 fake".to_vec(),
        });
        let raw = build_message(&message).unwrap();
        let parsed = MessageParser::default().parse(&raw).unwrap();
        let root = parsed.root_part().content_type().unwrap();
        assert_eq!(root.subtype(), Some("mixed"));
        assert_eq!(parsed.attachment_count(), 1);
        let attachment = parsed.attachment(0).unwrap();
        assert_eq!(attachment.attachment_name(), Some("plan v3.pdf"));
        assert_eq!(attachment.contents(), b"%PDF-1.7 fake");
        assert_eq!(parsed.text_body_count(), 1);
        assert_eq!(parsed.html_body_count(), 1);
    }

    #[test]
    fn needs_a_sender_and_an_id() {
        let mut message = sample();
        message.from = None;
        assert_eq!(build_message(&message), Err(ComposeError::NoSender));
        let mut message = sample();
        message.message_id.clear();
        assert_eq!(build_message(&message), Err(ComposeError::NoMessageId));
        assert!(!format!("{:?}", sample()).contains("anna"));
    }

    #[test]
    fn message_ids_use_the_senders_domain() {
        assert_eq!(
            new_message_id(&[1; 16], "Distr.Example"),
            "01010101010101010101010101010101@distr.example"
        );
        assert!(new_message_id(&[1; 16], "").ends_with("@mazemail.invalid"));
    }

    #[test]
    fn subject_prefixes_are_not_duplicated() {
        assert_eq!(reply_subject("Q3 offsite"), "Re: Q3 offsite");
        assert_eq!(reply_subject("Re: Q3 offsite"), "Re: Q3 offsite");
        assert_eq!(reply_subject("AW: Angebot"), "AW: Angebot");
        assert_eq!(reply_subject("RE: re: x"), "RE: re: x");
        assert_eq!(reply_subject("Fwd: Report"), "Re: Fwd: Report");
        assert_eq!(reply_subject(""), "Re: ");
        assert_eq!(forward_subject("Report"), "Fwd: Report");
        assert_eq!(forward_subject("Fwd: Report"), "Fwd: Report");
        assert_eq!(forward_subject("WG: Bericht"), "WG: Bericht");
        assert_eq!(forward_subject("Re: Report"), "Fwd: Re: Report");
        assert_eq!(forward_subject("Agenda: Monday"), "Fwd: Agenda: Monday");
    }

    #[test]
    fn references_chain_grows_and_stays_bounded() {
        assert_eq!(
            reply_references(&[], None, Some("<a@x>")),
            vec!["a@x".to_owned()]
        );
        assert_eq!(
            reply_references(&[], Some("<p@x>"), Some("a@x")),
            vec!["p@x".to_owned(), "a@x".to_owned()]
        );
        assert_eq!(
            reply_references(&["r@x".into(), "p@x".into()], Some("p@x"), Some("a@x")),
            vec!["r@x".to_owned(), "p@x".to_owned(), "a@x".to_owned()]
        );
        // Duplicates are dropped.
        assert_eq!(
            reply_references(&["r@x".into(), "a@x".into()], None, Some("a@x")),
            vec!["r@x".to_owned(), "a@x".to_owned()]
        );
        let long: Vec<String> = (0..40).map(|i| format!("{i}@x")).collect();
        let refs = reply_references(&long, None, Some("new@x"));
        assert_eq!(refs.len(), MAX_REFERENCES);
        assert_eq!(refs[0], "0@x");
        assert_eq!(refs.last().map(String::as_str), Some("new@x"));
    }

    #[test]
    fn quotes_line_by_line() {
        assert_eq!(
            quote("Hi,\r\n\r\nthanks\n> earlier\n"),
            "> Hi,\n>\n> thanks\n>> earlier\n"
        );
    }

    #[test]
    fn html_quotes_become_markdown_without_images_or_scripts() {
        let markdown = html_to_markdown(
            "<html><head><style>p{}</style></head><body><p>Hello <b>Bob</b>,</p>\
             <p>see <a href=\"https://example.com/x\">the plan</a>.</p>\
             <img src=\"https://tracker.example/p.gif\"><script>evil()</script>\
             <ul><li>one</li><li>two</li></ul></body></html>",
        );
        assert!(markdown.contains("Hello **Bob**,"), "{markdown}");
        assert!(
            markdown.contains("[the plan](https://example.com/x)"),
            "{markdown}"
        );
        assert!(markdown.contains("one") && markdown.contains("two"));
        assert!(!markdown.contains("tracker") && !markdown.contains("evil"));
    }

    #[test]
    fn line_breaks_are_kept_in_html() {
        let html = markdown_to_html("one\ntwo");
        assert!(html.contains("one<br />\ntwo"), "{html}");
    }

    #[test]
    fn content_types_by_extension() {
        assert_eq!(content_type_for("a.PDF"), "application/pdf");
        assert_eq!(content_type_for("noext"), "application/octet-stream");
    }
}
