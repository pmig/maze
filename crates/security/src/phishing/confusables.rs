//! A TR39-style skeleton for spotting lookalike domains and names.
//!
//! [Unicode TR39](https://www.unicode.org/reports/tr39/#Confusable_Detection) maps
//! characters that look alike to a common prototype; two strings are confusable
//! when their skeletons are equal. The full `confusables.txt` has thousands of
//! entries. This table keeps the ones that matter for mail: Cyrillic and Greek
//! letters that look like Latin ones, digits and symbols that pass for letters,
//! a few Latin variants and fullwidth forms. Unlike TR39 it also folds common
//! accented Latin letters to their base letter, since `pàypal.com` is as deceptive
//! in a domain as `pаypal.com`. Sources are lowercase; [`skeleton`] lowercases its
//! input first.
//!
//! The data lives in [`CONFUSABLES`], sorted by source character, so it can be
//! extended without touching code.

/// `(source, prototype)` pairs, sorted by `source`. Prototypes are ASCII.
pub(crate) const CONFUSABLES: &[(char, &str)] = &[
    ('0', "o"),
    ('1', "l"),
    ('m', "rn"),
    ('|', "l"),
    ('à', "a"),
    ('á', "a"),
    ('â', "a"),
    ('ã', "a"),
    ('ä', "a"),
    ('å', "a"),
    ('ç', "c"),
    ('è', "e"),
    ('é', "e"),
    ('ê', "e"),
    ('ë', "e"),
    ('ì', "i"),
    ('í', "i"),
    ('î', "i"),
    ('ï', "i"),
    ('ñ', "n"),
    ('ò', "o"),
    ('ó', "o"),
    ('ô', "o"),
    ('õ', "o"),
    ('ö', "o"),
    ('ø', "o"),
    ('ù', "u"),
    ('ú', "u"),
    ('û', "u"),
    ('ü', "u"),
    ('ý', "y"),
    ('ÿ', "y"),
    ('ı', "i"),
    ('ŀ', "l"),
    ('ł', "l"),
    ('ſ', "f"),
    ('ƅ', "b"),
    ('ǀ', "l"),
    ('ɑ', "a"),
    ('ɡ', "g"),
    ('ɩ', "i"),
    ('ɪ', "i"),
    ('ʏ', "y"),
    ('ͺ', "i"),
    ('ά', "a"),
    ('έ', "e"),
    ('ή', "n"),
    ('ί', "i"),
    ('α', "a"),
    ('γ', "y"),
    ('ε', "e"),
    ('η', "n"),
    ('ι', "i"),
    ('κ', "k"),
    ('ν', "v"),
    ('ο', "o"),
    ('ρ', "p"),
    ('ς', "c"),
    ('τ', "t"),
    ('υ', "u"),
    ('χ', "x"),
    ('ω', "w"),
    ('ϊ', "i"),
    ('ϋ', "u"),
    ('ό', "o"),
    ('ύ', "u"),
    ('ϲ', "c"),
    ('ϳ', "j"),
    ('а', "a"),
    ('г', "r"),
    ('е', "e"),
    ('к', "k"),
    ('о', "o"),
    ('п', "n"),
    ('р', "p"),
    ('с', "c"),
    ('у', "y"),
    ('х', "x"),
    ('ь', "b"),
    ('ѐ', "e"),
    ('ё', "e"),
    ('ѕ', "s"),
    ('і', "i"),
    ('ї', "i"),
    ('ј', "j"),
    ('һ', "h"),
    ('ӏ', "l"),
    ('ԁ', "d"),
    ('ԛ', "q"),
    ('ԝ', "w"),
    ('ᴄ', "c"),
    ('ᴏ', "o"),
    ('ᴠ', "v"),
    ('ᴡ', "w"),
    ('ᴢ', "z"),
    ('‐', "-"),
    ('‑', "-"),
    ('‒', "-"),
    ('–', "-"),
    ('—', "-"),
    ('․', "."),
    ('ℓ', "l"),
    ('ⅰ', "i"),
    ('ⅼ', "l"),
    ('ⅽ', "c"),
    ('ⅾ', "d"),
    ('ⅿ', "rn"),
    ('−', "-"),
    ('。', "."),
    ('ꓲ', "l"),
];

/// The prototype of one character, or `None` if it maps to itself.
fn prototype(c: char) -> Option<&'static str> {
    CONFUSABLES
        .binary_search_by_key(&c, |&(source, _)| source)
        .ok()
        .map(|index| CONFUSABLES[index].1)
}

/// The skeleton of `input`: lowercased, every confusable character replaced by
/// its prototype, and fullwidth ASCII folded to ASCII. Equal skeletons mean the
/// strings can be mistaken for each other.
pub fn skeleton(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars().flat_map(char::to_lowercase) {
        let c = fold_fullwidth(c);
        match prototype(c) {
            Some(proto) => out.push_str(proto),
            None => out.push(c),
        }
    }
    out
}

/// Fullwidth ASCII variants (U+FF01–U+FF5E) to ASCII.
fn fold_fullwidth(c: char) -> char {
    let code = u32::from(c);
    if (0xFF01..=0xFF5E).contains(&code) {
        char::from_u32(code - 0xFF01 + 0x21)
            .map(|c| c.to_ascii_lowercase())
            .unwrap_or(c)
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_unique_and_ascii() {
        for pair in CONFUSABLES.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{:?} before {:?}", pair[0], pair[1]);
        }
        for &(source, proto) in CONFUSABLES {
            assert!(proto.is_ascii() && !proto.is_empty(), "{source:?}");
            assert_ne!(source.to_string(), proto, "identity mapping for {source:?}");
            assert_eq!(
                skeleton(proto),
                skeleton(&source.to_string()),
                "{source:?} not stable"
            );
        }
    }

    #[test]
    fn skeletons_of_lookalikes_match() {
        assert_eq!(skeleton("paypa1.com"), skeleton("paypal.com"));
        assert_eq!(skeleton("аpple.com"), skeleton("apple.com"));
        assert_eq!(skeleton("rnicrosoft.com"), skeleton("microsoft.com"));
        assert_eq!(skeleton("gооgle.com"), skeleton("google.com"));
        assert_eq!(skeleton("gοοgle.com"), skeleton("google.com"));
        assert_eq!(skeleton("pàypal.com"), skeleton("paypal.com"));
        assert_eq!(skeleton("ｐａｙｐａｌ．ｃｏｍ"), skeleton("paypal.com"));
        assert_eq!(skeleton("PayPal.com"), skeleton("paypal.com"));
    }

    #[test]
    fn different_names_differ() {
        assert_ne!(skeleton("paypal.com"), skeleton("paypai.com"));
        assert_ne!(skeleton("apple.com"), skeleton("appel.com"));
        assert_ne!(skeleton("example.com"), skeleton("example.net"));
    }
}
