//! Domain names: IDNA normalization, registrable domains (Public Suffix List) and the
//! built-in list of free-mail (personal) domains.
//!
//! The registrable domain ("organizational domain" in DMARC terms) is the part of a
//! host name someone can register: `mail.eu.northwind.example` gives
//! `northwind.example`, `bmi.gv.at` stays `bmi.gv.at` (`gv.at` is a public suffix).
//! Maze groups contacts and mail by it (PLAN-contacts §3b).

/// Normalizes a host or domain to lowercase ASCII (IDNA/punycode), without a trailing
/// dot. Returns `None` for empty or invalid names.
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

/// The Unicode form of an ASCII domain (punycode labels decoded). Labels that don't
/// decode are kept as they are.
pub fn to_unicode(ascii: &str) -> String {
    idna::domain_to_unicode(ascii).0
}

/// The registrable domain of a host name (any case, Unicode or punycode), as
/// lowercase ASCII: `Mail.EU.Northwind.example` gives `northwind.example`,
/// `www.bücher.de` gives `xn--bcher-kva.de`.
///
/// `None` for IP addresses (also in brackets, as in `user@[192.0.2.1]`), single
/// labels (`localhost`), bare public suffixes (`co.uk`) and invalid names. Unknown
/// top-level domains follow the Public Suffix List's default rule (the last label is
/// the suffix): see [`is_known_suffix`] to tell them apart.
pub fn registrable_domain(host: &str) -> Option<String> {
    let host = host.trim();
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if unbracketed.parse::<std::net::IpAddr>().is_ok()
        || unbracketed.to_ascii_lowercase().starts_with("ipv6:")
    {
        return None;
    }
    let ascii = to_ascii(unbracketed)?;
    if ascii.contains(':') || ascii.contains('_') {
        return None;
    }
    // A dotted quad that `IpAddr` didn't take (`1.2.3`) is no domain either.
    if ascii
        .rsplit('.')
        .next()
        .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let domain = psl::domain_str(&ascii)?;
    if domain
        .split('.')
        .any(|label| label.is_empty() || label.len() > 63)
    {
        return None;
    }
    Some(domain.to_owned())
}

/// The registrable domain of an email address (the part after the last `@`); see
/// [`registrable_domain`].
pub fn address_domain(email: &str) -> Option<String> {
    let (_, host) = email.trim().rsplit_once('@')?;
    registrable_domain(host.trim_end_matches('>'))
}

/// Whether the domain's public suffix is on the Public Suffix List (`com`, `co.uk`,
/// `gv.at`), not only implied by its default rule (`example`, `local`, `internal`).
pub fn is_known_suffix(ascii_domain: &str) -> bool {
    psl::suffix(ascii_domain.as_bytes()).is_some_and(|suffix| suffix.is_known())
}

/// Free-mail providers whose registrable domain only differs by the country suffix
/// (`hotmail.de`, `yahoo.co.uk`, `gmx.at`): the label before the public suffix.
const PERSONAL_FAMILIES: &[&str] = &[
    "aol", "gmx", "hotmail", "live", "outlook", "yahoo", "yandex", "ymail",
];

/// Free-mail and consumer ISP domains (registrable domains, ASCII). Addresses there
/// belong to people, not organizations, so the Domains list doesn't show them as one.
/// Keep this sorted; the families above cover every suffix of their name.
const PERSONAL_DOMAINS: &[&str] = &[
    "126.com",
    "163.com",
    "a1.net",
    "aim.com",
    "aon.at",
    "att.net",
    "bigpond.com",
    "bluewin.ch",
    "btinternet.com",
    "chello.at",
    "comcast.net",
    "cox.net",
    "duck.com",
    "fastmail.com",
    "fastmail.fm",
    "free.fr",
    "freenet.de",
    "gmail.com",
    "googlemail.com",
    "hey.com",
    "icloud.com",
    "inbox.ru",
    "interia.pl",
    "kpnmail.nl",
    "laposte.net",
    "libero.it",
    "list.ru",
    "mac.com",
    "mail.com",
    "mail.de",
    "mail.ru",
    "mailbox.org",
    "me.com",
    "msn.com",
    "naver.com",
    "o2.pl",
    "onet.pl",
    "orange.fr",
    "pm.me",
    "posteo.de",
    "posteo.net",
    "proton.me",
    "protonmail.ch",
    "protonmail.com",
    "qq.com",
    "rambler.ru",
    "sbcglobal.net",
    "seznam.cz",
    "sky.com",
    "t-online.de",
    "tuta.com",
    "tuta.io",
    "tutanota.com",
    "tutanota.de",
    "verizon.net",
    "virgilio.it",
    "web.de",
    "wp.pl",
    "ziggo.nl",
    "zoho.com",
];

