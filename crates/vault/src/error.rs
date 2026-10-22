use std::io;

use crate::KeyId;

/// Errors from key handling, sealing and opening.
///
/// Authentication failures are deliberately coarse: a flipped bit, the wrong key, the
/// wrong blob id and a stream cut at a chunk boundary all surface as [`Error::Decrypt`],
/// because the AEAD can't (and shouldn't) tell them apart.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The authentication tag didn't verify: the data was modified, truncated at a chunk
    /// or block boundary, or sealed with a different key or associated data.
    #[error(
        "decryption failed: the data was modified or truncated, or was sealed with a different key"
    )]
    Decrypt,

    /// The data doesn't start with the expected magic bytes.
    #[error("not a Maze encrypted file (bad magic)")]
    BadMagic,

    /// The format version in the header is not one this build understands.
    #[error("unsupported encrypted format version {0}")]
    UnsupportedVersion(u8),

    /// The header names a key generation other than the one this cipher holds.
    #[error("data was sealed with key generation {found}, this cipher holds generation {expected}")]
    KeyIdMismatch {
        /// Key generation recorded in the header.
        found: u32,
        /// Key generation of the cipher that tried to open it.
        expected: KeyId,
    },

    /// The data ends before a complete header or a complete final chunk.
    #[error("encrypted data is truncated")]
    Truncated,

    /// A header field has a value outside its allowed range.
    #[error("invalid encrypted header: {0}")]
    InvalidHeader(&'static str),

    /// A block passed to the block cipher has the wrong length for its position.
    #[error("invalid block: {0}")]
    InvalidBlock(&'static str),

    /// The plaintext is too large for one sealed stream (more than 2^32 chunks).
    #[error("plaintext is too large for one sealed stream")]
    TooLarge,

    /// Encrypted data exists but the key store has no master key. Maze must offer
    /// "Enter recovery key" or "Start fresh and re-sync"; it never creates a new key
    /// silently in this situation.
    #[error("encrypted data exists but no master key was found")]
    MissingKeyForExistingData,

    /// A master key already exists and the operation would replace it.
    #[error("a different master key already exists in the key store")]
    KeyAlreadyExists,

    /// The key store accepted a key but returned something else when read back.
    #[error("the key store did not return the key that was just stored")]
    KeyStoreVerification,

    /// The operating system's random number generator failed.
    #[error("the system random number generator failed: {0}")]
    Random(getrandom::Error),

    /// The key store failed.
    #[error(transparent)]
    KeyStore(#[from] KeyStoreError),

    /// A recovery key couldn't be decoded.
    #[error(transparent)]
    RecoveryKey(#[from] RecoveryKeyError),

    /// Reading or writing the underlying stream failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<getrandom::Error> for Error {
    fn from(err: getrandom::Error) -> Self {
        Self::Random(err)
    }
}

impl From<Error> for io::Error {
    fn from(err: Error) -> Self {
        match err {
            Error::Io(io) => io,
            other => io::Error::new(io::ErrorKind::InvalidData, other),
        }
    }
}

/// Errors from a [`KeyStore`](crate::KeyStore) backend.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KeyStoreError {
    /// The stored item exists but isn't a valid Maze master key.
    #[error("the stored master key is malformed")]
    Malformed,

    /// The platform key store (Keychain, Secret Service) reported an error.
    #[error("key store error{}: {message}", code.map(|c| format!(" {c}")).unwrap_or_default())]
    Platform {
        /// Platform status code, if there is one (`OSStatus` on macOS).
        code: Option<i32>,
        /// Human-readable description from the platform.
        message: String,
    },

    /// A file-backed key store failed.
    #[error("key file: {0}")]
    Io(#[from] io::Error),
}

/// Errors from decoding a recovery key.
///
/// None of the variants echo the input, so they are safe to log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RecoveryKeyError {
    /// The key has the wrong number of base32 characters (separators don't count).
    #[error("a recovery key has {expected} characters, this one has {found}")]
    WrongLength {
        /// Characters a recovery key has.
        expected: usize,
        /// Characters found after removing separators.
        found: usize,
    },

    /// A character outside the Crockford base32 alphabet.
    #[error("character {position} is not valid in a recovery key")]
    InvalidCharacter {
        /// 1-based position among the base32 characters (separators don't count).
        position: usize,
    },

    /// The checksum group doesn't match: most likely a typo.
    #[error("the recovery key has a typo (checksum mismatch)")]
    ChecksumMismatch,

    /// The checksum matched but the encoded key generation is zero.
    #[error("the recovery key encodes an invalid key generation")]
    InvalidKeyId,
}
