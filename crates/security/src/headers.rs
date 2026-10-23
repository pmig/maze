//! A minimal, order-preserving scanner for the raw header block.
//!
//! Trust decisions about `Authentication-Results` depend on where a header sits
//! relative to `Received` headers, so this module keeps the exact top-to-bottom order
//! and does nothing clever: no decoding, no merging, no reordering.

use std::borrow::Cow;
use std::net::IpAddr;

/// Headers past this count are ignored. The trust logic only looks at the top of the
/// block, and a cap keeps hostile inputs with huge header blocks cheap.
pub(crate) const MAX_HEADERS: usize = 1_000;

/// One header field, as it appears in the message.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RawHeader<'a> {
    /// The field name, without the colon or surrounding whitespace.
    pub(crate) name: Cow<'a, str>,
    /// The unfolded value (CRLF/LF before whitespace removed), leading and trailing
    /// whitespace trimmed. Invalid UTF-8 is replaced.
    pub(crate) value: String,
}

impl RawHeader<'_> {
    /// Case-insensitive field name comparison.
    pub(crate) fn is(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }
}

/// Split the header block of `raw` into fields, top (most recently added) first.
///
/// Stops at the first empty line, at the end of input, or after [`MAX_HEADERS`]
/// fields. Lines that are neither a field nor a continuation (no colon) end the
/// header block, as they would for a strict parser.
pub(crate) fn scan(raw: &[u8]) -> Vec<RawHeader<'_>> {
    let mut headers = Vec::new();
    let lines = Lines { rest: raw };
    let mut current: Option<(&[u8], Vec<u8>)> = None;

    for line in lines {
        if line.is_empty() {
            break;
        }
        if matches!(line.first(), Some(b' ' | b'\t')) {
            match current.as_mut() {
                // A continuation line: unfolding removes only the line break.
                Some((_, value)) => value.extend_from_slice(line),
                // Leading whitespace before any field: not a header block.
                None => break,
            }
            continue;
        }
        if let Some((name, value)) = current.take() {
            headers.push(finish(name, &value));
            if headers.len() >= MAX_HEADERS {
                return headers;
            }
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            break;
        };
        let name = trim_ascii(&line[..colon]);
        if name.is_empty() || !name.iter().all(|&b| b.is_ascii_graphic()) {
            break;
        }
        current = Some((name, line[colon + 1..].to_vec()));
    }
    if let Some((name, value)) = current {
        headers.push(finish(name, &value));
    }
    headers
}

fn finish<'a>(name: &'a [u8], value: &[u8]) -> RawHeader<'a> {
    let name = String::from_utf8_lossy(name);
    let value = String::from_utf8_lossy(trim_ascii(value)).into_owned();
    RawHeader { name, value }
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |p| p + 1);
    &bytes[start..end]
}

/// Lines split on LF, with a trailing CR removed.
struct Lines<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Lines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        let (line, rest) = match self.rest.iter().position(|&b| b == b'\n') {
            Some(lf) => (&self.rest[..lf], &self.rest[lf + 1..]),
            None => (self.rest, &[][..]),
        };
        self.rest = rest;
        Some(line.strip_suffix(b"\r").unwrap_or(line))
    }
}

/// The host in the `by` clause of a `Received` header, lowercased, without a
/// trailing dot or IP-literal brackets.
///
/// Comments are removed first, so text like `(authenticated by foo)` doesn't count.
/// Returns `None` when the header has no `by` clause.
pub(crate) fn received_by_host(value: &str) -> Option<String> {
    // The date follows the last semicolon; it never contains the by clause.
    let clauses = value.rsplit_once(';').map_or(value, |(clauses, _)| clauses);
    let stripped = strip_comments(clauses);
    let mut tokens = stripped.split_ascii_whitespace();
    while let Some(token) = tokens.next() {
        if token.eq_ignore_ascii_case("by") {
            let host = tokens.next()?;
            let host = host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim_end_matches('.');
            return (!host.is_empty()).then(|| host.to_ascii_lowercase());
        }
    }
    None
}

/// The `from` clause of a `Received` header: the host the sender named and every
/// IP address in the clause (the receiving server records the connecting address
/// there, usually as `[192.0.2.1]` inside a comment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReceivedFrom {
    /// The first word after `from`, lowercased, without a trailing dot. Chosen by the
    /// sender (`HELO`), so only meaningful when the clause has no address.
    pub(crate) host: Option<String>,
    /// Every IP address in the clause, comments included.
    pub(crate) addresses: Vec<IpAddr>,
}

