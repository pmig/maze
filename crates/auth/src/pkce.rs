//! PKCE (RFC 7636) and random URL-safe tokens.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::AuthError;

/// Random bytes in a code verifier: 64 bytes give 86 base64url characters, within
/// RFC 7636's 43–128 (the same as Thunderbird).
const VERIFIER_BYTES: usize = 64;

/// Random bytes in the `state` parameter.
pub(crate) const STATE_BYTES: usize = 32;

/// A PKCE code verifier (RFC 7636 §4.1). The challenge sent in the authorization
/// request is derived with `S256`; the verifier itself only goes to the token endpoint.
#[derive(Clone, PartialEq, Eq)]
pub struct PkceVerifier(Zeroizing<String>);

impl PkceVerifier {
    /// A fresh high-entropy verifier from the system random number generator.
    pub fn generate() -> Result<Self, AuthError> {
        random_token(VERIFIER_BYTES).map(|token| Self(Zeroizing::new(token)))
    }

    /// A verifier from an existing string, checked against RFC 7636 §4.1: 43 to 128
    /// characters from `[A-Za-z0-9-._~]`. Returns `None` if it doesn't qualify.
    pub fn from_string(verifier: String) -> Option<Self> {
        let valid = (43..=128).contains(&verifier.len())
            && verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
        if valid {
            Some(Self(Zeroizing::new(verifier)))
        } else {
            let mut verifier = verifier;
            verifier.zeroize();
            None
        }
    }

    /// The verifier, for the token request.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The `S256` challenge: `BASE64URL(SHA256(ASCII(verifier)))` without padding.
    pub fn challenge(&self) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(self.0.as_bytes()))
    }

    /// The `code_challenge_method` sent with [`challenge`](Self::challenge).
    pub const fn challenge_method() -> &'static str {
        "S256"
    }
}

impl fmt::Debug for PkceVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PkceVerifier(<redacted>)")
    }
}

/// `bytes` random bytes from the system RNG, base64url-encoded without padding.
pub(crate) fn random_token(bytes: usize) -> Result<String, AuthError> {
    let mut buf = Zeroizing::new(vec![0u8; bytes]);
    getrandom::fill(&mut buf).map_err(|err| AuthError::Random(err.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(&*buf))
}

/// Compares two strings in time independent of where they differ.
pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// RFC 7636 Appendix B.
    #[test]
    fn rfc7636_test_vector() {
        let verifier =
            PkceVerifier::from_string("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_owned())
                .unwrap();
        assert_eq!(
            verifier.challenge(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(PkceVerifier::challenge_method(), "S256");
    }

    #[test]
    fn generated_verifiers_are_valid_and_unique() {
        let a = PkceVerifier::generate().unwrap();
        let b = PkceVerifier::generate().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 86);
        assert!(PkceVerifier::from_string(a.as_str().to_owned()).is_some());
        assert_eq!(a.challenge().len(), 43);
    }

    #[test]
    fn rejects_invalid_verifiers() {
        assert!(PkceVerifier::from_string("short".to_owned()).is_none());
        assert!(PkceVerifier::from_string("a".repeat(129)).is_none());
        assert!(PkceVerifier::from_string(format!("{}+", "a".repeat(50))).is_none());
        assert!(PkceVerifier::from_string("a".repeat(43)).is_some());
    }

    #[test]
    fn debug_is_redacted() {
        let verifier = PkceVerifier::generate().unwrap();
        assert!(!format!("{verifier:?}").contains(verifier.as_str()));
    }

    #[test]
    fn compares_in_constant_time() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn random_tokens_are_url_safe() {
        let token = random_token(STATE_BYTES).unwrap();
        assert_eq!(token.len(), 43);
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }
}