/// Whether a registrable domain (lowercase ASCII, from [`registrable_domain`]) is a
/// free-mail or consumer provider (`gmail.com`, `hotmail.de`, `web.de`, …), whose
/// addresses belong to people rather than an organization.
pub fn is_personal(registrable: &str) -> bool {
    if PERSONAL_DOMAINS.binary_search(&registrable).is_ok() {
        return true;
    }
    let Some(suffix) = psl::suffix_str(registrable) else {
        return false;
    };
    registrable
        .strip_suffix(suffix)
        .and_then(|rest| rest.strip_suffix('.'))
        .is_some_and(|label| PERSONAL_FAMILIES.contains(&label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(to_ascii("Example.COM.").as_deref(), Some("example.com"));
        assert_eq!(to_ascii("bücher.de").as_deref(), Some("xn--bcher-kva.de"));
        assert_eq!(to_ascii(""), None);
        assert_eq!(to_ascii("bad domain"), None);
        assert_eq!(to_unicode("xn--bcher-kva.de"), "bücher.de");
    }

    #[test]
    fn registrable_domains() {
        let cases = [
            ("mail.eu.northwind.example", Some("northwind.example")),
            ("northwind.example", Some("northwind.example")),
            ("Mail.Northwind.COM", Some("northwind.com")),
            ("mail.example.co.uk", Some("example.co.uk")),
            ("example.co.uk", Some("example.co.uk")),
            ("bmi.gv.at", Some("bmi.gv.at")),
            ("post.bmi.gv.at", Some("bmi.gv.at")),
            ("www.bücher.de", Some("xn--bcher-kva.de")),
            ("WWW.BÜCHER.DE", Some("xn--bcher-kva.de")),
            ("xn--bcher-kva.de", Some("xn--bcher-kva.de")),
            ("example.com.", Some("example.com")),
            ("co.uk", None),
            ("gv.at", None),
            ("com", None),
            ("localhost", None),
            ("192.0.2.1", None),
            ("[192.0.2.1]", None),
            ("[IPv6:2001:db8::1]", None),
            ("2001:db8::1", None),
            ("1.2.3", None),
            ("", None),
            ("bad domain.com", None),
            ("a..b.com", None),
        ];
        for (host, expected) in cases {
            assert_eq!(registrable_domain(host).as_deref(), expected, "{host}");
        }
    }

    #[test]
    fn address_domains() {
        assert_eq!(
            address_domain("Anna@Mail.Northwind.example").as_deref(),
            Some("northwind.example")
        );
        assert_eq!(
            address_domain("\"a@b\"@shop.example.co.uk").as_deref(),
            Some("example.co.uk")
        );
        assert_eq!(address_domain("root@[192.0.2.1]"), None);
        assert_eq!(address_domain("no-at-sign"), None);
        assert_eq!(address_domain("anna@"), None);
    }

    #[test]
    fn known_suffixes() {
        assert!(is_known_suffix("northwind.com"));
        assert!(is_known_suffix("example.co.uk"));
        assert!(is_known_suffix("bmi.gv.at"));
        assert!(!is_known_suffix("northwind.example"));
        assert!(!is_known_suffix("printer.local"));
        assert!(!is_known_suffix("corp.internal"));
    }

    #[test]
    fn personal_domains() {
        for domain in [
            "gmail.com",
            "googlemail.com",
            "outlook.com",
            "outlook.de",
            "hotmail.com",
            "hotmail.co.uk",
            "live.com",
            "icloud.com",
            "me.com",
            "yahoo.com",
            "yahoo.co.jp",
            "gmx.at",
            "gmx.net",
            "web.de",
            "proton.me",
            "protonmail.com",
            "aol.com",
        ] {
            assert!(is_personal(domain), "{domain}");
        }
        for domain in [
            "northwind.example",
            "google.com",
            "microsoft.com",
            "gmail.example.com",
            "example.co.uk",
            "bmi.gv.at",
        ] {
            assert!(!is_personal(domain), "{domain}");
        }
    }

    #[test]
    fn personal_list_is_sorted_and_registrable() {
        assert!(PERSONAL_DOMAINS.windows(2).all(|w| w[0] < w[1]));
        for domain in PERSONAL_DOMAINS {
            assert_eq!(registrable_domain(domain).as_deref(), Some(*domain));
        }
    }
}
