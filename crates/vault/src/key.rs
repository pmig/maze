use std::fmt;
use std::num::NonZeroU32;

use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{Error, KeyStoreError, RecoveryKey, RecoveryKeyError, SecretString};

/// Length of the master key and of every subkey, in bytes.
pub const KEY_LEN: usize = 32;

/// Info labels for [`MasterKey::derive`].
///
/// The HKDF info string is `mazemail/vault/v1/<label>`, so labels are domain-separated
/// from anything else that might ever be derived from the same key material.
pub mod label {
    /// SQLCipher raw key for `mazemail.db`, `sendlog.db` and their backups.
    pub const SQLCIPHER: &str = "sqlcipher";
    /// XChaCha20-Poly1305 key for `MZB1` blobs and caches.
    pub const BLOB: &str = "blob";
    /// Keyed-BLAKE3 key for content-addressed blob ids.
    pub const BLOB_ID: &str = "blob-id";
    /// XChaCha20-Poly1305 key for `MZI1` index blocks.
    pub const INDEX: &str = "index";
}

const HKDF_INFO_PREFIX: &[u8] = b"mazemail/vault/v1/";

/// Generation of a master key, recorded in every encrypted header.
///
/// The first key is generation 1 ([`KeyId::FIRST`]); key rotation (M7) creates the next
/// one. Zero is never a valid generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyId(NonZeroU32);

impl KeyId {
    /// The generation of the first master key.
    pub const FIRST: Self = Self(NonZeroU32::MIN);

    /// Wraps a raw generation number; `None` for zero.
    pub const fn new(raw: u32) -> Option<Self> {
        match NonZeroU32::new(raw) {
            Some(id) => Some(Self(id)),
            None => None,
        }
    }

