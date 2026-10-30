//! Checking whole texts paragraph by paragraph.

use std::borrow::Cow;
use std::fmt;
use std::ops::Range;

use crate::speller::{PersonalWords, SpellbookSpeller, Speller};
use crate::tokenize::{Word, is_all_caps, is_apostrophe, is_hyphen, words};
use crate::{Lang, LoadError, suggest};

/// A misspelled word in a checked text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Misspelling {
    /// Byte range in the checked text.
    pub range: Range<usize>,
    /// The word as written (`&text[range]`).
    pub word: String,
    /// The language the word was checked in: the paragraph's detected language, or `None`
    /// if the paragraph was too short to tell (or in no enabled language) and the word
    /// is unknown in every enabled language. [`Checker::suggest`] uses it.
    pub lang: Option<Lang>,
}

/// Which spellers decide about a paragraph.
#[derive(Debug, Clone, Copy)]
enum Judge {
    /// The speller at this index (the paragraph's language).
    One(usize),
    /// A word is fine if any speller accepts it.
    Any,
}

/// Spell checks texts in a set of languages, with a personal word list.
///
/// Cheap to create once the dictionaries are loaded (they are shared per process); keep
/// one per composer, on a background thread: [`Checker::check_text`] takes about a
/// millisecond per paragraph, [`Checker::suggest`] tens of milliseconds.
pub struct Checker {
    spellers: Vec<(Lang, Box<dyn Speller>)>,
    personal: PersonalWords,
}

impl fmt::Debug for Checker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Checker")
            .field("langs", &self.langs().collect::<Vec<_>>())
            .field("personal", &self.personal.len())
            .finish()
    }
}

impl Checker {
    /// A checker for `langs` (in order of preference: with `[de-AT, de-DE]` German
    /// paragraphs are checked as de-AT) using the bundled dictionaries. Loads dictionaries
    /// that aren't loaded yet; duplicates in `langs` are ignored.
    pub fn new(langs: &[Lang], personal: PersonalWords) -> Result<Self, LoadError> {
        let mut spellers: Vec<(Lang, Box<dyn Speller>)> = Vec::with_capacity(langs.len());
        for &lang in langs {
            if spellers.iter().all(|(l, _)| *l != lang) {
                spellers.push((lang, Box::new(SpellbookSpeller::new(lang)?)));
            }
        }
        Ok(Self::with_spellers(spellers, personal))
    }

    /// A checker with custom spellers, e.g. a platform spell checker or a test double.
    pub fn with_spellers(spellers: Vec<(Lang, Box<dyn Speller>)>, personal: PersonalWords) -> Self {
        Self { spellers, personal }
    }

