//! Thunderbird's per-message status headers (`X-Mozilla-Status`, `X-Mozilla-Status2`,
//! `X-Mozilla-Keys`) and the reconstruction of the server's bytes from a stored copy
//! (PLAN §4.14 "Reconstruct the server bytes").

use mazemail_types::Flags;

/// `nsMsgMessageFlags` bits Maze reads (`mailnews/base/public/nsMsgMessageFlags.idl`).
pub mod status {
    /// `Read`.
    pub const READ: u32 = 0x0000_0001;
    /// `Replied`.
    pub const REPLIED: u32 = 0x0000_0002;
    /// `Marked` (starred).
    pub const MARKED: u32 = 0x0000_0004;
    /// `Expunged`: deleted in a local folder, waiting for compaction.
    pub const EXPUNGED: u32 = 0x0000_0008;
    /// `Forwarded`.
    pub const FORWARDED: u32 = 0x0000_1000;
    /// `IMAPDeleted`: `\Deleted` on the server.
    pub const IMAP_DELETED: u32 = 0x0020_0000;
    /// The pre-tags label bits (labels 1–7 became `$label1`…`$label7`).
    pub const LABELS: u32 = 0x0E00_0000;
    /// Shift of [`LABELS`].
    pub const LABEL_SHIFT: u32 = 25;
}

/// Header names, as Thunderbird writes them.
pub const X_MOZILLA_STATUS: &str = "X-Mozilla-Status";
/// See [`X_MOZILLA_STATUS`].
pub const X_MOZILLA_STATUS2: &str = "X-Mozilla-Status2";
/// See [`X_MOZILLA_STATUS`].
pub const X_MOZILLA_KEYS: &str = "X-Mozilla-Keys";

/// What Thunderbird recorded about a stored message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MozillaState {
    /// `X-Mozilla-Status` (16 bits, hex).
    pub status: Option<u32>,
    /// `X-Mozilla-Status2` (upper flag bits, hex).
    pub status2: Option<u32>,
    /// `X-Mozilla-Keys`: keywords as written.
    pub keys: Vec<String>,
}

impl MozillaState {
    /// Builds the state for flags and keywords (the reverse of [`MozillaState::flags`]
    /// and [`MozillaState::keywords`]), for the exporter.
    pub fn from_flags(flags: Flags, keywords: &[String]) -> Self {
        let mut status = 0;
        let mut status2 = 0;
        if flags.contains(Flags::SEEN) {
            status |= status::READ;
        }
        if flags.contains(Flags::ANSWERED) {
            status |= status::REPLIED;
        }
        if flags.contains(Flags::FLAGGED) {
            status |= status::MARKED;
        }
        if flags.contains(Flags::FORWARDED) {
            status |= status::FORWARDED;
        }
        if flags.contains(Flags::DELETED) {
            status2 |= status::IMAP_DELETED;
        }
        let mut keys: Vec<String> = keywords.to_vec();
        if flags.contains(Flags::JUNK) {
            keys.push("Junk".to_owned());
        }
        if flags.contains(Flags::NOT_JUNK) {
            keys.push("NonJunk".to_owned());
        }
        if flags.contains(Flags::DRAFT) {
            keys.push("$Draft".to_owned());
        }
        Self {
            status: Some(status),
            status2: Some(status2),
            keys,
        }
    }

    /// Whether any of the headers was present.
    pub fn is_present(&self) -> bool {
        self.status.is_some() || self.status2.is_some() || !self.keys.is_empty()
    }

    /// The message was deleted in a local folder and only waits for compaction; the
    /// importer skips it.
    pub fn is_expunged(&self) -> bool {
        self.status.is_some_and(|s| s & status::EXPUNGED != 0)
    }

    /// IMAP flags: read, replied, starred, forwarded, `\Deleted`, plus flags spelled as
    /// keywords (`$Forwarded`, `Junk`, `NonJunk`, …).
    pub fn flags(&self) -> Flags {
        let mut flags = Flags::empty();
        if let Some(s) = self.status {
            if s & status::READ != 0 {
                flags |= Flags::SEEN;
            }
            if s & status::REPLIED != 0 {
                flags |= Flags::ANSWERED;
            }
            if s & status::MARKED != 0 {
                flags |= Flags::FLAGGED;
            }
            if s & status::FORWARDED != 0 {
                flags |= Flags::FORWARDED;
            }
        }
        if self.status2.is_some_and(|s| s & status::IMAP_DELETED != 0) {
            flags |= Flags::DELETED;
        }
        for key in &self.keys {
            if let Some(flag) = flag_keyword(key) {
                flags |= flag;
            }
        }
        flags
    }

