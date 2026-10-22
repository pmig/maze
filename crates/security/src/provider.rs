//! Trusted `Authentication-Results` from the account's receiving server.
//!
//! Anyone can add an `Authentication-Results` header to a message before sending
//! it, so Maze trusts exactly one header and only when its position and its
//! `authserv-id` both show that the account's provider wrote it (RFC 8601 §5, §7.2;
//! PLAN §4.7).
//!
//! # The trust rule
//!
//! A [`ProviderProfile`] describes the provider: which `Received` headers it writes
//! and which `authserv-id` it uses. For a message, [`ProviderProfile::evaluate`]
//! does this:
//!
//! 1. **Find the provider's hops.** Walk the `Received` headers from the top. The
//!    provider's own hops come first, because it added them last. The walk stops at
//!    the first `Received` header whose `by` host isn't one of the provider's hosts
//!    ([`ProviderProfile::inbound_hosts`] or [`ProviderProfile::internal_hosts`]).
//! 2. **Find the boundary.** The boundary is the lowest header in that run that is
//!    written by an inbound host: the hop at which the provider accepted the message
//!    from the outside. Everything below it was written by someone else. Without a
//!    boundary nothing is trusted.
//! 3. **Collect candidates.** Candidates are the `Authentication-Results` headers
//!    above the boundary. Gmail writes its header just *below* its own `Received`
//!    header (it prepends `Received` last), so profiles with
//!    [`ResultsPlacement::AboveOrDirectlyBelow`] also look at the headers between the
//!    boundary and the next `Received` header.
//! 4. **Match the authserv-id.** A candidate counts only if its `authserv-id`
//!    equals [`ProviderProfile::authserv_id`] (ASCII case-insensitive, as for any
//!    host name). There is no pattern matching of any kind, so ids like
//!    `*.google.com` or `mx.google.com.evil.example` never match.
//! 5. **Exactly one.** If exactly one candidate matches, and it parses, it is
//!    trusted. Two or more matches, or a matching header that doesn't parse, make
//!    the result [`ProviderStatus::Ambiguous`] and nothing is trusted.
//!
//! The "exactly one" rule is what makes step 3 safe for Gmail: a forged header
//! that a sending server put on top of its own header block lands in the same
//! zone as the real one, and two matches fail closed. Forged `Received` headers
//! that imitate the provider below a foreign hop are never reached by step 1.
//!
//! Hosts are matched as exact names or as whole-label domain suffixes
//! ([`HostMatcher`]); that applies only to `Received` hosts, never to the
//! `authserv-id`.
//!
//! # Microsoft 365
//!
//! Exchange Online writes its header without any `authserv-id`
//! (`Authentication-Results: spf=pass (sender IP is …) smtp.mailfrom=…`). By default
//! the Microsoft 365 profile therefore expects [`AuthservId::Absent`] and accepts
//! only headers that have no id at all, positioned above the boundary
//! `Received … by *.mail.protection.outlook.com`. Tenants whose headers carry an id
//! (hybrid setups, third-party gateways) set it with
//! [`ProviderProfile::with_authserv_id`], and then only that exact id is accepted.

use std::fmt;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::auth_results::{self, AuthenticationResults};
use crate::headers::{self, RawHeader};

/// The kind of provider. Selects the result normalizer (each provider phrases a few
/// results its own way).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Gmail and Google Workspace.
    Gmail,
    /// Microsoft 365 / Exchange Online.
    Microsoft365,
    /// Any other host, fully configured by the user.
    Generic,
}

/// What `authserv-id` the provider's header must carry.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum AuthservId {
    /// The id must equal this value (ASCII case-insensitive).
    Exact(String),
    /// The header must have no id at all (Exchange Online).
    Absent,
}

impl AuthservId {
    fn matches(&self, id: Option<&str>) -> bool {
        match (self, id) {
            (Self::Exact(expected), Some(id)) => {
                !id.is_empty() && expected.eq_ignore_ascii_case(id)
            }
            (Self::Absent, None) => true,
            _ => false,
        }
    }
}

