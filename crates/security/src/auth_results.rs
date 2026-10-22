//! Parser for `Authentication-Results` header values (RFC 8601).
//!
//! The parser is strict where trust depends on it and lenient elsewhere:
//!
//! - The `authserv-id` must be a single token or quoted string. A value that starts
//!   directly with `method=result` has no `authserv-id` (Exchange Online writes it
//!   that way); [`AuthenticationResults::authserv_id`] is then `None`.
//! - Every result must have the `method=result` shape. A malformed result makes the
//!   whole header fail to parse, so a broken header is never half-trusted.
//! - Unknown `key=value` pairs after a result (Microsoft's `action=none`) are skipped.
//! - Comments are ignored everywhere.

use std::fmt;

/// Upper bound on results per header. Real headers carry a handful; more is hostile.
const MAX_RESULTS: usize = 64;
/// Upper bound on properties per result.
const MAX_PROPERTIES: usize = 32;

/// A parsed `Authentication-Results` header value.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthenticationResults {
    /// The `authserv-id` as written (not lowercased), or `None` when the header has
    /// none.
    pub authserv_id: Option<String>,
    /// The optional `authres-version` after the `authserv-id`.
    pub version: Option<u32>,
    /// The results, in header order. Empty for `; none`.
    pub results: Vec<MethodResult>,
}

/// One `method=result` entry with its reason and properties.
#[derive(Clone, PartialEq, Eq)]
pub struct MethodResult {
    /// The method, lowercased (`dkim`, `spf`, `dmarc`, `arc`, …).
    pub method: String,
    /// The optional method version (`dkim/1=pass`).
    pub method_version: Option<u32>,
    /// The result keyword, lowercased (`pass`, `fail`, `softfail`, …).
    pub result: String,
    /// The `reason=` value, if any.
    pub reason: Option<String>,
    /// Properties like `header.d=example.com`, in order.
    pub properties: Vec<Property>,
}

/// A `ptype.property=value` entry.
#[derive(Clone, PartialEq, Eq)]
pub struct Property {
    /// The property type, lowercased (`smtp`, `header`, `body`, `policy`).
    pub ptype: String,
    /// The property name, lowercased (`mailfrom`, `d`, `i`, `from`, …).
    pub name: String,
    /// The value as written, without quotes.
    pub value: String,
}

impl MethodResult {
    /// The value of the first property `ptype.name`, compared case-insensitively.
    pub fn property(&self, ptype: &str, name: &str) -> Option<&str> {
        self.properties
            .iter()
            .find(|p| p.ptype.eq_ignore_ascii_case(ptype) && p.name.eq_ignore_ascii_case(name))
            .map(|p| p.value.as_str())
    }
}

// Header content is sensitive: only show structure in `Debug`.
impl fmt::Debug for AuthenticationResults {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthenticationResults")
            .field(
                "authserv_id",
                &self.authserv_id.as_ref().map(|_| "<redacted>"),
            )
            .field("version", &self.version)
            .field("results", &self.results)
            .finish()
    }
}

impl fmt::Debug for MethodResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MethodResult")
            .field("method", &self.method)
            .field("result", &self.result)
            .field("properties", &self.properties.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Property {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}=<redacted>", self.ptype, self.name)
    }
}

/// Why a header value couldn't be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthResultsError {
    /// The value is empty or only whitespace and comments.
    #[error("empty Authentication-Results header")]
    Empty,
    /// A result doesn't have the `method=result` shape.
    #[error("malformed result in Authentication-Results header")]
    MalformedResult,
    /// The header has more results or properties than any real header would.
    #[error("Authentication-Results header is too large")]
    TooLarge,
}

