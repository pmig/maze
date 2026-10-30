//! Words to check in composer text (Markdown prose).
//!
//! [`words`] yields the words of a text with their byte ranges, leaving out what isn't
//! prose:
//!
//! - fenced code blocks (```` ``` ```` and `~~~`), inline code (`` `x` ``),
//! - quoted lines (`>`), link reference definitions (`[1]: https://…`),
//! - link destinations (`[text](https://…)` checks only "text"), autolinks and HTML tags
//!   (`<https://…>`, `<br>`), bare URLs, domains and email addresses,
//! - code-like chunks: `snake_case`, paths (`/usr/bin`, `~/x`, `./x`), `a=b`, `{x}`,
//!   `a\b`, `a::b`, `file.txt` (a dot between letters), `@handles`, `#tags`,
//! - words with digits (`2nd`, `MP3-Player`), ALL-CAPS words (`HTTP`, `USA`), camelCase
//!   (`iPhone`, `JavaScript`) and single letters.
//!
//! Markdown markup (`#` headings, `*emphasis*`, `**strong**`, `_x_`, `~~x~~`, list
//! bullets) is punctuation and never part of a word.
//!
//! A word is a run of letters (any script, umlauts and ß included), joined by apostrophes
//! (`don't`, `it’s`) or hyphens (`E-Mail-Adresse`, `well-known`) that sit between two
//! letters. Leading and trailing apostrophes and hyphens are not part of the word.
//!
//! Known gaps: inline code and link destinations must close on the same line; indented
//! code blocks are checked like prose; decomposed text (`u` + U+0308) is kept together as
//! a word, but the dictionaries only know the composed (NFC) spelling.

use std::ops::Range;

/// A word to check: its byte range in the text and the text itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word<'a> {
    /// Byte range in the text passed to [`words`].
    pub range: Range<usize>,
    /// `&text[range]`.
    pub text: &'a str,
}

/// The words to check in `text`, in order. See the module docs for what is skipped.
pub fn words(text: &str) -> Vec<Word<'_>> {
    let mut out = Vec::new();
    let mut pos = 0;
    for skip in skip_ranges(text) {
        if skip.start > pos {
            scan_words(text, pos..skip.start, &mut out);
        }
        pos = pos.max(skip.end);
    }
    if pos < text.len() {
        scan_words(text, pos..text.len(), &mut out);
    }
    out
}

/// Apostrophes that may join two parts of a word.
pub(crate) fn is_apostrophe(c: char) -> bool {
    matches!(c, '\'' | '\u{2019}' | '\u{02BC}')
}

/// Hyphens that may join two parts of a word.
pub(crate) fn is_hyphen(c: char) -> bool {
    matches!(c, '-' | '\u{2010}' | '\u{2011}')
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || is_combining_mark(c)
}

/// Combining diacritical marks, so decomposed umlauts don't split a word.
fn is_combining_mark(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{1DC0}'..='\u{1DFF}' | '\u{20D0}'..='\u{20FF}' | '\u{FE20}'..='\u{FE2F}')
}

