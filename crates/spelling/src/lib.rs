//! Spell checking for the Maze composer (PLAN-spellcheck).
//!
//! Layer L1: no GPUI, no IO, no async. Depends only on `mazemail_mime` (L0) for language
//! detection.
//!
//! - [`Lang`]: the bundled languages, de-DE, de-AT, de-CH, en-US and en-GB, parsed from
//!   BCP 47 tags or POSIX locales (`de_AT`, `en`).
//! - [`dictionary`]: the Hunspell dictionaries from `wooorm/dictionaries`, embedded
//!   zstd-compressed (about 1.2 MB in the binary) and decompressed and parsed on first
//!   use per language and process. Origin and licenses:
//!   `third_party/dictionaries/README.md`.
//! - [`Speller`]: one language (`check`, `suggest`); [`SpellbookSpeller`] is backed by
//!   [spellbook](https://github.com/helix-editor/spellbook), a pure-Rust Nuspell port.
//! - [`words`]: the words of composer Markdown worth checking, with byte ranges (skips
//!   code, quotes, URLs, email addresses, words with digits, ALL-CAPS; see [`tokenize`]).
//! - [`Checker`] / [`check_text`]: checks a text paragraph by paragraph in each
//!   paragraph's detected language, with a [`PersonalWords`] list on top.
//! - [`suggest::refine`]: filters and re-ranks the dictionaries' suggestions.
//!
//! ```
//! use spelling::{Checker, Lang, PersonalWords};
//!
//! let checker = Checker::new(&[Lang::DeAt, Lang::EnUs], PersonalWords::new())?;
//! let text = "Wir sehen uns im Jänner zum Frühstuck.\n\nI will recieve it tomorrow.";
//! let misspelled = checker.check_text(text);
//! let words: Vec<_> = misspelled.iter().map(|m| m.word.as_str()).collect();
//! assert_eq!(words, ["Frühstuck", "recieve"]);
//! assert_eq!(checker.suggest(&misspelled[0])[0], "Frühstück");
//! # Ok::<(), spelling::LoadError>(())
//! ```
//!
//! # Threading and timings
//!
//! Nothing here may run on the UI thread: loading blocks, and suggestions take tens of
//! milliseconds. Measured on an Apple M-series laptop, release build
//! (`cargo nextest run -p spelling --release --run-ignored only timings --no-capture`):
//!
//! | | English | German |
//! |---|---|---|
//! | first load (decompress + parse) | ~6 ms | ~10 ms |
//! | check one word | ~3 µs | ~1–100 µs (long compounds are the slow ones) |
//! | check a paragraph (incl. language detection) | ~0.2 ms | ~0.2 ms |
//! | suggest one word | ~10 ms | ~25 ms (up to ~100 ms for long compounds) |
//! | memory once loaded | ~4.5 MB | ~8 MB |
//!
//! Debug builds are about three times slower. Checking changed paragraphs on every
//! (debounced) edit is cheap; suggestions belong behind a right click, computed off the
//! UI thread.

pub mod dictionary;
pub mod suggest;
pub mod tokenize;

mod check;
mod lang;
mod speller;

pub use check::{Checker, Misspelling, check_text};
pub use dictionary::LoadError;
pub use lang::{Lang, UnknownLang};
pub use speller::{PersonalWords, SpellbookSpeller, Speller};
pub use tokenize::{Word, words};
