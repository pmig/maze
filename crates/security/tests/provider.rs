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
        automatic(&["reader.example", "in1-smtp.messagingengine.com"]),
    ] {
        let json = serde_json::to_string(&profile).unwrap();
        let back: ProviderProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, profile);
    }
}

/// The profile Maze stored for IMAP accounts before automatic profiles: the IMAP host
/// as `authserv-id` and as the only inbound host.
fn legacy_imap_profile(imap_host: &str) -> ProviderProfile {
    ProviderProfile::generic(imap_host, vec![HostMatcher::Exact(imap_host.into())])
}

#[test]
fn the_legacy_imap_profile_never_finds_real_world_results() {
    for (name, imap_host) in [
        ("generic_fastmail.eml", "imap.fastmail.com"),
        ("generic_postfix.eml", "imap.reader.example"),
        // Even with one host for IMAP and SMTP: Stalwart writes below its Received.
        ("generic_stalwart.eml", "mx.reader.example"),
    ] {
        let outcome = evaluate(&legacy_imap_profile(imap_host), name);
        assert_eq!(outcome.status, ProviderStatus::NotFound, "{name}");
    }
}

fn automatic(domains: &[&str]) -> ProviderProfile {
    ProviderProfile::automatic(domains.iter().copied())
}

fn assert_spf_pass(outcome: &ProviderOutcome, authserv_id: &str, name: &str) {
    assert_eq!(
        outcome.status,
        ProviderStatus::Trusted {
            authserv_id: Some(authserv_id.into())
        },
        "{name}"
    );
    let results = outcome.results();
    let spf = results.spf.unwrap();
    assert_eq!(spf.result, MechanismResult::Pass, "{name}");
    assert_eq!(spf.domain.as_deref(), Some("example.com"), "{name}");
    // The sender's forged `dmarc=fail` below the boundary is never read.
    assert_eq!(
        results.dmarc.unwrap().result,
        MechanismResult::Pass,
        "{name}"
    );
}

#[test]
fn automatic_profiles_find_real_world_results() {
    // Fastmail: address domain, IMAP host and the MX `in1-smtp.messagingengine.com`.
    let fastmail = automatic(&[
        "reader.example",
        "imap.fastmail.com",
        "in1-smtp.messagingengine.com",
    ]);
    let outcome = evaluate(&fastmail, "generic_fastmail.eml");
    assert_spf_pass(&outcome, "mx6.messagingengine.com", "fastmail");

    // Self-hosted Postfix with a content filter and Dovecot's LMTP hop on top.
    let own = automatic(&["reader.example", "imap.reader.example"]);
    assert_spf_pass(
        &evaluate(&own, "generic_postfix.eml"),
        "mail.reader.example",
        "postfix",
    );
    // Stalwart writes its header directly below its own Received.
    assert_spf_pass(
        &evaluate(&own, "generic_stalwart.eml"),
        "mx.reader.example",
        "stalwart",
    );
    // The older single-host layout still works.
    let outcome = evaluate(&own, "generic_trusted.eml");
    assert_eq!(
        outcome.status,
        ProviderStatus::Trusted {
            authserv_id: Some("mail.reader.example".into())
        }
    );
}

#[test]
fn automatic_profiles_ignore_hops_forged_below_the_boundary() {
    let own = automatic(&["reader.example"]);
    // The server writes no header; the sender forged one together with
    // provider-looking hops below the real boundary.
    assert_eq!(
        evaluate(&own, "generic_forged_hops.eml").status,
        ProviderStatus::NotFound
    );
    // A HELO that is a private address literal doesn't hide the sender's address.
    assert_eq!(
        evaluate(&own, "generic_forged_helo.eml").status,
        ProviderStatus::NotFound
    );
    // The lowest-inbound rule would have walked into the forged hops.
    let lowest = legacy_imap_profile("mail.reader.example");
    assert!(matches!(
        evaluate(&lowest, "generic_forged_hops.eml").status,
        ProviderStatus::Trusted { .. }
    ));
}

#[test]
fn automatic_profiles_keep_the_exactly_one_rule() {
    let own = automatic(&["reader.example"]);
    assert_eq!(
        evaluate(&own, "generic_stalwart_duplicate.eml").status,
        ProviderStatus::Ambiguous
    );
    assert_eq!(
        evaluate(&own, "generic_broken.eml").status,
        ProviderStatus::Ambiguous
    );
}

#[test]
fn automatic_profiles_only_trust_their_own_domains() {
    // Another provider's domains: no hop of theirs is an inbound host here.
    let other = automatic(&["elsewhere.example", "imap.elsewhere.example"]);
    for name in [
        "generic_fastmail.eml",
        "generic_postfix.eml",
        "generic_stalwart.eml",
        "gmail_trusted.eml",
        "microsoft_trusted.eml",
    ] {
        assert_eq!(
            evaluate(&other, name).status,
            ProviderStatus::NotFound,
            "{name}"
        );
    }
    // Public suffixes, IP addresses and single labels never become domains.
    let profile = automatic(&["co.uk", "192.0.2.1", "localhost", "", "Mail.Example.CO.UK."]);
    assert_eq!(
        profile.inbound_hosts,
        vec![HostMatcher::DomainSuffix("example.co.uk".into())]
    );
    assert_eq!(
        automatic(&["co.uk"])
            .evaluate(b"Received: by x.co.uk; d\r\n\r\n")
            .status,
        ProviderStatus::NotConfigured
    );
}

#[test]
fn profiles_stored_before_the_boundary_field_still_load() {
    let mut json: serde_json::Value = serde_json::to_value(reader_example()).unwrap();
    json.as_object_mut().unwrap().remove("boundary");
    let back: ProviderProfile = serde_json::from_value(json).unwrap();
    assert_eq!(back, reader_example());
}

#[tokio::test]
async fn automatic_profiles_include_the_mx_hosts() {
    let resolver = security::Resolver::from_static(
        security::StaticDns::new().mx("reader.example", ["in1-smtp.messagingengine.com"]),
    )
    .unwrap();
    let verifier = security::Verifier::new(resolver);
    let profile = verifier
        .automatic_profile(Some("reader.example"), "imap.fastmail.com")
        .await;
    assert_eq!(
        profile,
        automatic(&["fastmail.com", "reader.example", "messagingengine.com"])
    );
    assert_spf_pass(
        &evaluate(&profile, "generic_fastmail.eml"),
        "mx6.messagingengine.com",
        "fastmail",
    );
    // Without MX hosts, Fastmail's hops aren't recognized: nothing is trusted.
    let without_mx = verifier
        .automatic_profile(Some("other.example"), "imap.fastmail.com")
        .await;
    assert_eq!(
        evaluate(&without_mx, "generic_fastmail.eml").status,
        ProviderStatus::NotFound
    );
}