impl fmt::Debug for AuthservId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Provider host names aren't personal data.
            Self::Exact(id) => f.debug_tuple("Exact").field(id).finish(),
            Self::Absent => f.write_str("Absent"),
        }
    }
}

/// Matches the `by` host of a `Received` header.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum HostMatcher {
    /// The host equals this name (ASCII case-insensitive, trailing dot ignored).
    Exact(String),
    /// The host equals this domain or is a subdomain of it, on a label boundary:
    /// `outlook.com` matches `x.outlook.com` but not `evil-outlook.com`.
    DomainSuffix(String),
    /// The host is an IP address literal. Gmail's internal hops look like
    /// `by 2002:a05:6504:… with SMTP id …`.
    IpLiteral,
}

impl HostMatcher {
    /// Whether `host` (lowercase, no trailing dot) matches.
    pub fn matches(&self, host: &str) -> bool {
        match self {
            Self::Exact(name) => normalize_host(name) == host,
            Self::DomainSuffix(domain) => {
                let domain = normalize_host(domain);
                !domain.is_empty()
                    && (host == domain
                        || host
                            .strip_suffix(domain.as_str())
                            .is_some_and(|prefix| prefix.ends_with('.')))
            }
            Self::IpLiteral => host.parse::<IpAddr>().is_ok(),
        }
    }
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Where the provider writes its `Authentication-Results` relative to the
/// boundary `Received` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultsPlacement {
    /// Above the boundary only (Exchange Online, most milter setups).
    Above,
    /// Above the boundary, or in the headers directly below it up to the next
    /// `Received` header (Gmail).
    AboveOrDirectlyBelow,
}

/// How to recognize the account provider's own `Authentication-Results`.
///
/// Stored with the account settings; see the [module docs](self) for the rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// Selects the result normalizer.
    pub kind: ProviderKind,
    /// The required `authserv-id`.
    pub authserv_id: AuthservId,
    /// Hosts that accept mail from the outside (the boundary hop).
    pub inbound_hosts: Vec<HostMatcher>,
    /// Further hosts inside the provider, above the boundary.
    pub internal_hosts: Vec<HostMatcher>,
    /// Where the provider puts its header.
    pub placement: ResultsPlacement,
}

impl ProviderProfile {
    /// Gmail and Google Workspace: `authserv-id` `mx.google.com`, boundary
    /// `Received … by mx.google.com`, internal hops by IP literal or under
    /// `google.com`.
    pub fn gmail() -> Self {
        Self {
            kind: ProviderKind::Gmail,
            authserv_id: AuthservId::Exact("mx.google.com".to_owned()),
            inbound_hosts: vec![HostMatcher::Exact("mx.google.com".to_owned())],
            internal_hosts: vec![
                HostMatcher::IpLiteral,
                HostMatcher::DomainSuffix("google.com".to_owned()),
            ],
            placement: ResultsPlacement::AboveOrDirectlyBelow,
        }
    }

    /// Microsoft 365 / Exchange Online with the default detection: no
    /// `authserv-id` (see the [module docs](self#microsoft-365)), boundary
    /// `Received … by *.mail.protection.outlook.com`, internal hops under
    /// `outlook.com` and `office365.com`.
    pub fn microsoft365() -> Self {
        Self {
            kind: ProviderKind::Microsoft365,
            authserv_id: AuthservId::Absent,
            inbound_hosts: vec![HostMatcher::DomainSuffix(
                "mail.protection.outlook.com".to_owned(),
            )],
            internal_hosts: vec![
                HostMatcher::DomainSuffix("outlook.com".to_owned()),
                HostMatcher::DomainSuffix("office365.com".to_owned()),
                HostMatcher::IpLiteral,
            ],
            placement: ResultsPlacement::Above,
        }
    }

