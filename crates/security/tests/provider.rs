//! Trusted provider results: real-world layouts and forged headers.

#![allow(clippy::unwrap_used)] // Tests may unwrap (AGENTS.md).

mod common;

use common::fixture;
use pretty_assertions::assert_eq;
use security::{HostMatcher, MechanismResult, ProviderOutcome, ProviderProfile, ProviderStatus};

fn evaluate(profile: &ProviderProfile, name: &str) -> ProviderOutcome {
    profile.evaluate(&fixture(&format!("provider/{name}")))
}

#[test]
fn gmail_header_below_its_received_is_trusted() {
    let outcome = evaluate(&ProviderProfile::gmail(), "gmail_trusted.eml");
    assert_eq!(
        outcome.status,
        ProviderStatus::Trusted {
            authserv_id: Some("mx.google.com".into())
        }
    );
    let results = outcome.results();
    let spf = results.spf.unwrap();
    assert_eq!(spf.result, MechanismResult::Pass);
    assert_eq!(spf.domain.as_deref(), Some("example.com"));
    assert_eq!(results.dkim.len(), 1);
    assert_eq!(results.dkim[0].domain.as_deref(), Some("example.com"));
    assert_eq!(results.dkim[0].selector.as_deref(), Some("s1"));
    let dmarc = results.dmarc.unwrap();
    assert_eq!(dmarc.result, MechanismResult::Pass);
    assert_eq!(dmarc.domain.as_deref(), Some("example.com"));
}

#[test]
fn gmail_ignores_header_below_the_senders_received() {
    let outcome = evaluate(&ProviderProfile::gmail(), "gmail_forged_below.eml");
    assert_eq!(outcome.status, ProviderStatus::NotFound);
    assert_eq!(outcome.results(), Default::default());
}

#[test]
fn gmail_duplicate_headers_are_ambiguous() {
    let outcome = evaluate(&ProviderProfile::gmail(), "gmail_duplicate.eml");
    assert_eq!(outcome.status, ProviderStatus::Ambiguous);
    assert_eq!(outcome.header, None);
}

#[test]
fn gmail_forged_received_below_a_foreign_hop_is_never_reached() {
    let outcome = evaluate(&ProviderProfile::gmail(), "gmail_forged_received.eml");
    assert_eq!(outcome.status, ProviderStatus::NotFound);
}

#[test]
fn gmail_rejects_wrong_and_wildcard_looking_ids() {
    let outcome = evaluate(&ProviderProfile::gmail(), "gmail_wrong_id.eml");
    assert_eq!(outcome.status, ProviderStatus::NotFound);
}

#[test]
fn gmail_profile_does_not_trust_other_providers() {
    for name in ["microsoft_trusted.eml", "generic_trusted.eml"] {
        let outcome = evaluate(&ProviderProfile::gmail(), name);
        assert_eq!(outcome.status, ProviderStatus::NotFound, "{name}");
    }
}

#[test]
fn microsoft_header_without_id_above_boundary_is_trusted() {
    let outcome = evaluate(&ProviderProfile::microsoft365(), "microsoft_trusted.eml");
    assert_eq!(
        outcome.status,
        ProviderStatus::Trusted { authserv_id: None }
    );
    let results = outcome.results();
    assert_eq!(results.spf.unwrap().domain.as_deref(), Some("example.com"));
    assert_eq!(results.dkim[0].result, MechanismResult::Pass);
    assert_eq!(results.dkim[0].domain.as_deref(), Some("example.com"));
    assert_eq!(results.dmarc.unwrap().result, MechanismResult::Pass);
}

#[test]
fn microsoft_ignores_header_below_boundary() {
    let outcome = evaluate(
        &ProviderProfile::microsoft365(),
        "microsoft_forged_below.eml",
    );
    assert_eq!(outcome.status, ProviderStatus::NotFound);
}

#[test]
fn microsoft_tenant_id_is_exact() {
    // The default detection expects no id, so a header with one doesn't count.
    let outcome = evaluate(&ProviderProfile::microsoft365(), "microsoft_tenant_id.eml");
    assert_eq!(outcome.status, ProviderStatus::NotFound);

    let tenant = ProviderProfile::microsoft365().with_authserv_id("gateway.contoso.example");
    let outcome = evaluate(&tenant, "microsoft_tenant_id.eml");
    assert_eq!(
        outcome.status,
        ProviderStatus::Trusted {
            authserv_id: Some("gateway.contoso.example".into())
        }
    );
    let results = outcome.results();
    assert!(results.spf.unwrap().result.is_fail());
    assert!(results.dmarc.unwrap().result.is_fail());

    // A configured tenant no longer accepts the id-less default format.
    let outcome = evaluate(&tenant, "microsoft_trusted.eml");
    assert_eq!(outcome.status, ProviderStatus::NotFound);

    let other_tenant = ProviderProfile::microsoft365().with_authserv_id("contoso.example");
    let outcome = evaluate(&other_tenant, "microsoft_tenant_id.eml");
    assert_eq!(outcome.status, ProviderStatus::NotFound);
}

fn reader_example() -> ProviderProfile {
    ProviderProfile::generic(
        "mail.reader.example",
        vec![HostMatcher::Exact("mail.reader.example".into())],
    )
}

#[test]
fn generic_host_trusts_only_the_header_above_its_received() {
    let outcome = evaluate(&reader_example(), "generic_trusted.eml");
    assert_eq!(
        outcome.status,
        ProviderStatus::Trusted {
            authserv_id: Some("mail.reader.example".into())
        }
    );
    // The forged `dmarc=fail` below the boundary is ignored.
    assert_eq!(
        outcome.results().dmarc.unwrap().result,
        MechanismResult::Pass
    );
}

#[test]
fn generic_unparseable_header_with_matching_id_is_ambiguous() {
    let outcome = evaluate(&reader_example(), "generic_broken.eml");
    assert_eq!(outcome.status, ProviderStatus::Ambiguous);
}

#[test]
fn generic_host_matchers_have_no_wildcards() {
    let wildcard = ProviderProfile::generic(
        "mail.reader.example",
        vec![HostMatcher::Exact("*.reader.example".into())],
    );
    let outcome = evaluate(&wildcard, "generic_trusted.eml");
    assert_eq!(outcome.status, ProviderStatus::NotFound);

    let wildcard_id = ProviderProfile::generic(
        "*.reader.example",
        vec![HostMatcher::Exact("mail.reader.example".into())],
    );
    let outcome = evaluate(&wildcard_id, "generic_trusted.eml");
    assert_eq!(outcome.status, ProviderStatus::NotFound);
}

#[test]
fn messages_without_received_headers_have_no_boundary() {
    let raw = b"Authentication-Results: mx.google.com; dmarc=pass header.from=paypal.com\r\nFrom: a@paypal.com\r\n\r\nx";
    let outcome = ProviderProfile::gmail().evaluate(raw);
    assert_eq!(outcome.status, ProviderStatus::NotFound);
}

#[test]
fn profiles_round_trip_through_json() {
    for profile in [
        ProviderProfile::gmail(),
        ProviderProfile::microsoft365().with_authserv_id("gateway.contoso.example"),
        reader_example(),
    ] {
        let json = serde_json::to_string(&profile).unwrap();
        let back: ProviderProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, profile);
    }
}
