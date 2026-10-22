//! Subject normalization for threading and display.

use serde::{Deserialize, Serialize};

/// Reply prefixes, lowercase, without the colon.
const REPLY_PREFIXES: &[&str] = &[
    "re", "aw", "antw", "antwort", "sv", "vs", "odp", "res", "r", "rif", "ynt", "atb", "ats",
    "odg", "vá", "απ", "σχετ", "отв", "回复", "答复", "回覆",
];

/// Forward prefixes, lowercase, without the colon.
const FORWARD_PREFIXES: &[&str] = &[
    "fw", "fwd", "wg", "tr", "i", "enc", "rv", "pd", "doorst", "vl", "trs", "πρθ", "转发", "轉寄",
];

/// Longest prefix word we look at, in characters.
const MAX_PREFIX_CHARS: usize = 8;

/// Longest mailing-list tag (`[list-name]`) we strip, in characters.
const MAX_TAG_CHARS: usize = 64;

/// A subject split into its base and what was stripped.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SubjectInfo {
    /// The subject without reply/forward prefixes and leading list tags, whitespace
    /// collapsed. Case is preserved for display.
    pub base: String,
    /// A reply prefix (`Re:`, `AW:`, `Antw:`, `SV:`, …) was present.
    pub is_reply: bool,
    /// A forward prefix (`Fwd:`, `WG:`, `TR:`, …) was present.
    pub is_forward: bool,
}

impl SubjectInfo {
    /// Whether any reply or forward prefix was stripped.
    pub fn is_response(&self) -> bool {
        self.is_reply || self.is_forward
    }

    /// The comparison key used for subject threading: the base, lowercased.
    pub fn key(&self) -> String {
        self.base.to_lowercase()
    }
}

/// Split a subject into base and prefixes.
///
/// Strips any sequence of reply/forward prefixes in common languages (`Re:`,
/// `RE[2]:`, `Re^3:`, `AW:`, `WG:`, `Antw:`, `Fwd:`, `TR :`, full-width colons) and
/// mailing-list tags like `[dev]` anywhere in that leading sequence, plus a trailing
/// `(fwd)`.
pub fn parse_subject(subject: &str) -> SubjectInfo {
    let mut info = SubjectInfo::default();
    // Collapse first, so exotic whitespace can't hide a prefix until a second pass.
    let collapsed = collapse_whitespace(subject);
    let mut rest = collapsed.as_str();
    loop {
        if let Some(after) = strip_list_tag(rest) {
            rest = after.trim_start();
            continue;
        }
        let Some((kind, after)) = strip_prefix_word(rest) else {
            break;
        };
        match kind {
            Prefix::Reply => info.is_reply = true,
            Prefix::Forward => info.is_forward = true,
        }
        rest = after.trim_start();
    }
    let mut base = rest.to_owned();
    while let Some(stripped) = strip_suffix_ignore_case(&base, "(fwd)") {
        info.is_forward = true;
        base = stripped.trim_end().to_owned();
    }
    info.base = base;
    info
}

