//! Phishing heuristics (CONCEPT §15, PLAN §4.7).
//!
//! Pure functions over data the caller already has: the `From` address, the links
//! of the reader document, the [`AuthReport`], known contacts. Each returns
//! [`Warning`]s with an English message for the banner. [`analyze`] runs them all.
//!
//! - [`check_links`]: link text that shows one domain while the link opens another.
//! - [`check_domain`]: lookalike domains (TR39-style [skeletons](skeleton) compared
//!   against a [`DomainWatchlist`]), mixed-script labels, international (punycode)
//!   domains, and known domains used as a subdomain of something else.
//! - [`check_display_name`]: a known contact's name with an unknown address, or a
//!   display name that is itself a different address.
//! - [`check_auth`]: a DMARC failure.

mod confusables;
mod contacts;
mod domains;
mod links;

use std::fmt;

use mazemail_types::Address;
use serde::{Deserialize, Serialize};

pub use confusables::skeleton;
pub use contacts::{ContactIndex, KnownContacts, check_display_name, normalize_display_name};
pub use domains::{DomainWatchlist, check_domain};
pub use links::{Link, check_links, links_from_reader_doc};

use crate::report::{AuthReport, MechanismResult};

/// How serious a warning is. Ordered from least to most serious.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Worth knowing, no action needed (an international domain).
    Info,
    /// Look twice before clicking or replying.
    Caution,
    /// Likely phishing.
    Danger,
}

/// What a warning is about. Domains are lowercase; `*_unicode` fields hold the
/// decoded form of punycode names.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WarningKind {
    /// The link text shows `text_domain`, the link opens `href_domain`.
    LinkTextMismatch {
        text_domain: String,
        href_domain: String,
    },
    /// `domain` looks like the watched domain `resembles`.
    LookalikeDomain { domain: String, resembles: String },
    /// `domain` contains the watched domain `contains` as a subdomain of something
    /// else (`paypal.com.account-check.example`).
    DeceptiveSubdomain { domain: String, contains: String },
    /// A label of `domain` mixes Latin, Greek or Cyrillic letters.
    MixedScriptDomain { domain: String, unicode: String },
    /// `domain` uses international characters (punycode `xn--`).
    InternationalDomain { domain: String, unicode: String },
    /// The sender uses a known contact's name, but not one of their addresses.
    DisplayNameSpoofing {
        display_name: String,
        address: String,
        known_addresses: Vec<String>,
    },
    /// The display name is an address, and not the sender's.
    DisplayNameAddressMismatch {
        display_name: String,
        address: String,
    },
    /// The message failed its `From` domain's DMARC check.
    DmarcFailure { domain: String },
}

impl WarningKind {
    /// The variant name, for logs (the fields are message content).
    pub fn name(&self) -> &'static str {
        match self {
            Self::LinkTextMismatch { .. } => "link_text_mismatch",
            Self::LookalikeDomain { .. } => "lookalike_domain",
            Self::DeceptiveSubdomain { .. } => "deceptive_subdomain",
            Self::MixedScriptDomain { .. } => "mixed_script_domain",
            Self::InternationalDomain { .. } => "international_domain",
            Self::DisplayNameSpoofing { .. } => "display_name_spoofing",
            Self::DisplayNameAddressMismatch { .. } => "display_name_address_mismatch",
            Self::DmarcFailure { .. } => "dmarc_failure",
        }
    }
}

impl fmt::Debug for WarningKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}(<redacted>)", self.name())
    }
}

/// One phishing warning for the message banner.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Warning {
    pub kind: WarningKind,
    pub severity: Severity,
    /// One English sentence for the banner.
    pub message: String,
}

impl fmt::Debug for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Warning")
            .field("kind", &self.kind)
            .field("severity", &self.severity)
            .finish_non_exhaustive()
    }
}

/// A banner warning when the message failed DMARC.
pub fn check_auth(report: &AuthReport) -> Vec<Warning> {
    if !matches!(report.dmarc.result, MechanismResult::Fail { .. }) {
        return Vec::new();
    }
    let domain = report
        .dmarc
        .domain
        .clone()
        .or_else(|| report.from_domain.clone())
        .unwrap_or_default();
    vec![Warning {
        message: format!(
            "This message claims to be from {domain}, but it failed {domain}'s sender check. It may be forged."
        ),
        kind: WarningKind::DmarcFailure { domain },
        severity: Severity::Danger,
    }]
}

/// Everything [`analyze`] looks at.
#[derive(Clone, Copy)]
pub struct PhishingInput<'a> {
    /// The `From` address.
    pub from: Option<&'a Address>,
    /// The message's links, see [`links_from_reader_doc`].
    pub links: &'a [Link],
    /// The verification result, if already computed.
    pub auth: Option<&'a AuthReport>,
    /// Known contacts, for display-name spoofing.
    pub contacts: Option<&'a dyn KnownContacts>,
    /// Domains to protect from lookalikes.
    pub watchlist: &'a DomainWatchlist,
}

impl fmt::Debug for PhishingInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhishingInput")
            .field("links", &self.links.len())
            .field("auth", &self.auth.map(|a| a.verdict))
            .field("contacts", &self.contacts.is_some())
            .finish_non_exhaustive()
    }
}

/// Run every heuristic. Warnings come most serious first, without duplicates.
pub fn analyze(input: &PhishingInput<'_>) -> Vec<Warning> {
    let mut warnings = Vec::new();
    if let Some(report) = input.auth {
        warnings.extend(check_auth(report));
    }
    if let Some(from) = input.from {
        warnings.extend(check_domain(&from.domain(), input.watchlist));
        warnings.extend(check_display_name(from, input.contacts));
    }
    warnings.extend(check_links(input.links));
    for host in links::link_hosts(input.links) {
        warnings.extend(check_domain(&host, input.watchlist));
    }

    let mut seen = std::collections::HashSet::new();
    warnings.retain(|w| seen.insert(w.kind.clone()));
    warnings.sort_by_key(|w| std::cmp::Reverse(w.severity));
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_content() {
        let warning = Warning {
            kind: WarningKind::DmarcFailure {
                domain: "secret.example".into(),
            },
            severity: Severity::Danger,
            message: "secret.example".into(),
        };
        let debug = format!("{warning:?}");
        assert!(!debug.contains("secret"));
        assert!(debug.contains("dmarc_failure"));
    }

    #[test]
    fn json_shape() {
        let warning = Warning {
            kind: WarningKind::LookalikeDomain {
                domain: "paypa1.com".into(),
                resembles: "paypal.com".into(),
            },
            severity: Severity::Danger,
            message: "m".into(),
        };
        let json = serde_json::to_value(&warning).unwrap();
        assert_eq!(json["kind"]["kind"], "lookalike_domain");
        assert_eq!(json["kind"]["resembles"], "paypal.com");
        assert_eq!(json["severity"], "danger");
    }
}
