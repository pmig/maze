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
//!    `*.google.com` or `mx.google.com.evil.example` never match. (Automatic
//!    profiles accept any name under the provider's domains; see below.)
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
//!
//! # Any other provider
//!
//! For other IMAP accounts the exact hosts and `authserv-id` aren't known, so
//! [`ProviderProfile::automatic`] derives them from the provider's registrable
//! domains (the address's domain, its MX hosts, the IMAP host). Matching by domain is
//! looser, so the boundary is found differently ([`Boundary::FirstExternal`]): it is
//! the *first* hop from the top that accepted the message from outside, judged by
//! the connecting address the server recorded in the `from` clause. The walk never
//! goes below it, so a sender can't extend the provider's run with forged hops, and
//! the zone holds only headers the provider wrote. That matters for servers that
//! write no `Authentication-Results` at all, where the exactly-one rule can't help.
//!
//! Real servers put their header in two places: above their own `Received` header
//! (Postfix with a milter, Fastmail) or directly below it (Stalwart, which writes
//! `Received` and `Authentication-Results` as one block). Directly below is also where
//! the sender's own headers start, so that zone is only searched when the boundary
//! header names software that always writes its header there
//! ([`ResultsPlacement::AboveOrBelowForKnownSoftware`]); a forged copy next to the
//! real one makes the result ambiguous.

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
    /// The id must be one of these domains or a host under one, on a label boundary
    /// (`mx6.messagingengine.com` for `messagingengine.com`). Only for
    /// [`ProviderProfile::automatic`], where the provider's exact id isn't known; the
    /// position rules there leave only headers the provider wrote.
    WithinDomains(Vec<String>),
}

impl AuthservId {
    fn matches(&self, id: Option<&str>) -> bool {
        match (self, id) {
            (Self::Exact(expected), Some(id)) => {
                !id.is_empty() && expected.eq_ignore_ascii_case(id)
            }
            (Self::Absent, None) => true,
            (Self::WithinDomains(domains), Some(id)) => {
                let id = normalize_host(id);
                !id.is_empty() && domains.iter().any(|domain| within_domain(&id, domain))
            }
            _ => false,
        }
    }

    fn is_configured(&self) -> bool {
        match self {
            Self::Exact(id) => !id.trim().is_empty(),
            Self::Absent => true,
            Self::WithinDomains(domains) => domains.iter().any(|d| !d.trim().is_empty()),
        }
    }
}

impl fmt::Debug for AuthservId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Provider host names aren't personal data.
            Self::Exact(id) => f.debug_tuple("Exact").field(id).finish(),
            Self::Absent => f.write_str("Absent"),
            Self::WithinDomains(domains) => f.debug_tuple("WithinDomains").field(domains).finish(),
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
    /// A name that only exists inside a network: a single label (`sloti44n03`),
    /// `localhost`, or a name under `internal`, `local`, `lan`, `localdomain` or
    /// `home.arpa` (`mailmx.nyi.internal`).
    Private,
}

/// Top-level names that never leave a private network.
const PRIVATE_DOMAINS: &[&str] = &["internal", "local", "lan", "localdomain", "home.arpa"];

impl HostMatcher {
    /// Whether `host` (lowercase, no trailing dot) matches.
    pub fn matches(&self, host: &str) -> bool {
        match self {
            Self::Exact(name) => normalize_host(name) == host,
            Self::DomainSuffix(domain) => within_domain(host, domain),
            Self::IpLiteral => host.parse::<IpAddr>().is_ok(),
            Self::Private => {
                !host.is_empty()
                    && host.parse::<IpAddr>().is_err()
                    && (!host.contains('.')
                        || host == "localhost"
                        || PRIVATE_DOMAINS.iter().any(|d| within_domain(host, d)))
            }
        }
    }
}

