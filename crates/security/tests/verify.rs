//! Local DKIM, ARC and DMARC verification against a static resolver, and how it
//! merges with trusted provider results.

#![allow(clippy::unwrap_used)] // Tests may unwrap (AGENTS.md).

mod common;

use common::{arc_seal, dkim_sign, ed25519_key, fixture, prepend, replace, rsa_key};
use pretty_assertions::assert_eq;
use security::{
    AUTH_REPORT_SCHEMA, AuthReport, DmarcPolicy, HostMatcher, MechanismResult, ProviderProfile,
    ProviderStatus, Resolver, Source, StaticDns, Verdict, Verifier, VerifyContext,
};
use time::{Duration, OffsetDateTime};

async fn verify(dns: StaticDns, message: &[u8], ctx: &VerifyContext<'_>) -> AuthReport {
    let verifier = Verifier::new(Resolver::from_static(dns).unwrap());
    verifier.verify(message, ctx).await
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

#[tokio::test]
async fn dkim_pass_with_dmarc_is_verified() {
    let key = ed25519_key();
    let message = dkim_sign(key.key, "example.com", "s1", &fixture("plain.eml"));
    let dns = StaticDns::new()
        .txt("s1._domainkey.example.com", &key.record)
        .txt("_dmarc.example.com", "v=DMARC1; p=reject");

    let report = verify(dns, &message, &VerifyContext::new()).await;

    assert_eq!(report.verdict, Verdict::Verified);
    assert_eq!(report.dkim.result, MechanismResult::Pass);
    assert_eq!(report.dkim.source, Source::Local);
    assert_eq!(report.dkim.domain.as_deref(), Some("example.com"));
    assert_eq!(report.dmarc.result, MechanismResult::Pass);
    assert_eq!(report.dmarc_policy, Some(DmarcPolicy::Reject));
    assert_eq!(report.from_domain.as_deref(), Some("example.com"));
    assert_eq!(report.aligned_domain.as_deref(), Some("example.com"));
    assert_eq!(report.spf.result, MechanismResult::NotAvailable);
    assert_eq!(report.arc.result, MechanismResult::None);
    assert_eq!(report.provider, ProviderStatus::NotConfigured);
    assert_eq!(report.dkim_signatures.len(), 1);
    assert_eq!(report.dkim_signatures[0].selector.as_deref(), Some("s1"));
    assert!(report.dkim_signatures[0].aligned);
    assert_eq!(
        report.explanation,
        "Signed by example.com, which matches the sender address."
    );
    assert!(!report.needs_recheck());
}

#[tokio::test]
async fn rsa_signatures_verify() {
    let key = rsa_key();
    let message = dkim_sign(key.key, "example.com", "rsa2048", &fixture("plain.eml"));
    let dns = StaticDns::new().txt("rsa2048._domainkey.example.com", &key.record);

    let report = verify(dns, &message, &VerifyContext::new()).await;

    assert_eq!(report.dkim.result, MechanismResult::Pass);
    // No DMARC record: an aligned signature still verifies the sender.
    assert_eq!(report.dmarc.result, MechanismResult::None);
    assert_eq!(report.dmarc_policy, None);
    assert_eq!(report.verdict, Verdict::Verified);
}

#[tokio::test]
async fn modified_body_fails_dkim() {
    let key = ed25519_key();
    let signed = dkim_sign(key.key, "example.com", "s1", &fixture("plain.eml"));
    let tampered = replace(&signed, "as promised", "at evil.example");
    let dns = StaticDns::new().txt("s1._domainkey.example.com", &key.record);

    let report = verify(dns, &tampered, &VerifyContext::new()).await;

    assert_eq!(
        report.dkim.result,
        MechanismResult::fail("the message body was changed after signing")
    );
    assert_eq!(report.verdict, Verdict::Unverified);
    assert!(report.explanation.contains("changed after it was signed"));
}

#[tokio::test]
async fn modified_header_fails_dkim() {
    let key = ed25519_key();
    let signed = dkim_sign(key.key, "example.com", "s1", &fixture("plain.eml"));
    let tampered = replace(
        &signed,
        "Subject: Quarterly numbers",
        "Subject: Urgent wire transfer",
    );
    let dns = StaticDns::new().txt("s1._domainkey.example.com", &key.record);

    let report = verify(dns, &tampered, &VerifyContext::new()).await;

    assert_eq!(
        report.dkim.result,
        MechanismResult::fail("the signature doesn't match the message")
    );
}

#[tokio::test]
async fn wrong_key_fails_dkim() {
    let signing = ed25519_key();
    let published = ed25519_key();
    let message = dkim_sign(signing.key, "example.com", "s1", &fixture("plain.eml"));
    let dns = StaticDns::new().txt("s1._domainkey.example.com", &published.record);

    let report = verify(dns, &message, &VerifyContext::new()).await;

    assert!(report.dkim.result.is_fail());
}

#[tokio::test]
async fn rotated_key_on_old_mail_is_not_forgery() {
    let key = ed25519_key();
    let message = dkim_sign(key.key, "example.com", "2023q1", &fixture("plain.eml"));
    // The 2023q1 selector is gone; a newer key is published under another selector.
    let dns = StaticDns::new()
        .txt("2024q1._domainkey.example.com", &ed25519_key().record)
        .txt("_dmarc.example.com", "v=DMARC1; p=reject");
    let ctx = VerifyContext::new().with_received_at(now() - Duration::days(400));

    let report = verify(dns, &message, &ctx).await;

    assert_eq!(report.dkim.result, MechanismResult::KeyNoLongerPublished);
    assert_eq!(report.dmarc.result, MechanismResult::KeyNoLongerPublished);
    assert_eq!(report.verdict, Verdict::Unverified);
    assert!(report.explanation.contains("no longer published"));
}

#[tokio::test]
async fn revoked_key_on_old_mail_counts_as_rotated() {
    let key = ed25519_key();
    let message = dkim_sign(key.key, "example.com", "s1", &fixture("plain.eml"));
    let dns = StaticDns::new().txt("s1._domainkey.example.com", "v=DKIM1; k=ed25519; p=");
    // No receive time: the signature's own timestamp tells the age.
    let ctx = VerifyContext::new().at(now() + Duration::days(30));

    let report = verify(dns, &message, &ctx).await;

    assert_eq!(report.dkim.result, MechanismResult::KeyNoLongerPublished);
}

#[tokio::test]
async fn missing_key_on_new_mail_is_an_error() {
    let key = ed25519_key();
    let message = dkim_sign(key.key, "example.com", "s1", &fixture("plain.eml"));
    let ctx = VerifyContext::new().with_received_at(now() - Duration::hours(1));

    let report = verify(StaticDns::new(), &message, &ctx).await;

    assert_eq!(report.dkim.result, MechanismResult::PermError);
    assert_eq!(report.verdict, Verdict::Unverified);
}

#[tokio::test]
async fn dns_timeouts_are_temporary() {
    let key = ed25519_key();
    let message = dkim_sign(key.key, "example.com", "s1", &fixture("plain.eml"));
    let dns = StaticDns::new().temp_error("s1._domainkey.example.com");

    let report = verify(dns, &message, &VerifyContext::new()).await;

    assert_eq!(report.dkim.result, MechanismResult::TempError);
    assert_eq!(report.verdict, Verdict::Unverified);
    assert!(report.needs_recheck());
    assert!(report.explanation.contains("try again"));
}

#[tokio::test]
async fn relaxed_alignment_with_organizational_policy() {
    let key = ed25519_key();
    let message = dkim_sign(
        key.key,
        "example.com",
        "s1",
        &fixture("subdomain_sender.eml"),
    );
    let dns = StaticDns::new()
        .txt("s1._domainkey.example.com", &key.record)
        .txt("_dmarc.example.com", "v=DMARC1; p=quarantine");

    let report = verify(dns, &message, &VerifyContext::new()).await;

    assert_eq!(report.from_domain.as_deref(), Some("news.example.com"));
    assert_eq!(report.dmarc.result, MechanismResult::Pass);
    assert_eq!(report.aligned_domain.as_deref(), Some("example.com"));
    assert_eq!(report.verdict, Verdict::Verified);
}

#[tokio::test]
async fn strict_alignment_is_not_met_by_parent_domain() {
    let key = ed25519_key();
    let message = dkim_sign(
        key.key,
        "example.com",
        "s1",
        &fixture("subdomain_sender.eml"),
    );
    let dns = StaticDns::new()
        .txt("s1._domainkey.example.com", &key.record)
        .txt("_dmarc.example.com", "v=DMARC1; p=reject; adkim=s");

    let report = verify(dns, &message, &VerifyContext::new()).await;

    // No DKIM pass in strict alignment and no SPF: DMARC can't be decided here.
    assert_eq!(report.dmarc.result, MechanismResult::NotAvailable);
    assert_eq!(report.verdict, Verdict::Unverified);
    assert_eq!(
        report.explanation,
        "Signed by example.com, but news.example.com only accepts signatures from exactly its own domain."
    );
}

#[tokio::test]
async fn unaligned_signature_without_spf_is_unverified_not_suspicious() {
    let key = ed25519_key();
    let message = dkim_sign(key.key, "esp.example", "mta", &fixture("plain.eml"));
    let dns = StaticDns::new()
        .txt("mta._domainkey.esp.example", &key.record)
        .txt("_dmarc.example.com", "v=DMARC1; p=reject");

    let report = verify(dns, &message, &VerifyContext::new()).await;

    assert_eq!(report.dkim.result, MechanismResult::Pass);
    assert!(!report.dkim_signatures[0].aligned);
    assert_eq!(report.dmarc.result, MechanismResult::NotAvailable);
    assert_eq!(report.aligned_domain, None);
    assert_eq!(report.verdict, Verdict::Unverified);
    assert_eq!(
        report.explanation,
        "Signed by esp.example, not by example.com, the sender's domain."
    );
}

fn reader_example() -> ProviderProfile {
    ProviderProfile::generic(
        "mx.reader.example",
        vec![HostMatcher::Exact("mx.reader.example".into())],
    )
}

fn delivered(results: &str, message: &[u8]) -> Vec<u8> {
    let received = "Received: from mail.sender.example (mail.sender.example [198.51.100.7]) by mx.reader.example (Postfix) with ESMTPS id 1; Tue, 2 Jan 2024 10:00:05 +0000";
    prepend(
        &format!("Authentication-Results: mx.reader.example; {results}"),
        &prepend(received, message),
    )
}

#[tokio::test]
async fn dmarc_fails_when_provider_spf_is_known() {
    let key = ed25519_key();
    let signed = dkim_sign(key.key, "esp.example", "mta", &fixture("plain.eml"));
    let message = delivered("spf=fail smtp.mailfrom=bounce@esp.example", &signed);
    let dns = StaticDns::new()
        .txt("mta._domainkey.esp.example", &key.record)
        .txt("_dmarc.example.com", "v=DMARC1; p=reject");
    let profile = reader_example();
    let ctx = VerifyContext::new().with_provider(&profile);

    let report = verify(dns, &message, &ctx).await;

    assert!(report.spf.result.is_fail());
    assert_eq!(report.spf.source, Source::Provider);
    assert_eq!(report.spf.domain.as_deref(), Some("esp.example"));
    assert!(report.dmarc.result.is_fail());
    assert_eq!(report.dmarc.source, Source::Local);
    assert_eq!(report.verdict, Verdict::Suspicious);
    assert!(report.explanation.contains("claims to be from example.com"));
}

#[tokio::test]
async fn provider_spf_pass_completes_dmarc() {
    // Unsigned mail from an SPF-only sender passes DMARC through the provider's SPF.
    let message = delivered(
        "spf=pass smtp.mailfrom=bounce@example.com",
        &fixture("plain.eml"),
    );
    let dns = StaticDns::new().txt("_dmarc.example.com", "v=DMARC1; p=reject");
    let profile = reader_example();
    let ctx = VerifyContext::new().with_provider(&profile);

    let report = verify(dns, &message, &ctx).await;

    assert_eq!(report.dkim.result, MechanismResult::None);
    assert_eq!(report.spf.result, MechanismResult::Pass);
    assert_eq!(report.dmarc.result, MechanismResult::Pass);
    assert_eq!(report.dmarc.source, Source::Local);
    assert_eq!(report.aligned_domain.as_deref(), Some("example.com"));
    assert_eq!(report.verdict, Verdict::Verified);
}

#[tokio::test]
async fn provider_pass_wins_over_rotated_key() {
    let key = ed25519_key();
    let signed = dkim_sign(key.key, "example.com", "old", &fixture("plain.eml"));
    let message = delivered(
        "dkim=pass header.d=example.com header.s=old; spf=pass smtp.mailfrom=example.com; dmarc=pass header.from=example.com",
        &signed,
    );
    let dns = StaticDns::new().txt("_dmarc.example.com", "v=DMARC1; p=reject");
    let profile = reader_example();
    let ctx = VerifyContext::new()
        .with_provider(&profile)
        .with_received_at(now() - Duration::days(900));

    let report = verify(dns, &message, &ctx).await;

    assert_eq!(
        report.provider,
        ProviderStatus::Trusted {
            authserv_id: Some("mx.reader.example".into())
        }
    );
    assert_eq!(report.dkim.result, MechanismResult::Pass);
    assert_eq!(report.dkim.source, Source::Provider);
    assert_eq!(report.dmarc.result, MechanismResult::Pass);
    assert_eq!(report.verdict, Verdict::Verified);
    // Both views are kept: the local one says the key is gone.
    let local = report
        .dkim_signatures
        .iter()
        .find(|s| s.source == Source::Local)
        .unwrap();
    assert_eq!(local.result, MechanismResult::KeyNoLongerPublished);
}

#[tokio::test]
async fn provider_dmarc_pass_is_verified_by_provider() {
    let message = delivered("dmarc=pass header.from=example.com", &fixture("plain.eml"));
    let profile = reader_example();
    let ctx = VerifyContext::new().with_provider(&profile);

    let report = verify(StaticDns::new(), &message, &ctx).await;

    assert_eq!(report.dmarc.source, Source::Provider);
    assert_eq!(report.verdict, Verdict::Verified);
    assert_eq!(
        report.explanation,
        "Your mail provider confirmed at delivery that this message comes from example.com."
    );
}

#[tokio::test]
async fn forged_provider_header_is_ignored() {
    // The sender wrote its own "dmarc=pass" below the provider's Received header.
    let received = "Received: from mail.evil.example (mail.evil.example [198.51.100.66]) by mx.reader.example (Postfix) with ESMTPS id 2; Tue, 2 Jan 2024 10:00:05 +0000";
    let forged = prepend(
        "Authentication-Results: mx.reader.example; dkim=pass header.d=example.com; dmarc=pass header.from=example.com",
        &fixture("plain.eml"),
    );
    let message = prepend(received, &forged);
    let dns = StaticDns::new().txt("_dmarc.example.com", "v=DMARC1; p=reject");
    let profile = reader_example();
    let ctx = VerifyContext::new().with_provider(&profile);

    let report = verify(dns, &message, &ctx).await;

    assert_eq!(report.provider, ProviderStatus::NotFound);
    assert_eq!(report.dkim.result, MechanismResult::None);
    assert_ne!(report.verdict, Verdict::Verified);
    assert_eq!(report.spf.result, MechanismResult::NotAvailable);
}

#[tokio::test]
async fn arc_chain_passes_after_list_modification() {
    let author = ed25519_key();
    let list = ed25519_key();
    let signed = dkim_sign(author.key, "example.com", "s1", &fixture("plain.eml"));
    // The mailing list adds a footer, which breaks the author's signature, and seals.
    let forwarded = replace(&signed, "Anna\r\n", "Anna\r\n\r\n-- \r\nlist footer\r\n");
    let sealed = arc_seal(list.key, "lists.example", "arc", &forwarded);
    let dns = StaticDns::new()
        .txt("s1._domainkey.example.com", &author.record)
        .txt("arc._domainkey.lists.example", &list.record);

    let report = verify(dns, &sealed, &VerifyContext::new()).await;

    assert_eq!(report.arc.result, MechanismResult::Pass);
    assert_eq!(report.arc.source, Source::Local);
    assert_eq!(report.arc.domain.as_deref(), Some("lists.example"));
    assert!(report.dkim.result.is_fail());
    // ARC alone is no proof of the original sender.
    assert_eq!(report.verdict, Verdict::Unverified);
}

#[tokio::test]
async fn tampered_arc_chain_fails() {
    let list = ed25519_key();
    let sealed = arc_seal(list.key, "lists.example", "arc", &fixture("plain.eml"));
    let tampered = replace(&sealed, "Subject: Quarterly numbers", "Subject: Changed");
    let dns = StaticDns::new().txt("arc._domainkey.lists.example", &list.record);

    let report = verify(dns, &tampered, &VerifyContext::new()).await;

    assert!(report.arc.result.is_fail(), "{:?}", report.arc.result);
}

#[tokio::test]
async fn unsigned_mail_is_unverified() {
    let report = verify(
        StaticDns::new(),
        &fixture("plain.eml"),
        &VerifyContext::new(),
    )
    .await;
    assert_eq!(report.verdict, Verdict::Unverified);
    assert_eq!(report.dkim.result, MechanismResult::None);
    assert_eq!(report.dmarc.result, MechanismResult::None);
    assert_eq!(
        report.explanation,
        "This message isn't signed by example.com, so the sender can't be verified."
    );
}

#[tokio::test]
async fn garbage_is_unverified() {
    for input in [
        &b""[..],
        b"\x00\x01\x02",
        b"no headers here",
        b"\r\n\r\nbody only",
    ] {
        let report = verify(StaticDns::new(), input, &VerifyContext::new()).await;
        assert_eq!(report.verdict, Verdict::Unverified);
    }
}

#[tokio::test]
async fn report_round_trips_through_json() {
    let key = ed25519_key();
    let message = dkim_sign(key.key, "example.com", "s1", &fixture("plain.eml"));
    let dns = StaticDns::new()
        .txt("s1._domainkey.example.com", &key.record)
        .txt("_dmarc.example.com", "v=DMARC1; p=reject");
    let report = verify(dns, &message, &VerifyContext::new()).await;

    let json = serde_json::to_string(&report).unwrap();
    let back: AuthReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back, report);
    assert_eq!(back.schema, AUTH_REPORT_SCHEMA);

    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["verdict"], "verified");
    assert_eq!(value["dkim"]["result"]["status"], "pass");
    assert_eq!(value["spf"]["result"]["status"], "not_available");
    assert_eq!(value["provider"]["status"], "not_configured");

    // Debug output never shows domains or the explanation.
    let debug = format!("{report:?}");
    assert!(!debug.contains("example.com"), "{debug}");
}

#[test]
fn verify_future_is_send() {
    fn assert_send<T: Send>(_: T) {}
    let verifier = Verifier::new(Resolver::from_static(StaticDns::new()).unwrap());
    let ctx = VerifyContext::new();
    assert_send(verifier.verify(b"", &ctx));
}
