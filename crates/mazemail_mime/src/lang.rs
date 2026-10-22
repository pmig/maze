//! Language hint for search analyzers (PLAN §4.4).

use serde::{Deserialize, Serialize};

/// The languages the search index has stemmed fields for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lang {
    /// German.
    De,
    /// English.
    En,
    /// Detected with confidence, but neither German nor English.
    Other,
}

/// Texts shorter than this (in characters) are too short to classify.
const MIN_CHARS: usize = 20;

/// Only the start of long texts is looked at; it's enough and keeps this fast.
const MAX_BYTES: usize = 4096;

/// Below this whatlang confidence the result is discarded.
const MIN_CONFIDENCE: f64 = 0.5;

/// Detect the language of a message body.
///
/// Returns `None` when the text is too short or the detection isn't confident; the
/// search index then fills only the language-neutral fields.
pub fn detect_language(text: &str) -> Option<Lang> {
    let mut end = text.len().min(MAX_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let sample = text[..end].trim();
    if sample.chars().filter(|c| c.is_alphabetic()).count() < MIN_CHARS {
        return None;
    }
    let info = whatlang::detect(sample)?;
    if info.confidence() < MIN_CONFIDENCE {
        return None;
    }
    Some(match info.lang() {
        whatlang::Lang::Deu => Lang::De,
        whatlang::Lang::Eng => Lang::En,
        _ => Lang::Other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_german_and_english() {
        assert_eq!(
            detect_language(
                "Hallo zusammen, anbei die Unterlagen für die Besprechung am Donnerstag. \
                 Bitte gebt mir bis morgen Bescheid, ob der Termin für euch passt."
            ),
            Some(Lang::De)
        );
        assert_eq!(
            detect_language(
                "Hi everyone, attached are the documents for Thursday's meeting. \
                 Please let me know by tomorrow whether the time works for you."
            ),
            Some(Lang::En)
        );
        assert_eq!(
            detect_language(
                "Bonjour à tous, vous trouverez ci-joint les documents pour la réunion de jeudi. \
                 Merci de me dire avant demain si l'horaire vous convient."
            ),
            Some(Lang::Other)
        );
    }

    #[test]
    fn short_or_empty_text_has_no_language() {
        assert_eq!(detect_language(""), None);
        assert_eq!(detect_language("ok"), None);
        assert_eq!(detect_language("1234 5678 !!!! ----"), None);
    }

    #[test]
    fn long_text_is_sampled_on_a_char_boundary() {
        let text = "ä".repeat(10_000);
        let _ = detect_language(&text);
    }
}