    /// The raw generation number (never zero).
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// The following generation, or `None` on overflow.
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(id) => Some(Self(id)),
            None => None,
        }
    }

    pub(crate) fn from_be_bytes(bytes: [u8; 4]) -> Option<Self> {
        Self::new(u32::from_be_bytes(bytes))
    }

    pub(crate) fn to_be_bytes(self) -> [u8; 4] {
        self.get().to_be_bytes()
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The 256-bit master key that every other key is derived from.
///
/// The key bytes live on the heap (so moves don't leave copies on the stack), are wiped on
/// drop, and are never printed: `Debug` shows only the [`KeyId`]. There is deliberately
/// no `Clone`.
pub struct MasterKey {
    bytes: Box<[u8; KEY_LEN]>,
    key_id: KeyId,
}

/// Magic of the serialized form a [`KeyStore`](crate::KeyStore) keeps.
const STORED_MAGIC: [u8; 4] = *b"MZK1";
/// Length of [`MasterKey::to_stored_bytes`]: magic, key id, key.
const STORED_LEN: usize = 4 + 4 + KEY_LEN;

impl MasterKey {
    /// Generates a fresh key from the operating system's random number generator.
    pub fn generate(key_id: KeyId) -> Result<Self, Error> {
        let mut bytes = Box::new([0u8; KEY_LEN]);
        getrandom::fill(bytes.as_mut_slice())?;
        Ok(Self { bytes, key_id })
    }

    /// Builds a key from raw bytes, for example key material from a test vector.
    ///
    /// The caller's array is wiped after it has been copied.
    pub fn from_bytes(key_id: KeyId, mut bytes: [u8; KEY_LEN]) -> Self {
        let key = Self {
            bytes: Box::new(bytes),
            key_id,
        };
        bytes.zeroize();
        key
    }

    /// The generation of this key.
    pub fn key_id(&self) -> KeyId {
        self.key_id
    }

    /// The raw key bytes. Only hand these to a [`KeyStore`](crate::KeyStore) or a KDF.
    pub fn expose_secret(&self) -> &[u8; KEY_LEN] {
        &self.bytes
    }

    /// Derives a purpose-bound subkey with HKDF-SHA256.
    ///
    /// The info string is `mazemail/vault/v1/<label>`; there is no salt, because the master
    /// key is already uniformly random. Use the constants in [`label`] for the built-in
    /// purposes. Different labels give independent keys; the same label always gives the
    /// same key for the same master key.
    pub fn derive(&self, label: &str) -> SubKey {
        let hkdf = Hkdf::<Sha256>::new(None, self.bytes.as_slice());
        let mut info = Vec::with_capacity(HKDF_INFO_PREFIX.len() + label.len());
        info.extend_from_slice(HKDF_INFO_PREFIX);
        info.extend_from_slice(label.as_bytes());
        let mut bytes = Box::new([0u8; KEY_LEN]);
        // HKDF-SHA256 can expand up to 255 * 32 bytes; 32 bytes can't fail.
        if hkdf.expand(&info, bytes.as_mut_slice()).is_err() {
            unreachable!("HKDF-SHA256 output of {KEY_LEN} bytes is always valid");
        }
        SubKey {
            bytes,
            key_id: self.key_id,
        }
    }

    /// The SQLCipher raw key in the form `x'<64 hex digits>'`.
    ///
    /// Use it as `PRAGMA key = "x'…'"`, i.e. wrap the returned text in double quotes.
    /// SQLCipher then uses the 256-bit key directly, without PBKDF2 for each connection.
    pub fn sqlcipher_pragma_key(&self) -> SecretString {
        let subkey = self.derive(label::SQLCIPHER);
        let mut text = String::with_capacity(3 + 2 * KEY_LEN);
        text.push_str("x'");
        for byte in subkey.expose_secret() {
            text.push(hex_digit(byte >> 4));
            text.push(hex_digit(byte & 0x0f));
        }
        text.push('\'');
        SecretString::new(text)
    }

    /// Renders this key as a recovery key for the user to keep in a password manager.
    pub fn to_recovery_key(&self) -> RecoveryKey {
        RecoveryKey::encode(self)
    }

    /// Decodes a recovery key typed by the user (see [`RecoveryKey`] for the accepted forms).
    pub fn from_recovery_key(input: &str) -> Result<Self, RecoveryKeyError> {
        RecoveryKey::decode(input)
    }

    /// The serialized form a [`KeyStore`](crate::KeyStore) persists: `MZK1`, the key id as
    /// big-endian `u32`, then the 32 key bytes (40 bytes in total).
    pub fn to_stored_bytes(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(STORED_LEN));
        out.extend_from_slice(&STORED_MAGIC);
        out.extend_from_slice(&self.key_id.to_be_bytes());
        out.extend_from_slice(self.bytes.as_slice());
        out
    }

    /// Parses the output of [`MasterKey::to_stored_bytes`].
    pub fn from_stored_bytes(stored: &[u8]) -> Result<Self, KeyStoreError> {
        if stored.len() != STORED_LEN || stored[..4] != STORED_MAGIC {
            return Err(KeyStoreError::Malformed);
        }
        let mut id = [0u8; 4];
        id.copy_from_slice(&stored[4..8]);
        let key_id = KeyId::from_be_bytes(id).ok_or(KeyStoreError::Malformed)?;
        let mut bytes = Box::new([0u8; KEY_LEN]);
        bytes.copy_from_slice(&stored[8..]);
        Ok(Self { bytes, key_id })
    }

    /// Whether both keys have the same bytes and generation. The key bytes are compared
    /// in constant time.
    pub fn ct_eq(&self, other: &Self) -> bool {
        let diff = self
            .bytes
            .iter()
            .zip(other.bytes.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b));
        diff == 0 && self.key_id == other.key_id
    }
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        self.bytes.as_mut_slice().zeroize();
    }
}

impl ZeroizeOnDrop for MasterKey {}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MasterKey")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// A key derived from the [`MasterKey`] for one purpose, wiped on drop, never printed.
pub struct SubKey {
    bytes: Box<[u8; KEY_LEN]>,
    key_id: KeyId,
}

impl SubKey {
    /// Generation of the master key this subkey was derived from.
    pub fn key_id(&self) -> KeyId {
        self.key_id
    }

    /// The raw subkey bytes.
    pub fn expose_secret(&self) -> &[u8; KEY_LEN] {
        &self.bytes
    }
}

impl Drop for SubKey {
    fn drop(&mut self) {
        self.bytes.as_mut_slice().zeroize();
    }
}

impl ZeroizeOnDrop for SubKey {}

impl fmt::Debug for SubKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubKey")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

