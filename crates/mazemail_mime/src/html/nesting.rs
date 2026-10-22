//! Bound element nesting before the HTML parser sees the input.
//!
//! html5ever, like the HTML spec it implements, checks the stack of open elements on
//! most start tags, so a document of n nested elements costs O(n²): 50 000 nested
//! `<div>`s take seconds. Browsers cap the tree depth (Blink at 512). This linear
//! pre-pass does the same: start tags beyond [`MAX_DEPTH`] open elements are dropped
//! together with their end tags, and their content is kept.

use std::borrow::Cow;
use std::collections::HashMap;

/// Maximum number of open (non-void) elements.
pub(crate) const MAX_DEPTH: usize = 512;

/// Elements without content.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "keygen", "link", "meta", "param",
    "source", "track", "wbr",
];

/// Elements whose end tag is usually omitted; counting them would overestimate depth.
const OPTIONAL_END: &[&str] = &[
    "p", "li", "dt", "dd", "tr", "td", "th", "option", "optgroup", "tbody", "thead", "tfoot",
    "colgroup", "rb", "rt", "rp", "rtc", "caption", "html", "head", "body",
];

/// Elements whose content is raw text: tags inside don't count.
const RAW_TEXT: &[&str] = &[
    "script",
    "style",
    "textarea",
    "title",
    "xmp",
    "iframe",
    "noembed",
    "noframes",
    "noscript",
    "plaintext",
];

/// Return `html` with start tags beyond [`MAX_DEPTH`] (and their end tags) removed.
pub(crate) fn limit_nesting(html: &str) -> Cow<'_, str> {
    if html.bytes().filter(|&b| b == b'<').count() <= MAX_DEPTH {
        return Cow::Borrowed(html);
    }
    let bytes = html.as_bytes();
    let mut out = String::new();
    let mut copied = 0;
    let mut stack: Vec<String> = Vec::new();
    let mut skipped: HashMap<String, usize> = HashMap::new();
    let mut i = 0;
    while let Some(offset) = memchr_lt(&bytes[i..]) {
        let start = i + offset;
        let rest = &bytes[start + 1..];
        if rest.starts_with(b"!--") {
            i = find(bytes, start + 4, b"-->").map_or(bytes.len(), |e| e + 3);
            continue;
        }
        let (is_end, name_start) = match rest.first() {
            Some(b'/') => (true, start + 2),
            Some(b) if b.is_ascii_alphabetic() => (false, start + 1),
            _ => {
                i = start + 1;
                continue;
            }
        };
        let name_end = bytes[name_start..]
            .iter()
            .position(|b| b.is_ascii_whitespace() || matches!(b, b'/' | b'>'))
            .map_or(bytes.len(), |p| name_start + p);
        let name = html[name_start..name_end].to_ascii_lowercase();
        let tag_end = tag_end(bytes, name_end);
        if name.is_empty() {
            i = tag_end;
            continue;
        }
        if is_end {
            if let Some(count) = skipped.get_mut(&name).filter(|c| **c > 0) {
                *count -= 1;
                out.push_str(&html[copied..start]);
                copied = tag_end;
            } else if let Some(pos) = stack.iter().rposition(|open| *open == name) {
                stack.truncate(pos);
            }
            i = tag_end;
            continue;
        }
        let name_ref = name.as_str();
        if RAW_TEXT.contains(&name_ref) {
            let close = format!("</{name}");
            i = find_ignore_case(bytes, tag_end, close.as_bytes()).unwrap_or(bytes.len());
            continue;
        }
        if !VOID.contains(&name_ref) && !OPTIONAL_END.contains(&name_ref) {
            if stack.len() >= MAX_DEPTH {
                *skipped.entry(name).or_default() += 1;
                out.push_str(&html[copied..start]);
                copied = tag_end;
            } else {
                stack.push(name);
            }
        }
        i = tag_end;
    }
    if copied == 0 {
        return Cow::Borrowed(html);
    }
    out.push_str(&html[copied..]);
    Cow::Owned(out)
}

fn memchr_lt(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|&b| b == b'<')
}

/// Index just past the `>` that ends a tag, honoring quoted attribute values.
fn tag_end(bytes: &[u8], from: usize) -> usize {
    let mut quote = None;
    for (offset, &b) in bytes[from..].iter().enumerate() {
        match (quote, b) {
            (None, b'"' | b'\'') => quote = Some(b),
            (Some(q), _) if b == q => quote = None,
            (None, b'>') => return from + offset + 1,
            _ => {}
        }
    }
    bytes.len()
}

fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

fn find_ignore_case(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
        .map(|p| from + p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_normal_html_alone() {
        let html = "<p>x</p>".repeat(1000);
        assert!(matches!(limit_nesting(&html), Cow::Borrowed(_)));
    }

    #[test]
    fn cuts_deep_nesting_and_keeps_text() {
        let html = "<div>".repeat(2000) + "deep" + &"</div>".repeat(2000) + "<b>after</b>";
        let out = limit_nesting(&html);
        assert_eq!(out.matches("<div>").count(), MAX_DEPTH);
        assert_eq!(out.matches("</div>").count(), MAX_DEPTH);
        assert!(out.contains("deep"));
        assert!(out.ends_with("<b>after</b>"));
    }

    #[test]
    fn ignores_tags_in_raw_text_comments_and_attributes() {
        let html = "<style>".to_owned()
            + &"<div>".repeat(600)
            + "</style><!-- "
            + &"<div>".repeat(600)
            + " --><p title=\"<div>\">"
            + &"<br>".repeat(600);
        assert!(matches!(limit_nesting(&html), Cow::Borrowed(_)));
    }
}
