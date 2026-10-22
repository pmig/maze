//! A tolerant `prefs.js` parser.
//!
//! Reads `user_pref("name", value);` (also `pref`, `sticky_pref`, `lockPref`) with
//! string, integer and boolean values. Strings may use single or double quotes and JS
//! escapes (`\"`, `\\`, `\n`, `\t`, `\xHH`, `\uHHHH` including surrogate pairs).
//! Comments (`//`, `/* */`, `#`) are skipped. A malformed statement is skipped up to
//! the next `;` and counted, never fatal.

use std::collections::BTreeMap;

use serde::Serialize;

/// A preference value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum PrefValue {
    String(String),
    Int(i64),
    Bool(bool),
}

/// All preferences of a `prefs.js`, later statements overriding earlier ones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prefs {
    values: BTreeMap<String, PrefValue>,
    /// Statements that couldn't be parsed (skipped).
    pub errors: usize,
}

impl Prefs {
    /// Parses the text of a `prefs.js` (or `user.js`).
    pub fn parse(text: &str) -> Self {
        let mut parser = Parser {
            chars: text.chars().collect(),
            pos: 0,
        };
        let mut prefs = Self::default();
        loop {
            parser.skip_space_and_comments();
            if parser.at_end() {
                break;
            }
            match parser.statement() {
                Some((name, value)) => {
                    prefs.values.insert(name, value);
                }
                None => {
                    prefs.errors += 1;
                    parser.skip_statement();
                }
            }
        }
        prefs
    }