/// Whether `host` (lowercase, no trailing dot) is `domain` or a name under it, on a
/// label boundary: `x.outlook.com` is under `outlook.com`, `evil-outlook.com` isn't.
fn within_domain(host: &str, domain: &str) -> bool {
    let domain = normalize_host(domain);
    !domain.is_empty()
        && (host == domain
            || host
                .strip_suffix(domain.as_str())
                .is_some_and(|prefix| prefix.ends_with('.')))
}

/// Addresses that don't route on the internet: loopback, private, link-local,
/// carrier-grade NAT, unique local and unspecified.
fn is_internal_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                // 100.64.0.0/10, shared address space (RFC 6598).
                || (v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64)
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_internal_address(IpAddr::V4(v4)),
            None => {
                let first = v6.segments()[0];
                v6.is_loopback()
                    || v6.is_unspecified()
                    // fc00::/7 unique local, fe80::/10 link-local.
                    || first & 0xfe00 == 0xfc00
                    || first & 0xffc0 == 0xfe80
            }
        },
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
    /// Above the boundary, and directly below it too when the boundary header names
    /// server software that always writes its header there (Stalwart).
    AboveOrBelowForKnownSoftware,
}

/// Server software, as named in the comment after the `by` host of its `Received`
/// header, that writes its `Authentication-Results` directly below that header.
const WRITES_RESULTS_BELOW: &[&str] = &["Stalwart SMTP"];

