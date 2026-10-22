//! Sender verification: local DKIM, ARC and DMARC, merged with trusted provider
//! results into an [`AuthReport`].

use mail_auth::arc::ArcError;
use mail_auth::common::crypto::CryptoError;
use mail_auth::dkim::DkimError;
use mail_auth::dmarc::Policy;
use mail_auth::dmarc::verify::DmarcParameters;
use mail_auth::{
    AuthenticatedMessage, DkimResult, DmarcResult, DnsError, Error, Parameters, SpfOutput,
    SpfResult,
};
use time::{Duration, OffsetDateTime};

use crate::dns::Resolver;
use crate::domain::relaxed_aligned;
use crate::headers;
use crate::normalize::ProviderResults;
use crate::provider::{ProviderOutcome, ProviderProfile, ProviderStatus};
use crate::report::{
    AUTH_REPORT_SCHEMA, AuthReport, Check, DkimSignatureCheck, DmarcPolicy, MechanismResult,
    Source, Verdict,
};

/// Tunables for [`Verifier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyOptions {
    /// A DKIM key that is missing from DNS counts as rotated
    /// ([`MechanismResult::KeyNoLongerPublished`]) once the message is at least this
    /// old. On newer mail a missing key is a [`MechanismResult::PermError`].
    pub key_rotation_age: Duration,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            key_rotation_age: Duration::days(7),
        }
    }
}

/// Per-message input besides the raw bytes.
#[derive(Debug, Clone, Copy)]
pub struct VerifyContext<'a> {
    /// The account's provider, to find its trusted `Authentication-Results`.
    pub provider: Option<&'a ProviderProfile>,
    /// When the server received the message (IMAP `INTERNALDATE`). Used to tell
    /// rotated keys from missing ones; falls back to the signature's `t=` tag and
    /// then the `Date` header.
    pub received_at: Option<OffsetDateTime>,
    /// The current time.
    pub now: OffsetDateTime,
}

impl Default for VerifyContext<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> VerifyContext<'a> {
    /// No provider, unknown receive time, the current time.
    pub fn new() -> Self {
        Self {
            provider: None,
            received_at: None,
            now: OffsetDateTime::now_utc(),
        }
    }

    /// Trust headers from this provider.
    #[must_use]
    pub fn with_provider(mut self, provider: &'a ProviderProfile) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Set the server's receive time.
    #[must_use]
    pub fn with_received_at(mut self, received_at: OffsetDateTime) -> Self {
        self.received_at = Some(received_at);
        self
    }

    /// Evaluate as of `now` instead of the current time.
    #[must_use]
    pub fn at(mut self, now: OffsetDateTime) -> Self {
        self.now = now;
        self
    }
}

/// Verifies messages. Cheap to clone; clones share the DNS cache.
#[derive(Debug, Clone)]
pub struct Verifier {
    resolver: Resolver,
    options: VerifyOptions,
}

impl Verifier {
    /// A verifier with default options.
    pub fn new(resolver: Resolver) -> Self {
        Self::with_options(resolver, VerifyOptions::default())
    }

    /// A verifier with explicit options.
    pub fn with_options(resolver: Resolver, options: VerifyOptions) -> Self {
        Self { resolver, options }
    }

    /// Verify one message.
    ///
    /// Never fails: input that isn't a message gives an
    /// [`Verdict::Unverified`] report saying so, and DNS problems show up as
    /// [`MechanismResult::TempError`] (see [`AuthReport::needs_recheck`]).
    pub async fn verify(&self, raw_message: &[u8], ctx: &VerifyContext<'_>) -> AuthReport {
        let scanned = headers::scan(raw_message);
        let provider = match ctx.provider {
            Some(profile) => profile.evaluate_headers(&scanned),
            None => ProviderOutcome {
                status: ProviderStatus::NotConfigured,
                header: None,
                kind: crate::provider::ProviderKind::Generic,
            },
        };
        let trusted = provider.results();
        let spf = spf_check(&trusted);

        let Some(message) = AuthenticatedMessage::parse(raw_message) else {
            return unreadable(provider.status, spf, ctx.now);
        };
        let from_domain = single_from_domain(&message.from);
        let local = self
            .local_checks(&message, ctx, raw_message, &trusted, from_domain.as_deref())
            .await;

        let report = assemble(Assembly {
            from_domain,
            spf,
            local,
            trusted: &trusted,
            provider: provider.status,
            now: ctx.now,
        });
        // Only the outcome: domains and header content are message data.
        tracing::debug!(
            verdict = ?report.verdict,
            dkim = report.dkim.result.label(),
            dmarc = report.dmarc.result.label(),
            arc = report.arc.result.label(),
            provider = ?report.provider,
            "verified message"
        );
        report
    }

