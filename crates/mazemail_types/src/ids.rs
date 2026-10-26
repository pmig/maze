use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Error returned when parsing a public id such as `m_8f3a2c` fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseIdError {
    #[error("expected prefix `{expected}_`")]
    WrongPrefix { expected: &'static str },
    #[error("invalid id body")]
    InvalidBody,
}

/// Ids with a short, stable public form for the CLI and local API
/// (`m_` message, `t_` thread, `f_` folder, …).
pub trait PublicId: Sized + Copy {
    const PREFIX: &'static str;
    fn raw(self) -> i64;
    fn from_raw(raw: i64) -> Self;

    /// Lowercase base32 (RFC 4648 alphabet without padding) of the raw id.
    fn to_public(self) -> String {
        format!("{}_{}", Self::PREFIX, encode_base32(self.raw() as u64))
    }

    fn parse_public(s: &str) -> Result<Self, ParseIdError> {
        let body = s
            .strip_prefix(Self::PREFIX)
            .and_then(|rest| rest.strip_prefix('_'))
            .ok_or(ParseIdError::WrongPrefix {
                expected: Self::PREFIX,
            })?;
        let value = decode_base32(body).ok_or(ParseIdError::InvalidBody)?;
        let raw = i64::try_from(value).map_err(|_| ParseIdError::InvalidBody)?;
        Ok(Self::from_raw(raw))
    }
}

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

fn encode_base32(mut value: u64) -> String {
    if value == 0 {
        return "a".to_owned();
    }
    let mut out = Vec::with_capacity(13);
    while value > 0 {
        out.push(ALPHABET[(value & 31) as usize]);
        value >>= 5;
    }
    out.reverse();
    // The alphabet is ASCII, so this cannot fail.
    String::from_utf8(out).unwrap_or_default()
}

fn decode_base32(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 13 {
        return None;
    }
    let mut value: u64 = 0;
    for byte in s.bytes() {
        let digit = ALPHABET.iter().position(|&c| c == byte)? as u64;
        value = value.checked_mul(32)?.checked_add(digit)?;
    }
    Some(value)
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl PublicId for $name {
            const PREFIX: &'static str = $prefix;
            fn raw(self) -> i64 {
                self.0
            }
            fn from_raw(raw: i64) -> Self {
                Self(raw)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_public())
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse_public(s)
            }
        }
    };
}

define_id!(
    /// An account (IMAP, Gmail, Microsoft 365).
    AccountId, "a"
);
define_id!(
    /// A folder, or a Gmail label shown as a folder.
    FolderId, "f"
);
define_id!(
    /// One logical message, even if it sits in several folders or labels.
    MessageId, "m"
);
define_id!(
    /// A conversation thread.
    ThreadId, "t"
);
define_id!(
    /// A sending identity.
    IdentityId, "i"
);
define_id!(
    /// A tag (IMAP keyword).
    TagId, "g"
);
define_id!(
    /// A message being written (a local draft, PLAN §4.10).
    DraftId, "d"
);

/// Content address of a stored blob: a keyed BLAKE3 hash (32 bytes).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BlobId(pub [u8; 32]);

impl BlobId {
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hex = std::str::from_utf8(chunk).ok()?;
            out[i] = u8::from_str_radix(hex, 16).ok()?;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobId({})", &self.to_hex()[..12])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_ids_round_trip() {
        for raw in [0, 1, 31, 32, 1_000_000, i64::MAX] {
            let id = MessageId(raw);
            let public = id.to_public();
            assert!(public.starts_with("m_"));
            assert_eq!(public.parse::<MessageId>(), Ok(id));
        }
    }

    #[test]
    fn wrong_prefix_is_rejected() {
        assert_eq!(
            "t_abc".parse::<MessageId>(),
            Err(ParseIdError::WrongPrefix { expected: "m" })
        );
        assert_eq!("m_".parse::<MessageId>(), Err(ParseIdError::InvalidBody));
        assert_eq!("m_A1".parse::<MessageId>(), Err(ParseIdError::InvalidBody));
    }

    #[test]
    fn blob_id_hex_round_trip() {
        let id = BlobId([0xab; 32]);
        assert_eq!(BlobId::from_hex(&id.to_hex()), Some(id));
        assert_eq!(BlobId::from_hex("zz"), None);
    }
}
