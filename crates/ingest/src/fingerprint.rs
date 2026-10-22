//! Header fingerprints and the body-reuse lookup (PLAN §4.14 "Offline mail reuse").

use std::fmt;

use mazemail_mime::MessageHeaders;
use serde::{Deserialize, Serialize};

/// Domain separation for the fingerprint hash; bump the version if the inputs change.
const CONTEXT: &str = "mazemail 2026 ingest header fingerprint v1";

/// A hash of a message's `Date`, `From` and `Subject` (PLAN §4.14).
///
/// Two copies of the same message (the server's and one in a Thunderbird offline
/// store) have the same fingerprint; together with Message-ID and size it identifies
/// the bytes well enough to reuse a body without downloading it.
///
/// Inputs are normalized: the date as Unix seconds (`None` if missing or invalid), the
/// first `From` address lowercased, the decoded subject with whitespace runs collapsed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HeaderFingerprint(pub [u8; 16]);

impl HeaderFingerprint {
    /// The fingerprint of these normalized inputs.
    pub fn new(date: Option<i64>, from: Option<&str>, subject: Option<&str>) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key(CONTEXT);
        match date {
            Some(date) => {
                hasher.update(&[1]);
                hasher.update(&date.to_le_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
        let from = from.map(|f| f.trim().to_lowercase()).unwrap_or_default();
        hasher.update(&(from.len() as u64).to_le_bytes());
        hasher.update(from.as_bytes());
        let subject = subject
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        hasher.update(&(subject.len() as u64).to_le_bytes());
        hasher.update(subject.as_bytes());
        let mut out = [0u8; 16];
        out.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
        Self(out)
    }

    /// The fingerprint of parsed headers (the `Date` header only, no fallbacks).
    pub fn from_headers(headers: &MessageHeaders) -> Self {
        Self::new(
            headers.date.map(|d| d.unix_timestamp()),
            headers.from.first().map(|a| a.email.as_str()),
            headers.subject.as_deref(),
        )
    }

    /// The fingerprint of a raw header block or whole message; `None` if no header can
    /// be read.
    pub fn from_raw(raw: &[u8]) -> Option<Self> {
        mazemail_mime::parse_headers_only(raw)
            .ok()
            .map(|h| Self::from_headers(&h))
    }

    /// The fingerprint bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// From stored bytes; `None` if the length is wrong.
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }
}

impl fmt::Debug for HeaderFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HeaderFingerprint(")?;
        for byte in &self.0[..4] {
            write!(f, "{byte:02x}")?;
        }
        write!(f, "…)")
    }
}

/// A local source of message bodies, asked during the first sync before downloading.
///
/// Implemented by `import::BodyReuse` (the index of a Thunderbird offline store);
/// `mazemail_core` hands it to sync as `Arc<dyn BodyLookup>`.
///
/// An implementation must return bytes only when they are, with certainty, the
/// server's message: Message-ID, `RFC822.SIZE` and fingerprint all match one
/// distinct candidate. A size-only or ambiguous match returns `None`, and the body is
/// fetched from the network.
pub trait BodyLookup: Send + Sync {
    /// The raw message with this Message-ID (angle brackets optional), size in bytes
    /// and fingerprint, if exactly one known body matches.
    fn lookup(
        &self,
        rfc_message_id: &str,
        size: u64,
        fingerprint: &HeaderFingerprint,
    ) -> Option<Vec<u8>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &[u8] = b"Date: Tue, 1 Oct 2024 10:00:00 +0200\r\n\
From: Anna Example <Anna@Example.com>\r\n\
Subject: Quarterly\r\n  numbers\r\n\
Message-ID: <q@example.com>\r\n\r\nbody\r\n";

    #[test]
    fn same_headers_same_fingerprint() {
        let a = HeaderFingerprint::from_raw(RAW).unwrap();
        let b = HeaderFingerprint::new(
            Some(1_727_769_600),
            Some("anna@example.com"),
            Some("Quarterly numbers"),
        );
        assert_eq!(a, b);
    }

    #[test]
    fn every_input_matters() {
        let base = HeaderFingerprint::new(Some(1), Some("a@x"), Some("s"));
        assert_ne!(
            base,
            HeaderFingerprint::new(Some(2), Some("a@x"), Some("s"))
        );
        assert_ne!(base, HeaderFingerprint::new(None, Some("a@x"), Some("s")));
        assert_ne!(
            base,
            HeaderFingerprint::new(Some(1), Some("b@x"), Some("s"))
        );
        assert_ne!(
            base,
            HeaderFingerprint::new(Some(1), Some("a@x"), Some("t"))
        );
        // Field boundaries can't be shifted.
        assert_ne!(
            HeaderFingerprint::new(None, Some("ab"), Some("c")),
            HeaderFingerprint::new(None, Some("a"), Some("bc"))
        );
    }

    #[test]
    fn round_trips_bytes_and_redacts_debug() {
        let fp = HeaderFingerprint::new(None, None, None);
        assert_eq!(HeaderFingerprint::from_slice(fp.as_bytes()), Some(fp));
        assert_eq!(HeaderFingerprint::from_slice(&[0; 3]), None);
        assert!(format!("{fp:?}").starts_with("HeaderFingerprint("));
        assert!(HeaderFingerprint::from_raw(b"").is_none());
    }
}