    async fn local_checks(
        &self,
        message: &AuthenticatedMessage<'_>,
        ctx: &VerifyContext<'_>,
        raw_message: &[u8],
        trusted: &ProviderResults,
        from_domain: Option<&str>,
    ) -> LocalChecks {
        let authenticator = &self.resolver.authenticator;
        let dkim_outputs = authenticator
            .verify_dkim(
                Parameters::new(message)
                    .with_txt_cache(&self.resolver.txt)
                    .with_ipv4_cache(&self.resolver.ipv4),
            )
            .await;
        let arc_output = authenticator
            .verify_arc(
                Parameters::new(message)
                    .with_txt_cache(&self.resolver.txt)
                    .with_ipv4_cache(&self.resolver.ipv4),
            )
            .await;

        let mut age = MessageAge::new(ctx, raw_message, self.options.key_rotation_age);
        let mut signatures = Vec::new();
        let mut malformed_signatures = false;
        for output in &dkim_outputs {
            let Some(signature) = output.signature() else {
                malformed_signatures = true;
                continue;
            };
            let result = map_auth_result(output.result(), || age.is_old(signature.t));
            let domain = signature.d.to_ascii_lowercase();
            signatures.push(DkimSignatureCheck {
                aligned: from_domain.is_some_and(|from| relaxed_aligned(&domain, from)),
                result,
                source: Source::Local,
                domain,
                selector: Some(signature.s.to_ascii_lowercase()),
            });
        }

        let arc_domain = arc_output
            .sets()
            .last()
            .map(|set| set.seal.header.d.to_ascii_lowercase());
        let arc_signed_at = arc_output.sets().last().map_or(0, |set| set.seal.header.t);
        let arc = Check::new(
            map_auth_result(arc_output.result(), || age.is_old(arc_signed_at)),
            Source::Local,
            arc_domain,
        );

        // DMARC: SPF can't be checked here; feed in the trusted provider's result if
        // there is one, otherwise "none" so only DKIM alignment counts.
        let (spf_domain, spf_result) = match &trusted.spf {
            Some(spf) => match (&spf.result, &spf.domain) {
                (MechanismResult::Pass, Some(domain)) => (domain.clone(), SpfResult::Pass),
                (MechanismResult::TempError, Some(domain)) => {
                    (domain.clone(), SpfResult::TempError)
                }
                _ => (String::new(), SpfResult::None),
            },
            None => (String::new(), SpfResult::None),
        };
        // SPF is "known" when the provider's result can't have produced an aligned
        // pass we're not seeing (a pass without a domain could have).
        let spf_known = trusted
            .spf
            .as_ref()
            .is_some_and(|spf| !spf.result.is_pass() || spf.domain.is_some());
        let spf_output = SpfOutput::new(spf_domain.clone()).with_result(spf_result);
        let dmarc_output = authenticator
            .verify_dmarc(
                Parameters::new(DmarcParameters::new(
                    message,
                    &dkim_outputs,
                    &spf_domain,
                    &spf_output,
                ))
                .with_txt_cache(&self.resolver.txt)
                .with_ipv4_cache(&self.resolver.ipv4),
            )
            .await;

        let has_record = dmarc_output.dmarc_record().is_some();
        let dmarc_policy = has_record.then(|| match dmarc_output.policy() {
            Policy::Reject => DmarcPolicy::Reject,
            Policy::Quarantine => DmarcPolicy::Quarantine,
            Policy::None | Policy::Unspecified => DmarcPolicy::None,
        });
        let aligned_rotated = signatures
            .iter()
            .any(|s| s.aligned && s.result == MechanismResult::KeyNoLongerPublished);
        let dmarc_result = match dmarc_output.result() {
            _ if from_domain.is_none() => MechanismResult::None,
            DmarcResult::Pass => MechanismResult::Pass,
            DmarcResult::TempError(_) => MechanismResult::TempError,
            DmarcResult::PermError(_) => MechanismResult::PermError,
            DmarcResult::None => MechanismResult::None,
            DmarcResult::Fail(_) if aligned_rotated => MechanismResult::KeyNoLongerPublished,
            // Without SPF, a missing DKIM pass doesn't mean DMARC failed: the sender
            // may rely on SPF alone.
            DmarcResult::Fail(_) if !spf_known => MechanismResult::NotAvailable,
            DmarcResult::Fail(_) => {
                MechanismResult::fail("no signature or SPF result matches the sender's domain")
            }
        };

        LocalChecks {
            signatures,
            malformed_signatures,
            arc,
            dmarc: Check::new(dmarc_result, Source::Local, from_domain.map(str::to_owned)),
            dmarc_policy,
        }
    }
}

