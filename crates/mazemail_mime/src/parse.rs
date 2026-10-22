//! Message parsing on top of `mail-parser`.

use std::borrow::Cow;
use std::fmt;

use mail_parser::{MessageParser, MessagePart, MimeHeaders, PartType};
use mazemail_types::Address;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset};

use crate::html::{PreparedHtml, SanitizeOptions, prepare};
use crate::lang::{Lang, detect_language};
use crate::reader::{self, ReaderDoc, TextFormat, from_plain_text};

/// Length of [`ParsedMessage::snippet`], in characters.
pub const SNIPPET_CHARS: usize = 200;

/// The raw input isn't a message at all (no header could be read).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The input has no readable header block.
    #[error("no message headers found")]
    NoHeaders,
}

/// The headers Maze stores and displays.
///
/// Message-IDs are stored without angle brackets. Addresses without an email part
/// (bare group names like `undisclosed-recipients:;`) are left out. Header values
/// are decoded (RFC 2047/2231) and unfolded.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MessageHeaders {
    pub message_id: Option<String>,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    /// The `Date` header with its original UTC offset. `None` if missing or invalid.
    #[serde(with = "time::serde::rfc3339::option")]
    pub date: Option<OffsetDateTime>,
    pub subject: Option<String>,
    pub from: Vec<Address>,
    pub sender: Option<Address>,
    pub reply_to: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    /// `Delivered-To` addresses, top (most recent hop) first. Used for identity
    /// selection when replying (CONCEPT §8.6).
    pub delivered_to: Vec<String>,
    /// `X-Original-To` addresses, top first.
    pub x_original_to: Vec<String>,
    /// `Envelope-To` addresses, top first.
    pub envelope_to: Vec<String>,
    /// The list identifier from `List-Id` (the part in angle brackets).
    pub list_id: Option<String>,
    /// `List-Unsubscribe` URIs (`mailto:` and `https:`), in header order.
    pub list_unsubscribe: Vec<String>,
    /// `List-Unsubscribe-Post` (RFC 8058 one-click), e.g. `List-Unsubscribe=One-Click`.
    pub list_unsubscribe_post: Option<String>,
    /// Raw `Authentication-Results` values, top (added last) first. The `security`
    /// crate decides which ones to trust.
    pub authentication_results: Vec<String>,
}

impl fmt::Debug for MessageHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageHeaders")
            .field(
                "message_id",
                &self.message_id.as_ref().map(|_| "<redacted>"),
            )
            .field("references", &self.references.len())
            .field("date", &self.date)
            .field("subject", &self.subject.as_ref().map(|_| "<redacted>"))
            .field("from", &self.from.len())
            .field("to", &self.to.len())
            .field("cc", &self.cc.len())
            .field("list_id", &self.list_id.is_some())
            .finish_non_exhaustive()
    }
}

/// Whether a part is meant to be shown inline or offered as a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Shown in the message body (`Content-Disposition: inline`, or a Content-ID
    /// without disposition).
    Inline,
    /// Offered as a download.
    Attachment,
}

/// Metadata of a non-body MIME part.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentMeta {
    /// IMAP-style part path (`1`, `2.1`, …), usable with `BODY[<path>]`.
    pub part_path: String,
    /// Decoded file name, if the part has one.
    pub filename: Option<String>,
    /// Lowercase MIME type, `application/octet-stream` if unknown.
    pub content_type: String,
    /// Decoded size in bytes.
    pub size: usize,
    /// Content-ID without angle brackets, for `cid:` references.
    pub content_id: Option<String>,
    pub disposition: Disposition,
}

impl fmt::Debug for AttachmentMeta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttachmentMeta")
            .field("part_path", &self.part_path)
            .field("filename", &self.filename.as_ref().map(|_| "<redacted>"))
            .field("content_type", &self.content_type)
            .field("size", &self.size)
            .field("content_id", &self.content_id.is_some())
            .field("disposition", &self.disposition)
            .finish()
    }
}

impl AttachmentMeta {
    /// Whether this part counts as an attachment for the "has attachments" flag.
    /// Inline parts referenced by Content-ID (images in HTML mail) don't.
    pub fn counts_as_attachment(&self) -> bool {
        !(self.disposition == Disposition::Inline && self.content_id.is_some())
    }
}

/// A charset-decoded `text/plain` body.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBody {
    pub text: String,
    pub format: TextFormat,
}

