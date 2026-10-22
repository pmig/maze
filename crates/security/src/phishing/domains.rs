//! Lookalike, mixed-script and international domain checks.

use std::collections::BTreeMap;

use super::confusables::skeleton;
use super::{Severity, Warning, WarningKind};
use crate::domain::{registrable_domain, to_ascii, to_unicode};

/// Domains often impersonated in phishing mail. The watchlist protects them from
/// lookalikes; add the user's own and their contacts' domains with
/// [`DomainWatchlist::with_domains`].
const BUILTIN_DOMAINS: &[&str] = &[
    "adobe.com",
    "amazon.com",
    "amazon.de",
    "apple.com",
    "bankofamerica.com",
    "binance.com",
    "chase.com",
    "coinbase.com",
    "commerzbank.de",
    "dhl.com",
    "dhl.de",
    "docusign.com",
    "dropbox.com",
    "ebay.com",
    "facebook.com",
    "fedex.com",
    "github.com",
    "gmail.com",
    "google.com",
    "icloud.com",
    "instagram.com",
    "irs.gov",
    "linkedin.com",
    "live.com",
    "microsoft.com",
    "netflix.com",
    "office.com",
    "outlook.com",
    "paypal.com",
    "paypal.de",
    "postbank.de",
    "sparkasse.de",
    "ups.com",
    "usps.com",
    "wellsfargo.com",
    "yahoo.com",
];

/// Domains to protect from lookalikes, indexed by skeleton.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomainWatchlist {
    /// Skeleton of the registrable domain → registrable domains (ASCII).
    by_skeleton: BTreeMap<String, Vec<String>>,
}

impl DomainWatchlist {
    /// An empty watchlist.
    pub fn new() -> Self {
        Self::default()
    }

    /// The built-in list of commonly impersonated domains.
    pub fn builtin() -> Self {
        Self::new().with_domains(BUILTIN_DOMAINS)
    }

    /// Add domains (ASCII or Unicode; subdomains are reduced to their registrable
    /// domain).
    #[must_use]
    pub fn with_domains<I, S>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for domain in domains {
            self.add(domain.as_ref());
        }
        self
    }

    /// Add one domain. Invalid names are ignored.
    pub fn add(&mut self, domain: &str) {
        let Some(ascii) = to_ascii(domain) else {
            return;
        };
        let registrable = registrable_domain(&ascii).to_owned();
        let entry = self
            .by_skeleton
            .entry(skeleton(&to_unicode(&registrable)))
            .or_default();
        if !entry.contains(&registrable) {
            entry.push(registrable);
        }
    }

    /// Whether this exact registrable domain (ASCII) is on the list.
    pub fn contains(&self, registrable_ascii: &str) -> bool {
        self.by_skeleton
            .get(&skeleton(&to_unicode(registrable_ascii)))
            .is_some_and(|domains| domains.iter().any(|d| d == registrable_ascii))
    }

    /// The number of domains on the list.
    pub fn len(&self) -> usize {
        self.by_skeleton.values().map(Vec::len).sum()
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.by_skeleton.is_empty()
    }

    fn resembling(&self, registrable_ascii: &str) -> Option<&str> {
        self.by_skeleton
            .get(&skeleton(&to_unicode(registrable_ascii)))?
            .iter()
            .find(|d| *d != registrable_ascii)
            .map(String::as_str)
    }

    fn domains(&self) -> impl Iterator<Item = &str> {
        self.by_skeleton.values().flatten().map(String::as_str)
    }
}

/// Check a domain or host (from a `From` address or a link) for lookalikes,
/// deceptive subdomains, mixed scripts and international characters.
pub fn check_domain(domain: &str, watchlist: &DomainWatchlist) -> Vec<Warning> {
    let Some(ascii) = to_ascii(domain) else {
        return Vec::new();
    };
    if ascii.parse::<std::net::IpAddr>().is_ok() {
        return Vec::new();
    }
    let registrable = registrable_domain(&ascii);
    let unicode = to_unicode(&ascii);
    let mut warnings = Vec::new();

    if !watchlist.contains(registrable) {
        if let Some(target) = watchlist.resembling(registrable) {
            warnings.push(Warning {
                message: format!("{unicode} looks like {target} but is a different domain."),
                kind: WarningKind::LookalikeDomain {
                    domain: ascii.clone(),
                    resembles: target.to_owned(),
                },
                severity: Severity::Danger,
            });
        }
        if let Some(contained) = deceptive_subdomain(&ascii, registrable, watchlist) {
            warnings.push(Warning {
                message: format!(
                    "{unicode} starts with {contained} but belongs to {}.",
                    to_unicode(registrable)
                ),
                kind: WarningKind::DeceptiveSubdomain {
                    domain: ascii.clone(),
                    contains: contained.to_owned(),
                },
                severity: Severity::Danger,
            });
        }
    }

    if unicode != ascii {
        if unicode.split('.').any(mixes_scripts) {
            warnings.push(Warning {
                message: format!(
                    "{unicode} mixes letters from different alphabets, a common trick to imitate another domain."
                ),
                kind: WarningKind::MixedScriptDomain {
                    domain: ascii.clone(),
                    unicode,
                },
                severity: Severity::Danger,
            });
        } else if warnings.is_empty() {
            warnings.push(Warning {
                message: format!("{unicode} uses international characters ({ascii})."),
                kind: WarningKind::InternationalDomain {
                    domain: ascii.clone(),
                    unicode,
                },
                severity: Severity::Info,
            });
        }
    }
    warnings
}