struct LocalChecks {
    signatures: Vec<DkimSignatureCheck>,
    malformed_signatures: bool,
    arc: Check,
    dmarc: Check,
    dmarc_policy: Option<DmarcPolicy>,
}

/// Lazily works out whether the message is old enough for a missing key to count as
/// rotated.
struct MessageAge<'a> {
    now: OffsetDateTime,
    received_at: Option<OffsetDateTime>,
    raw_message: &'a [u8],
    threshold: Duration,
    date_header: Option<Option<OffsetDateTime>>,
}

impl<'a> MessageAge<'a> {
    fn new(ctx: &VerifyContext<'_>, raw_message: &'a [u8], threshold: Duration) -> Self {
        Self {
            now: ctx.now,
            received_at: ctx.received_at,
            raw_message,
            threshold,
            date_header: None,
        }
    }

    /// Whether the message is at least `threshold` old. `signed_at` is the
    /// signature's `t=` tag (0 when absent). Unknown age counts as new.
    fn is_old(&mut self, signed_at: u64) -> bool {
        let reference = self
            .received_at
            .or_else(|| {
                (signed_at > 0)
                    .then(|| i64::try_from(signed_at).ok())
                    .flatten()
                    .and_then(|t| OffsetDateTime::from_unix_timestamp(t).ok())
            })
            .or_else(|| {
                let raw = self.raw_message;
                *self.date_header.get_or_insert_with(|| {
                    mazemail_mime::parse_headers_only(raw)
                        .ok()
                        .and_then(|h| h.date)
                })
            });
        reference.is_some_and(|at| self.now - at >= self.threshold)
    }
}

/// Map a `mail-auth` DKIM or ARC result. `key_rotated` says whether a missing key
/// should count as rotated; it is only called when the key is missing.
fn map_auth_result(result: &DkimResult, key_rotated: impl FnOnce() -> bool) -> MechanismResult {
    let (err, temporary) = match result {
        DkimResult::Pass => return MechanismResult::Pass,
        DkimResult::None => return MechanismResult::None,
        // A failed check of the signature itself. Ed25519 reports this as a crypto
        // library error, RSA as `FailedVerification`.
        DkimResult::Fail(Error::Crypto(
            CryptoError::FailedVerification | CryptoError::Library(_),
        )) => return MechanismResult::fail("the signature doesn't match the message"),
        DkimResult::TempError(err) => (err, true),
        DkimResult::Neutral(err) | DkimResult::Fail(err) | DkimResult::PermError(err) => {
            (err, false)
        }
    };
    match err {
        Error::Dkim(DkimError::FailedBodyHashMatch) | Error::Arc(ArcError::FailedBodyHashMatch) => {
            MechanismResult::fail("the message body was changed after signing")
        }
        Error::Crypto(CryptoError::FailedVerification) => {
            MechanismResult::fail("the signature doesn't match the message")
        }
        Error::Dkim(DkimError::FailedAuidMatch) => {
            MechanismResult::fail("the signer identity doesn't match the signing domain")
        }
        Error::Arc(
            ArcError::BrokenChain
            | ArcError::InvalidCV
            | ArcError::InvalidInstance(_)
            | ArcError::ChainTooLong,
        ) => MechanismResult::fail("the ARC chain is broken"),
        Error::Dkim(DkimError::SignatureExpired) | Error::Arc(ArcError::SignatureExpired) => {
            MechanismResult::Neutral
        }
        Error::Dns(DnsError::RecordNotFound(_) | DnsError::InvalidRecordType)
        | Error::Dkim(DkimError::RevokedPublicKey) => {
            if key_rotated() {
                MechanismResult::KeyNoLongerPublished
            } else {
                MechanismResult::PermError
            }
        }
        Error::Dns(DnsError::Resolver(_)) => MechanismResult::TempError,
        _ if temporary => MechanismResult::TempError,
        _ => MechanismResult::PermError,
    }
}

