//! Spell checking languages (one per bundled dictionary).

use std::fmt;
use std::str::FromStr;

/// A language with a bundled dictionary.
///
/// Written and parsed as a BCP 47 tag (`de-AT`, `en-US`). [`FromStr`] also takes POSIX
/// locale spellings (`de_AT`, `de_AT.UTF-8`), any case, and bare languages: `de` is
/// [`Lang::DeDe`], `en` is [`Lang::EnUs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lang {
    /// German (Germany).
    DeDe,
    /// German (Austria).
    DeAt,
    /// German (Switzerland, no ß).
    DeCh,
    /// English (United States).
    EnUs,
    /// English (United Kingdom, "-ise" spelling).
    EnGb,
}

impl Lang {
    /// Every bundled language, in a stable order.
    pub const ALL: [Lang; 5] = [Lang::DeDe, Lang::DeAt, Lang::DeCh, Lang::EnUs, Lang::EnGb];

    /// The BCP 47 tag, e.g. `de-AT`.
    pub fn as_str(self) -> &'static str {
        match self {
            Lang::DeDe => "de-DE",
            Lang::DeAt => "de-AT",
            Lang::DeCh => "de-CH",
            Lang::EnUs => "en-US",
            Lang::EnGb => "en-GB",
        }
    }

    /// The language without the region, as language detection reports it.
    pub fn family(self) -> mazemail_mime::Lang {
        match self {
            Lang::DeDe | Lang::DeAt | Lang::DeCh => mazemail_mime::Lang::De,
            Lang::EnUs | Lang::EnGb => mazemail_mime::Lang::En,
        }
    }

    /// Position in [`Lang::ALL`], for per-language tables.
    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The string named no bundled language.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no spell checking dictionary for language {0:?}")]
pub struct UnknownLang(pub String);

impl FromStr for Lang {
    type Err = UnknownLang;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // `de_AT.UTF-8@euro` → `de_AT`
        let tag = s.trim().split(['.', '@']).next().unwrap_or_default();
        let mut parts = tag.split(['-', '_']);
        let language = parts.next().unwrap_or_default().to_ascii_lowercase();
        // Skip a script subtag (`de-Latn-AT`): four letters.
        let region = parts
            .find(|part| part.len() != 4)
            .map(str::to_ascii_uppercase);
        let lang = match (language.as_str(), region.as_deref()) {
            ("de", None | Some("DE" | "LU" | "BE")) => Lang::DeDe,
            ("de", Some("AT")) => Lang::DeAt,
            ("de", Some("CH" | "LI")) => Lang::DeCh,
            ("en", None | Some("US")) => Lang::EnUs,
            ("en", Some("GB" | "UK" | "IE")) => Lang::EnGb,
            _ => return Err(UnknownLang(s.to_owned())),
        };
        Ok(lang)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tags_locales_and_bare_languages() {
        let cases = [
            ("de-AT", Lang::DeAt),
            ("de_AT", Lang::DeAt),
            ("de_AT.UTF-8", Lang::DeAt),
            ("DE-at", Lang::DeAt),
            ("de", Lang::DeDe),
            ("de-DE", Lang::DeDe),
            ("de-CH", Lang::DeCh),
            ("de-Latn-CH", Lang::DeCh),
            ("en", Lang::EnUs),
            ("en_US", Lang::EnUs),
            ("en-GB", Lang::EnGb),
            (" en-gb ", Lang::EnGb),
        ];
        for (input, lang) in cases {
            assert_eq!(input.parse::<Lang>(), Ok(lang), "{input}");
        }
    }

    #[test]
    fn rejects_languages_without_a_dictionary() {
        for input in ["", "fr", "fr-FR", "en-AU", "de-XX", "english"] {
            assert!(input.parse::<Lang>().is_err(), "{input}");
        }
    }

    #[test]
    fn round_trips_through_display() {
        for lang in Lang::ALL {
            assert_eq!(lang.to_string().parse::<Lang>(), Ok(lang));
            assert_eq!(Lang::ALL[lang.index()], lang);
        }
    }
}
