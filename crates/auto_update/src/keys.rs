//! Update signing keys and their file formats.
//!
//! - **Private key file** (`update-key.secret`, the content of the CI secret
//!   `UPDATE_SIGNING_KEY`): optional `#` comment lines, then one line with the base64
//!   (standard alphabet, padded) encoding of the 32-byte Ed25519 seed.
//! - **Public key** (`update-key.pub`, and the lines of
//!   `crates/auto_update/keys/update_public_keys.txt`): base64 of the 32-byte Ed25519
//!   public key.
//!
//! The same functions are used by `cargo xtask gen-update-key`/`release-manifest` and by
//! the updater, so the formats can't drift.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{SigningKey, VerifyingKey};

include!(concat!(env!("OUT_DIR"), "/public_keys.rs"));

/// Error reading a key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// No key line was found.
    #[error("the key file contains no key")]
    Empty,
    /// More than one key line was found.
    #[error("the key file contains more than one key")]
    MultipleKeys,
    /// The key isn't valid base64.
    #[error("the key is not valid base64")]
    Base64,
    /// The key has the wrong length.
    #[error("the key must be 32 bytes, found {0}")]
    Length(usize),
    /// The bytes aren't a valid Ed25519 public key.
    #[error("the key is not a valid Ed25519 public key")]
    InvalidPoint,
}

/// The public keys compiled into this build (at most two, for rotation). Invalid
/// entries are skipped (a unit test makes sure there are none).
pub fn embedded_public_keys() -> Vec<VerifyingKey> {
    EMBEDDED_PUBLIC_KEYS
        .iter()
        .filter_map(|bytes| VerifyingKey::from_bytes(bytes).ok())
        .collect()
}

/// Parses a private key file (see the module docs).
pub fn parse_signing_key(text: &str) -> Result<SigningKey, KeyError> {
    let bytes = decode_single_key(text)?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// Parses a public key (see the module docs).
pub fn parse_public_key(text: &str) -> Result<VerifyingKey, KeyError> {
    let bytes = decode_single_key(text)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| KeyError::InvalidPoint)
}

/// The content of a private key file for `key`.
pub fn encode_signing_key(key: &SigningKey) -> String {
    format!(
        "# Maze update signing key: Ed25519 seed, base64. Keep it secret; store it\n\
         # verbatim in the CI secret UPDATE_SIGNING_KEY.\n{}\n",
        STANDARD.encode(key.to_bytes())
    )
}

/// The public key line for `key`.
pub fn encode_public_key(key: &VerifyingKey) -> String {
    STANDARD.encode(key.to_bytes())
}

fn decode_single_key(text: &str) -> Result<[u8; 32], KeyError> {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let line = lines.next().ok_or(KeyError::Empty)?;
    if lines.next().is_some() {
        return Err(KeyError::MultipleKeys);
    }
    let bytes = STANDARD.decode(line).map_err(|_| KeyError::Base64)?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| KeyError::Length(bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_keys_are_valid() {
        assert!(EMBEDDED_PUBLIC_KEYS.len() <= 2);
        assert_eq!(embedded_public_keys().len(), EMBEDDED_PUBLIC_KEYS.len());
    }

    #[test]
    fn key_files_round_trip() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let file = encode_signing_key(&key);
        assert!(file.starts_with('#'));
        assert_eq!(parse_signing_key(&file).unwrap().to_bytes(), key.to_bytes());
        // Without comments and with surrounding whitespace, as a CI secret might be.
        let bare = format!("  {}  \n\n", STANDARD.encode([7; 32]));
        assert_eq!(parse_signing_key(&bare).unwrap().to_bytes(), key.to_bytes());

        let public = encode_public_key(&key.verifying_key());
        assert_eq!(parse_public_key(&public).unwrap(), key.verifying_key());
    }

    #[test]
    fn bad_key_files() {
        assert_eq!(
            parse_signing_key("# only a comment\n"),
            Err(KeyError::Empty)
        );
        assert_eq!(parse_signing_key("not base64!"), Err(KeyError::Base64));
        assert_eq!(
            parse_signing_key(&STANDARD.encode([1; 16])),
            Err(KeyError::Length(16))
        );
        let two = format!(
            "{}\n{}\n",
            STANDARD.encode([1; 32]),
            STANDARD.encode([2; 32])
        );
        assert_eq!(parse_signing_key(&two), Err(KeyError::MultipleKeys));
    }
}