impl fmt::Debug for TextBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextBody")
            .field(
                "text",
                &format_args!("<{} bytes redacted>", self.text.len()),
            )
            .field("format", &self.format)
            .finish()
    }
}

/// A parsed message.
///
/// `html_body` is the message's own, **unsanitized** HTML: pass it through
/// [`html::sanitize`](crate::html::sanitize) or use [`ParsedMessage::prepare_html`]
/// before showing it anywhere.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedMessage {
    pub headers: MessageHeaders,
    /// The `text/plain` body (several inline text parts are joined).
    pub text_body: Option<TextBody>,
    /// The `text/html` body, raw.
    pub html_body: Option<String>,
    /// Every non-body part, in message order.
    pub attachments: Vec<AttachmentMeta>,
    /// Whether any part [counts as an attachment](AttachmentMeta::counts_as_attachment).
    pub has_attachments: bool,
    /// Preview text, see [`ReaderDoc::snippet`].
    pub snippet: String,
    /// Size of the raw message in bytes.
    pub size: usize,
}

impl fmt::Debug for ParsedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedMessage")
            .field("headers", &self.headers)
            .field("text_body", &self.text_body)
            .field("html_body", &self.html_body.as_ref().map(String::len))
            .field("attachments", &self.attachments)
            .field("has_attachments", &self.has_attachments)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl ParsedMessage {
    /// The reader document: from the HTML body if there is one, else from the text
    /// body, else empty.
    pub fn reader_doc(&self) -> ReaderDoc {
        if let Some(html) = &self.html_body {
            return reader::from_html(html);
        }
        self.text_reader_doc().unwrap_or_default()
    }

    fn text_reader_doc(&self) -> Option<ReaderDoc> {
        self.text_body
            .as_ref()
            .map(|body| from_plain_text(&body.text, body.format))
    }

    /// Sanitized HTML plus reader document, if the message has an HTML body.
    pub fn prepare_html(&self, options: &SanitizeOptions) -> Option<PreparedHtml> {
        self.html_body.as_deref().map(|html| prepare(html, options))
    }

    /// Body text for the search index: the plain-text body (flowed text unwrapped),
    /// or the text of the HTML body.
    pub fn body_text(&self) -> String {
        match self.text_reader_doc() {
            Some(doc) => doc.plain_text(),
            None => self.reader_doc().plain_text(),
        }
    }

    /// Language hint for the search analyzers, from subject and body.
    pub fn language(&self) -> Option<Lang> {
        let mut text = self.headers.subject.clone().unwrap_or_default();
        text.push('\n');
        text.push_str(&self.body_text());
        detect_language(&text)
    }
}

/// Parse a raw RFC 5322 message.
///
/// Never panics. Broken MIME structure, unknown charsets and invalid encodings are
/// decoded on a best-effort basis (invalid bytes become U+FFFD). Fails only if not
/// a single header can be read.
pub fn parse(raw: &[u8]) -> Result<ParsedMessage, ParseError> {
    let message = MessageParser::default()
        .parse(raw)
        .ok_or(ParseError::NoHeaders)?;
    if message.parts.is_empty() {
        return Err(ParseError::NoHeaders);
    }
    let headers = extract_headers(&message);
    let paths = part_paths(&message);

    let text_parts: Vec<&MessagePart<'_>> = body_parts(&message, &message.text_body)
        .filter(|p| matches!(p.body, PartType::Text(_)))
        .collect();
    let text_body = (!text_parts.is_empty()).then(|| TextBody {
        text: join_bodies(&text_parts, message.raw_message.as_ref()).replace("\r\n", "\n"),
        format: text_format(text_parts[0]),
    });
    let html_parts: Vec<&MessagePart<'_>> = body_parts(&message, &message.html_body)
        .filter(|p| matches!(p.body, PartType::Html(_)))
        .collect();
    let html_body =
        (!html_parts.is_empty()).then(|| join_bodies(&html_parts, message.raw_message.as_ref()));

    let attachments: Vec<AttachmentMeta> = message
        .attachments
        .iter()
        .filter_map(|&id| {
            let part = message.parts.get(id as usize)?;
            let path = paths.get(id as usize).cloned().unwrap_or_default();
            Some(attachment_meta(part, path))
        })
        .collect();
    let has_attachments = attachments.iter().any(AttachmentMeta::counts_as_attachment);

    let mut parsed = ParsedMessage {
        headers,
        text_body,
        html_body,
        attachments,
        has_attachments,
        snippet: String::new(),
        size: raw.len(),
    };
    parsed.snippet = parsed
        .text_reader_doc()
        .filter(|doc| !doc.is_empty())
        .unwrap_or_else(|| parsed.reader_doc())
        .snippet(SNIPPET_CHARS);
    Ok(parsed)
}

