//! Domain name helpers: IDNA normalization, registrable domains, alignment.

/// Normalize a host or domain to lowercase ASCII (IDNA/punycode), without a
/// trailing dot. Returns `None` for empty or invalid names.
pub fn to_ascii(domain: &str) -> Option<String> {
    let trimmed = domain.trim().trim_end_matches('.');
    if trimmed.is_empty() {
        return None;
    }
    let ascii = idna::domain_to_ascii(trimmed).ok()?;
    let valid = !ascii.is_empty()
        && !ascii.starts_with('.')
        && !ascii.contains("..")
        && ascii
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b':'));
    valid.then_some(ascii)
}

/// The Unicode form of an ASCII domain (punycode labels decoded). Labels that
/// don't decode are kept as they are.
pub fn to_unicode(ascii: &str) -> String {
    idna::domain_to_unicode(ascii).0
}

/// The registrable domain ("organizational domain" in DMARC terms) of a lowercase
/// ASCII host, using the Public Suffix List: `mail.example.co.uk` gives
/// `example.co.uk`. Hosts without a registrable part (IP addresses, single labels,
/// bare public suffixes) are returned unchanged.
pub fn registrable_domain(ascii_host: &str) -> &str {
    if ascii_host.parse::<std::net::IpAddr>().is_ok() {
        return ascii_host;
    }
    psl::domain_str(ascii_host).unwrap_or(ascii_host)
}

/// DMARC relaxed alignment: both domains have the same registrable domain.
pub(crate) fn relaxed_aligned(a: &str, b: &str) -> bool {
    match (to_ascii(a), to_ascii(b)) {
        (Some(a), Some(b)) => registrable_domain(&a) == registrable_domain(&b),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(to_ascii("Example.COM.").as_deref(), Some("example.com"));
        assert_eq!(to_ascii("bücher.de").as_deref(), Some("xn--bcher-kva.de"));
        assert_eq!(to_ascii("аpple.com").as_deref(), Some("xn--pple-43d.com"));
        assert_eq!(to_ascii(""), None);
        assert_eq!(to_ascii(" . "), None);
        assert_eq!(to_ascii("bad domain"), None);
        assert_eq!(to_ascii("a..b"), None);
        assert_eq!(to_ascii("<script>"), None);
        assert_eq!(to_unicode("xn--bcher-kva.de"), "bücher.de");
    }

    #[test]
    fn registrable_domains() {
        assert_eq!(registrable_domain("mail.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("a.b.example.com"), "example.com");
        assert_eq!(registrable_domain("example.com"), "example.com");
        assert_eq!(registrable_domain("co.uk"), "co.uk");
        assert_eq!(registrable_domain("192.0.2.1"), "192.0.2.1");
    }

    #[test]
    fn alignment() {
        assert!(relaxed_aligned("mail.example.com", "Example.com"));
        assert!(!relaxed_aligned("example.com", "example.net"));
        assert!(!relaxed_aligned("a.co.uk", "b.co.uk"));
        assert!(!relaxed_aligned("", "example.com"));
    }
}
