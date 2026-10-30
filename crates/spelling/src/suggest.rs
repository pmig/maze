//! Cleaning up and re-ranking the dictionaries' suggestions.
//!
//! spellbook's suggestions follow Nuspell's order, which is weak for German: for
//! "Frühstuck" it ranks "Frühstück" only fourth and offers splits like "Strass e".
//! [`refine`] drops splits the input didn't ask for, carries the input's capitalization
//! over and sorts by a weighted edit distance that treats umlaut and accent mix-ups,
//! ß/ss and swapped neighbors as small mistakes.

use std::collections::HashSet;

/// At most this many suggestions are offered.
pub const MAX_SUGGESTIONS: usize = 5;

/// Clean up `raw` suggestions (in the dictionary's order) for the misspelled `word`.
///
/// - Drops suggestions with a space or hyphen when `word` has none (splits like
///   "Strass e" or "re-cieve"), the word itself and duplicates.
/// - Applies the input's capitalization: an ALL-CAPS input gets ALL-CAPS suggestions, a
///   capitalized one capitalized suggestions; lowercase input keeps the dictionary's case
///   (German nouns stay capitalized).
/// - Sorts by [`distance`], the dictionary's order breaking ties, and keeps the first
///   [`MAX_SUGGESTIONS`].
pub fn refine(word: &str, raw: impl IntoIterator<Item = String>) -> Vec<String> {
    let has_space = word.contains(char::is_whitespace);
    let has_hyphen = word.contains('-');
    let case = Case::of(word);
    let mut seen = HashSet::new();
    let mut scored: Vec<(f32, usize, String)> = Vec::new();
    for (rank, suggestion) in raw.into_iter().enumerate() {
        if (!has_space && suggestion.contains(char::is_whitespace))
            || (!has_hyphen && suggestion.contains('-'))
        {
            continue;
        }
        let suggestion = case.apply(&suggestion);
        if suggestion == word || !seen.insert(suggestion.clone()) {
            continue;
        }
        // The dictionary's order counts a little, so it decides between equal distances
        // and a far better distance still wins.
        let score = distance(word, &suggestion) + rank as f32 * 0.05;
        scored.push((score, rank, suggestion));
    }
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.truncate(MAX_SUGGESTIONS);
    scored.into_iter().map(|(_, _, s)| s).collect()
}

/// The capitalization pattern of a word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Case {
    /// `HAUS`
    Upper,
    /// `Haus`
    Title,
    /// `haus`, `iPhone`, …
    Other,
}

impl Case {
    pub(crate) fn of(word: &str) -> Self {
        if crate::tokenize::is_all_caps(word) {
            Case::Upper
        } else if word.chars().next().is_some_and(char::is_uppercase) {
            Case::Title
        } else {
            Case::Other
        }
    }

    pub(crate) fn apply(self, word: &str) -> String {
        match self {
            Case::Upper => word.to_uppercase(),
            Case::Title => capitalize(word),
            Case::Other => word.to_owned(),
        }
    }
}

/// Uppercase the first letter.
pub(crate) fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Lowercase the first letter.
pub(crate) fn decapitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A weighted Damerau–Levenshtein distance (optimal string alignment) between two words.
///
/// Inserting, deleting or replacing a letter costs 1, swapping two neighbors 0.7, a
/// letter for the same letter with another accent or umlaut (`u`/`ü`, `a`/`ä`, `e`/`é`)
/// 0.3, `ß` for `ss` 0.3 and a different case of the same letters 0.1.
pub fn distance(a: &str, b: &str) -> f32 {
    let fold = |s: &str| -> Vec<char> {
        s.chars()
            .flat_map(char::to_lowercase)
            .flat_map(|c| if c == 'ß' { vec!['s', 's'] } else { vec![c] })
            .collect()
    };
    let (fa, fb) = (fold(a), fold(b));
    let mut cost = osa(&fa, &fb);
    if a.to_lowercase() != b.to_lowercase() && cost == 0.0 {
        // Only ß vs ss differs.
        cost += 0.3;
    }
    if a != b && a.to_lowercase() == b.to_lowercase() {
        cost += 0.1;
    }
    cost
}

fn osa(a: &[char], b: &[char]) -> f32 {
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0.0f32; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i as f32;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j as f32;
    }
    for i in 1..=n {
        for j in 1..=m {
            let sub = substitution_cost(a[i - 1], b[j - 1]);
            let mut best = (d[i - 1][j] + 1.0)
                .min(d[i][j - 1] + 1.0)
                .min(d[i - 1][j - 1] + sub);
            if i > 1
                && j > 1
                && a[i - 1] == b[j - 2]
                && a[i - 2] == b[j - 1]
                && a[i - 1] != a[i - 2]
            {
                best = best.min(d[i - 2][j - 2] + 0.7);
            }
            d[i][j] = best;
        }
    }
    d[n][m]
}

fn substitution_cost(a: char, b: char) -> f32 {
    if a == b {
        0.0
    } else if base_letter(a) == base_letter(b) {
        0.3
    } else {
        1.0
    }
}

/// The letter without its accent or umlaut (lowercase input).
fn base_letter(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'ç' => 'c',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ñ' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ý' | 'ÿ' => 'y',
        c => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn owned(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn umlaut_mix_ups_rank_first() {
        // spellbook's order for "Frühstuck" with de-DE puts "Frühstück" fourth.
        let raw = owned(&[
            "Frühstücks",
            "Frühstücke",
            "Frühs tuck",
            "Frühstück",
            "Frühstuck-",
        ]);
        assert_eq!(
            refine("Frühstuck", raw),
            ["Frühstück", "Frühstücks", "Frühstücke"]
        );
    }

    #[test]
    fn drops_splits_the_input_did_not_have() {
        let raw = owned(&["Strass e", "Stra-sse", "Straße", "Strasse"]);
        assert_eq!(refine("Strasse", raw), ["Straße"]);
        let raw = owned(&["well known", "well-known"]);
        assert_eq!(refine("well-knwon", raw), ["well-known"]);
    }

    #[test]
    fn keeps_the_input_capitalization() {
        assert_eq!(refine("Recieve", owned(&["receive"])), ["Receive"]);
        assert_eq!(refine("RECIEVE", owned(&["receive"])), ["RECEIVE"]);
        // Lowercase input keeps capitalized nouns.
        assert_eq!(
            refine("hauss", owned(&["Haus", "Hauses"])),
            ["Haus", "Hauses"]
        );
    }

    #[test]
    fn deduplicates_and_caps_the_list() {
        let raw = owned(&[
            "Receive", "receive", "relieve", "recede", "receiver", "deceive", "reweave", "recieve",
        ]);
        let refined = refine("Recieve", raw);
        assert_eq!(refined.len(), MAX_SUGGESTIONS);
        assert_eq!(refined[0], "Receive");
        assert_eq!(refined.iter().filter(|s| *s == "Receive").count(), 1);
    }

    #[test]
    fn distance_weights() {
        assert_eq!(distance("haus", "haus"), 0.0);
        assert!((distance("Frühstuck", "Frühstück") - 0.3).abs() < 1e-6);
        assert!((distance("recieve", "receive") - 0.7).abs() < 1e-6);
        assert!((distance("Strasse", "Straße") - 0.3).abs() < 1e-6);
        assert!((distance("haus", "Haus") - 0.1).abs() < 1e-6);
        assert_eq!(distance("cat", "cut"), 1.0);
        assert_eq!(distance("", "abc"), 3.0);
    }
}
