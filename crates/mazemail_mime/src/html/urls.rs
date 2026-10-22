//! URL policy shared by the sanitizer, the resource rewrite and the reader model.

use url::Url;

/// What a resource URL (`img src`, `background`, CSS `url()`) points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resource {
    /// A MIME part of the same message (`cid:`), with the decoded Content-ID.
    Cid(String),
    /// A remote `http(s)` URL, normalized.
    Remote(String),
    /// An inline `data:image/...` URL. Kept as is.
    DataImage,
    /// Anything else: dropped.
    Rejected,
}

/// Image types allowed in `data:` URLs. SVG is left out on purpose: it is a document
/// format, and the reader model would have to sandbox it.
const DATA_IMAGE_TYPES: &[&str] = &[
    "image/png",
    "image/gif",
    "image/jpeg",
    "image/jpg",
    "image/webp",
    "image/bmp",
    "image/avif",
    "image/x-icon",
    "image/vnd.microsoft.icon",
];

pub(crate) fn classify_resource(value: &str) -> Resource {
    let value = value.trim_matches(|c: char| c.is_whitespace() || c.is_control());
    if let Some(id) = strip_prefix_ignore_case(value, "cid:") {
        let id = percent_decode(id.trim());
        let id = id.trim_start_matches('<').trim_end_matches('>');
        return if id.is_empty() {
            Resource::Rejected
        } else {
            Resource::Cid(id.to_owned())
        };
    }
    if let Some(rest) = strip_prefix_ignore_case(value, "data:") {
        let mime = rest
            .split([';', ','])
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        return if DATA_IMAGE_TYPES.contains(&mime.as_str()) {
            Resource::DataImage
        } else {
            Resource::Rejected
        };
    }
    match Url::parse(value) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.host().is_some() => {
            Resource::Remote(url.into())
        }
        _ => Resource::Rejected,
    }
}

/// Whether a link target (`a href`) may stay in the document.
pub(crate) fn is_allowed_link(value: &str) -> bool {
    let value = value.trim();
    if value.starts_with('#') {
        return true;
    }
    match Url::parse(value) {
        Ok(url) => matches!(url.scheme(), "http" | "https" | "mailto" | "tel"),
        Err(_) => false,
    }
}

/// Percent-encode a value for use as one URL path segment.
pub(crate) fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'@') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0xf)]));
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn percent_decode(value: &str) -> String {
    if !value.contains('%') {
        return value.to_owned();
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|&b| hex_value(b)),
                bytes.get(i + 2).and_then(|&b| hex_value(b)),
            )
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(b: u8) -> Option<u8> {
    char::from(b)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

pub(crate) fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &value[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_resources() {
        assert_eq!(
            classify_resource(" cid:logo%40example.com "),
            Resource::Cid("logo@example.com".into())
        );
        assert_eq!(classify_resource("CID:<x>"), Resource::Cid("x".into()));
        assert_eq!(classify_resource("cid:"), Resource::Rejected);
        assert_eq!(
            classify_resource("https://example.com/a b.png"),
            Resource::Remote("https://example.com/a%20b.png".into())
        );
        assert_eq!(
            classify_resource("data:image/png;base64,AAAA"),
            Resource::DataImage
        );
        assert_eq!(
            classify_resource("data:image/svg+xml,<svg/>"),
            Resource::Rejected
        );
        assert_eq!(classify_resource("data:text/html,x"), Resource::Rejected);
        assert_eq!(classify_resource("javascript:alert(1)"), Resource::Rejected);
        assert_eq!(
            classify_resource("java\tscript:alert(1)"),
            Resource::Rejected
        );
        assert_eq!(classify_resource("images/logo.png"), Resource::Rejected);
        assert_eq!(classify_resource("ftp://example.com/x"), Resource::Rejected);
    }

    #[test]
    fn link_policy() {
        assert!(is_allowed_link("https://example.com"));
        assert!(is_allowed_link("mailto:anna@example.com"));
        assert!(is_allowed_link("#section"));
        assert!(!is_allowed_link("javascript:alert(1)"));
        assert!(!is_allowed_link("data:text/html,x"));
        assert!(!is_allowed_link("relative/page.html"));
        assert!(!is_allowed_link("vbscript:x"));
    }

    #[test]
    fn encodes_path_segments() {
        assert_eq!(encode_path_segment("a.b@c"), "a.b@c");
        assert_eq!(encode_path_segment("</x y>"), "%3C%2Fx%20y%3E");
        assert_eq!(encode_path_segment("ü"), "%C3%BC");
    }
}