/// A watched domain appearing as whole labels in the subdomain part of `ascii`.
fn deceptive_subdomain<'w>(
    ascii: &str,
    registrable: &str,
    watchlist: &'w DomainWatchlist,
) -> Option<&'w str> {
    let subdomain = ascii.strip_suffix(registrable)?.strip_suffix('.')?;
    let padded = format!(".{subdomain}.");
    watchlist
        .domains()
        .find(|watched| padded.contains(&format!(".{watched}.")))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    Latin,
    Greek,
    Cyrillic,
}

fn script(c: char) -> Option<Script> {
    let code = u32::from(c);
    match code {
        _ if c.is_ascii_alphabetic() => Some(Script::Latin),
        0x00C0..=0x024F | 0x1E00..=0x1EFF if c.is_alphabetic() => Some(Script::Latin),
        0x0370..=0x03FF | 0x1F00..=0x1FFF => Some(Script::Greek),
        0x0400..=0x052F | 0x1C80..=0x1C8F | 0x2DE0..=0x2DFF | 0xA640..=0xA69F => {
            Some(Script::Cyrillic)
        }
        _ => None,
    }
}

/// Whether a label has letters from more than one of Latin, Greek and Cyrillic.
/// Mixing other scripts (Latin with Han, say) is normal and not flagged.
fn mixes_scripts(label: &str) -> bool {
    let mut first = None;
    for script in label.chars().filter_map(script) {
        match first {
            None => first = Some(script),
            Some(seen) if seen != script => return true,
            Some(_) => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(warnings: &[Warning]) -> Vec<&'static str> {
        warnings.iter().map(|w| w.kind.name()).collect()
    }

    #[test]
    fn lookalikes_of_watched_domains() {
        let list = DomainWatchlist::builtin();
        let warnings = check_domain("paypa1.com", &list);
        assert_eq!(kinds(&warnings), ["lookalike_domain"]);
        assert_eq!(
            warnings[0].kind,
            WarningKind::LookalikeDomain {
                domain: "paypa1.com".into(),
                resembles: "paypal.com".into()
            }
        );
        assert_eq!(warnings[0].severity, Severity::Danger);

        // Cyrillic "а" in apple.com, as Unicode and as punycode.
        for input in ["аpple.com", "xn--pple-43d.com", "login.xn--pple-43d.com"] {
            let warnings = check_domain(input, &list);
            assert_eq!(
                kinds(&warnings),
                ["lookalike_domain", "mixed_script_domain"],
                "{input}"
            );
        }
        // All-Cyrillic lookalike: no mixing, still a lookalike.
        assert_eq!(
            kinds(&check_domain("аррӏе.com", &list)),
            ["lookalike_domain"]
        );
        assert_eq!(
            kinds(&check_domain("rnicrosoft.com", &list)),
            ["lookalike_domain"]
        );
    }

    #[test]
    fn real_domains_are_fine() {
        let list = DomainWatchlist::builtin();
        for domain in [
            "paypal.com",
            "www.paypal.com",
            "mail.google.com",
            "example.org",
            "192.0.2.1",
            "",
        ] {
            assert!(check_domain(domain, &list).is_empty(), "{domain}");
        }
    }

    #[test]
    fn deceptive_subdomains() {
        let list = DomainWatchlist::builtin();
        let warnings = check_domain("paypal.com.account-check.example", &list);
        assert_eq!(kinds(&warnings), ["deceptive_subdomain"]);
        assert!(warnings[0].message.contains("account-check.example"));
        assert!(check_domain("notpaypal.com.example", &list).is_empty());
    }

    #[test]
    fn international_domains() {
        let list = DomainWatchlist::builtin();
        let warnings = check_domain("bücher.de", &list);
        assert_eq!(kinds(&warnings), ["international_domain"]);
        assert_eq!(warnings[0].severity, Severity::Info);
        assert!(warnings[0].message.contains("xn--bcher-kva.de"));
        // Whole-script Cyrillic that resembles nothing watched.
        assert_eq!(
            kinds(&check_domain("пример.рф", &list)),
            ["international_domain"]
        );
    }

    #[test]
    fn custom_watchlist_entries() {
        let list = DomainWatchlist::new().with_domains(["mail.example-bank.de", "bad domain"]);
        assert_eq!(list.len(), 1);
        assert!(list.contains("example-bank.de"));
        assert_eq!(
            kinds(&check_domain("examp1e-bank.de", &list)),
            ["lookalike_domain"]
        );
        assert!(DomainWatchlist::new().is_empty());
    }

    #[test]
    fn script_mixing() {
        assert!(mixes_scripts("pаypal"));
        assert!(!mixes_scripts("paypal"));
        assert!(!mixes_scripts("пример"));
        assert!(!mixes_scripts("bücher"));
        assert!(!mixes_scripts("日本abc"));
        assert!(mixes_scripts("αpple"));
    }
}
