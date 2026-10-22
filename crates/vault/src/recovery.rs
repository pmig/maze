use std::fmt;

use zeroize::Zeroizing;

use crate::{KEY_LEN, KeyId, MasterKey, RecoveryKeyError, SecretString};

/// Crockford base32: digits and letters without I, L, O and U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Key bytes followed by the key id.
const PAYLOAD_LEN: usize = KEY_LEN + 4;
const CHECKSUM_LEN: usize = 4;
const RAW_LEN: usize = PAYLOAD_LEN + CHECKSUM_LEN;
/// Base32 characters in a recovery key: 40 bytes are exactly 64 characters.
const CHARS: usize = RAW_LEN * 8 / 5;
const GROUP_LEN: usize = 4;
const CHECKSUM_CONTEXT: &str = "mazemail vault recovery key checksum v1";

/// The master key rendered for people: 64 Crockford base32 characters in 16 groups of 4,
/// for example `7K2Q-…-M4XA`.
///
/// The 40 encoded bytes are the 32 key bytes, the key id (u32 big-endian) and a 4-byte
/// checksum (keyless BLAKE3 in derive-key mode over the first 36 bytes). The last group is
/// pure checksum bits, so a typo is caught with probability 1 − 2⁻³².
///
/// Decoding is forgiving about how the key was typed or pasted: case doesn't matter,
/// spaces, tabs, newlines and hyphens are ignored, `O`/`o` reads as zero and `I`/`i`/`L`/`l`
/// as one (Crockford's rules). `U` and other characters are rejected.
///
/// `Debug` is redacted; the string is wiped on drop.
pub struct RecoveryKey(SecretString);

impl RecoveryKey {
    pub(crate) fn encode(key: &MasterKey) -> Self {
        let mut raw = Zeroizing::new([0u8; RAW_LEN]);
        raw[..KEY_LEN].copy_from_slice(key.expose_secret());
        raw[KEY_LEN..PAYLOAD_LEN].copy_from_slice(&key.key_id().to_be_bytes());
        let sum = checksum(&raw[..PAYLOAD_LEN]);
        raw[PAYLOAD_LEN..].copy_from_slice(&sum);

        let mut text = String::with_capacity(CHARS + CHARS / GROUP_LEN);
        let mut written = 0;
        for block in raw.as_chunks::<5>().0 {
            // 5 bytes = 40 bits = 8 characters, no bits left over.
            let mut bits = Zeroizing::new(0u64);
            for &byte in block {
                *bits = (*bits << 8) | u64::from(byte);
            }
            for shift in (0..8).rev() {
                if written > 0 && written % GROUP_LEN == 0 {
                    text.push('-');
                }
                let index = ((*bits >> (shift * 5)) & 0x1f) as usize;
                text.push(char::from(ALPHABET[index]));
                written += 1;
            }
        }
        Self(SecretString::new(text))
    }

    pub(crate) fn decode(input: &str) -> Result<MasterKey, RecoveryKeyError> {
        let mut values = Zeroizing::new(Vec::with_capacity(CHARS));
        for c in input.chars() {
            if c.is_whitespace() || c == '-' {
                continue;
            }
            let value = decode_char(c).ok_or(RecoveryKeyError::InvalidCharacter {
                position: values.len() + 1,
            })?;
            values.push(value);
        }
        if values.len() != CHARS {
            return Err(RecoveryKeyError::WrongLength {
                expected: CHARS,
                found: values.len(),
            });
        }

        let mut raw = Zeroizing::new([0u8; RAW_LEN]);
        for (group, out) in values
            .as_chunks::<8>()
            .0
            .iter()
            .zip(raw.as_chunks_mut::<5>().0)
        {
            let mut bits = Zeroizing::new(0u64);
            for &value in group {
                *bits = (*bits << 5) | u64::from(value);
            }
            for (i, byte) in out.iter_mut().enumerate() {
                *byte = (*bits >> (32 - 8 * i)) as u8;
            }
        }

        if checksum(&raw[..PAYLOAD_LEN]) != raw[PAYLOAD_LEN..] {
            return Err(RecoveryKeyError::ChecksumMismatch);
        }
        let key_id = KeyId::from_be_bytes([raw[32], raw[33], raw[34], raw[35]])
            .ok_or(RecoveryKeyError::InvalidKeyId)?;
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&raw[..KEY_LEN]);
        Ok(MasterKey::from_bytes(key_id, key))
    }

    /// The formatted recovery key, to show once to the user.
    pub fn expose_secret(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryKey(<redacted>)")
    }
}

fn decode_char(c: char) -> Option<u8> {
    let c = match c.to_ascii_uppercase() {
        'O' => '0',
        'I' | 'L' => '1',
        other => other,
    };
    let byte = u8::try_from(c).ok()?;
    ALPHABET
        .iter()
        .position(|&a| a == byte)
        .and_then(|i| u8::try_from(i).ok())
}