/// Parse an `Authentication-Results` header value (without the field name).
pub fn parse(value: &str) -> Result<AuthenticationResults, AuthResultsError> {
    let mut cursor = Cursor::new(value);
    cursor.skip_cfws();
    if cursor.at_end() {
        return Err(AuthResultsError::Empty);
    }

    let authserv_id = if cursor.starts_with_methodspec() {
        None
    } else {
        Some(cursor.value().ok_or(AuthResultsError::MalformedResult)?)
    };

    cursor.skip_cfws();
    let version = if authserv_id.is_some() {
        cursor.version()
    } else {
        None
    };

    let mut results = Vec::new();
    let mut first = authserv_id.is_none();
    loop {
        cursor.skip_cfws();
        if cursor.at_end() {
            break;
        }
        if !first {
            if !cursor.eat(';') {
                return Err(AuthResultsError::MalformedResult);
            }
            cursor.skip_cfws();
            // Trailing semicolon, as Exchange Online sometimes writes.
            if cursor.at_end() {
                break;
            }
        }
        first = false;
        // A semicolon right after a result: an empty resinfo, skip it.
        if cursor.peek() == Some(';') {
            continue;
        }
        let keyword = cursor.keyword().ok_or(AuthResultsError::MalformedResult)?;
        cursor.skip_cfws();
        if results.is_empty() && keyword.eq_ignore_ascii_case("none") && cursor.at_end() {
            break;
        }
        if results.len() >= MAX_RESULTS {
            return Err(AuthResultsError::TooLarge);
        }
        results.push(cursor.method_result(keyword)?);
    }

    Ok(AuthenticationResults {
        authserv_id,
        version,
        results,
    })
}

struct Cursor<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn at_end(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.pos += expected.len_utf8();
            true
        } else {
            false
        }
    }

    /// Skip whitespace and (nested) comments.
    fn skip_cfws(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('(') => self.skip_comment(),
                _ => return,
            }
        }
    }

    fn skip_comment(&mut self) {
        let mut depth = 0usize;
        while let Some(c) = self.bump() {
            match c {
                '\\' => {
                    self.bump();
                }
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether the input at the cursor is `keyword [CFWS] ["/" version] [CFWS] "="`,
    /// meaning the header has no authserv-id.
    fn starts_with_methodspec(&self) -> bool {
        let mut probe = Cursor {
            input: self.input,
            pos: self.pos,
        };
        if probe.keyword().is_none() {
            return false;
        }
        probe.skip_cfws();
        if probe.eat('/') {
            probe.skip_cfws();
            if probe.digits().is_none() {
                return false;
            }
            probe.skip_cfws();
        }
        probe.peek() == Some('=')
    }

    /// An RFC 8601 `Keyword` (letters, digits, `-`, `_`), case preserved.
    fn keyword(&mut self) -> Option<&'a str> {
        let rest = self.rest();
        let len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(rest.len());
        if len == 0 {
            return None;
        }
        self.pos += len;
        Some(&rest[..len])
    }

    fn digits(&mut self) -> Option<u32> {
        let rest = self.rest();
        let len = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if len == 0 {
            return None;
        }
        let n = rest[..len].parse().ok()?;
        self.pos += len;
        Some(n)
    }

    /// An `authres-version`: digits followed by whitespace, a comment, `;` or the end.
    fn version(&mut self) -> Option<u32> {
        let start = self.pos;
        let n = self.digits()?;
        match self.peek() {
            None | Some(';' | '(') => Some(n),
            Some(c) if c.is_whitespace() => Some(n),
            _ => {
                self.pos = start;
                None
            }
        }
    }

    /// An RFC 2045 `value`: a quoted string or a token.
    fn value(&mut self) -> Option<String> {
        if self.peek() == Some('"') {
            return self.quoted_string();
        }
        let rest = self.rest();
        let len = rest
            .find(|c: char| c.is_whitespace() || c.is_control() || is_tspecial(c))
            .unwrap_or(rest.len());
        if len == 0 {
            return None;
        }
        self.pos += len;
        Some(rest[..len].to_owned())
    }

    fn quoted_string(&mut self) -> Option<String> {
        if !self.eat('"') {
            return None;
        }
        let mut out = String::new();
        while let Some(c) = self.bump() {
            match c {
                '"' => return Some(out),
                '\\' => out.push(self.bump()?),
                _ => out.push(c),
            }
        }
        None
    }

    /// A property value: a quoted string, or everything up to whitespace, `;` or a
    /// comment. Lenient on purpose, since values like `@example.com` or base64
    /// fragments with `=` aren't RFC 2045 tokens.
    fn pvalue(&mut self) -> Option<String> {
        if self.peek() == Some('"') {
            return self.quoted_string();
        }
        let rest = self.rest();
        let len = rest
            .find(|c: char| c.is_whitespace() || c == ';' || c == '(')
            .unwrap_or(rest.len());
        if len == 0 {
            return None;
        }
        self.pos += len;
        Some(rest[..len].to_owned())
    }

    /// The rest of a resinfo after its method keyword.
    fn method_result(&mut self, method: &str) -> Result<MethodResult, AuthResultsError> {
        let malformed = AuthResultsError::MalformedResult;
        let mut method_version = None;
        if self.eat('/') {
            self.skip_cfws();
            method_version = Some(self.digits().ok_or(malformed)?);
            self.skip_cfws();
        }
        if !self.eat('=') {
            return Err(malformed);
        }
        self.skip_cfws();
        let result = self.keyword().ok_or(malformed)?;
        let mut parsed = MethodResult {
            method: method.to_ascii_lowercase(),
            method_version,
            result: result.to_ascii_lowercase(),
            reason: None,
            properties: Vec::new(),
        };

        loop {
            self.skip_cfws();
            if self.at_end() || self.peek() == Some(';') {
                return Ok(parsed);
            }
            let key_start = self.pos;
            let key = self.property_key().ok_or(malformed)?;
            self.skip_cfws();
            if !self.eat('=') {
                // A bare word without `=`: not a property. Treat as malformed rather
                // than guessing where the next result starts.
                self.pos = key_start;
                return Err(malformed);
            }
            self.skip_cfws();
            let value = self.pvalue().unwrap_or_default();
            match key {
                PropertyKey::Reason => parsed.reason = Some(value),
                PropertyKey::Property { ptype, name } => {
                    if parsed.properties.len() >= MAX_PROPERTIES {
                        return Err(AuthResultsError::TooLarge);
                    }
                    parsed.properties.push(Property {
                        ptype: ptype.to_ascii_lowercase(),
                        name: name.to_ascii_lowercase(),
                        value,
                    });
                }
                PropertyKey::Other => {}
            }
        }
    }

    /// `reason`, `ptype.property`, or some other keyword (ignored by the caller).
    fn property_key(&mut self) -> Option<PropertyKey<'a>> {
        let first = self.keyword()?;
        let save = self.pos;
        self.skip_cfws();
        if self.eat('.') {
            self.skip_cfws();
            // RFC 8601 property names may contain dots too (`header.b`) but are
            // otherwise keywords.
            let rest = self.rest();
            let len = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
                .unwrap_or(rest.len());
            if len == 0 {
                return None;
            }
            self.pos += len;
            return Some(PropertyKey::Property {
                ptype: first,
                name: &rest[..len],
            });
        }
        self.pos = save;
        if first.eq_ignore_ascii_case("reason") {
            Some(PropertyKey::Reason)
        } else {
            Some(PropertyKey::Other)
        }
    }
}