    /// The enabled languages, in order of preference.
    pub fn langs(&self) -> impl Iterator<Item = Lang> + '_ {
        self.spellers.iter().map(|(lang, _)| *lang)
    }

    /// The personal word list.
    pub fn personal(&self) -> &PersonalWords {
        &self.personal
    }

    /// The personal word list, to add or remove words.
    pub fn personal_mut(&mut self) -> &mut PersonalWords {
        &mut self.personal
    }

    /// The misspelled words in `text` (composer Markdown), in order.
    ///
    /// The text is split into paragraphs at blank lines. Each paragraph's language is
    /// detected ([`mazemail_mime::detect_language`], from its words only, at least 20
    /// letters); if it's German or English and a matching language is enabled, the
    /// paragraph is checked in the first such language. Otherwise a word is accepted if
    /// any enabled language knows it. With no languages enabled nothing is reported.
    ///
    /// Hyphenated words that the dictionary doesn't know as a whole are checked part by
    /// part, and only the unknown parts are reported. A word followed by `.` is also
    /// tried with the period (abbreviations: `bzw.`, `etc.`), one followed by a hyphen
    /// with it (`Haupt- und Nebeneingang`).
    pub fn check_text(&self, text: &str) -> Vec<Misspelling> {
        let mut out = Vec::new();
        if self.spellers.is_empty() {
            return out;
        }
        let words = words(text);
        let mut words = words.as_slice();
        for paragraph in paragraphs(text) {
            let count = words
                .iter()
                .take_while(|w| w.range.start < paragraph.end)
                .count();
            let (here, rest) = words.split_at(count);
            words = rest;
            if here.is_empty() {
                continue;
            }
            let judge = self.judge(here);
            let lang = match judge {
                Judge::One(i) => Some(self.spellers[i].0),
                Judge::Any => None,
            };
            for word in here {
                self.check_word(text, word, judge, lang, &mut out);
            }
        }
        out
    }

    /// Corrections for a misspelling from [`Checker::check_text`], best first, at most
    /// [`suggest::MAX_SUGGESTIONS`]. Asks the misspelling's language, or every enabled
    /// language if it has none.
    ///
    /// Slow: tens of milliseconds per language for German. Never call this on the UI
    /// thread.
    pub fn suggest(&self, misspelling: &Misspelling) -> Vec<String> {
        let word = normalize(&misspelling.word);
        let spellers = self
            .spellers
            .iter()
            .filter(|(lang, _)| misspelling.lang.is_none_or(|l| l == *lang));
        let mut all = Vec::new();
        for (_, speller) in spellers {
            all.extend(speller.suggest(&word));
        }
        suggest::refine(&word, all)
    }

    /// Pick the speller for a paragraph from the language of its words.
    fn judge(&self, words: &[Word<'_>]) -> Judge {
        let mut sample = String::new();
        for word in words {
            sample.push_str(word.text);
            sample.push(' ');
        }
        let detected = mazemail_mime::detect_language(&sample);
        detected
            .and_then(|family| {
                self.spellers
                    .iter()
                    .position(|(lang, _)| lang.family() == family)
            })
            .map_or(Judge::Any, Judge::One)
    }

    fn accepts(&self, word: &str, judge: Judge) -> bool {
        self.personal.contains(word)
            || match judge {
                Judge::One(i) => self.spellers[i].1.check(word),
                Judge::Any => self.spellers.iter().any(|(_, s)| s.check(word)),
            }
    }

    fn check_word(
        &self,
        text: &str,
        word: &Word<'_>,
        judge: Judge,
        lang: Option<Lang>,
        out: &mut Vec<Misspelling>,
    ) {
        let normalized = normalize(word.text);
        if self.accepts(&normalized, judge) {
            return;
        }
        let after = text[word.range.end..].chars().next();
        if after == Some('.') && self.accepts(&format!("{normalized}."), judge) {
            return;
        }
        if after.is_some_and(is_hyphen) && self.accepts(&format!("{normalized}-"), judge) {
            return;
        }
        let mut report = |range: Range<usize>| {
            out.push(Misspelling {
                word: text[range.clone()].to_owned(),
                range,
                lang,
            });
        };
        if !word.text.contains(is_hyphen) {
            report(word.range.clone());
            return;
        }
        // Unknown compound with hyphens: report the unknown parts.
        let mut start = word.range.start;
        let parts: Vec<&str> = word.text.split(is_hyphen).collect();
        let last = parts.len() - 1;
        for (i, part) in parts.into_iter().enumerate() {
            let range = start..start + part.len();
            // Skip the hyphen (one char, 1–3 bytes).
            start = range.end + text[range.end..].chars().next().map_or(0, char::len_utf8);
            if part.chars().count() < 2 || is_all_caps(part) {
                continue;
            }
            let part = normalize(part);
            let known = self.accepts(&part, judge)
                // A first part like "Rechnungs-" may only exist with the hyphen.
                || (i < last && self.accepts(&format!("{part}-"), judge));
            if !known {
                report(range);
            }
        }
    }
}

/// Check `text` in `langs` with `personal` words: [`Checker::check_text`] without keeping a
/// [`Checker`].
pub fn check_text(
    text: &str,
    langs: &[Lang],
    personal: &PersonalWords,
) -> Result<Vec<Misspelling>, LoadError> {
    Ok(Checker::new(langs, personal.clone())?.check_text(text))
}