    /// Keywords (tags) that aren't flags: `X-Mozilla-Keys` plus an old-style label,
    /// lowercased, deduplicated, sorted.
    pub fn keywords(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .keys
            .iter()
            .filter(|k| flag_keyword(k).is_none())
            .map(|k| k.to_lowercase())
            .collect();
        if let Some(s2) = self.status2 {
            let label = (s2 & status::LABELS) >> status::LABEL_SHIFT;
            if label != 0 {
                out.push(format!("$label{label}"));
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// The three header lines (with `eol`), as Thunderbird writes them.
    pub fn header_lines(&self, eol: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(160);
        out.extend_from_slice(
            format!(
                "{X_MOZILLA_STATUS}: {:04x}",
                self.status.unwrap_or(0) & 0xffff
            )
            .as_bytes(),
        );
        out.extend_from_slice(eol);
        out.extend_from_slice(
            format!("{X_MOZILLA_STATUS2}: {:08x}", self.status2.unwrap_or(0)).as_bytes(),
        );
        out.extend_from_slice(eol);
        // Thunderbird pads the keys to 80 characters so it can rewrite them in place.
        let keys = self.keys.join(" ");
        out.extend_from_slice(format!("{X_MOZILLA_KEYS}: {keys:<80}").as_bytes());
        out.extend_from_slice(eol);
        out
    }
}

/// Keywords that are really flags. `$Draft` is Maze's own spelling for exports
/// (Thunderbird keeps drafts in a folder, not as a flag).
fn flag_keyword(key: &str) -> Option<Flags> {
    if key.eq_ignore_ascii_case("$draft") {
        return Some(Flags::DRAFT);
    }
    Flags::from_imap(key)
}

/// A message with Thunderbird's headers removed and line endings normalized.
#[derive(Clone, PartialEq, Eq)]
pub struct Reconstructed {
    /// The server's bytes: CRLF line endings, no `X-Mozilla-*` headers.
    pub bytes: Vec<u8>,
    /// What the removed headers said.
    pub mozilla: MozillaState,
}

impl std::fmt::Debug for Reconstructed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reconstructed")
            .field("bytes", &self.bytes.len())
            .field("mozilla", &self.mozilla)
            .finish()
    }
}

/// Removes Thunderbird's status headers from the top-level header block (parsing
/// them) and normalizes every line ending to CRLF (a lone `\r` stays as it is).
///
/// The result is what the IMAP server holds for messages Thunderbird downloaded, so
/// its length is the server's `RFC822.SIZE`.
pub fn reconstruct(message: &[u8]) -> Reconstructed {
    let mut out = Vec::with_capacity(message.len() + message.len() / 32 + 2);
    let mut mozilla = MozillaState::default();
    let mut pos = 0;
    // The header field being dropped (with its continuation lines), if any.
    let mut dropping: Option<Dropped> = None;
    let mut in_headers = true;
    while pos < message.len() {
        let end = memchr_newline(&message[pos..]).map_or(message.len(), |i| pos + i + 1);
        let line = &message[pos..end];
        pos = end;
        if in_headers {
            let content = trim_eol(line);
            if content.is_empty() {
                in_headers = false;
                dropping = None;
            } else if content[0] == b' ' || content[0] == b'\t' {
                // A continuation line belongs to the field before it.
                if let Some(kind) = dropping {
                    if kind == Dropped::Keys {
                        mozilla.keys.extend(
                            String::from_utf8_lossy(content)
                                .split_whitespace()
                                .map(str::to_owned),
                        );
                    }
                    continue;
                }
            } else {
                dropping = parse_mozilla_header(content, &mut mozilla);
                if dropping.is_some() {
                    continue;
                }
            }
        }
        push_crlf_line(&mut out, line);
    }
    Reconstructed {
        bytes: out,
        mozilla,
    }
}

/// Which of Thunderbird's headers is being dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dropped {
    Status,
    Keys,
}