/// Parse only the header block: cheap, for listing and threading before bodies are
/// fetched.
pub fn parse_headers_only(raw: &[u8]) -> Result<MessageHeaders, ParseError> {
    let message = MessageParser::default()
        .parse_headers(raw)
        .ok_or(ParseError::NoHeaders)?;
    if message.parts.is_empty() {
        return Err(ParseError::NoHeaders);
    }
    Ok(extract_headers(&message))
}

fn body_parts<'m>(
    message: &'m mail_parser::Message<'m>,
    ids: &'m [u32],
) -> impl Iterator<Item = &'m MessagePart<'m>> {
    ids.iter().filter_map(|&id| message.parts.get(id as usize))
}

fn join_bodies(parts: &[&MessagePart<'_>], raw: &[u8]) -> String {
    let mut out = String::new();
    for part in parts {
        let text = match &part.body {
            PartType::Text(t) | PartType::Html(t) => {
                fallback_decode(part, t, raw).map_or(Cow::Borrowed(t.as_ref()), Cow::Owned)
            }
            _ => continue,
        };
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&text);
    }
    out
}

/// Re-decode a text part as windows-1252 when its charset is missing or unknown and
/// the bytes aren't UTF-8 (old 8-bit mail, mislabeled charsets). mail-parser falls
/// back to lossy UTF-8 there, which turns every accented letter into U+FFFD;
/// Thunderbird uses a Western fallback encoding the same way.
fn fallback_decode(part: &MessagePart<'_>, decoded: &str, raw: &[u8]) -> Option<String> {
    if !decoded.contains('\u{FFFD}') {
        return None;
    }
    let label = part
        .content_type()
        .and_then(|ct| ct.attribute("charset"))
        .map(|c| c.trim().to_ascii_lowercase());
    let declared_known = label.as_deref().is_some_and(|label| {
        matches!(label, "utf-8" | "utf8")
            || mail_parser::decoders::charsets::map::charset_decoder(label.as_bytes()).is_some()
    });
    if declared_known {
        return None;
    }
    let body = raw.get(part.offset_body as usize..part.offset_end as usize)?;
    let bytes: Cow<'_, [u8]> = match part.encoding {
        mail_parser::Encoding::None => Cow::Borrowed(body),
        mail_parser::Encoding::QuotedPrintable => Cow::Owned(decode_quoted_printable(body)),
        mail_parser::Encoding::Base64 => return None,
    };
    if std::str::from_utf8(&bytes).is_ok() {
        return None;
    }
    let decode = mail_parser::decoders::charsets::map::charset_decoder(b"windows-1252")?;
    Some(decode(&bytes))
}

