//! Modified UTF-7 mailbox names (RFC 3501 §5.1.3).
//!
//! IMAP4rev1 servers send and expect mailbox names in a variant of UTF-7:
//!
//! - printable US-ASCII (`0x20..=0x7e`) except `&` stands for itself,
//! - `&` is written as `&-`,
//! - every other run of characters is UTF-16BE, encoded with a modified base64
//!   alphabet (`,` instead of `/`, no padding) between `&` and `-`.
//!
//! Gmail uses the same encoding for user labels in `X-GM-LABELS`.

use thiserror::Error;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

/// A mailbox name that is not valid modified UTF-7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Utf7Error {
    /// A byte outside printable US-ASCII appeared outside a shifted run.
    #[error("non-printable or non-ASCII byte at offset {0}")]
    InvalidByte(usize),
    /// A shifted run (`&…`) is not terminated with `-`.
    #[error("unterminated shifted run starting at offset {0}")]
    Unterminated(usize),
    /// A shifted run contains a character outside the modified base64 alphabet.
    #[error("invalid base64 character at offset {0}")]
    InvalidBase64(usize),
    /// A shifted run does not decode to well-formed UTF-16.
    #[error("invalid UTF-16 in shifted run starting at offset {0}")]
    InvalidUtf16(usize),
}

/// Encode a UTF-8 mailbox name as modified UTF-7.
///
/// Every Rust string can be encoded, so this never fails.
pub fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending: Vec<u16> = Vec::new();
    for ch in name.chars() {
        if (' '..='~').contains(&ch) {
            flush_shifted(&mut out, &mut pending);
            if ch == '&' {
                out.push_str("&-");
            } else {
                out.push(ch);
            }
        } else {
            let mut buf = [0u16; 2];
            pending.extend_from_slice(ch.encode_utf16(&mut buf));
        }
    }
    flush_shifted(&mut out, &mut pending);
    out
}

fn flush_shifted(out: &mut String, pending: &mut Vec<u16>) {
    if pending.is_empty() {
        return;
    }
    let bytes: Vec<u8> = pending.iter().flat_map(|unit| unit.to_be_bytes()).collect();
    out.push('&');
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        out.push(char::from(ALPHABET[usize::from(b0 >> 2)]));
        out.push(char::from(
            ALPHABET[usize::from(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4))],
        ));
        if let Some(b1) = b1 {
            out.push(char::from(
                ALPHABET[usize::from(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6))],
            ));
        }
        if let Some(b2) = b2 {
            out.push(char::from(ALPHABET[usize::from(b2 & 0x3f)]));
        }
    }
    out.push('-');
    pending.clear();
}

/// Decode a modified UTF-7 mailbox name to UTF-8.
pub fn decode(name: &str) -> Result<String, Utf7Error> {
    let bytes = name.as_bytes();
    let mut out = String::with_capacity(name.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'&' {
            let start = i;
            let end = bytes[i + 1..]
                .iter()
                .position(|&c| c == b'-')
                .map(|p| i + 1 + p)
                .ok_or(Utf7Error::Unterminated(start))?;
            if end == i + 1 {
                out.push('&');
            } else {
                decode_shifted(&bytes[i + 1..end], i + 1, &mut out)?;
            }
            i = end + 1;
        } else if (0x20..=0x7e).contains(&b) {
            out.push(char::from(b));
            i += 1;
        } else {
            return Err(Utf7Error::InvalidByte(i));
        }
    }
    Ok(out)
}

/// Decode a mailbox name, falling back to the raw name if it isn't valid modified UTF-7.
///
/// Some servers send raw UTF-8 names despite the RFC; showing them as-is is better
/// than failing the whole folder list.
pub fn decode_lossy(name: &str) -> String {
    decode(name).unwrap_or_else(|_| name.to_owned())
}

