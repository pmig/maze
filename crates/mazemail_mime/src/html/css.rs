//! Minimal CSS rewriting: `url(...)` references and a few dangerous constructs.
//!
//! This is not a CSS parser. It finds `url(` tokens (case-insensitive), hands the
//! referenced URL to a callback and replaces the token with what the callback returns.
//! The Content-Security-Policy is the real boundary for anything this misses (escaped
//! identifiers, `image-set("…")` strings): it only lets images load from the resource
//! origin.

use std::borrow::Cow;

/// Rewrite every `url(...)` in `css`.
///
/// `map` receives the unquoted, trimmed URL and returns the replacement URL, or `None`
/// to blank the reference (`url("")`, which loads nothing). The output also has
/// `@import` rules removed and legacy script hooks (`expression(`, `behavior:`,
/// `-moz-binding:`) neutralized.
pub(crate) fn rewrite_css(css: &str, mut map: impl FnMut(&str) -> Option<String>) -> Cow<'_, str> {
    if !needs_work(css) {
        return Cow::Borrowed(css);
    }
    let bytes = css.as_bytes();
    let mut out = String::with_capacity(css.len() + 16);
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if starts_with_ignore_case(bytes, i, b"url(") && !is_ident_char_before(bytes, i) {
            let (url, end) = read_url_argument(css, i + 4);
            out.push_str(&css[copied..i]);
            out.push_str("url(\"");
            if let Some(new) = map(&url) {
                push_css_string_content(&mut out, &new);
            }
            out.push_str("\")");
            i = end;
            copied = end;
            continue;
        }
        if starts_with_ignore_case(bytes, i, b"@import") {
            // Drop the whole rule; the CSP would block the load anyway.
            out.push_str(&css[copied..i]);
            let end = bytes[i..]
                .iter()
                .position(|&b| b == b';' || b == b'}')
                .map_or(bytes.len(), |p| i + p + usize::from(bytes[i + p] == b';'));
            i = end;
            copied = end;
            continue;
        }
        if let Some((pattern, replacement)) = DANGEROUS
            .iter()
            .find(|(pattern, _)| starts_with_ignore_case(bytes, i, pattern.as_bytes()))
        {
            out.push_str(&css[copied..i]);
            out.push_str(replacement);
            i += pattern.len();
            copied = i;
            continue;
        }
        i += 1;
    }
    out.push_str(&css[copied..]);
    Cow::Owned(out)
}

/// Patterns that are renamed into invalid tokens, which browsers then drop.
const DANGEROUS: &[(&str, &str)] = &[
    ("expression(", "maze-blocked("),
    ("behavior:", "maze-blocked:"),
    ("-moz-binding:", "maze-blocked:"),
];

fn needs_work(css: &str) -> bool {
    let lower = css.to_ascii_lowercase();
    lower.contains("url(") || DANGEROUS.iter().any(|(pattern, _)| lower.contains(pattern))
}

fn starts_with_ignore_case(haystack: &[u8], at: usize, needle: &[u8]) -> bool {
    haystack
        .get(at..at + needle.len())
        .is_some_and(|window| window.eq_ignore_ascii_case(needle))
}

/// `myurl(` is not a `url(` token.
fn is_ident_char_before(bytes: &[u8], at: usize) -> bool {
    at > 0
        && bytes
            .get(at - 1)
            .is_some_and(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Read the argument of `url(` starting right after the parenthesis.
/// Returns the unquoted URL and the index just past the closing `)`.
fn read_url_argument(css: &str, start: usize) -> (String, usize) {
    let bytes = css.as_bytes();
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut url = String::new();
    match bytes.get(i) {
        Some(&quote @ (b'"' | b'\'')) => {
            i += 1;
            let begin = i;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            url.push_str(css.get(begin..i.min(bytes.len())).unwrap_or_default());
            // Skip the closing quote and anything up to `)`.
            while i < bytes.len() && bytes[i] != b')' {
                i += 1;
            }
        }
        _ => {
            let begin = i;
            while i < bytes.len() && bytes[i] != b')' {
                i += 1;
            }
            url.push_str(css.get(begin..i).unwrap_or_default().trim_end());
        }
    }
    // Past the `)`, or the end of input for an unterminated token.
    let end = (i + 1).min(bytes.len());
    (unescape_css(&url), end)
}

/// Resolve the simple escapes that appear in URLs (`\"`, `\)`, `\\`).
fn unescape_css(value: &str) -> String {
    if !value.contains('\\') {
        return value.trim().to_owned();
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_owned()
}

/// Escape a value for a double-quoted CSS string.
fn push_css_string_content(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            // Newlines and other controls end or corrupt a CSS string; drop them.
            c if c.is_control() => {}
            // `<` can't end a <style> element inside a CSS string, but never emit `</`.
            '<' => out.push_str("\\3c "),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upper(css: &str) -> String {
        rewrite_css(css, |u| Some(u.to_uppercase())).into_owned()
    }

    #[test]
    fn leaves_plain_css_untouched() {
        assert!(matches!(
            rewrite_css("color: red; width: 10px", |_| None),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn rewrites_all_url_forms() {
        assert_eq!(upper("background:url(a.png)"), "background:url(\"A.PNG\")");
        assert_eq!(
            upper("background:URL( 'b.png' ) x"),
            "background:url(\"B.PNG\") x"
        );
        assert_eq!(upper("a{b:url(\"c\\\")d\")}"), "a{b:url(\"C\\\")D\")}");
        assert_eq!(upper("x:myurl(a)"), "x:myurl(a)");
    }

    #[test]
    fn blanks_rejected_urls_and_unterminated_tokens() {
        assert_eq!(
            rewrite_css("background:url(javascript:alert(1))", |_| None),
            "background:url(\"\"))"
        );
        assert_eq!(rewrite_css("x:url('abc", |_| None), "x:url(\"\")");
    }

    #[test]
    fn neutralizes_imports_and_script_hooks() {
        let css = "@import 'x.css'; a { width: expression(alert(1)); behavior: url(x.htc) }";
        assert_eq!(
            rewrite_css(css, |_| None),
            " a { width: maze-blocked(alert(1)); maze-blocked: url(\"\") }"
        );
    }

    #[test]
    fn escapes_replacement_values() {
        assert_eq!(
            rewrite_css("x:url(a)", |_| Some("</style>\"\n".into())),
            "x:url(\"\\3c /style>\\\"\")"
        );
    }
}