fn spf_check(trusted: &ProviderResults) -> Check {
    match &trusted.spf {
        Some(spf) => Check::new(spf.result.clone(), Source::Provider, spf.domain.clone()),
        None => Check::local(MechanismResult::NotAvailable),
    }
}

/// The From domain if every From address has the same one.
fn single_from_domain(from: &[String]) -> Option<String> {
    let mut domains = from.iter().filter_map(|address| {
        address
            .rsplit_once('@')
            .map(|(_, domain)| domain.trim().trim_end_matches('>').to_ascii_lowercase())
    });
    let first = domains.next().filter(|d| !d.is_empty())?;
    domains.all(|d| d == first).then_some(first)
}

fn unreadable(provider: ProviderStatus, spf: Check, now: OffsetDateTime) -> AuthReport {
    AuthReport {
        schema: AUTH_REPORT_SCHEMA,
        verdict: Verdict::Unverified,
        explanation: "Maze couldn't read the message headers, so the sender can't be verified."
            .to_owned(),
        from_domain: None,
        aligned_domain: None,
        spf,
        dkim: Check::local(MechanismResult::None),
        dmarc: Check::local(MechanismResult::None),
        arc: Check::local(MechanismResult::None),
        dmarc_policy: None,
        dkim_signatures: Vec::new(),
        provider,
        checked_at: now,
    }
}

struct Assembly<'a> {
    from_domain: Option<String>,
    spf: Check,
    local: LocalChecks,
    trusted: &'a ProviderResults,
    provider: ProviderStatus,
    now: OffsetDateTime,
}

/// Merge local and provider results and work out the verdict.
///
/// A pass wins from either source: the provider's header is trusted, and it saw
/// the message at delivery, before keys were rotated or a server rewrote the
/// message. Otherwise local results win, except where Maze has nothing to say.
fn assemble(input: Assembly<'_>) -> AuthReport {
    let Assembly {
        from_domain,
        spf,
        local,
        trusted,
        provider,
        now,
    } = input;
    let from = from_domain.as_deref();

    let mut signatures = local.signatures;
    signatures.extend(trusted.dkim.iter().filter_map(|dkim| {
        let domain = dkim.domain.clone()?;
        Some(DkimSignatureCheck {
            aligned: from.is_some_and(|from| relaxed_aligned(&domain, from)),
            result: dkim.result.clone(),
            source: Source::Provider,
            domain,
            selector: dkim.selector.clone(),
        })
    }));
    let dkim = signatures
        .iter()
        .min_by_key(|s| (dkim_rank(s), s.source == Source::Provider))
        .map(|s| Check::new(s.result.clone(), s.source, Some(s.domain.clone())))
        .unwrap_or_else(|| {
            Check::local(if local.malformed_signatures {
                MechanismResult::PermError
            } else {
                MechanismResult::None
            })
        });

    let provider_dmarc = trusted.dmarc.as_ref().map(|dmarc| {
        Check::new(
            dmarc.result.clone(),
            Source::Provider,
            dmarc.domain.clone().or_else(|| from_domain.clone()),
        )
    });
    let dmarc = match provider_dmarc {
        Some(provider) if !local.dmarc.result.is_pass() => provider,
        _ => local.dmarc,
    };

    let arc = match &trusted.arc {
        Some(arc) if local.arc.result == MechanismResult::None => {
            Check::new(arc.result.clone(), Source::Provider, None)
        }
        _ => local.arc,
    };

    let aligned_domain = aligned_domain(&dmarc, &signatures, &spf, from);
    let (verdict, explanation) = assess(&Facts {
        from,
        dmarc: &dmarc,
        signatures: &signatures,
        aligned_domain: aligned_domain.as_deref(),
    });

    AuthReport {
        schema: AUTH_REPORT_SCHEMA,
        verdict,
        explanation,
        from_domain,
        aligned_domain,
        spf,
        dkim,
        dmarc,
        arc,
        dmarc_policy: local.dmarc_policy,
        dkim_signatures: signatures,
        provider,
        checked_at: now,
    }
}