/// Typographic apostrophes become `'`, Unicode hyphens `-`: the dictionaries' spelling.
pub(crate) fn normalize(word: &str) -> Cow<'_, str> {
    if word.contains(|c: char| (is_apostrophe(c) && c != '\'') || (is_hyphen(c) && c != '-')) {
        Cow::Owned(
            word.chars()
                .map(|c| match c {
                    c if is_apostrophe(c) => '\'',
                    c if is_hyphen(c) => '-',
                    c => c,
                })
                .collect(),
        )
    } else {
        Cow::Borrowed(word)
    }
}

/// Byte ranges of the paragraphs: runs of non-blank lines.
fn paragraphs(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut current: Option<Range<usize>> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let range = offset..offset + line.len();
        offset = range.end;
        if line.trim().is_empty() {
            out.extend(current.take());
        } else {
            match &mut current {
                Some(paragraph) => paragraph.end = range.end,
                None => current = Some(range),
            }
        }
    }
    out.extend(current);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn checker(langs: &[Lang]) -> Checker {
        Checker::new(langs, PersonalWords::new()).unwrap()
    }

    /// The misspelled words of `text`, checked in `langs`.
    fn misspelled(langs: &[Lang], text: &str) -> Vec<String> {
        let found = checker(langs).check_text(text);
        for m in &found {
            assert_eq!(text[m.range.clone()], m.word);
        }
        found.into_iter().map(|m| m.word).collect()
    }

    fn speller(lang: Lang) -> SpellbookSpeller {
        SpellbookSpeller::new(lang).unwrap()
    }

    const GERMAN: &str = "Die Donaudampfschifffahrtsgesellschaft hat den Bericht an den \
        Rechnungsprüfungsausschuss geschickt, bzw. an die zuständige Stelle.";

    #[test]
    fn german_compounds_are_known() {
        for lang in [Lang::DeDe, Lang::DeAt, Lang::DeCh] {
            assert_eq!(misspelled(&[lang], GERMAN), Vec::<String>::new(), "{lang}");
        }
    }

    #[test]
    fn austrian_words_need_de_at() {
        // igerman98's de-DE and de-CH lists don't have Austrian terms; that's what spellbook
        // (like Hunspell) reports, and why de-AT is a language of its own.
        let austrian = ["Jänner", "Paradeiser", "Sackerl", "Topfen"];
        for word in austrian {
            assert!(speller(Lang::DeAt).check(word), "de-AT: {word}");
        }
        for word in ["Jänner", "Paradeiser", "Sackerl"] {
            assert!(!speller(Lang::DeDe).check(word), "de-DE: {word}");
            assert!(!speller(Lang::DeCh).check(word), "de-CH: {word}");
        }
        // Words every variant has.
        for lang in [Lang::DeDe, Lang::DeAt, Lang::DeCh] {
            assert!(speller(lang).check("Januar"), "{lang}");
            assert!(speller(lang).check("Tomate"), "{lang}");
        }
        let text = "Im Jänner gibt es keine Paradeiser aus dem eigenen Garten, leider.";
        assert_eq!(misspelled(&[Lang::DeAt], text), Vec::<String>::new());
        assert_eq!(misspelled(&[Lang::DeDe], text), ["Jänner", "Paradeiser"]);
    }

    #[test]
    fn swiss_german_writes_ss() {
        assert!(speller(Lang::DeCh).check("Strasse"));
        assert!(speller(Lang::DeCh).check("Grüsse"));
        assert!(!speller(Lang::DeDe).check("Strasse"));
        assert!(!speller(Lang::DeAt).check("Grüsse"));
        let suggestions = speller(Lang::DeCh).suggest("Strase");
        assert!(
            suggestions.iter().any(|s| s == "Strasse"),
            "{suggestions:?}"
        );
        assert!(
            suggestions.iter().all(|s| !s.contains('ß')),
            "{suggestions:?}"
        );
    }

    #[test]
    fn english_basics() {
        let us = speller(Lang::EnUs);
        let gb = speller(Lang::EnGb);
        for word in [
            "receive",
            "color",
            "realize",
            "don't",
            "it's",
            "well-known",
            "the",
        ] {
            assert!(us.check(word), "en-US: {word}");
        }
        for word in ["receive", "colour", "realise", "don't"] {
            assert!(gb.check(word), "en-GB: {word}");
        }
        assert!(!us.check("colour"));
        assert!(!gb.check("color"));
        assert!(!us.check("recieve"));
    }

    #[test]
    fn finds_misspellings_with_their_ranges() {
        let text = "Wir treffen uns morgen zum Frühstuck im Kaffeehaus am Ring.";
        let found = checker(&[Lang::DeAt]).check_text(text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].word, "Frühstuck");
        assert_eq!(
            found[0].range,
            text.find("Frühstuck")
                .map(|s| s..s + "Frühstuck".len())
                .unwrap()
        );
        assert_eq!(found[0].lang, Some(Lang::DeAt));

        let text = "I hope to recieve your answer by tomorrow, thanks a lot.";
        assert_eq!(misspelled(&[Lang::EnUs], text), ["recieve"]);
    }

    #[test]
    fn suggestions_rank_the_obvious_fix_first() {
        let en = checker(&[Lang::EnUs]);
        let found = en.check_text("I hope to recieve your answer by tomorrow, thanks a lot.");
        assert_eq!(en.suggest(&found[0])[0], "receive");

        for lang in [Lang::DeDe, Lang::DeAt] {
            let de = checker(&[lang]);
            let found = de.check_text("Wir treffen uns morgen zum Frühstuck im Kaffeehaus.");
            let suggestions = de.suggest(&found[0]);
            assert_eq!(suggestions[0], "Frühstück", "{lang}");
            assert!(suggestions.len() <= suggest::MAX_SUGGESTIONS);
        }

        // Capitalization follows the input; no splits.
        let suggestions = speller(Lang::EnUs).suggest("Recieve");
        assert_eq!(suggestions[0], "Receive");
        let suggestions = speller(Lang::DeDe).suggest("Strasse");
        assert_eq!(suggestions[0], "Straße");
        assert!(
            suggestions.iter().all(|s| !s.contains([' ', '-'])),
            "{suggestions:?}"
        );
    }

    #[test]
    fn markdown_and_non_prose_are_skipped() {
        let text = "# Teh heading\n\n\
            Please *recieve* the [documnet](https://exmaple.com/documnet_v2) today.\n\n\
            Run `cargo bulid` or mail jdoe@exmaple.com, see https://exmaple.org/pathh.\n\n\
            > I qoute this line\n\n\
            ```\nlet wrod = teh;\n```\n\n\
            The NASA and HTTPS stuff, the 2nd MP3 and iPhone are fine.";
        assert_eq!(
            misspelled(&[Lang::EnUs], text),
            ["Teh", "recieve", "documnet"]
        );
    }

    #[test]
    fn mixed_languages_check_each_paragraph_in_its_language() {
        let langs = [Lang::EnUs, Lang::DeAt];
        let text = "Hallo Maria,\n\n\
            im Jänner fahren wir nach Wien und kaufen dort Paradeiser und Erdäpfel \
            für das Abendesen.\n\n\
            I will send you the documents tomorrow becuase the office is closed today.\n\n\
            Liebe Grüße";
        let found = checker(&langs).check_text(text);
        let summary: Vec<(&str, Option<Lang>)> =
            found.iter().map(|m| (m.word.as_str(), m.lang)).collect();
        assert_eq!(
            summary,
            [
                ("Abendesen", Some(Lang::DeAt)),
                ("becuase", Some(Lang::EnUs))
            ]
        );
        // The German paragraph is checked in German only: an English word is flagged
        // there, but accepted in the English one.
        let text = "Wir sehen uns morgen im Büro und besprechen dann alles weitere, tomorrow.\n\n\
            We will meet tomorrow at the office and discuss everything else then.";
        assert_eq!(misspelled(&langs, text), ["tomorrow"]);
    }

    #[test]
    fn short_paragraphs_accept_words_of_any_enabled_language() {
        let langs = [Lang::EnUs, Lang::DeAt];
        assert_eq!(
            misspelled(&langs, "Danke, thanks!\n\nLG Philip"),
            Vec::<String>::new()
        );
        let found = checker(&langs).check_text("Danke, thnaks!");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].word, "thnaks");
        assert_eq!(found[0].lang, None);
        // Suggestions then come from every enabled language.
        let suggestions = checker(&langs).suggest(&found[0]);
        assert!(suggestions.iter().any(|s| s == "thanks"), "{suggestions:?}");
    }

    #[test]
    fn personal_words_are_accepted() {
        let text = "Greetings from the Glasskube team, see you at Mazecon.";
        assert_eq!(misspelled(&[Lang::EnUs], text), ["Glasskube", "Mazecon"]);
        let personal: PersonalWords = ["Glasskube", "mazecon"].into_iter().collect();
        assert_eq!(
            check_text(text, &[Lang::EnUs], &personal).unwrap(),
            Vec::<Misspelling>::new()
        );
        let mut checker = checker(&[Lang::EnUs]);
        checker.personal_mut().insert("Glasskube");
        assert_eq!(checker.check_text(text).len(), 1);
    }

    #[test]
    fn hyphenated_words_report_only_unknown_parts() {
        let text = "Unser Software-Entwicklr hat die E-Mail-Adresse für Haupt- und \
            Nebeneingang geprüft, z.B. die Rechnungs- und Lieferadresse.";
        let found = checker(&[Lang::DeDe]).check_text(text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].word, "Entwicklr");
        assert_eq!(&text[found[0].range.clone()], "Entwicklr");

        let text = "It is a well-known fact that the self-contained setup works, etc.";
        assert_eq!(misspelled(&[Lang::EnUs], text), Vec::<String>::new());
        let text = "It is a well-knwon fact that the old setup works very well for us.";
        assert_eq!(misspelled(&[Lang::EnUs], text), ["knwon"]);
    }

    #[test]
    fn apostrophes() {
        let text = "Don't worry, it’s fine and we're happy with the team's work so far.";
        assert_eq!(misspelled(&[Lang::EnUs], text), Vec::<String>::new());
        let text = "Wie geht's dir? Gibt’s schon Neuigkeiten von der Bank wegen des Kredits?";
        assert_eq!(misspelled(&[Lang::DeAt], text), Vec::<String>::new());
    }

    #[test]
    fn no_languages_report_nothing() {
        assert_eq!(
            misspelled(&[], "Teh qiuck brwon fox."),
            Vec::<String>::new()
        );
    }

    #[test]
    fn paragraphs_split_at_blank_lines() {
        assert_eq!(paragraphs("a\nb\n\n  \nc\n"), [0..4, 8..10]);
        assert_eq!(paragraphs(""), Vec::<Range<usize>>::new());
    }

    /// Load and check timings, printed rather than asserted. Run with
    /// `cargo nextest run -p spelling --release --run-ignored only timings --no-capture`.
    #[test]
    #[ignore = "prints timings"]
    #[allow(clippy::print_stderr)]
    fn timings() {
        use std::time::Instant;
        let samples = [
            (
                Lang::EnUs,
                "I hope to receive your answer by tomorrow, thanks a lot for the help.",
                "recieve",
            ),
            (
                Lang::DeAt,
                "Die Donaudampfschifffahrtsgesellschaft hat den Rechnungsprüfungsausschuss informiert.",
                "Frühstuck",
            ),
        ];
        for lang in Lang::ALL {
            let start = Instant::now();
            let _ = crate::dictionary::load(lang).unwrap();
            eprintln!("{lang}: load {:?}", start.elapsed());
        }
        for (lang, text, typo) in samples {
            let checker = checker(&[lang]);
            let words = words(text);
            let speller = speller(lang);
            let start = Instant::now();
            for word in &words {
                let _ = speller.check(word.text);
            }
            eprintln!("{lang}: check {} words {:?}", words.len(), start.elapsed());
            let start = Instant::now();
            let _ = checker.check_text(text);
            eprintln!("{lang}: check_text paragraph {:?}", start.elapsed());
            let start = Instant::now();
            let suggestions = speller.suggest(typo);
            eprintln!(
                "{lang}: suggest {typo} {:?} -> {suggestions:?}",
                start.elapsed()
            );
        }
    }
}