/// Lenient quoted-printable decoding (RFC 2045 §6.7) for the charset fallback.
fn decode_quoted_printable(input: &[u8]) -> Vec<u8> {
    let hex = |b: u8| {
        char::from(b)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
    };
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'=' {
            match (input.get(i + 1), input.get(i + 2)) {
                (Some(b'\r'), Some(b'\n')) => {
                    i += 3;
                    continue;
                }
                (Some(b'\n'), _) => {
                    i += 2;
                    continue;
                }
                (Some(&h), Some(&l)) => {
                    if let (Some(h), Some(l)) = (hex(h), hex(l)) {
                        out.push(h << 4 | l);
                        i += 3;
                        continue;
                    }
                }
                _ => {}
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn text_format(part: &MessagePart<'_>) -> TextFormat {
    let attr = |name: &str| {
        part.content_type()
            .and_then(|ct| ct.attribute(name))
            .map(|v| v.trim().to_ascii_lowercase())
    };
    TextFormat {
        flowed: attr("format").as_deref() == Some("flowed"),
        delsp: attr("delsp").as_deref() == Some("yes"),
    }
}

fn extract_headers(message: &mail_parser::Message<'_>) -> MessageHeaders {
    let raw = |name: &str| -> Vec<String> {
        message
            .headers_raw()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| unfold(v))
            .filter(|v| !v.is_empty())
            .collect()
    };
    let first_raw = |name: &str| raw(name).into_iter().next();

    MessageHeaders {
        message_id: message.message_id().and_then(clean_id).map(str::to_owned),
        in_reply_to: id_list(message.in_reply_to()),
        references: id_list(message.references()),
        date: message.date().and_then(convert_date),
        subject: message
            .subject()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()),
        from: message.from().map(convert_addresses).unwrap_or_default(),
        sender: message
            .sender()
            .map(convert_addresses)
            .and_then(|a| a.into_iter().next()),
        reply_to: message
            .reply_to()
            .map(convert_addresses)
            .unwrap_or_default(),
        to: message.all_to().flat_map(convert_addresses).collect(),
        cc: message.all_cc().flat_map(convert_addresses).collect(),
        bcc: message.all_bcc().flat_map(convert_addresses).collect(),
        delivered_to: raw("Delivered-To")
            .iter()
            .flat_map(|v| envelope_addresses(v))
            .collect(),
        x_original_to: raw("X-Original-To")
            .iter()
            .flat_map(|v| envelope_addresses(v))
            .collect(),
        envelope_to: raw("Envelope-To")
            .iter()
            .flat_map(|v| envelope_addresses(v))
            .collect(),
        list_id: first_raw("List-Id").and_then(|v| list_id(&v)),
        list_unsubscribe: raw("List-Unsubscribe")
            .iter()
            .flat_map(|v| angle_bracketed(v))
            .collect(),
        list_unsubscribe_post: first_raw("List-Unsubscribe-Post"),
        authentication_results: raw("Authentication-Results"),
    }
}

/// Unfold a raw header value: line breaks plus following whitespace become one space.
fn unfold(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut space = false;
    for c in value.chars() {
        if c == '\r' || c == '\n' || c == '\t' || c == ' ' {
            space = true;
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    out
}

fn clean_id(id: &str) -> Option<&str> {
    let id = id
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    (!id.is_empty() && !id.contains(char::is_whitespace)).then_some(id)
}

fn id_list(value: &mail_parser::HeaderValue<'_>) -> Vec<String> {
    let ids: Vec<&str> = match value {
        mail_parser::HeaderValue::Text(id) => vec![id.as_ref()],
        mail_parser::HeaderValue::TextList(ids) => ids.iter().map(Cow::as_ref).collect(),
        _ => Vec::new(),
    };
    ids.into_iter()
        .filter_map(clean_id)
        .map(str::to_owned)
        .collect()
}

fn convert_date(date: &mail_parser::DateTime) -> Option<OffsetDateTime> {
    if !date.is_valid() {
        return None;
    }
    let utc = OffsetDateTime::from_unix_timestamp(date.to_timestamp()).ok()?;
    let sign: i8 = if date.tz_before_gmt { -1 } else { 1 };
    let hours = i8::try_from(date.tz_hour).ok()?;
    let minutes = i8::try_from(date.tz_minute).ok()?;
    Some(match UtcOffset::from_hms(sign * hours, sign * minutes, 0) {
        Ok(offset) => utc.to_offset(offset),
        Err(_) => utc,
    })
}

fn convert_addresses(address: &mail_parser::Address<'_>) -> Vec<Address> {
    let convert = |addr: &mail_parser::Addr<'_>| {
        let email = addr.address.as_deref()?.trim();
        if email.is_empty() {
            return None;
        }
        let name = addr
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty() && *n != email);
        Some(Address::new(name, email))
    };
    match address {
        mail_parser::Address::List(list) => list.iter().filter_map(convert).collect(),
        mail_parser::Address::Group(groups) => groups
            .iter()
            .flat_map(|g| g.addresses.iter())
            .filter_map(convert)
            .collect(),
    }
}

/// Addresses in envelope headers: `a@b`, `<a@b>`, or comma-separated lists.
fn envelope_addresses(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            let addr = match (part.find('<'), part.rfind('>')) {
                (Some(open), Some(close)) if open < close => &part[open + 1..close],
                _ => part,
            };
            let addr = addr.trim();
            (addr.contains('@') && !addr.contains(char::is_whitespace)).then(|| addr.to_owned())
        })
        .collect()
}

/// `List-Id: Developer list <dev.lists.example.org>` → `dev.lists.example.org`.
fn list_id(value: &str) -> Option<String> {
    let id = angle_bracketed(value)
        .into_iter()
        .next()
        .unwrap_or_else(|| value.trim().to_owned());
    (!id.is_empty()).then_some(id)
}

