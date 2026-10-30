//! Single-language spellers and the personal word list.

use std::collections::HashSet;
use std::fmt;

use spellbook::Dictionary;

use crate::suggest::{self, Case};
use crate::{Lang, LoadError};

/// Checks words in one language.
///
/// Words come from [`crate::words`]: no surrounding punctuation, apostrophes normalized to
/// `'` and hyphens to `-`.
pub trait Speller: Send + Sync {
    /// Whether `word` is spelled correctly.
    fn check(&self, word: &str) -> bool;

    /// Corrections for `word`, best first, at most [`suggest::MAX_SUGGESTIONS`].
    ///
    /// Slow: tens of milliseconds for German. Never call this on the UI thread.
    fn suggest(&self, word: &str) -> Vec<String>;
}

/// A [`Speller`] backed by a bundled Hunspell dictionary through spellbook.
#[derive(Clone, Copy)]
pub struct SpellbookSpeller {
    lang: Lang,
    dictionary: &'static Dictionary,
}

impl SpellbookSpeller {
    /// The speller for `lang`. The first call per language and process decompresses and
    /// parses the dictionary (about 10 ms, see the crate docs); later calls are free.
    pub fn new(lang: Lang) -> Result<Self, LoadError> {
        Ok(Self {
            lang,
            dictionary: crate::dictionary::dictionary(lang)?,
        })
    }

    /// The speller's language.
    pub fn lang(&self) -> Lang {
        self.lang
    }
}

impl fmt::Debug for SpellbookSpeller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpellbookSpeller")
            .field("lang", &self.lang)
            .finish_non_exhaustive()
    }
}

impl SpellbookSpeller {
    /// The dictionary, plus Swiss spelling for de-CH: the bundled de-CH list spells
    /// "Straße" with ß like de-DE, while Swiss German always writes "Strasse".
    fn check_spelling(&self, word: &str) -> bool {
        self.dictionary.check(word)
            || (self.lang == Lang::DeCh && swiss_variants(word).any(|v| self.dictionary.check(&v)))
    }
}

impl Speller for SpellbookSpeller {
    fn check(&self, word: &str) -> bool {
        if self.check_spelling(word) {
            return true;
        }
        // German contracts "es" to "'s" after almost any word ("geht's", "wie's"); the
        // dictionaries list only a few such forms.
        self.lang.family() == mazemail_mime::Lang::De
            && word
                .strip_suffix("'s")
                .is_some_and(|stem| stem.chars().count() >= 2 && self.check_spelling(stem))
    }

    fn suggest(&self, word: &str) -> Vec<String> {
        let mut raw = Vec::new();
        self.dictionary.suggest(word, &mut raw);
        if self.lang == Lang::DeCh {
            for suggestion in &mut raw {
                if suggestion.contains('ß') {
                    *suggestion = suggestion.replace('ß', "ss");
                }
            }
        }
        suggest::refine(word, raw)
    }
}

/// Spellings of a Swiss word with some of its "ss" written "ß" (at most the first three
/// "ss", so at most seven variants).
fn swiss_variants(word: &str) -> impl Iterator<Item = String> + '_ {
    let positions: Vec<usize> = word.match_indices("ss").map(|(i, _)| i).take(3).collect();
    let count = positions.len();
    (1u32..(1 << count)).map(move |mask| {
        let mut out = String::with_capacity(word.len());
        let mut pos = 0;
        for (bit, &at) in positions.iter().enumerate() {
            if mask & (1 << bit) != 0 {
                out.push_str(&word[pos..at]);
                out.push('ß');
                pos = at + 2;
            }
        }
        out.push_str(&word[pos..]);
        out
    })
}

/// Extra words to accept in every language: the personal dictionary.
///
/// Matching follows Hunspell: a lowercase entry also accepts the capitalized and
/// ALL-CAPS forms (`maze` accepts `Maze` and `MAZE`), a capitalized entry its ALL-CAPS
/// form; everything else must match exactly (`Maze` doesn't accept `maze`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersonalWords {
    words: HashSet<String>,
}

impl PersonalWords {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a word; returns whether it was new.
    pub fn insert(&mut self, word: &str) -> bool {
        self.words
            .insert(crate::check::normalize(word).into_owned())
    }

    /// Remove a word; returns whether it was there.
    pub fn remove(&mut self, word: &str) -> bool {
        self.words.remove(crate::check::normalize(word).as_ref())
    }

    /// Whether `word` is accepted (see the type docs for case rules).
    pub fn contains(&self, word: &str) -> bool {
        if self.words.is_empty() {
            return false;
        }
        let word = crate::check::normalize(word);
        if self.words.contains(word.as_ref()) {
            return true;
        }
        match Case::of(&word) {
            Case::Title => self.words.contains(&suggest::decapitalize(&word)),
            Case::Upper => {
                let lower = word.to_lowercase();
                self.words.contains(&suggest::capitalize(&lower)) || self.words.contains(&lower)
            }
            Case::Other => false,
        }
    }

    /// Number of words.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

impl<S: AsRef<str>> FromIterator<S> for PersonalWords {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        let mut words = Self::new();
        for word in iter {
            words.insert(word.as_ref());
        }
        words
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personal_words_follow_hunspell_case_rules() {
        let words: PersonalWords = ["maze", "Glasskube", "mazemail’s"].into_iter().collect();
        assert!(words.contains("maze"));
        assert!(words.contains("Maze"));
        assert!(words.contains("MAZE"));
        assert!(words.contains("Glasskube"));
        assert!(words.contains("GLASSKUBE"));
        assert!(!words.contains("glasskube"));
        assert!(words.contains("mazemail's"));
        assert!(!words.contains("mazes"));
        assert!(!PersonalWords::new().contains("maze"));
    }

    #[test]
    fn swiss_variants_replace_ss() {
        let variants: Vec<String> = swiss_variants("Grüsse aus Strassburg").collect();
        assert_eq!(
            variants,
            [
                "Grüße aus Strassburg",
                "Grüsse aus Straßburg",
                "Grüße aus Straßburg"
            ]
        );
        assert_eq!(swiss_variants("Haus").count(), 0);
    }

    #[test]
    fn insert_and_remove() {
        let mut words = PersonalWords::new();
        assert!(words.insert("Jdoe"));
        assert!(!words.insert("Jdoe"));
        assert_eq!(words.len(), 1);
        assert!(words.remove("Jdoe"));
        assert!(words.is_empty());
    }
}
