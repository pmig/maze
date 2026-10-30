//! The bundled Hunspell dictionaries.
//!
//! The `.aff`/`.dic` files from `third_party/dictionaries/` are embedded zstd-compressed
//! (about 1.2 MB for all five languages) and decompressed and parsed on first use, once
//! per process and language. Origin and licenses: `third_party/dictionaries/README.md`.

use std::io::Read;
use std::sync::OnceLock;
use std::time::Instant;

use spellbook::Dictionary;

use crate::Lang;

/// A bundled dictionary could not be loaded. The data is fixed at build time and tested,
/// so this means a broken build rather than a runtime condition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    /// The embedded zstd data didn't decompress to UTF-8.
    #[error("could not decompress the {lang} dictionary: {message}")]
    Decompress {
        /// The dictionary's language.
        lang: Lang,
        /// What went wrong.
        message: String,
    },
    /// spellbook rejected the dictionary.
    #[error("could not parse the {lang} dictionary: {message}")]
    Parse {
        /// The dictionary's language.
        lang: Lang,
        /// What went wrong.
        message: String,
    },
}

/// Compressed `(aff, dic)` per language.
struct Embedded {
    aff: &'static [u8],
    dic: &'static [u8],
}

macro_rules! embedded {
    ($dir:literal) => {
        Embedded {
            aff: include_bytes!(concat!(
                "../../../third_party/dictionaries/",
                $dir,
                "/index.aff.zst"
            )),
            dic: include_bytes!(concat!(
                "../../../third_party/dictionaries/",
                $dir,
                "/index.dic.zst"
            )),
        }
    };
}

fn embedded(lang: Lang) -> Embedded {
    match lang {
        Lang::DeDe => embedded!("de-DE"),
        Lang::DeAt => embedded!("de-AT"),
        Lang::DeCh => embedded!("de-CH"),
        Lang::EnUs => embedded!("en-US"),
        Lang::EnGb => embedded!("en-GB"),
    }
}

/// Embedded (compressed) size of one language's dictionary in bytes.
pub fn embedded_size(lang: Lang) -> usize {
    let data = embedded(lang);
    data.aff.len() + data.dic.len()
}

type Cached = Result<Dictionary, LoadError>;

static CACHE: [OnceLock<Cached>; Lang::ALL.len()] = [const { OnceLock::new() }; Lang::ALL.len()];

/// The parsed dictionary for `lang`, loaded on first use and kept for the life of the
/// process.
///
/// The first call per language blocks for the decompression and parsing: about 6 ms for
/// English and 10 ms for German in a release build (see the crate docs). Concurrent
/// first calls wait for the same load.
pub(crate) fn dictionary(lang: Lang) -> Result<&'static Dictionary, LoadError> {
    CACHE[lang.index()]
        .get_or_init(|| load(lang))
        .as_ref()
        .map_err(Clone::clone)
}

/// Decompress and parse, uncached.
pub(crate) fn load(lang: Lang) -> Cached {
    let start = Instant::now();
    let data = embedded(lang);
    let aff = decompress(lang, data.aff)?;
    let dic = decompress(lang, data.dic)?;
    let decompressed = start.elapsed();
    let dictionary = Dictionary::new(&aff, &dic).map_err(|err| LoadError::Parse {
        lang,
        message: err.to_string(),
    })?;
    tracing::debug!(
        %lang,
        decompress_ms = decompressed.as_millis() as u64,
        total_ms = start.elapsed().as_millis() as u64,
        "loaded spelling dictionary"
    );
    Ok(dictionary)
}

fn decompress(lang: Lang, mut compressed: &[u8]) -> Result<String, LoadError> {
    let error = |message: String| LoadError::Decompress { lang, message };
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut compressed)
        .map_err(|e| error(e.to_string()))?;
    let mut text = String::new();
    decoder
        .read_to_string(&mut text)
        .map_err(|e| error(e.to_string()))?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_dictionary_loads() {
        for lang in Lang::ALL {
            let dictionary = dictionary(lang).unwrap();
            let word = match lang.family() {
                mazemail_mime::Lang::De => "Haus",
                _ => "house",
            };
            assert!(dictionary.check(word), "{lang}: {word}");
            // The same reference twice: cached.
            assert!(std::ptr::eq(dictionary, super::dictionary(lang).unwrap()));
        }
    }

    #[test]
    fn embedded_data_is_about_a_megabyte() {
        let total: usize = Lang::ALL.into_iter().map(embedded_size).sum();
        assert!((1_000_000..1_500_000).contains(&total), "{total}");
    }
}