/// All `<...>` items of a header value.
fn angle_bracketed(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else {
            break;
        };
        let item = after[..close].trim();
        if !item.is_empty() {
            out.push(item.to_owned());
        }
        rest = &after[close + 1..];
    }
    out
}

/// IMAP part paths for every part: the root multipart is `""`, its children `1`, `2`,
/// grandchildren `1.1`, …; a single-part message's only part is `1`.
fn part_paths(message: &mail_parser::Message<'_>) -> Vec<String> {
    let mut paths = vec![String::new(); message.parts.len()];
    let mut visited = vec![false; message.parts.len()];
    let mut stack = vec![(0usize, String::new())];
    while let Some((id, path)) = stack.pop() {
        let Some(part) = message.parts.get(id) else {
            continue;
        };
        if std::mem::replace(&mut visited[id], true) {
            continue;
        }
        match &part.body {
            PartType::Multipart(children) => {
                for (n, &child) in children.iter().enumerate() {
                    let child_path = if path.is_empty() {
                        (n + 1).to_string()
                    } else {
                        format!("{path}.{}", n + 1)
                    };
                    stack.push((child as usize, child_path));
                }
                paths[id] = path;
            }
            _ => {
                paths[id] = if path.is_empty() {
                    "1".to_owned()
                } else {
                    path
                }
            }
        }
    }
    paths
}