/// Find the words in `text[range]` and keep the ones worth checking.
fn scan_words<'a>(text: &'a str, range: Range<usize>, out: &mut Vec<Word<'a>>) {
    let segment = &text[range.clone()];
    let mut chars = segment.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if !is_word_char(c) {
            continue;
        }
        let mut end = start + c.len_utf8();
        while let Some(&(i, c)) = chars.peek() {
            if is_word_char(c) {
                chars.next();
                end = i + c.len_utf8();
            } else if is_apostrophe(c) || is_hyphen(c) {
                // A joiner only counts if a word character follows.
                let next = segment[i + c.len_utf8()..].chars().next();
                if next.is_some_and(is_word_char) {
                    chars.next();
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        let word = &segment[start..end];
        if worth_checking(word) {
            let range = range.start + start..range.start + end;
            out.push(Word { range, text: word });
        }
    }
}

/// Words with digits, ALL-CAPS, camelCase and single letters aren't checked.
fn worth_checking(word: &str) -> bool {
    if word.chars().any(|c| c.is_numeric()) {
        return false;
    }
    let letters = word.chars().filter(|c| c.is_alphabetic()).count();
    if letters < 2 {
        return false;
    }
    if is_all_caps(word) {
        return false;
    }
    !is_camel_case(word)
}

/// Every cased letter is uppercase (`HTTP`, `USA`, `GMBH`).
pub(crate) fn is_all_caps(word: &str) -> bool {
    word.chars().filter(|c| c.is_alphabetic()).count() >= 2 && !word.chars().any(char::is_lowercase)
}

/// An uppercase letter right after a lowercase one, within a hyphen-separated part.
fn is_camel_case(word: &str) -> bool {
    let mut prev_lower = false;
    for c in word.chars() {
        if c.is_uppercase() && prev_lower {
            return true;
        }
        prev_lower = c.is_lowercase();
    }
    false
}

/// Byte ranges that are not prose, sorted and merged.
fn skip_ranges(text: &str) -> Vec<Range<usize>> {
    let mut skips = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    let mut offset = 0;
    for raw_line in text.split_inclusive('\n') {
        let start = offset;
        offset += raw_line.len();
        let line = raw_line.trim_end_matches(['\n', '\r']);
        let whole = start..start + line.len();
        let indent = line.len() - line.trim_start_matches(' ').len();
        let body = line.trim_start();

        if let Some((fence_char, fence_len)) = fence {
            skips.push(whole);
            if indent <= 3
                && let Some((c, len)) = fence_marker(body)
                && c == fence_char
                && len >= fence_len
                && body[len..].trim().is_empty()
            {
                fence = None;
            }
            continue;
        }
        if indent <= 3
            && let Some(marker) = fence_marker(body)
        {
            fence = Some(marker);
            skips.push(whole);
            continue;
        }
        if body.starts_with('>') || (indent <= 3 && is_reference_definition(body)) {
            skips.push(whole);
            continue;
        }
        let first = skips.len();
        inline_skips(line, start, &mut skips);
        link_skips(line, start, &mut skips);
        // Judge chunks without the code, links and tags found so far: a URL with an
        // underscore must not make the link text before it look like code.
        let mut masked = line.as_bytes().to_vec();
        for range in &skips[first..] {
            masked[range.start - start..range.end - start].fill(b' ');
        }
        // Skip ranges start and end on char boundaries, so this can't fail.
        let masked = String::from_utf8(masked).unwrap_or_else(|_| line.to_owned());
        chunk_skips(&masked, start, &mut skips);
    }
    skips.sort_by_key(|r| (r.start, r.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(skips.len());
    for range in skips {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// ```` ``` ```` or `~~~` (three or more) at the start of `body`: the char and run length.
fn fence_marker(body: &str) -> Option<(u8, usize)> {
    let first = *body.as_bytes().first()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let len = body.bytes().take_while(|&b| b == first).count();
    // An info string after a backtick fence may not contain backticks.
    (len >= 3 && !(first == b'`' && body[len..].contains('`'))).then_some((first, len))
}

/// `[label]: destination`
fn is_reference_definition(body: &str) -> bool {
    body.strip_prefix('[')
        .and_then(|rest| rest.find("]:").map(|end| end > 0))
        .unwrap_or(false)
}

/// Inline code spans, link destinations (`](…)`, `][…]`) and `<…>` tags and autolinks.
fn inline_skips(line: &str, base: usize, skips: &mut Vec<Range<usize>>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'`' => {
                let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                match find_backtick_run(&bytes[i + run..], run) {
                    Some(close) => {
                        let end = i + run + close + run;
                        skips.push(base + i..base + end);
                        i = end;
                    }
                    None => i += run,
                }
            }
            b']' if bytes.get(i + 1) == Some(&b'(') => match matching_paren(&bytes[i + 1..]) {
                Some(len) => {
                    skips.push(base + i + 1..base + i + 1 + len);
                    i += 1 + len;
                }
                None => i += 2,
            },
            b']' if bytes.get(i + 1) == Some(&b'[') => {
                match bytes[i + 1..].iter().position(|&b| b == b']') {
                    Some(close) => {
                        skips.push(base + i + 1..base + i + 2 + close);
                        i += 2 + close;
                    }
                    None => i += 2,
                }
            }
            b'<' if bytes
                .get(i + 1)
                .is_some_and(|b| b.is_ascii_alphabetic() || matches!(b, b'/' | b'!' | b'?')) =>
            {
                match bytes[i..].iter().position(|&b| b == b'>') {
                    Some(close) => {
                        skips.push(base + i..base + i + close + 1);
                        i += close + 1;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
}

/// Offset of the next run of exactly `run` backticks.
fn find_backtick_run(bytes: &[u8], run: usize) -> Option<usize> {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let len = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            if len == run {
                return Some(i);
            }
            i += len;
        } else {
            i += 1;
        }
    }
    None
}

/// `bytes` starts with `(`: the length up to and including the matching `)`.
fn matching_paren(bytes: &[u8]) -> Option<usize> {
    let mut depth = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Bare URLs, domains (`example.com`) and email addresses.
fn link_skips(line: &str, base: usize, skips: &mut Vec<Range<usize>>) {
    let mut finder = linkify::LinkFinder::new();
    finder.url_must_have_scheme(false);
    for link in finder.links(line) {
        skips.push(base + link.start()..base + link.end());
    }
}

/// Whitespace-separated chunks that look like code, paths, handles or file names.
fn chunk_skips(line: &str, base: usize, skips: &mut Vec<Range<usize>>) {
    let mut start = None;
    for (i, c) in line
        .char_indices()
        .chain(std::iter::once((line.len(), ' ')))
    {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                if is_code_like(&line[s..i]) {
                    skips.push(base + s..base + i);
                }
                start = None;
            }
            _ => {}
        }
    }
}

fn is_code_like(chunk: &str) -> bool {
    let trimmed = chunk.trim_start_matches(|c: char| {
        matches!(
            c,
            '(' | '[' | '"' | '\'' | '*' | '_' | '“' | '„' | '‚' | '‘' | '«' | '‹'
        )
    });
    if trimmed.starts_with(['/', '@', '#'])
        && trimmed[1..].starts_with(|c: char| c.is_alphanumeric() || c == '/')
    {
        return true;
    }
    if trimmed.starts_with("~/") || trimmed.starts_with("./") || trimmed.starts_with("../") {
        return true;
    }
    if chunk.contains(['=', '{', '}', '|']) || chunk.contains("::") {
        return true;
    }
    let chars: Vec<char> = chunk.chars().collect();
    chars.windows(3).any(|w| {
        let between = w[0].is_alphanumeric() && w[2].is_alphanumeric();
        (between && matches!(w[1], '_' | '.'))
            // A backslash that doesn't escape Markdown punctuation.
            || (w[1] == '\\' && !w[2].is_ascii_punctuation())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn texts(text: &str) -> Vec<&str> {
        words(text).into_iter().map(|w| w.text).collect()
    }

    #[test]
    fn ranges_point_into_the_text() {
        let text = "Grüße, „Jürgen“ — schön!";
        for word in words(text) {
            assert_eq!(&text[word.range.clone()], word.text);
        }
        assert_eq!(texts(text), ["Grüße", "Jürgen", "schön"]);
    }

    #[test]
    fn apostrophes_and_hyphens_join_words() {
        assert_eq!(
            texts("Don't worry, it’s a well-known 'quote' - really."),
            ["Don't", "worry", "it’s", "well-known", "quote", "really"]
        );
        assert_eq!(
            texts("Die E-Mail-Adresse, Haupt- und Nebeneingang, Straße."),
            [
                "Die",
                "E-Mail-Adresse",
                "Haupt",
                "und",
                "Nebeneingang",
                "Straße"
            ]
        );
    }

    #[test]
    fn skips_digits_caps_camel_case_and_single_letters() {
        assert_eq!(
            texts("Meet at 3pm, the 2nd MP3-Player, HTTP and USA, iPhone, JavaScript, a I x ok"),
            ["Meet", "at", "the", "and", "ok"]
        );
    }

    #[test]
    fn skips_urls_emails_and_domains() {
        assert_eq!(
            texts(
                "See https://exmaple.com/pathh?q=wrod or www.exmaple.org, mail jdoe@exmaple.com."
            ),
            ["See", "or", "mail"]
        );
        assert_eq!(texts("Visit exmaple.com today"), ["Visit", "today"]);
    }

    #[test]
    fn skips_inline_code_and_fenced_code_blocks() {
        let text = "Run `cargo bulid` now.\n\n```rust\nlet wrod = 1;\n```\nAfter ``a ` b`` that.\n~~~\nfoo barr\n~~~\nDone";
        assert_eq!(texts(text), ["Run", "now", "After", "that", "Done"]);
    }

    #[test]
    fn unclosed_fence_skips_to_the_end() {
        assert_eq!(texts("Text\n```\ncodde\nmore"), ["Text"]);
    }

    #[test]
    fn skips_quoted_lines() {
        let text = "Hello,\n> quoted txet here\n  > nested qoute\nthanks";
        assert_eq!(texts(text), ["Hello", "thanks"]);
    }

    #[test]
    fn markdown_links_check_the_text_not_the_destination() {
        assert_eq!(
            texts("Read [the manaul](https://exmaple.com/docs_page) and ![a logo](logo.png)."),
            ["Read", "the", "manaul", "and", "logo"]
        );
        assert_eq!(
            texts(
                "See [the docs][docref] and <https://exmaple.com>.\n[docref]: https://exmaple.com/x"
            ),
            ["See", "the", "docs", "and"]
        );
    }

    #[test]
    fn markdown_markup_is_punctuation() {
        assert_eq!(
            texts("# Heading one\n\n## Second\n\n- *emph* and **strong** and _under_ and ~~gone~~"),
            [
                "Heading", "one", "Second", "emph", "and", "strong", "and", "under", "and", "gone"
            ]
        );
    }

    #[test]
    fn skips_code_like_chunks() {
        assert_eq!(
            texts(
                "Edit /etc/hostss or ~/confg and ./scrpt, set foo_barr=1 in {confg}, \
                 call Vec::neww, open main.rs, ping @jdoee about #releasse, C:\\Users\\jdoe"
            ),
            [
                "Edit", "or", "and", "set", "in", "call", "open", "ping", "about"
            ]
        );
    }

    #[test]
    fn slashes_between_words_split_them() {
        assert_eq!(
            texts("and/or Kunden/Kundinnen"),
            ["and", "or", "Kunden", "Kundinnen"]
        );
    }

    #[test]
    fn html_tags_are_skipped() {
        assert_eq!(
            texts("Line<br>break <span class=\"x\">texxt</span>"),
            ["Line", "break", "texxt"]
        );
    }

    #[test]
    fn unicode_punctuation_separates_words() {
        assert_eq!(
            texts("«Bonjour» ‹Hallo› „Gut“ ‚so‘ Ende… Anfang—Schluss; (Klammer) ¿Qué? ¡Sí!"),
            [
                "Bonjour", "Hallo", "Gut", "so", "Ende", "Anfang", "Schluss", "Klammer", "Qué",
                "Sí"
            ]
        );
    }
}