/// The subject without reply/forward prefixes and list tags (see [`parse_subject`]).
pub fn normalize_subject(subject: &str) -> String {
    parse_subject(subject).base
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prefix {
    Reply,
    Forward,
}

/// Strip `[tag]` at the start.
fn strip_list_tag(s: &str) -> Option<&str> {
    let inner = s.strip_prefix('[')?;
    let (end, _) = inner
        .char_indices()
        .take(MAX_TAG_CHARS)
        .find(|&(_, c)| c == ']' || c == '[')?;
    if inner[end..].starts_with('[') || end == 0 {
        return None;
    }
    Some(&inner[end + 1..])
}

/// Strip one `word[n]:` / `word^n:` / `word :` prefix if `word` is a known prefix.
fn strip_prefix_word(s: &str) -> Option<(Prefix, &str)> {
    let word_end = s
        .char_indices()
        .take(MAX_PREFIX_CHARS + 1)
        .find(|&(_, c)| !c.is_alphabetic())
        .map(|(i, _)| i)?;
    if word_end == 0 || s[..word_end].chars().count() > MAX_PREFIX_CHARS {
        return None;
    }
    let word = s[..word_end].to_lowercase();
    let kind = if REPLY_PREFIXES.contains(&word.as_str()) {
        Prefix::Reply
    } else if FORWARD_PREFIXES.contains(&word.as_str()) {
        Prefix::Forward
    } else {
        return None;
    };
    let mut rest = &s[word_end..];
    // Counters: Re[2]:, Re(2):, Re^2:
    if let Some(after) = rest.strip_prefix(['[', '(']) {
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        let after = &after[digits..];
        rest = after.strip_prefix([']', ')']).filter(|_| digits > 0)?;
    } else if let Some(after) = rest.strip_prefix('^') {
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &after[digits..];
    }
    // French typography puts a space before the colon ("RE :").
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    let rest = rest.strip_prefix([':', '：'])?;
    Some((kind, rest))
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn strip_suffix_ignore_case<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let start = s.len().checked_sub(suffix.len())?;
    let tail = s.get(start..)?;
    tail.eq_ignore_ascii_case(suffix).then(|| &s[..start])
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn base(s: &str) -> String {
        normalize_subject(s)
    }

    #[test]
    fn strips_prefixes_in_many_languages() {
        assert_eq!(base("Re: Lunch"), "Lunch");
        assert_eq!(base("RE: re: Fwd:  Lunch  plans "), "Lunch plans");
        assert_eq!(base("AW: WG: Angebot"), "Angebot");
        assert_eq!(base("Antw: Vergadering"), "Vergadering");
        assert_eq!(base("SV: Möte"), "Möte");
        assert_eq!(base("TR : Réunion"), "Réunion");
        assert_eq!(base("Re[3]: Status"), "Status");
        assert_eq!(base("Re^2: Status"), "Status");
        assert_eq!(base("回复：会议"), "会议");
        assert_eq!(base("Status update (fwd)"), "Status update");
    }

    #[test]
    fn strips_list_tags() {
        assert_eq!(base("[dev] Release 1.0"), "Release 1.0");
        assert_eq!(base("Re: [dev] Re: [dev] Release 1.0"), "Release 1.0");
        assert_eq!(base("[a][b] x"), "x");
    }

    #[test]
    fn leaves_ordinary_subjects_alone() {
        assert_eq!(base("Regarding: the plan"), "Regarding: the plan");
        assert_eq!(base("Reply needed"), "Reply needed");
        assert_eq!(base("Re:"), "");
        assert_eq!(base("[] empty tag"), "[] empty tag");
        assert_eq!(base("[unterminated tag"), "[unterminated tag");
        assert_eq!(base("Ergebnis: 3:1"), "Ergebnis: 3:1");
    }

    #[test]
    fn records_prefix_kinds() {
        let info = parse_subject("Fwd: Re: x");
        assert!(info.is_reply && info.is_forward && info.is_response());
        let info = parse_subject("x");
        assert!(!info.is_response());
        assert_eq!(parse_subject("Re: ÄRGER").key(), "ärger");
    }

    proptest! {
        #[test]
        fn normalizing_is_idempotent(s in "\\PC{0,40}") {
            let once = normalize_subject(&s);
            prop_assert_eq!(normalize_subject(&once), once);
        }

        #[test]
        fn adding_prefixes_does_not_change_the_base(
            s in "[A-Za-z0-9 äöü]{0,30}",
            prefix in prop::sample::select(vec!["Re: ", "AW: ", "Fwd: ", "WG: ", "[list] ", "Antw: "]),
        ) {
            let base = normalize_subject(&s);
            prop_assert_eq!(normalize_subject(&format!("{prefix}{base}")), base);
        }
    }
}