fn hex_digit(nibble: u8) -> char {
    char::from(b"0123456789abcdef"[usize::from(nibble & 0x0f)])
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn test_key() -> MasterKey {
        MasterKey::from_bytes(KeyId::FIRST, [7; KEY_LEN])
    }

    #[test]
    fn key_id_rejects_zero_and_counts_up() {
        assert_eq!(KeyId::new(0), None);
        assert_eq!(KeyId::FIRST.get(), 1);
        assert_eq!(KeyId::FIRST.next().map(KeyId::get), Some(2));
        assert_eq!(KeyId::new(u32::MAX).and_then(KeyId::next), None);
    }

    #[test]
    fn generated_keys_differ() {
        let a = MasterKey::generate(KeyId::FIRST).unwrap();
        let b = MasterKey::generate(KeyId::FIRST).unwrap();
        assert!(!a.ct_eq(&b));
        assert_ne!(a.expose_secret(), &[0; KEY_LEN]);
    }

    #[test]
    fn subkeys_differ_per_label_and_are_deterministic() {
        let key = test_key();
        let labels = [
            label::SQLCIPHER,
            label::BLOB,
            label::BLOB_ID,
            label::INDEX,
            "cache",
            "",
        ];
        let derived: HashSet<[u8; KEY_LEN]> = labels
            .iter()
            .map(|l| *key.derive(l).expose_secret())
            .collect();
        assert_eq!(derived.len(), labels.len());
        assert!(!derived.contains(key.expose_secret()));
        assert_eq!(
            key.derive(label::BLOB).expose_secret(),
            test_key().derive(label::BLOB).expose_secret()
        );
        assert_eq!(key.derive(label::INDEX).key_id(), KeyId::FIRST);
    }

    #[test]
    fn subkeys_differ_per_master_key() {
        let other = MasterKey::from_bytes(KeyId::FIRST, [8; KEY_LEN]);
        assert_ne!(
            test_key().derive(label::BLOB).expose_secret(),
            other.derive(label::BLOB).expose_secret()
        );
    }

    #[test]
    fn derive_matches_rfc5869_construction() {
        // Pin the derivation so a dependency bump can't silently change every key.
        let key = MasterKey::from_bytes(KeyId::FIRST, [0; KEY_LEN]);
        let mut expected = [0u8; KEY_LEN];
        Hkdf::<Sha256>::new(None, &[0; KEY_LEN])
            .expand(b"mazemail/vault/v1/blob", &mut expected)
            .unwrap();
        assert_eq!(key.derive(label::BLOB).expose_secret(), &expected);
    }

    #[test]
    fn sqlcipher_pragma_key_is_raw_hex_form() {
        let pragma = test_key().sqlcipher_pragma_key();
        let text = pragma.expose_secret();
        assert_eq!(text.len(), 67);
        assert!(text.starts_with("x'") && text.ends_with('\''));
        let hex = &text[2..66];
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        let expected: String = test_key()
            .derive(label::SQLCIPHER)
            .expose_secret()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hex, expected);
    }

    #[test]
    fn debug_never_prints_key_material() {
        let key = test_key();
        let printed = format!("{key:?} {:?}", key.derive(label::BLOB));
        assert_eq!(
            printed,
            "MasterKey { key_id: KeyId(1), .. } SubKey { key_id: KeyId(1), .. }"
        );
    }

    #[test]
    fn stored_bytes_round_trip() {
        let key = MasterKey::generate(KeyId::new(3).unwrap()).unwrap();
        let stored = key.to_stored_bytes();
        assert_eq!(stored.len(), STORED_LEN);
        let back = MasterKey::from_stored_bytes(&stored).unwrap();
        assert!(back.ct_eq(&key));
        assert_eq!(back.key_id().get(), 3);
    }

    #[test]
    fn malformed_stored_bytes_are_rejected() {
        let stored = test_key().to_stored_bytes();
        assert!(MasterKey::from_stored_bytes(&stored[..39]).is_err());
        let mut bad_magic = stored.to_vec();
        bad_magic[0] ^= 1;
        assert!(MasterKey::from_stored_bytes(&bad_magic).is_err());
        let mut zero_id = stored.to_vec();
        zero_id[4..8].copy_from_slice(&[0; 4]);
        assert!(MasterKey::from_stored_bytes(&zero_id).is_err());
    }
}
