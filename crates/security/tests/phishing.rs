//! Phishing heuristics on a parsed fixture message.

#![allow(clippy::unwrap_used)] // Tests may unwrap (AGENTS.md).

mod common;

use common::fixture;
use mazemail_types::Address;
use security::phishing::{
    ContactIndex, DomainWatchlist, PhishingInput, Severity, WarningKind, analyze, check_auth,
    links_from_reader_doc,
};
use security::{Resolver, StaticDns, Verdict, Verifier, VerifyContext};

fn kinds(warnings: &[security::phishing::Warning]) -> Vec<&'static str> {
    warnings.iter().map(|w| w.kind.name()).collect()
}

#[test]
fn flags_everything_in_the_phishing_fixture() {
    let message = mazemail_mime::parse(&fixture("phishing.eml")).unwrap();
    let from = message.headers.from.first().unwrap().clone();
    let links = links_from_reader_doc(&message.reader_doc());
    assert_eq!(links.len(), 4);
    assert_eq!(links[0].text, "www.paypal.com");

    let contacts: ContactIndex = [Address::new(Some("Anna Example"), "anna@example.com")]
        .iter()
        .collect();
    let watchlist = DomainWatchlist::builtin().with_domains(["example.com"]);
    let warnings = analyze(&PhishingInput {
        from: Some(&from),
        links: &links,
        auth: None,
        contacts: Some(&contacts),
        watchlist: &watchlist,
    });

    let kinds = kinds(&warnings);
    for expected in [
        "lookalike_domain",
        "deceptive_subdomain",
        "mixed_script_domain",
        "display_name_spoofing",
        "link_text_mismatch",
    ] {
        assert!(
            kinds.contains(&expected),
            "{expected} missing from {kinds:?}"
        );
    }
    // Most serious first.
    assert!(
        warnings
            .windows(2)
            .all(|pair| pair[0].severity >= pair[1].severity)
    );
    // paypa1.com appears as sender and link host but is reported once.
    let paypa1 = warnings
        .iter()
        .filter(|w| {
            matches!(&w.kind, WarningKind::LookalikeDomain { domain, .. } if domain.ends_with("paypa1.com"))
        })
        .count();
    assert_eq!(
        paypa1, 2,
        "billing@paypa1.com and www.paypa1.com are different hosts"
    );
    // The honest link to www.example.com/faq is fine.
    assert!(!warnings.iter().any(|w| matches!(
        &w.kind,
        WarningKind::LinkTextMismatch { text_domain, .. } if text_domain == "example.com"
    )));
}

#[test]
fn clean_message_has_no_warnings() {
    let message = mazemail_mime::parse(&fixture("plain.eml")).unwrap();
    let from = message.headers.from.first().unwrap().clone();
    let contacts: ContactIndex = [Address::new(Some("Anna Example"), "anna@example.com")]
        .iter()
        .collect();
    let warnings = analyze(&PhishingInput {
        from: Some(&from),
        links: &links_from_reader_doc(&message.reader_doc()),
        auth: None,
        contacts: Some(&contacts),
        watchlist: &DomainWatchlist::builtin(),
    });
    assert!(warnings.is_empty(), "{:?}", kinds(&warnings));
}

#[tokio::test]
async fn dmarc_failure_raises_a_banner() {
    let received = "Received: from mail.evil.example (mail.evil.example [198.51.100.66]) by mx.reader.example with ESMTPS id 3; Tue, 2 Jan 2024 10:00:05 +0000";
    let results = "Authentication-Results: mx.reader.example; spf=fail smtp.mailfrom=evil.example";
    let message = common::prepend(results, &common::prepend(received, &fixture("plain.eml")));
    let dns = StaticDns::new().txt("_dmarc.example.com", "v=DMARC1; p=reject");
    let profile = security::ProviderProfile::generic(
        "mx.reader.example",
        vec![security::HostMatcher::Exact("mx.reader.example".into())],
    );
    let verifier = Verifier::new(Resolver::from_static(dns).unwrap());
    let report = verifier
        .verify(&message, &VerifyContext::new().with_provider(&profile))
        .await;
    assert_eq!(report.verdict, Verdict::Suspicious);

    let warnings = check_auth(&report);
    assert_eq!(kinds(&warnings), ["dmarc_failure"]);
    assert_eq!(warnings[0].severity, Severity::Danger);
    assert_eq!(
        warnings[0].message,
        "This message claims to be from example.com, but it failed example.com's sender check. It may be forged."
    );

    let from = Address::new(Some("Anna Example"), "anna@example.com");
    let all = analyze(&PhishingInput {
        from: Some(&from),
        links: &[],
        auth: Some(&report),
        contacts: None,
        watchlist: &DomainWatchlist::builtin(),
    });
    assert_eq!(kinds(&all), ["dmarc_failure"]);
}