/// The `from` clause of a `Received` header, or `None` when it has none (a message
/// created on the receiving host itself).
pub(crate) fn received_from(value: &str) -> Option<ReceivedFrom> {
    let clauses = value.rsplit_once(';').map_or(value, |(clauses, _)| clauses);
    let words = top_level_words(clauses);
    let from = words
        .iter()
        .position(|w| w.text.eq_ignore_ascii_case("from"))?;
    let end = words[from + 1..]
        .iter()
        .find(|w| w.text.eq_ignore_ascii_case("by"))
        .map_or(clauses.len(), |by| by.start);
    let clause = clauses.get(words[from].end..end).unwrap_or("");
    let host = words
        .get(from + 1)
        .filter(|w| w.start < end)
        .map(|w| w.text.trim_end_matches('.').to_ascii_lowercase())
        .filter(|h| !h.is_empty());
    let addresses = clause
        .split(|c: char| c.is_whitespace() || "()[]=,;".contains(c))
        .filter_map(|token| {
            let token = token.trim_end_matches('.');
            let token = token
                .get(..5)
                .filter(|prefix| prefix.eq_ignore_ascii_case("ipv6:"))
                .map_or(token, |_| &token[5..]);
            token.parse::<IpAddr>().ok()
        })
        .collect();
    Some(ReceivedFrom { host, addresses })
}

/// The comment right after the `by` host of a `Received` header, which names the
/// receiving server's software: `Postfix` in `by mx.example.org (Postfix) with
/// ESMTPS`. The receiving server writes the whole `by` clause.
pub(crate) fn received_by_software(value: &str) -> Option<String> {
    let clauses = value.rsplit_once(';').map_or(value, |(clauses, _)| clauses);
    let words = top_level_words(clauses);
    let by = words
        .iter()
        .position(|w| w.text.eq_ignore_ascii_case("by"))?;
    let host = words.get(by + 1)?;
    let rest = clauses.get(host.end..)?.trim_start();
    let inner = rest.strip_prefix('(')?;
    let mut depth = 1usize;
    for (index, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(inner[..index].trim().to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

/// A word outside comments and quotes, with its byte range in the value.
struct Word<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

/// The whitespace-separated words of a header value that aren't inside comments or
/// quoted strings. Comments separate words.
fn top_level_words(value: &str) -> Vec<Word<'_>> {
    let mut words = Vec::new();
    let mut depth = 0usize;
    let mut in_quote = false;
    let mut escaped = false;
    let mut start = None;
    let mut push = |start: &mut Option<usize>, end: usize| {
        if let Some(begin) = start.take() {
            words.push(Word {
                text: &value[begin..end],
                start: begin,
                end,
            });
        }
    };
    for (index, c) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if depth > 0 || in_quote => escaped = true,
            '"' if depth == 0 => {
                push(&mut start, index);
                in_quote = !in_quote;
            }
            '(' if !in_quote => {
                push(&mut start, index);
                depth += 1;
            }
            ')' if !in_quote && depth > 0 => depth -= 1,
            c if depth == 0 && !in_quote => {
                if c.is_whitespace() {
                    push(&mut start, index);
                } else if start.is_none() {
                    start = Some(index);
                }
            }
            _ => {}
        }
    }
    if depth == 0 && !in_quote {
        push(&mut start, value.len());
    }
    words
}