    /// Any other host: `authserv_id` is required exactly, and `inbound_hosts` are
    /// the servers that accept mail for the account (usually the MX hosts).
    pub fn generic(authserv_id: impl Into<String>, inbound_hosts: Vec<HostMatcher>) -> Self {
        Self {
            kind: ProviderKind::Generic,
            authserv_id: AuthservId::Exact(authserv_id.into()),
            inbound_hosts,
            internal_hosts: Vec::new(),
            placement: ResultsPlacement::Above,
        }
    }

    /// Require this exact `authserv-id` (a Microsoft 365 tenant's value, say).
    #[must_use]
    pub fn with_authserv_id(mut self, authserv_id: impl Into<String>) -> Self {
        self.authserv_id = AuthservId::Exact(authserv_id.into());
        self
    }

    /// Add hosts inside the provider that may appear above the boundary.
    #[must_use]
    pub fn with_internal_hosts(mut self, hosts: impl IntoIterator<Item = HostMatcher>) -> Self {
        self.internal_hosts.extend(hosts);
        self
    }

    /// Set where the provider writes its header.
    #[must_use]
    pub fn with_placement(mut self, placement: ResultsPlacement) -> Self {
        self.placement = placement;
        self
    }

    /// Find the provider's trusted header in a raw message.
    pub fn evaluate(&self, raw_message: &[u8]) -> ProviderOutcome {
        self.evaluate_headers(&headers::scan(raw_message))
    }

    pub(crate) fn evaluate_headers(&self, headers: &[RawHeader<'_>]) -> ProviderOutcome {
        if matches!(&self.authserv_id, AuthservId::Exact(id) if id.trim().is_empty())
            || self.inbound_hosts.is_empty()
        {
            return ProviderOutcome::status(ProviderStatus::NotConfigured);
        }
        let Some(zone_end) = self.trusted_zone_end(headers) else {
            return ProviderOutcome::status(ProviderStatus::NotFound);
        };

        let mut matched = None;
        for header in headers[..zone_end]
            .iter()
            .filter(|h| h.is("Authentication-Results"))
        {
            let parsed = auth_results::parse(&header.value);
            let id = match &parsed {
                Ok(parsed) => parsed.authserv_id.clone(),
                // Unparseable: recover the id the way a reader would, so a broken
                // header claiming the provider's id makes the result ambiguous.
                Err(_) => leading_id(&header.value),
            };
            if !self.authserv_id.matches(id.as_deref()) {
                continue;
            }
            match (parsed, matched.is_some()) {
                (Ok(parsed), false) => matched = Some(parsed),
                _ => return ProviderOutcome::status(ProviderStatus::Ambiguous),
            }
        }

        match matched {
            Some(header) => ProviderOutcome {
                status: ProviderStatus::Trusted {
                    authserv_id: header.authserv_id.clone(),
                },
                header: Some(header),
                kind: self.kind,
            },
            None => ProviderOutcome::status(ProviderStatus::NotFound),
        }
    }

    /// The index one past the last header that may hold the provider's result, or
    /// `None` when there's no boundary.
    fn trusted_zone_end(&self, headers: &[RawHeader<'_>]) -> Option<usize> {
        let mut boundary = None;
        for (index, header) in headers.iter().enumerate() {
            if !header.is("Received") {
                continue;
            }
            let Some(host) = headers::received_by_host(&header.value) else {
                break;
            };
            if self.inbound_hosts.iter().any(|m| m.matches(&host)) {
                boundary = Some(index);
            } else if !self.internal_hosts.iter().any(|m| m.matches(&host)) {
                break;
            }
        }
        let boundary = boundary?;
        Some(match self.placement {
            ResultsPlacement::Above => boundary,
            ResultsPlacement::AboveOrDirectlyBelow => headers[boundary + 1..]
                .iter()
                .position(|h| h.is("Received"))
                .map_or(headers.len(), |offset| boundary + 1 + offset),
        })
    }
}

/// The first word of a header value, if it isn't a `method=result`: what the
/// `authserv-id` would be if the header parsed.
fn leading_id(value: &str) -> Option<String> {
    let stripped = headers::strip_comments(value);
    let word = stripped
        .split(|c: char| c.is_whitespace() || c == ';')
        .find(|w| !w.is_empty())?;
    (!word.contains('=')).then(|| word.trim_matches('"').to_owned())
}

/// What [`ProviderProfile::evaluate`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProviderStatus {
    /// No provider profile for the account, or an incomplete one.
    NotConfigured,
    /// No trusted header: no boundary, or no header with the right id in the zone.
    NotFound,
    /// Several headers (or an unparseable one) claim the provider's id; all ignored.
    Ambiguous,
    /// Exactly one trusted header.
    Trusted {
        /// The header's `authserv-id`; `None` for Exchange Online's format.
        authserv_id: Option<String>,
    },
}

/// The outcome of looking for the provider's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderOutcome {
    /// Whether a header was trusted, and if not, why.
    pub status: ProviderStatus,
    /// The trusted header, parsed.
    pub header: Option<AuthenticationResults>,
    /// The provider kind, for normalizing results.
    pub kind: ProviderKind,
}

