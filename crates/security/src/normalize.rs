//! Turn a trusted provider header into [`MechanismResult`]s.
//!
//! Providers agree on the RFC 8601 syntax but not on every word: Gmail and Exchange
//! Online write `dmarc=bestguesspass` when the domain publishes no policy, Exchange
//! Online writes `header.d=none` for unsigned mail, Gmail identifies DKIM signers by
//! `header.i=@domain`. The normalizers here smooth that out.

use crate::auth_results::{AuthenticationResults, MethodResult};
use crate::provider::ProviderKind;
use crate::report::MechanismResult;

/// One normalized provider result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderMechanism {
    pub result: MechanismResult,
    /// The domain the result is about, lowercased.
    pub domain: Option<String>,
}

/// One normalized provider DKIM result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDkim {
    pub result: MechanismResult,
    /// The signing domain, lowercased.
    pub domain: Option<String>,
    /// The selector (`header.s`), when the provider includes it.
    pub selector: Option<String>,
}

/// The results from a trusted provider header.
///
/// A mechanism that appears more than once (two `spf=` results, say) is ambiguous
/// and left out. DKIM may legitimately appear once per signature.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderResults {
    pub spf: Option<ProviderMechanism>,
    pub dkim: Vec<ProviderDkim>,
    pub dmarc: Option<ProviderMechanism>,
    pub arc: Option<ProviderMechanism>,
}

pub(crate) fn normalize(kind: ProviderKind, header: &AuthenticationResults) -> ProviderResults {
    let mut out = ProviderResults::default();
    let count = |method: &str| header.results.iter().filter(|r| r.method == method).count();
    let single = |method: &str| {
        (count(method) == 1)
            .then(|| header.results.iter().find(|r| r.method == method))
            .flatten()
    };

    out.spf = single("spf").and_then(|r| {
        Some(ProviderMechanism {
            result: map_result(&r.result)?,
            domain: spf_domain(r),
        })
    });
    out.dmarc = single("dmarc").and_then(|r| {
        Some(ProviderMechanism {
            result: map_result(&r.result)?,
            domain: r.property("header", "from").and_then(clean_domain),
        })
    });
    out.arc = single("arc").and_then(|r| {
        Some(ProviderMechanism {
            result: map_result(&r.result)?,
            domain: None,
        })
    });
    out.dkim = header
        .results
        .iter()
        .filter(|r| r.method == "dkim")
        .filter_map(|r| {
            let result = map_result(&r.result)?;
            let domain = dkim_domain(kind, r);
            // A provider "pass" without a domain can't be attributed to anyone.
            if result.is_pass() && domain.is_none() {
                return None;
            }
            Some(ProviderDkim {
                result,
                domain,
                selector: r.property("header", "s").map(|s| s.to_ascii_lowercase()),
            })
        })
        .collect();
    out
}

/// Map a result keyword. Unknown keywords give `None`, so that result is ignored.
fn map_result(keyword: &str) -> Option<MechanismResult> {
    Some(match keyword {
        "pass" => MechanismResult::Pass,
        "fail" | "hardfail" => MechanismResult::fail("reported by the provider"),
        "softfail" => MechanismResult::fail("soft fail reported by the provider"),
        "neutral" | "policy" => MechanismResult::Neutral,
        // `bestguesspass`: no published DMARC policy, but it would have passed.
        "none" | "bestguesspass" => MechanismResult::None,
        "temperror" => MechanismResult::TempError,
        "permerror" => MechanismResult::PermError,
        _ => return None,
    })
}

fn spf_domain(result: &MethodResult) -> Option<String> {
    result
        .property("smtp", "mailfrom")
        .or_else(|| result.property("smtp", "helo"))
        .and_then(clean_domain)
}

fn dkim_domain(kind: ProviderKind, result: &MethodResult) -> Option<String> {
    let d = || result.property("header", "d").and_then(clean_domain);
    let i = || result.property("header", "i").and_then(clean_domain);
    match kind {
        // Gmail identifies the signer by the AUID (`header.i=@domain`), whose domain
        // is the signing domain or a subdomain of it.
        ProviderKind::Gmail => i().or_else(d),
        // Exchange Online writes `header.d` only (`none` when unsigned).
        ProviderKind::Microsoft365 => d(),
        ProviderKind::Generic => d().or_else(i),
    }
}