/// How the boundary hop is found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    /// The lowest hop by an inbound host in the run of the provider's own
    /// `Received` headers (Gmail, Microsoft 365, configured hosts).
    #[default]
    LowestInbound,
    /// The first hop from the top that accepted the message from outside, written by
    /// an inbound host. A hop is from outside when its `from` clause names an
    /// address that routes on the internet; a clause without any address counts as
    /// inside only when it names one of the provider's hosts (Dovecot's LMTP
    /// hop). Nothing below this hop is looked at, so headers a sender forged to look
    /// like more provider hops are never reached.
    FirstExternal,
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
    /// How the boundary hop is found.
    #[serde(default)]
    pub boundary: Boundary,
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
            boundary: Boundary::LowestInbound,
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
            boundary: Boundary::LowestInbound,
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
            boundary: Boundary::LowestInbound,
        }
    }

    /// A profile for an account whose provider has no built-in profile, from the
    /// domains that identify it: usually the account address's domain, the IMAP
    /// host and the address domain's MX hosts. Each is reduced to its registrable
    /// domain (`mx6.messagingengine.com` gives `messagingengine.com`); IP addresses,
    /// single labels and bare public suffixes are dropped.
    ///
    /// Any host under those domains is an inbound host, IP literals and private
    /// names are internal hosts, the boundary is the first hop that accepted the
    /// message from outside ([`Boundary::FirstExternal`]), and the `authserv-id` may
    /// be any name under those domains ([`AuthservId::WithinDomains`]).
    pub fn automatic<I, S>(domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut orgs: Vec<String> = Vec::new();
        for domain in domains {
            let Some(ascii) = crate::domain::to_ascii(domain.as_ref()) else {
                continue;
            };
            if ascii.parse::<IpAddr>().is_ok() {
                continue;
            }
            // `None` for bare public suffixes (`co.uk`) and single labels.
            if let Some(org) = psl::domain_str(&ascii)
                && org.contains('.')
                && !orgs.iter().any(|known| known == org)
            {
                orgs.push(org.to_owned());
            }
        }
        Self {
            kind: ProviderKind::Generic,
            inbound_hosts: orgs
                .iter()
                .cloned()
                .map(HostMatcher::DomainSuffix)
                .collect(),
            authserv_id: AuthservId::WithinDomains(orgs),
            internal_hosts: vec![HostMatcher::IpLiteral, HostMatcher::Private],
            placement: ResultsPlacement::AboveOrBelowForKnownSoftware,
            boundary: Boundary::FirstExternal,
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
        if !self.authserv_id.is_configured() || self.inbound_hosts.is_empty() {
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
        let boundary = match self.boundary {
            Boundary::LowestInbound => self.lowest_inbound(headers)?,
            Boundary::FirstExternal => self.first_external(headers)?,
        };
        let below = match self.placement {
            ResultsPlacement::Above => false,
            ResultsPlacement::AboveOrDirectlyBelow => true,
            ResultsPlacement::AboveOrBelowForKnownSoftware => {
                headers::received_by_software(&headers[boundary].value)
                    .is_some_and(|software| WRITES_RESULTS_BELOW.contains(&software.as_str()))
            }
        };
        Some(if below {
            headers[boundary + 1..]
                .iter()
                .position(|h| h.is("Received"))
                .map_or(headers.len(), |offset| boundary + 1 + offset)
        } else {
            boundary
        })
    }

    /// [`Boundary::LowestInbound`].
    fn lowest_inbound(&self, headers: &[RawHeader<'_>]) -> Option<usize> {
        let mut boundary = None;
        for (index, header) in headers.iter().enumerate() {
            if !header.is("Received") {
                continue;
            }
            let Some(host) = headers::received_by_host(&header.value) else {
                break;
            };
            if self.is_inbound(&host) {
                boundary = Some(index);
            } else if !self.is_internal(&host) {
                break;
            }
        }
        boundary
    }

    /// [`Boundary::FirstExternal`].
    fn first_external(&self, headers: &[RawHeader<'_>]) -> Option<usize> {
        for (index, header) in headers.iter().enumerate() {
            if !header.is("Received") {
                continue;
            }
            let host = headers::received_by_host(&header.value)?;
            let inbound = self.is_inbound(&host);
            if !inbound && !self.is_internal(&host) {
                return None;
            }
            if self.accepted_from_outside(&header.value) {
                return inbound.then_some(index);
            }
        }
        None
    }

    /// Whether a `Received` header records a connection from outside the provider.
    fn accepted_from_outside(&self, value: &str) -> bool {
        // No from clause: the message was created on the host itself.
        let Some(from) = headers::received_from(value) else {
            return false;
        };
        if from.addresses.is_empty() {
            return !from
                .host
                .is_some_and(|host| self.is_inbound(&host) || self.is_internal(&host));
        }
        from.addresses.iter().any(|&a| !is_internal_address(a))
    }

    fn is_inbound(&self, host: &str) -> bool {
        self.inbound_hosts.iter().any(|m| m.matches(host))
    }

    fn is_internal(&self, host: &str) -> bool {
        self.internal_hosts.iter().any(|m| m.matches(host))
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
    fn private_names_and_addresses() {
        for host in [
            "sloti44n03",
            "localhost",
            "compute1.internal",
            "mailmx.nyi.internal",
            "nas.local",
            "box.home.arpa",
        ] {
            assert!(HostMatcher::Private.matches(host), "{host}");
        }
        for host in [
            "mx.example.org",
            "internal.example.com",
            "10.0.0.1",
            "::1",
            "",
        ] {
            assert!(!HostMatcher::Private.matches(host), "{host}");
        }

        for address in [
            "127.0.0.1",
            "10.202.2.41",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.0.1",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_internal_address(address.parse().unwrap()), "{address}");
        }
        for address in [
            "192.0.2.10",
            "203.0.113.5",
            "100.128.0.1",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_internal_address(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn authserv_id_within_domains() {
        let id = AuthservId::WithinDomains(vec!["messagingengine.com".into()]);
        assert!(id.matches(Some("mx6.messagingengine.com")));
        assert!(id.matches(Some("MessagingEngine.com.")));
        for other in [
            "evil-messagingengine.com",
            "messagingengine.com.evil.example",
            "",
            "com",
        ] {
            assert!(!id.matches(Some(other)), "{other}");
        }
        assert!(!id.matches(None));
        assert!(!AuthservId::WithinDomains(Vec::new()).is_configured());
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
