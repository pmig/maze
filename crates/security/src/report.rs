//! The verification result model, shown in the message header and stored as
//! `message.auth_json`.

use std::fmt;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::provider::ProviderStatus;

/// Version of the serialized [`AuthReport`]. Bump it when the JSON shape changes
/// incompatibly; the store re-verifies reports with an older version.
pub const AUTH_REPORT_SCHEMA: u32 = 1;

/// The result of one mechanism (SPF, DKIM, DMARC or ARC).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MechanismResult {
    /// The check passed.
    Pass,
    /// The check failed.
    Fail {
        /// A short English explanation, written by Maze.
        reason: String,
    },
    /// Nothing to check: no signature, no published policy.
    None,
    /// The check ran but says neither pass nor fail.
    Neutral,
    /// A transient error, usually DNS. Worth retrying later.
    TempError,
    /// A permanent error: malformed signature or record, unsupported algorithm.
    PermError,
    /// The check can't be done in Maze. SPF needs the connecting IP address, so it
    /// is only known from a trusted provider result.
    NotAvailable,
    /// The DKIM key the message was signed with is no longer in DNS. Keys get
    /// rotated, so on older mail this is expected and doesn't mean the message was
    /// forged.
    KeyNoLongerPublished,
}

impl MechanismResult {
    /// A `Fail` with this reason.
    pub fn fail(reason: impl Into<String>) -> Self {
        Self::Fail {
            reason: reason.into(),
        }
    }

    /// Whether this is [`MechanismResult::Pass`].
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    /// Whether this is [`MechanismResult::Fail`].
    pub fn is_fail(&self) -> bool {
        matches!(self, Self::Fail { .. })
    }

    /// A short English label for the header row ("pass", "fail", …).
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail { .. } => "fail",
            Self::None => "none",
            Self::Neutral => "neutral",
            Self::TempError => "temporary error",
            Self::PermError => "error",
            Self::NotAvailable => "not available",
            Self::KeyNoLongerPublished => "key no longer published",
        }
    }
}

impl fmt::Display for MechanismResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fail { reason } => write!(f, "fail ({reason})"),
            other => f.write_str(other.label()),
        }
    }
}

/// Who produced a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Maze checked it (or, for [`MechanismResult::NotAvailable`], couldn't).
    Local,
    /// The account's provider checked it at delivery, and its header was trusted.
    Provider,
}

/// The result of one mechanism, with where it came from and what domain it is
/// about.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub result: MechanismResult,
    pub source: Source,
    /// The domain the result is about: the DKIM signing domain (`d=`), the SPF
    /// envelope domain, the DMARC `From` domain, or the ARC sealer of the newest set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

impl Check {
    pub(crate) fn new(result: MechanismResult, source: Source, domain: Option<String>) -> Self {
        Self {
            result,
            source,
            domain,
        }
    }

    pub(crate) fn local(result: MechanismResult) -> Self {
        Self::new(result, Source::Local, None)
    }
}

impl fmt::Debug for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Check")
            .field("result", &self.result)
            .field("source", &self.source)
            .field("domain", &self.domain.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// One DKIM signature.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DkimSignatureCheck {
    pub result: MechanismResult,
    pub source: Source,
    /// The signing domain (`d=`).
    pub domain: String,
    /// The selector (`s=`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// Whether the signing domain is aligned (relaxed) with the `From` domain.
    pub aligned: bool,
}

impl fmt::Debug for DkimSignatureCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DkimSignatureCheck")
            .field("result", &self.result)
            .field("source", &self.source)
            .field("aligned", &self.aligned)
            .finish_non_exhaustive()
    }
}

/// The published DMARC policy of the `From` domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DmarcPolicy {
    None,
    Quarantine,
    Reject,
}

/// The overall assessment shown in the verification row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The sender's domain authenticated the message (DMARC pass, or an aligned
    /// DKIM signature).
    Verified,
    /// Nothing proves or disproves the sender: no signature, a rotated key, a
    /// signature from another domain, a DNS error.
    Unverified,
    /// The message claims a sender whose domain says it didn't send it (DMARC fail).
    Suspicious,
}

/// The full verification result for one message.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthReport {
    /// [`AUTH_REPORT_SCHEMA`] at the time of writing.
    pub schema: u32,
    /// The overall assessment.
    pub verdict: Verdict,
    /// One or two English sentences explaining the verdict.
    pub explanation: String,
    /// The domain of the `From` address, if there is exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_domain: Option<String>,
    /// The domain that authenticated in alignment with `From` (an aligned DKIM
    /// `d=` or SPF domain), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aligned_domain: Option<String>,
    pub spf: Check,
    /// The best DKIM signature (aligned passes first).
    pub dkim: Check,
    pub dmarc: Check,
    pub arc: Check,
    /// The published DMARC policy, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dmarc_policy: Option<DmarcPolicy>,
    /// Every DKIM signature checked, local and provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dkim_signatures: Vec<DkimSignatureCheck>,
    /// Whether a trusted provider header was found.
    pub provider: ProviderStatus,
    /// When the check ran.
    #[serde(with = "time::serde::rfc3339")]
    pub checked_at: OffsetDateTime,
}

impl AuthReport {
    /// Whether the report should be recomputed: it has a temporary error or was
    /// written by an older schema.
    pub fn needs_recheck(&self) -> bool {
        self.schema != AUTH_REPORT_SCHEMA
            || [&self.dkim, &self.dmarc, &self.arc]
                .iter()
                .any(|c| c.result == MechanismResult::TempError)
    }
}

impl fmt::Debug for AuthReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthReport")
            .field("schema", &self.schema)
            .field("verdict", &self.verdict)
            .field("spf", &self.spf)
            .field("dkim", &self.dkim)
            .field("dmarc", &self.dmarc)
            .field("arc", &self.arc)
            .field("dmarc_policy", &self.dmarc_policy)
            .field("dkim_signatures", &self.dkim_signatures)
            .field("provider", &self.provider)
            .field("checked_at", &self.checked_at)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mechanism_result_json_shape() {
        let json = serde_json::to_string(&MechanismResult::fail("body was modified")).unwrap();
        assert_eq!(json, r#"{"status":"fail","reason":"body was modified"}"#);
        let json = serde_json::to_string(&MechanismResult::KeyNoLongerPublished).unwrap();
        assert_eq!(json, r#"{"status":"key_no_longer_published"}"#);
        let back: MechanismResult = serde_json::from_str(r#"{"status":"not_available"}"#).unwrap();
        assert_eq!(back, MechanismResult::NotAvailable);
    }

    #[test]
    fn labels_and_display() {
        assert_eq!(
            MechanismResult::KeyNoLongerPublished.to_string(),
            "key no longer published"
        );
        assert_eq!(MechanismResult::fail("x").to_string(), "fail (x)");
        assert_eq!(MechanismResult::NotAvailable.label(), "not available");
    }

    #[test]
    fn verdict_orders_by_severity() {
        assert!(Verdict::Verified < Verdict::Unverified);
        assert!(Verdict::Unverified < Verdict::Suspicious);
    }
}