/// The domain of `value`, which may be `domain`, `@domain` or `local@domain`.
/// Exchange Online writes `none` for "no domain".
fn clean_domain(value: &str) -> Option<String> {
    let domain = value.rsplit_once('@').map_or(value, |(_, d)| d);
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let plausible = !domain.is_empty()
        && domain != "none"
        && domain.contains('.')
        && domain
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'));
    plausible.then_some(domain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_results::parse;

    #[test]
    fn normalizes_gmail() {
        let header = parse(
            "mx.google.com; dkim=pass header.i=@example.com header.s=s1 header.b=x; dkim=fail header.i=@other.example; spf=softfail smtp.mailfrom=bounce@mail.example.com; dmarc=bestguesspass header.from=example.com; arc=pass",
        )
        .unwrap();
        let results = normalize(ProviderKind::Gmail, &header);
        assert_eq!(results.dkim.len(), 2);
        assert_eq!(results.dkim[0].result, MechanismResult::Pass);
        assert_eq!(results.dkim[0].domain.as_deref(), Some("example.com"));
        assert_eq!(results.dkim[0].selector.as_deref(), Some("s1"));
        assert!(results.dkim[1].result.is_fail());
        let spf = results.spf.unwrap();
        assert!(spf.result.is_fail());
        assert_eq!(spf.domain.as_deref(), Some("mail.example.com"));
        assert_eq!(results.dmarc.unwrap().result, MechanismResult::None);
        assert_eq!(results.arc.unwrap().result, MechanismResult::Pass);
    }

    #[test]
    fn normalizes_microsoft() {
        let header = parse(
            "spf=pass (sender IP is 192.0.2.1) smtp.mailfrom=Example.COM; dkim=none (message not signed) header.d=none;dmarc=fail action=oreject header.from=example.com;compauth=fail reason=000",
        )
        .unwrap();
        let results = normalize(ProviderKind::Microsoft365, &header);
        let spf = results.spf.unwrap();
        assert_eq!(spf.result, MechanismResult::Pass);
        assert_eq!(spf.domain.as_deref(), Some("example.com"));
        assert_eq!(results.dkim.len(), 1);
        assert_eq!(results.dkim[0].result, MechanismResult::None);
        assert_eq!(results.dkim[0].domain, None);
        assert!(results.dmarc.unwrap().result.is_fail());
        assert_eq!(results.arc, None);
    }

    #[test]
    fn duplicate_mechanisms_are_dropped() {
        let header = parse("mx.example.org; spf=pass smtp.mailfrom=a.example; spf=fail smtp.mailfrom=b.example; dmarc=pass header.from=a.example").unwrap();
        let results = normalize(ProviderKind::Generic, &header);
        assert_eq!(results.spf, None);
        assert!(results.dmarc.is_some());
    }

    #[test]
    fn unknown_results_and_unattributed_passes_are_ignored() {
        let header =
            parse("mx.example.org; spf=maybe smtp.mailfrom=a.example; dkim=pass; dmarc=pass")
                .unwrap();
        let results = normalize(ProviderKind::Generic, &header);
        assert_eq!(results.spf, None);
        assert!(results.dkim.is_empty());
        let dmarc = results.dmarc.unwrap();
        assert_eq!(dmarc.result, MechanismResult::Pass);
        assert_eq!(dmarc.domain, None);
    }

    #[test]
    fn cleans_domains() {
        assert_eq!(
            clean_domain("@Example.com.").as_deref(),
            Some("example.com")
        );
        assert_eq!(clean_domain("a@b.example").as_deref(), Some("b.example"));
        assert_eq!(clean_domain("none"), None);
        assert_eq!(clean_domain("localhost"), None);
        assert_eq!(clean_domain("bad domain.com"), None);
    }
}
