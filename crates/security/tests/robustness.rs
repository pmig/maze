//! Garbage in, no panics out.

#![allow(clippy::unwrap_used)] // Tests may unwrap (AGENTS.md).

mod common;

use proptest::prelude::*;
use security::phishing::{
    DomainWatchlist, Link, check_display_name, check_domain, check_links, normalize_display_name,
    skeleton,
};
use security::{ProviderProfile, Resolver, StaticDns, Verifier, VerifyContext, auth_results};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Header-ish lines built from the pieces that matter to the parsers.
fn header_soup() -> impl Strategy<Value = Vec<u8>> {
    let line = prop_oneof![
        Just("Received: from x by mx.google.com; Tue, 2 Jan 2024".to_owned()),
        Just("Received: by 2002:a05::1 with SMTP id q;".to_owned()),
        Just("Received: (comment (nested) by mx.google.com".to_owned()),
        Just("Authentication-Results: mx.google.com; dkim=pass header.i=@example.com".to_owned()),
        Just("Authentication-Results: spf=pass smtp.mailfrom=example.com; dmarc=fail".to_owned()),
        Just("Authentication-Results: mx.google.com; dkim=".to_owned()),
        Just(
            "DKIM-Signature: v=1; a=ed25519-sha256; d=example.com; s=s1; h=from; bh=AAAA; b=AAAA"
                .to_owned()
        ),
        Just("ARC-Seal: i=1; a=rsa-sha256; cv=none; d=example.com; s=s; b=AA".to_owned()),
        Just(
            "ARC-Message-Signature: i=1; a=rsa-sha256; d=example.com; s=s; h=from; bh=A; b=A"
                .to_owned()
        ),
        Just("ARC-Authentication-Results: i=1; example.com".to_owned()),
        Just("From: Anna <anna@example.com>".to_owned()),
        Just("From: a@x, b@y".to_owned()),
        Just(" \tcontinuation".to_owned()),
        "[ -~]{0,80}",
        ".{0,40}",
    ];
    (prop::collection::vec(line, 0..24), "[ -~\n]{0,200}").prop_map(|(lines, body)| {
        let mut raw = lines.join("\r\n").into_bytes();
        raw.extend_from_slice(b"\r\n\r\n");
        raw.extend_from_slice(body.as_bytes());
        raw
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn auth_results_parser_never_panics(input in ".{0,300}") {
        let _ = auth_results::parse(&input);
    }

    #[test]
    fn provider_evaluation_never_panics(raw in header_soup()) {
        for profile in [ProviderProfile::gmail(), ProviderProfile::microsoft365()] {
            let outcome = profile.evaluate(&raw);
            let _ = outcome.results();
        }
    }

    #[test]
    fn verification_never_panics(raw in header_soup()) {
        let verifier = Verifier::new(Resolver::from_static(StaticDns::new()).unwrap());
        let profile = ProviderProfile::gmail();
        let ctx = VerifyContext::new().with_provider(&profile);
        let report = runtime().block_on(verifier.verify(&raw, &ctx));
        prop_assert!(!report.explanation.is_empty());
    }

    #[test]
    fn verification_of_random_bytes_never_panics(raw in prop::collection::vec(any::<u8>(), 0..512)) {
        let verifier = Verifier::new(Resolver::from_static(StaticDns::new()).unwrap());
        let report = runtime().block_on(verifier.verify(&raw, &VerifyContext::new()));
        prop_assert!(!report.explanation.is_empty());
    }

    #[test]
    fn heuristics_never_panic(text in ".{0,60}", href in ".{0,60}", name in ".{0,40}", email in ".{0,40}") {
        let watchlist = DomainWatchlist::builtin();
        let _ = check_links(&[Link::new(text.clone(), href.clone())]);
        let _ = check_domain(&text, &watchlist);
        let _ = check_domain(&href, &watchlist);
        let from = mazemail_types::Address::new(Some(name.clone()), email);
        let _ = check_display_name(&from, None);
        let _ = normalize_display_name(&name);
    }

    #[test]
    fn skeleton_is_idempotent(input in ".{0,40}") {
        let once = skeleton(&input);
        prop_assert_eq!(skeleton(&once), once);
    }
}