impl ProviderOutcome {
    fn status(status: ProviderStatus) -> Self {
        Self {
            status,
            header: None,
            kind: ProviderKind::Generic,
        }
    }

    /// The trusted results, normalized. Empty when nothing is trusted.
    pub fn results(&self) -> crate::normalize::ProviderResults {
        self.header
            .as_ref()
            .map(|h| crate::normalize::normalize(self.kind, h))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_matchers() {
        let exact = HostMatcher::Exact("MX.Google.com.".into());
        assert!(exact.matches("mx.google.com"));
        assert!(!exact.matches("mx.google.com.evil.example"));
        assert!(!exact.matches("xmx.google.com"));

        let suffix = HostMatcher::DomainSuffix("outlook.com".into());
        assert!(suffix.matches("outlook.com"));
        assert!(suffix.matches("am4pepf00027a5f.eurprd04.prod.outlook.com"));
        assert!(!suffix.matches("evil-outlook.com"));
        assert!(!suffix.matches("outlook.com.evil.example"));
        assert!(!HostMatcher::DomainSuffix(String::new()).matches("x"));

        assert!(HostMatcher::IpLiteral.matches("2002:a05:6504:1:b0:2:3:4"));
        assert!(HostMatcher::IpLiteral.matches("10.0.0.1"));
        assert!(!HostMatcher::IpLiteral.matches("mx.google.com"));
    }

    #[test]
    fn authserv_id_matching_is_exact() {
        let id = AuthservId::Exact("mx.google.com".into());
        assert!(id.matches(Some("mx.google.com")));
        assert!(id.matches(Some("MX.GOOGLE.COM")));
        for other in [
            "*.google.com",
            "google.com",
            "mx.google.com.",
            "mx.google.co",
            "",
        ] {
            assert!(!id.matches(Some(other)), "{other}");
        }
        assert!(!id.matches(None));
        assert!(AuthservId::Absent.matches(None));
        assert!(!AuthservId::Absent.matches(Some("mx.google.com")));
    }

    #[test]
    fn leading_id_recovers_broken_headers() {
        assert_eq!(
            leading_id("mx.google.com; dkim").as_deref(),
            Some("mx.google.com")
        );
        assert_eq!(
            leading_id("(c) \"mx.google.com\" x").as_deref(),
            Some("mx.google.com")
        );
        assert_eq!(leading_id("spf=pass x y"), None);
        assert_eq!(leading_id("   "), None);
    }

    #[test]
    fn incomplete_profiles_are_not_configured() {
        let raw =
            b"Received: by mx.example.org; date\r\nAuthentication-Results: ; spf=pass\r\n\r\n";
        let profile =
            ProviderProfile::generic(" ", vec![HostMatcher::Exact("mx.example.org".into())]);
        assert_eq!(profile.evaluate(raw).status, ProviderStatus::NotConfigured);
        let profile = ProviderProfile::generic("mx.example.org", Vec::new());
        assert_eq!(profile.evaluate(raw).status, ProviderStatus::NotConfigured);
    }
}