fn attachment_meta(part: &MessagePart<'_>, part_path: String) -> AttachmentMeta {
    let content_type = match (&part.body, part.content_type()) {
        (PartType::Message(_), _) => "message/rfc822".to_owned(),
        (_, Some(ct)) => match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub).to_ascii_lowercase(),
            None => ct.ctype().to_ascii_lowercase(),
        },
        (PartType::Text(_), None) => "text/plain".to_owned(),
        (PartType::Html(_), None) => "text/html".to_owned(),
        _ => "application/octet-stream".to_owned(),
    };
    let content_id = part.content_id().and_then(clean_id).map(str::to_owned);
    let disposition = match part
        .content_disposition()
        .map(|d| d.ctype().to_ascii_lowercase())
        .as_deref()
    {
        Some("inline") => Disposition::Inline,
        Some(_) => Disposition::Attachment,
        None if content_id.is_some() => Disposition::Inline,
        None => Disposition::Attachment,
    };
    AttachmentMeta {
        part_path,
        filename: part
            .attachment_name()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned),
        content_type,
        size: part.body.len(),
        content_id,
        disposition,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn rejects_input_without_headers() {
        assert_eq!(parse(b"").unwrap_err(), ParseError::NoHeaders);
        assert_eq!(parse_headers_only(b"").unwrap_err(), ParseError::NoHeaders);
    }

    #[test]
    fn parses_envelope_headers() {
        let raw = b"Delivered-To: anna@example.com\r\nDelivered-To: <alias@example.org>\r\n\
            X-Original-To: a@example.com, b@example.com\r\nEnvelope-To: not an address\r\n\
            List-Id: \"Dev\r\n list\" <dev.lists.example.org>\r\n\
            List-Unsubscribe: <mailto:leave@example.org?subject=unsubscribe>,\r\n <https://example.org/u/1>\r\n\
            List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
            Authentication-Results: mx.example.com;\r\n dkim=pass header.d=example.org\r\n\
            Message-ID: <abc@example.com>\r\nReferences: <a@x> <b@x>\r\n  <c@x>\r\nIn-Reply-To: <c@x>\r\n\
            Date: Tue, 1 Oct 2024 09:30:00 +0200\r\nSubject: Hi\r\n\r\nbody";
        let h = parse_headers_only(raw).unwrap();
        assert_eq!(
            h.delivered_to,
            vec!["anna@example.com", "alias@example.org"]
        );
        assert_eq!(h.x_original_to, vec!["a@example.com", "b@example.com"]);
        assert!(h.envelope_to.is_empty());
        assert_eq!(h.list_id.as_deref(), Some("dev.lists.example.org"));
        assert_eq!(
            h.list_unsubscribe,
            vec![
                "mailto:leave@example.org?subject=unsubscribe",
                "https://example.org/u/1"
            ]
        );
        assert_eq!(
            h.list_unsubscribe_post.as_deref(),
            Some("List-Unsubscribe=One-Click")
        );
        assert_eq!(
            h.authentication_results,
            vec!["mx.example.com; dkim=pass header.d=example.org"]
        );
        assert_eq!(h.message_id.as_deref(), Some("abc@example.com"));
        assert_eq!(h.references, vec!["a@x", "b@x", "c@x"]);
        assert_eq!(h.in_reply_to, vec!["c@x"]);
        let date = h.date.unwrap();
        assert_eq!(date.unix_timestamp(), 1_727_767_800);
        assert_eq!(date.offset().whole_hours(), 2);
    }

    #[test]
    fn unknown_charsets_fall_back_to_windows_1252() {
        let raw = b"Content-Type: text/plain; charset=x-unknown\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nCaf=E9 =80 soft=\r\nbreak\r\n";
        let m = parse(raw).unwrap();
        assert_eq!(m.text_body.unwrap().text, "Caf\u{e9} \u{20ac} softbreak\n");
        // A declared charset is trusted, even if the bytes don't match.
        let raw = b"Content-Type: text/plain; charset=utf-8\r\n\r\nCaf\xe9";
        assert!(
            parse(raw)
                .unwrap()
                .text_body
                .unwrap()
                .text
                .contains('\u{FFFD}')
        );
    }

    #[test]
    fn invalid_dates_are_dropped() {
        let h = parse_headers_only(b"Date: Tue, 45 Foo 2024 99:99:99 +0200\r\n\r\n").unwrap();
        assert_eq!(h.date, None);
    }

    #[test]
    fn computes_part_paths() {
        let raw = b"Content-Type: multipart/mixed; boundary=a\r\n\r\n--a\r\n\
            Content-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\n\
            Content-Type: text/plain\r\n\r\nplain\r\n--b\r\nContent-Type: text/html\r\n\r\n<p>html</p>\r\n--b--\r\n\
            --a\r\nContent-Type: application/pdf; name=\"r.pdf\"\r\nContent-Disposition: attachment\r\n\
            Content-Transfer-Encoding: base64\r\n\r\nJVBERi0=\r\n--a--\r\n";
        let m = parse(raw).unwrap();
        assert_eq!(m.attachments.len(), 1);
        let a = &m.attachments[0];
        assert_eq!(a.part_path, "2");
        assert_eq!(a.filename.as_deref(), Some("r.pdf"));
        assert_eq!(a.content_type, "application/pdf");
        assert_eq!(a.size, 5);
        assert!(m.has_attachments);
        assert_eq!(m.text_body.unwrap().text.trim(), "plain");
        assert_eq!(m.html_body.unwrap().trim(), "<p>html</p>");
    }

    #[test]
    fn debug_output_is_redacted() {
        let m = parse(b"Subject: Top secret\r\nFrom: Anna <anna@example.com>\r\n\r\nclassified")
            .unwrap();
        let debug = format!("{m:?}");
        assert!(
            !debug.contains("secret") && !debug.contains("anna") && !debug.contains("classified")
        );
    }

    #[test]
    fn snippet_falls_back_to_html() {
        let raw = b"Content-Type: text/html\r\n\r\n<style>p{}</style><p>Hello <b>there</b></p><blockquote>old</blockquote>";
        let m = parse(raw).unwrap();
        assert_eq!(m.snippet, "Hello there");
        assert!(m.text_body.is_none());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn never_panics_on_random_bytes(raw in prop::collection::vec(any::<u8>(), 0..2048)) {
            if let Ok(m) = parse(&raw) {
                let _ = m.reader_doc();
                let _ = m.body_text();
            }
            let _ = parse_headers_only(&raw);
        }

        #[test]
        fn never_panics_on_mail_like_input(
            headers in prop::collection::vec(
                (
                    prop::sample::select(vec![
                        "From", "To", "Subject", "Date", "Message-ID", "References",
                        "Content-Type", "Content-Transfer-Encoding", "Content-Disposition",
                        "Delivered-To", "List-Id",
                    ]),
                    "[ -~\u{80}-\u{ff}]{0,60}",
                ),
                0..10,
            ),
            body in "[ -~\r\n\u{80}-\u{ff}]{0,400}",
        ) {
            let mut raw = String::new();
            for (name, value) in headers {
                raw.push_str(&format!("{name}: {value}\r\n"));
            }
            raw.push_str("\r\n");
            raw.push_str(&body);
            if let Ok(m) = parse(raw.as_bytes()) {
                let _ = m.reader_doc();
                let _ = m.prepare_html(&SanitizeOptions::default());
            }
        }
    }
}