/// Remove RFC 5322 comments (nested parentheses, backslash escapes) and replace
/// each with a space.
pub(crate) fn strip_comments(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut depth = 0usize;
    let mut chars = value.chars();
    let mut in_quote = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' if depth > 0 || in_quote => {
                let next = chars.next();
                if depth == 0
                    && let Some(next) = next
                {
                    out.push('\\');
                    out.push(next);
                }
            }
            '"' if depth == 0 => {
                in_quote = !in_quote;
                out.push(c);
            }
            '(' if !in_quote => {
                depth += 1;
                if depth == 1 {
                    out.push(' ');
                }
            }
            ')' if !in_quote && depth > 0 => depth -= 1,
            _ if depth > 0 => {}
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_in_order_and_unfolds() {
        let raw = b"Received: from a\r\n by b; Mon, 1 Jan 2024\r\nX-Test:  one\r\n\ttwo \r\nSubject: hi\r\n\r\nBody: not a header\r\n";
        let headers = scan(raw);
        let pairs: Vec<_> = headers
            .iter()
            .map(|h| (h.name.as_ref(), h.value.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("Received", "from a by b; Mon, 1 Jan 2024"),
                ("X-Test", "one\ttwo"),
                ("Subject", "hi"),
            ]
        );
    }

    #[test]
    fn stops_at_garbage() {
        assert!(scan(b"").is_empty());
        assert!(scan(b" leading continuation\r\n").is_empty());
        assert_eq!(scan(b"A: 1\nnot a header\nB: 2\n").len(), 1);
        assert_eq!(scan(b"A: 1\n: empty name\n").len(), 1);
    }

    #[test]
    fn caps_header_count() {
        let raw = "X: y\n".repeat(MAX_HEADERS + 50);
        assert_eq!(scan(raw.as_bytes()).len(), MAX_HEADERS);
    }

    #[test]
    fn extracts_by_host() {
        assert_eq!(
            received_by_host(
                "from mail.example.com (mail.example.com. [192.0.2.1]) by mx.google.com with ESMTPS id x; Tue, 2 Jan 2024 10:00:00 -0800 (PST)"
            )
            .as_deref(),
            Some("mx.google.com")
        );
        assert_eq!(
            received_by_host("by 2002:a05:6504:1:b0:2:3:4 with SMTP id q; Tue, 2 Jan 2024")
                .as_deref(),
            Some("2002:a05:6504:1:b0:2:3:4")
        );
        assert_eq!(
            received_by_host("from x (authenticated by evil.example) by MX.Example.COM. ; date")
                .as_deref(),
            Some("mx.example.com")
        );
        assert_eq!(
            received_by_host("from x by [192.0.2.7]; date").as_deref(),
            Some("192.0.2.7")
        );
        assert_eq!(received_by_host("from x with SMTP; date"), None);
        assert_eq!(received_by_host("from x by"), None);
    }

    #[test]
    fn extracts_the_from_clause() {
        let postfix = received_from(
            "from mail.example.com (mail.example.com [192.0.2.10]) (using TLSv1.3) by mx.reader.example (Postfix) with ESMTPS id 4T; Tue, 2 Jan 2024 10:00:05 +0000",
        )
        .unwrap();
        assert_eq!(postfix.host.as_deref(), Some("mail.example.com"));
        assert_eq!(postfix.addresses, ["192.0.2.10".parse::<IpAddr>().unwrap()]);

        // Exim, Exchange and IPv6 literals; the HELO can be an address literal too.
        let exim = received_from(
            "from mail.attacker.example ([203.0.113.5] helo=[10.0.0.1]) by mx.example.org with esmtps (Exim 4.97) id 1r; date",
        )
        .unwrap();
        assert_eq!(
            exim.addresses,
            [
                "203.0.113.5".parse::<IpAddr>().unwrap(),
                "10.0.0.1".parse().unwrap()
            ]
        );
        let exchange = received_from(
            "from mail.example.com (192.0.2.10) by BN8.mail.protection.outlook.com (10.13.177.57) with Microsoft SMTP Server; date",
        )
        .unwrap();
        assert_eq!(
            exchange.addresses,
            ["192.0.2.10".parse::<IpAddr>().unwrap()]
        );
        let v6 = received_from("from x ([IPv6:2001:db8::1]) by y; date").unwrap();
        assert_eq!(v6.addresses, ["2001:db8::1".parse::<IpAddr>().unwrap()]);

        // Dovecot's LMTP hop names no address.
        let lmtp = received_from(
            "from mail.reader.example by mail.reader.example with LMTP id k (envelope-from <a@example.com>) for <b@reader.example>; date",
        )
        .unwrap();
        assert_eq!(lmtp.host.as_deref(), Some("mail.reader.example"));
        assert!(lmtp.addresses.is_empty());

        assert_eq!(
            received_from("by 2002:a05:6504:1:b0:2:3:4 with SMTP id q; date"),
            None
        );
        assert_eq!(
            received_from("(from x [192.0.2.1]) by y; date"),
            None,
            "a commented-out clause isn't a clause"
        );
    }

    #[test]
    fn extracts_the_by_software() {
        assert_eq!(
            received_by_software(
                "from a (a [192.0.2.10]) (using TLSv1.3 with cipher X) by mx.reader.example (Stalwart SMTP) with ESMTPS id 1A; date"
            )
            .as_deref(),
            Some("Stalwart SMTP")
        );
        assert_eq!(
            received_by_software("from a by mx.example.org (Postfix (nested)) with ESMTP; date")
                .as_deref(),
            Some("Postfix (nested)")
        );
        assert_eq!(
            received_by_software("from x (Stalwart SMTP) by mx.example.org with ESMTP; date"),
            None,
            "only the comment after the by host counts"
        );
        assert_eq!(
            received_by_software("from x by mx.example.org with ESMTP; date"),
            None
        );
        assert_eq!(received_by_software("from x with SMTP; date"), None);
    }

    #[test]
    fn strips_nested_comments() {
        assert_eq!(strip_comments("a (b (c) d) e"), "a   e");
        assert_eq!(strip_comments("a (b \\) c) d"), "a   d");
        assert_eq!(
            strip_comments("\"(not a comment)\" x"),
            "\"(not a comment)\" x"
        );
        assert_eq!(strip_comments("unbalanced ( forever"), "unbalanced  ");
    }
}