enum PropertyKey<'a> {
    Reason,
    Property { ptype: &'a str, name: &'a str },
    Other,
}

fn is_tspecial(c: char) -> bool {
    matches!(
        c,
        '(' | ')' | '<' | '>' | '@' | ',' | ';' | ':' | '\\' | '"' | '/' | '[' | ']' | '?' | '='
    )
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn summary(header: &AuthenticationResults) -> Vec<(String, String)> {
        header
            .results
            .iter()
            .map(|r| (r.method.clone(), r.result.clone()))
            .collect()
    }

    #[test]
    fn parses_gmail_header() {
        let header = parse(
            "mx.google.com;\r\n       dkim=pass header.i=@example.com header.s=sel1 header.b=Ab+/c=;\r\n       spf=pass (google.com: domain of a@example.com designates 192.0.2.1 as permitted sender) smtp.mailfrom=a@example.com;\r\n       dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=example.com",
        )
        .unwrap();
        assert_eq!(header.authserv_id.as_deref(), Some("mx.google.com"));
        assert_eq!(header.version, None);
        assert_eq!(
            summary(&header),
            [
                ("dkim".into(), "pass".into()),
                ("spf".into(), "pass".into()),
                ("dmarc".into(), "pass".into())
            ]
        );
        assert_eq!(
            header.results[0].property("header", "i"),
            Some("@example.com")
        );
        assert_eq!(header.results[0].property("header", "b"), Some("Ab+/c="));
        assert_eq!(
            header.results[1].property("smtp", "mailfrom"),
            Some("a@example.com")
        );
        assert_eq!(
            header.results[2].property("HEADER", "FROM"),
            Some("example.com")
        );
    }

    #[test]
    fn parses_microsoft_header_without_authserv_id() {
        let header = parse(
            "spf=pass (sender IP is 192.0.2.1) smtp.mailfrom=example.com; dkim=pass (signature was verified) header.d=example.com;dmarc=pass action=none header.from=example.com;compauth=pass reason=100",
        )
        .unwrap();
        assert_eq!(header.authserv_id, None);
        assert_eq!(
            summary(&header),
            [
                ("spf".into(), "pass".into()),
                ("dkim".into(), "pass".into()),
                ("dmarc".into(), "pass".into()),
                ("compauth".into(), "pass".into()),
            ]
        );
        assert_eq!(header.results[3].reason.as_deref(), Some("100"));
        assert_eq!(
            header.results[2].property("header", "from"),
            Some("example.com")
        );
    }

    #[test]
    fn parses_version_none_and_quoted_values() {
        let header = parse("example.org 1; none").unwrap();
        assert_eq!(header.authserv_id.as_deref(), Some("example.org"));
        assert_eq!(header.version, Some(1));
        assert!(header.results.is_empty());

        let header = parse(
            "\"mx.example.org\"; dkim/1 = fail reason=\"bad sig\" header.d = \"example.com\"",
        )
        .unwrap();
        assert_eq!(header.authserv_id.as_deref(), Some("mx.example.org"));
        let dkim = &header.results[0];
        assert_eq!(dkim.method_version, Some(1));
        assert_eq!(dkim.result, "fail");
        assert_eq!(dkim.reason.as_deref(), Some("bad sig"));
        assert_eq!(dkim.property("header", "d"), Some("example.com"));
    }

    #[test]
    fn keeps_authserv_id_verbatim() {
        // No normalization: the provider check compares the id exactly.
        for id in [
            "*.google.com",
            "mx.google.com.evil.example",
            "MX.Google.Com",
        ] {
            let header = parse(&format!("{id}; spf=pass smtp.mailfrom=example.com")).unwrap();
            // `*` is a token character, so the wildcard-looking id is kept as is.
            assert_eq!(header.authserv_id.as_deref(), Some(id));
        }
    }

    #[test]
    fn rejects_malformed_headers() {
        assert_eq!(parse(""), Err(AuthResultsError::Empty));
        assert_eq!(parse("  (just a comment) "), Err(AuthResultsError::Empty));
        assert_eq!(
            parse("mx.example.org; dkim"),
            Err(AuthResultsError::MalformedResult)
        );
        assert_eq!(
            parse("mx.example.org; dkim=pass stray-word"),
            Err(AuthResultsError::MalformedResult)
        );
        assert_eq!(
            parse("mx.example.org dkim=pass"),
            Err(AuthResultsError::MalformedResult)
        );
        assert_eq!(
            parse("mx.example.org; =pass"),
            Err(AuthResultsError::MalformedResult)
        );
        assert_eq!(parse("@; spf=pass"), Err(AuthResultsError::MalformedResult));
    }

    #[test]
    fn tolerates_trailing_and_empty_semicolons() {
        let header = parse("mx.example.org; spf=pass;; dkim=none;").unwrap();
        assert_eq!(header.results.len(), 2);
    }

    #[test]
    fn limits_size() {
        let many = (0..100).map(|_| "spf=pass").collect::<Vec<_>>().join("; ");
        assert_eq!(
            parse(&format!("mx.example.org; {many}")),
            Err(AuthResultsError::TooLarge)
        );
    }

    #[test]
    fn debug_redacts_values() {
        let header = parse("mx.example.org; spf=pass smtp.mailfrom=anna@example.com").unwrap();
        let debug = format!("{header:?}");
        assert!(!debug.contains("anna"));
        assert!(!debug.contains("mx.example.org"));
    }
}