fn decode_shifted(run: &[u8], offset: usize, out: &mut String) -> Result<(), Utf7Error> {
    let mut bits: u32 = 0;
    let mut nbits = 0u32;
    let mut bytes = Vec::with_capacity(run.len() * 3 / 4);
    for (k, &c) in run.iter().enumerate() {
        let value = ALPHABET
            .iter()
            .position(|&a| a == c)
            .ok_or(Utf7Error::InvalidBase64(offset + k))?;
        // `value` < 64, so the cast is lossless.
        bits = (bits << 6) | value as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            // Masked to a byte, so the cast is lossless.
            bytes.push(((bits >> nbits) & 0xff) as u8);
        }
    }
    // Leftover bits must be zero padding of fewer than 6 bits.
    if nbits >= 6 || bits & ((1 << nbits) - 1) != 0 || bytes.len() % 2 != 0 {
        return Err(Utf7Error::InvalidUtf16(offset - 1));
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes(*pair))
        .collect();
    let decoded = String::from_utf16(&units).map_err(|_| Utf7Error::InvalidUtf16(offset - 1))?;
    out.push_str(&decoded);
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn ascii_passes_through() {
        assert_eq!(encode("INBOX"), "INBOX");
        assert_eq!(encode("[Gmail]/All Mail"), "[Gmail]/All Mail");
        assert_eq!(decode("Sent Items").unwrap(), "Sent Items");
    }

    #[test]
    fn ampersand_is_escaped() {
        assert_eq!(encode("Tom & Jerry"), "Tom &- Jerry");
        assert_eq!(decode("Tom &- Jerry").unwrap(), "Tom & Jerry");
        assert_eq!(decode("&-&-").unwrap(), "&&");
    }

    #[test]
    fn rfc3501_example() {
        // RFC 3501 §5.1.3: "~peter/mail/台北/日本語"
        let name = "~peter/mail/\u{53f0}\u{5317}/\u{65e5}\u{672c}\u{8a9e}";
        let wire = "~peter/mail/&U,BTFw-/&ZeVnLIqe-";
        assert_eq!(encode(name), wire);
        assert_eq!(decode(wire).unwrap(), name);
    }

    #[test]
    fn german_and_emoji() {
        assert_eq!(encode("Entwürfe"), "Entw&APw-rfe");
        assert_eq!(decode("Entw&APw-rfe").unwrap(), "Entwürfe");
        assert_eq!(encode("Gelöschte Elemente"), "Gel&APY-schte Elemente");
        // Astral plane characters use surrogate pairs.
        let emoji = "Mail \u{1f4e7}";
        assert_eq!(decode(&encode(emoji)).unwrap(), emoji);
        assert_eq!(encode(emoji), "Mail &2D3c5w-");
    }

    #[test]
    fn rejects_malformed_input() {
        assert_eq!(decode("&U,BTFw"), Err(Utf7Error::Unterminated(0)));
        assert_eq!(decode("a&U*-"), Err(Utf7Error::InvalidBase64(3)));
        assert_eq!(decode("caf\u{e9}"), Err(Utf7Error::InvalidByte(3)));
        // A lone high surrogate (D83D) is not valid UTF-16.
        assert_eq!(decode("&2D0-"), Err(Utf7Error::InvalidUtf16(0)));
        // Odd number of bytes.
        assert_eq!(decode("&AP-"), Err(Utf7Error::InvalidUtf16(0)));
    }

    #[test]
    fn lossy_decode_keeps_raw_utf8_names() {
        assert_eq!(decode_lossy("Entwürfe"), "Entwürfe");
        assert_eq!(decode_lossy("Entw&APw-rfe"), "Entwürfe");
    }

    proptest! {
        #[test]
        fn round_trips(name in "\\PC*") {
            let wire = encode(&name);
            prop_assert!(wire.bytes().all(|b| (0x20..=0x7e).contains(&b)));
            prop_assert_eq!(decode(&wire).unwrap(), name);
        }
    }
}