/// Lower is better: aligned passes first, failures of aligned signatures before
/// noise from unrelated ones.
fn dkim_rank(signature: &DkimSignatureCheck) -> u8 {
    match (&signature.result, signature.aligned) {
        (MechanismResult::Pass, true) => 0,
        (MechanismResult::Pass, false) => 1,
        (MechanismResult::KeyNoLongerPublished, true) => 2,
        (MechanismResult::Fail { .. }, true) => 3,
        (MechanismResult::TempError, _) => 4,
        (MechanismResult::Fail { .. }, false) => 5,
        (MechanismResult::KeyNoLongerPublished, false) => 6,
        (MechanismResult::Neutral, _) => 7,
        (MechanismResult::PermError, _) => 8,
        (MechanismResult::None | MechanismResult::NotAvailable, _) => 9,
    }
}

fn aligned_domain(
    dmarc: &Check,
    signatures: &[DkimSignatureCheck],
    spf: &Check,
    from: Option<&str>,
) -> Option<String> {
    let from = from?;
    let dkim = signatures
        .iter()
        .filter(|s| s.aligned && s.result.is_pass())
        .min_by_key(|s| s.source == Source::Provider)
        .map(|s| s.domain.clone());
    let spf = || {
        spf.domain
            .clone()
            .filter(|d| spf.result.is_pass() && relaxed_aligned(d, from))
    };
    match dkim.or_else(spf) {
        Some(domain) => Some(domain),
        // The provider says DMARC passed but didn't say through what.
        None if dmarc.result.is_pass() => Some(from.to_owned()),
        None => None,
    }
}

struct Facts<'a> {
    from: Option<&'a str>,
    dmarc: &'a Check,
    signatures: &'a [DkimSignatureCheck],
    aligned_domain: Option<&'a str>,
}