/// If `line` is one of Thunderbird's status headers, records its value and says which.
fn parse_mozilla_header(line: &[u8], state: &mut MozillaState) -> Option<Dropped> {
    let colon = line.iter().position(|&b| b == b':')?;
    let name = &line[..colon];
    let value = String::from_utf8_lossy(&line[colon + 1..]);
    let value = value.trim();
    if name.eq_ignore_ascii_case(X_MOZILLA_STATUS.as_bytes()) {
        state.status = u32::from_str_radix(value, 16).ok().or(state.status);
        Some(Dropped::Status)
    } else if name.eq_ignore_ascii_case(X_MOZILLA_STATUS2.as_bytes()) {
        state.status2 = u32::from_str_radix(value, 16).ok().or(state.status2);
        Some(Dropped::Status)
    } else if name.eq_ignore_ascii_case(X_MOZILLA_KEYS.as_bytes()) {
        state
            .keys
            .extend(value.split_whitespace().map(str::to_owned));
        Some(Dropped::Keys)
    } else {
        None
    }
}

/// Appends `line` with its ending (if any) as CRLF.
fn push_crlf_line(out: &mut Vec<u8>, line: &[u8]) {
    match line {
        [rest @ .., b'\r', b'\n'] => {
            out.extend_from_slice(rest);
            out.extend_from_slice(b"\r\n");
        }
        [rest @ .., b'\n'] => {
            out.extend_from_slice(rest);
            out.extend_from_slice(b"\r\n");
        }
        _ => out.extend_from_slice(line),
    }
}

/// The line without its `\n` or `\r\n`.
pub(crate) fn trim_eol(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Position of the next `\n`.
pub(crate) fn memchr_newline(haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&b| b == b'\n')
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn decodes_status_bits_labels_and_keys() {
        let state = MozillaState {
            status: Some(0x1007),
            status2: Some(0x0220_0000 | 0x0400_0000),
            keys: vec![
                "$label1".to_owned(),
                "Todo".to_owned(),
                "NonJunk".to_owned(),
            ],
        };
        assert_eq!(
            state.flags(),
            Flags::SEEN
                | Flags::ANSWERED
                | Flags::FLAGGED
                | Flags::FORWARDED
                | Flags::DELETED
                | Flags::NOT_JUNK
        );
        // Label bits 0x06000000 >> 25 = 3.
        assert_eq!(state.keywords(), vec!["$label1", "$label3", "todo"]);
        assert!(!state.is_expunged());
        assert!(
            MozillaState {
                status: Some(0x0009),
                ..MozillaState::default()
            }
            .is_expunged()
        );
    }

    #[test]
    fn reconstruct_removes_headers_and_normalizes_line_endings() {
        let stored = b"X-Mozilla-Status: 0001\n\
X-Mozilla-Status2: 00000000\n\
X-Mozilla-Keys: $label2 project                                       \n\
Subject: Hello\r\n\
X-Other: keep\n\
\n\
Body line\n\
X-Mozilla-Status: 0001\n\
last\rline";
        let r = reconstruct(stored);
        assert_eq!(
            r.bytes,
            b"Subject: Hello\r\nX-Other: keep\r\n\r\nBody line\r\nX-Mozilla-Status: 0001\r\nlast\rline"
                .to_vec()
        );
        assert_eq!(r.mozilla.status, Some(1));
        assert_eq!(r.mozilla.status2, Some(0));
        assert_eq!(r.mozilla.keys, vec!["$label2", "project"]);
        assert_eq!(r.mozilla.flags(), Flags::SEEN);
    }

    #[test]
    fn reconstruct_drops_continuations_of_removed_headers_only() {
        let stored = b"X-Mozilla-Keys: a\n b\nSubject: x\n y\n\nbody\n";
        let r = reconstruct(stored);
        assert_eq!(r.bytes, b"Subject: x\r\n y\r\n\r\nbody\r\n".to_vec());
        assert_eq!(r.mozilla.keys, vec!["a", "b"]);
    }

    #[test]
    fn header_lines_round_trip_through_reconstruct() {
        let state = MozillaState::from_flags(
            Flags::SEEN | Flags::FLAGGED | Flags::DELETED | Flags::JUNK | Flags::DRAFT,
            &["$label1".to_owned(), "todo".to_owned()],
        );
        let mut stored = state.header_lines(b"\r\n");
        stored.extend_from_slice(b"Subject: s\r\n\r\nb\r\n");
        let r = reconstruct(&stored);
        assert_eq!(r.bytes, b"Subject: s\r\n\r\nb\r\n".to_vec());
        assert_eq!(
            r.mozilla.flags(),
            Flags::SEEN | Flags::FLAGGED | Flags::DELETED | Flags::JUNK | Flags::DRAFT
        );
        assert_eq!(r.mozilla.keywords(), vec!["$label1", "todo"]);
    }
}