    /// Builds prefs from pairs (tests, defaults).
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, PrefValue)>) -> Self {
        Self {
            values: pairs.into_iter().collect(),
            errors: 0,
        }
    }

    /// A value.
    pub fn get(&self, name: &str) -> Option<&PrefValue> {
        self.values.get(name)
    }

    /// A string value (integers and booleans aren't converted).
    pub fn string(&self, name: &str) -> Option<&str> {
        match self.values.get(name)? {
            PrefValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// An integer value; numeric strings are accepted (old profiles store some ints
    /// as strings).
    pub fn int(&self, name: &str) -> Option<i64> {
        match self.values.get(name)? {
            PrefValue::Int(i) => Some(*i),
            PrefValue::String(s) => s.trim().parse().ok(),
            PrefValue::Bool(_) => None,
        }
    }

    /// A boolean value; `"true"`/`"false"` strings are accepted.
    pub fn bool(&self, name: &str) -> Option<bool> {
        match self.values.get(name)? {
            PrefValue::Bool(b) => Some(*b),
            PrefValue::String(s) => match s.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
            PrefValue::Int(_) => None,
        }
    }

    /// The distinct keys `k` for which some pref `<prefix><k>.<…>` exists, sorted
    /// (`branch_keys("mail.server.")` gives `server1`, `server2`, …).
    pub fn branch_keys(&self, prefix: &str) -> Vec<String> {
        let mut keys: Vec<String> = self
            .values
            .range(prefix.to_owned()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .filter_map(|(k, _)| {
                k[prefix.len()..]
                    .split_once('.')
                    .map(|(key, _)| key.to_owned())
            })
            .collect();
        keys.dedup();
        keys
    }

    /// All preference names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    /// Applies `other` on top (later file wins), adding its error count.
    pub fn merge(&mut self, other: Self) {
        self.values.extend(other.values);
        self.errors += other.errors;
    }

    /// Number of preferences.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn at_end(&self) -> bool {
        self.pos >= self.chars.len()
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn skip_space_and_comments(&mut self) {
        loop {
            match (self.peek(), self.peek_at(1)) {
                (Some(c), _) if c.is_whitespace() => self.pos += 1,
                (Some('/'), Some('/')) | (Some('#'), _) => {
                    while let Some(c) = self.peek() {
                        self.pos += 1;
                        if c == '\n' {
                            break;
                        }
                    }
                }
                (Some('/'), Some('*')) => {
                    self.pos += 2;
                    while !self.at_end()
                        && !(self.peek() == Some('*') && self.peek_at(1) == Some('/'))
                    {
                        self.pos += 1;
                    }
                    self.pos = (self.pos + 2).min(self.chars.len());
                }
                _ => return,
            }
        }
    }

    fn skip_statement(&mut self) {
        while let Some(c) = self.peek() {
            self.pos += 1;
            if c == ';' || c == '\n' {
                return;
            }
        }
    }

    fn expect(&mut self, c: char) -> Option<()> {
        self.skip_space_and_comments();
        (self.peek() == Some(c)).then(|| self.pos += 1)
    }

    fn identifier(&mut self) -> String {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }

    fn statement(&mut self) -> Option<(String, PrefValue)> {
        let function = self.identifier();
        if !matches!(
            function.as_str(),
            "user_pref" | "pref" | "sticky_pref" | "lockPref"
        ) {
            return None;
        }
        self.expect('(')?;
        self.skip_space_and_comments();
        let name = self.string()?;
        self.expect(',')?;
        self.skip_space_and_comments();
        let value = self.value()?;
        // `pref("x", 1, locked)` style attributes are ignored.
        self.skip_space_and_comments();
        while self.peek() == Some(',') {
            self.pos += 1;
            self.skip_space_and_comments();
            self.identifier();
            self.skip_space_and_comments();
        }
        self.expect(')')?;
        self.expect(';')?;
        Some((name, value))
    }

    fn value(&mut self) -> Option<PrefValue> {
        match self.peek()? {
            '"' | '\'' => self.string().map(PrefValue::String),
            't' | 'f' => match self.identifier().as_str() {
                "true" => Some(PrefValue::Bool(true)),
                "false" => Some(PrefValue::Bool(false)),
                _ => None,
            },
            c if c == '-' || c == '+' || c.is_ascii_digit() => {
                let start = self.pos;
                self.pos += 1;
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.pos += 1;
                }
                let text: String = self.chars[start..self.pos].iter().collect();
                text.parse().ok().map(PrefValue::Int)
            }
            _ => None,
        }
    }

    fn hex(&mut self, digits: usize) -> Option<u32> {
        let text: String = self
            .chars
            .get(self.pos..self.pos + digits)?
            .iter()
            .collect();
        let value = u32::from_str_radix(&text, 16).ok()?;
        self.pos += digits;
        Some(value)
    }

    fn string(&mut self) -> Option<String> {
        let quote = self.peek().filter(|c| *c == '"' || *c == '\'')?;
        self.pos += 1;
        let mut out = String::new();
        loop {
            let c = self.peek()?;
            self.pos += 1;
            match c {
                c if c == quote => return Some(out),
                '\\' => {
                    let escaped = self.peek()?;
                    self.pos += 1;
                    match escaped {
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'x' => out.push(char::from_u32(self.hex(2)?)?),
                        'u' => {
                            let first = self.hex(4)?;
                            if (0xD800..0xDC00).contains(&first)
                                && self.peek() == Some('\\')
                                && self.peek_at(1) == Some('u')
                            {
                                self.pos += 2;
                                let second = self.hex(4)?;
                                let combined = 0x10000
                                    + ((first - 0xD800) << 10)
                                    + (second.wrapping_sub(0xDC00));
                                out.push(char::from_u32(combined).unwrap_or('\u{fffd}'));
                            } else {
                                out.push(char::from_u32(first).unwrap_or('\u{fffd}'));
                            }
                        }
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_values_escapes_and_comments() {
        let prefs = Prefs::parse(
            r#"// Mozilla User Preferences
/* block
   comment */
# hash comment
user_pref("a.string", "say \"hi\" \\ ä😀\x41\n");
user_pref("a.int", -42);
user_pref("a.bool", true);
user_pref('single', 'quoted');
pref("default.pref", 7, locked);
user_pref("broken", );
user_pref("after.broken", false);
garbage here;
user_pref("a.int", 43);
user_pref("num.string", "12");
"#,
        );
        assert_eq!(prefs.string("a.string"), Some("say \"hi\" \\ ä😀A\n"));
        assert_eq!(prefs.int("a.int"), Some(43));
        assert_eq!(prefs.bool("a.bool"), Some(true));
        assert_eq!(prefs.string("single"), Some("quoted"));
        assert_eq!(prefs.int("default.pref"), Some(7));
        assert_eq!(prefs.bool("after.broken"), Some(false));
        assert_eq!(prefs.int("num.string"), Some(12));
        assert_eq!(prefs.string("a.int"), None);
        assert_eq!(prefs.errors, 2);
    }

    #[test]
    fn lists_branch_keys() {
        let prefs = Prefs::parse(
            r#"user_pref("mail.server.server2.hostname", "b");
user_pref("mail.server.server1.hostname", "a");
user_pref("mail.server.server1.port", 993);
user_pref("mail.server.default.x", 1);
user_pref("mail.serverDefaultStoreContractID", "x");"#,
        );
        assert_eq!(
            prefs.branch_keys("mail.server."),
            vec!["default", "server1", "server2"]
        );
    }

    #[test]
    fn never_panics_on_truncated_input() {
        for text in [
            "user_pref(\"a",
            "user_pref(\"a\", \"\\u12",
            "/* open",
            "user_pref(\"a\", 1",
            "\\",
            "user_pref(\"a\", \"\\ud800\\u",
        ] {
            let _ = Prefs::parse(text);
        }
    }
}