/// The verdict and its explanation. Suspicious needs evidence (a DMARC fail);
/// everything that merely lacks proof is unverified.
fn assess(facts: &Facts<'_>) -> (Verdict, String) {
    let Some(from) = facts.from else {
        return (
            Verdict::Unverified,
            "The message doesn't have a single sender domain, so the sender can't be verified."
                .to_owned(),
        );
    };
    let aligned = |result: fn(&MechanismResult) -> bool| {
        facts
            .signatures
            .iter()
            .find(|s| s.aligned && result(&s.result))
    };

    if facts.dmarc.result.is_pass() {
        let domain = facts.aligned_domain.unwrap_or(from);
        let text = match facts.dmarc.source {
            Source::Provider => {
                format!(
                    "Your mail provider confirmed at delivery that this message comes from {domain}."
                )
            }
            Source::Local => format!("Signed by {domain}, which matches the sender address."),
        };
        return (Verdict::Verified, text);
    }
    if facts.dmarc.result.is_fail() {
        return (
            Verdict::Suspicious,
            format!(
                "This message claims to be from {from}, but it failed {from}'s sender check (DMARC). It may be forged."
            ),
        );
    }
    // An aligned signature is the sender's own proof, unless DMARC had its say:
    // a DMARC result that isn't a pass here means the policy wants strict
    // alignment, which a parent or sibling domain doesn't meet.
    if let Some(signature) = aligned(MechanismResult::is_pass) {
        match facts.dmarc.result {
            MechanismResult::NotAvailable => {
                return (
                    Verdict::Unverified,
                    format!(
                        "Signed by {}, but {from} only accepts signatures from exactly its own domain.",
                        signature.domain
                    ),
                );
            }
            MechanismResult::TempError => {}
            _ => {
                return (
                    Verdict::Verified,
                    format!(
                        "Signed by {}, which matches the sender address.",
                        signature.domain
                    ),
                );
            }
        }
    }
    if facts.dmarc.result == MechanismResult::KeyNoLongerPublished
        || aligned(|r| *r == MechanismResult::KeyNoLongerPublished).is_some()
    {
        return (
            Verdict::Unverified,
            format!(
                "The signing key of {from} is no longer published. That's normal for older mail; the signature just can't be checked anymore."
            ),
        );
    }
    if facts.dmarc.result == MechanismResult::TempError
        || facts
            .signatures
            .iter()
            .any(|s| s.result == MechanismResult::TempError)
    {
        return (
            Verdict::Unverified,
            "Maze couldn't look up the signing key right now and will try again later.".to_owned(),
        );
    }
    if let Some(signature) = aligned(MechanismResult::is_fail) {
        return (
            Verdict::Unverified,
            format!(
                "The signature of {} doesn't match anymore: the message was changed after it was signed, as mailing lists often do.",
                signature.domain
            ),
        );
    }
    if let Some(signature) = facts.signatures.iter().find(|s| s.result.is_pass()) {
        return (
            Verdict::Unverified,
            format!(
                "Signed by {}, not by {from}, the sender's domain.",
                signature.domain
            ),
        );
    }
    (
        Verdict::Unverified,
        format!("This message isn't signed by {from}, so the sender can't be verified."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_mail_auth_results() {
        let never = || -> bool { unreachable!("only called for missing keys") };
        assert_eq!(
            map_auth_result(&DkimResult::Pass, never),
            MechanismResult::Pass
        );
        assert_eq!(
            map_auth_result(&DkimResult::None, never),
            MechanismResult::None
        );
        assert!(
            map_auth_result(
                &DkimResult::Neutral(Error::Dkim(DkimError::FailedBodyHashMatch)),
                never
            )
            .is_fail()
        );
        assert_eq!(
            map_auth_result(
                &DkimResult::Neutral(Error::Dkim(DkimError::SignatureExpired)),
                never
            ),
            MechanismResult::Neutral
        );
        assert_eq!(
            map_auth_result(
                &DkimResult::TempError(Error::Dns(DnsError::Resolver("timeout".into()))),
                never
            ),
            MechanismResult::TempError
        );
        let gone = DkimResult::PermError(Error::Dkim(DkimError::RevokedPublicKey));
        assert_eq!(
            map_auth_result(&gone, || true),
            MechanismResult::KeyNoLongerPublished
        );
        assert_eq!(map_auth_result(&gone, || false), MechanismResult::PermError);
        assert_eq!(
            map_auth_result(
                &DkimResult::PermError(Error::Dkim(DkimError::UnsupportedAlgorithm)),
                never
            ),
            MechanismResult::PermError
        );
    }

    #[test]
    fn from_domain_must_be_unique() {
        assert_eq!(
            single_from_domain(&["a@Example.com".into(), "b@example.com".into()]).as_deref(),
            Some("example.com")
        );
        assert_eq!(
            single_from_domain(&["a@example.com".into(), "b@example.net".into()]),
            None
        );
        assert_eq!(single_from_domain(&[]), None);
        assert_eq!(single_from_domain(&["no-at-sign".into()]), None);
    }

    fn signature(result: MechanismResult, aligned: bool, source: Source) -> DkimSignatureCheck {
        DkimSignatureCheck {
            result,
            source,
            domain: if aligned {
                "example.com"
            } else {
                "esp.example"
            }
            .to_owned(),
            selector: None,
            aligned,
        }
    }

    #[test]
    fn ranks_signatures() {
        let mut list = [
            signature(MechanismResult::fail("x"), false, Source::Local),
            signature(MechanismResult::Pass, false, Source::Local),
            signature(MechanismResult::KeyNoLongerPublished, true, Source::Local),
            signature(MechanismResult::Pass, true, Source::Provider),
        ];
        list.sort_by_key(dkim_rank);
        assert_eq!(list[0].result, MechanismResult::Pass);
        assert!(list[0].aligned);
        assert!(!list[1].aligned);
        assert_eq!(list[2].result, MechanismResult::KeyNoLongerPublished);
    }

    #[test]
    fn assess_needs_evidence_for_suspicious() {
        let none = Check::local(MechanismResult::None);
        let not_available = Check::local(MechanismResult::NotAvailable);
        let facts = |dmarc, signatures| Facts {
            from: Some("example.com"),
            dmarc,
            signatures,
            aligned_domain: None,
        };
        assert_eq!(assess(&facts(&none, &[])).0, Verdict::Unverified);
        assert_eq!(assess(&facts(&not_available, &[])).0, Verdict::Unverified);
        let fail = Check::local(MechanismResult::fail("x"));
        assert_eq!(assess(&facts(&fail, &[])).0, Verdict::Suspicious);
        let modified = [signature(MechanismResult::fail("x"), true, Source::Local)];
        let (verdict, text) = assess(&facts(&none, &modified));
        assert_eq!(verdict, Verdict::Unverified);
        assert!(text.contains("changed after it was signed"));
        let unaligned = [signature(MechanismResult::Pass, false, Source::Local)];
        let (verdict, text) = assess(&facts(&none, &unaligned));
        assert_eq!(verdict, Verdict::Unverified);
        assert!(text.contains("not by example.com"));
        let aligned = [signature(MechanismResult::Pass, true, Source::Local)];
        assert_eq!(assess(&facts(&none, &aligned)).0, Verdict::Verified);
    }
}