fn checksum(payload: &[u8]) -> [u8; CHECKSUM_LEN] {
    let mut hasher = blake3::Hasher::new_derive_key(CHECKSUM_CONTEXT);
    hasher.update(payload);
    let mut out = [0u8; CHECKSUM_LEN];
    out.copy_from_slice(&hasher.finalize().as_bytes()[..CHECKSUM_LEN]);
    out
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn key(bytes: [u8; KEY_LEN], id: u32) -> MasterKey {
        MasterKey::from_bytes(KeyId::new(id).unwrap(), bytes)
    }

    #[test]
    fn format_is_sixteen_groups_of_four() {
        let recovery = key([0xa5; KEY_LEN], 1).to_recovery_key();
        let text = recovery.expose_secret();
        let groups: Vec<&str> = text.split('-').collect();
        assert_eq!(groups.len(), 16);
        assert!(groups.iter().all(|g| g.len() == 4));
        assert!(text.bytes().all(|b| b == b'-' || ALPHABET.contains(&b)));
        assert_eq!(format!("{recovery:?}"), "RecoveryKey(<redacted>)");
    }

    #[test]
    fn known_vector() {
        // All-zero key, generation 1: pins the encoding against accidental changes.
        let text = key([0; KEY_LEN], 1).to_recovery_key();
        assert_eq!(
            text.expose_secret(),
            "0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-0000-04SB-ZHRE"
        );
        let back = MasterKey::from_recovery_key(text.expose_secret()).unwrap();
        assert_eq!(back.expose_secret(), &[0; KEY_LEN]);
    }

    #[test]
    fn typing_variants_are_accepted() {
        let original = key(*b"0123456789abcdefghijklmnopqrstuv", 7);
        let text = original.to_recovery_key().expose_secret().to_owned();
        let variants = [
            text.to_lowercase(),
            text.replace('-', ""),
            text.replace('-', " "),
            format!("  {}\n", text.replace('-', "\t")),
            text.replace('0', "O").replace('1', "l"),
            text.replace('0', "o").replace('1', "I"),
        ];
        for variant in variants {
            let back = MasterKey::from_recovery_key(&variant).unwrap();
            assert!(back.ct_eq(&original), "{variant}");
            assert_eq!(back.key_id().get(), 7);
        }
    }

    #[test]
    fn errors_are_specific() {
        let text = key([3; KEY_LEN], 1)
            .to_recovery_key()
            .expose_secret()
            .to_owned();
        assert_eq!(
            MasterKey::from_recovery_key(&text[..text.len() - 5]).unwrap_err(),
            RecoveryKeyError::WrongLength {
                expected: 64,
                found: 60
            }
        );
        let mut with_u = text;
        with_u.replace_range(5..6, "U");
        assert_eq!(
            MasterKey::from_recovery_key(&with_u).unwrap_err(),
            RecoveryKeyError::InvalidCharacter { position: 5 }
        );
        assert!(matches!(
            MasterKey::from_recovery_key("").unwrap_err(),
            RecoveryKeyError::WrongLength { found: 0, .. }
        ));
    }

    #[test]
    fn zero_key_id_is_rejected_even_with_valid_checksum() {
        let mut raw = [0u8; RAW_LEN];
        let sum = checksum(&raw[..PAYLOAD_LEN]);
        raw[PAYLOAD_LEN..].copy_from_slice(&sum);
        let mut text = String::new();
        for block in raw.as_chunks::<5>().0 {
            let bits = block.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
            for shift in (0..8).rev() {
                text.push(char::from(
                    ALPHABET[((bits >> (shift * 5)) & 0x1f) as usize],
                ));
            }
        }
        assert_eq!(
            MasterKey::from_recovery_key(&text).unwrap_err(),
            RecoveryKeyError::InvalidKeyId
        );
    }

    proptest! {
        #[test]
        fn round_trip(bytes in any::<[u8; KEY_LEN]>(), id in 1u32..) {
            let original = key(bytes, id);
            let text = original.to_recovery_key();
            let back = MasterKey::from_recovery_key(text.expose_secret()).unwrap();
            prop_assert!(back.ct_eq(&original));
        }

        #[test]
        fn single_substitution_is_caught(
            bytes in any::<[u8; KEY_LEN]>(),
            position in 0usize..CHARS,
            replacement in 0usize..32,
        ) {
            let text = key(bytes, 1).to_recovery_key().expose_secret().replace('-', "");
            let mut chars: Vec<u8> = text.into_bytes();
            prop_assume!(chars[position] != ALPHABET[replacement]);
            chars[position] = ALPHABET[replacement];
            let typo = String::from_utf8(chars).unwrap();
            prop_assert_eq!(
                MasterKey::from_recovery_key(&typo).unwrap_err(),
                RecoveryKeyError::ChecksumMismatch
            );
        }

        #[test]
        fn swapped_neighbours_are_caught(bytes in any::<[u8; KEY_LEN]>(), position in 0usize..CHARS - 1) {
            let text = key(bytes, 1).to_recovery_key().expose_secret().replace('-', "");
            let mut chars: Vec<u8> = text.into_bytes();
            prop_assume!(chars[position] != chars[position + 1]);
            chars.swap(position, position + 1);
            let typo = String::from_utf8(chars).unwrap();
            prop_assert!(MasterKey::from_recovery_key(&typo).is_err());
        }
    }
}
