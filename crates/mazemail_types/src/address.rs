use std::fmt;

use serde::{Deserialize, Serialize};

/// An email address with an optional display name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Address {
    pub name: Option<String>,
    pub email: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an email address")]
pub struct AddressParseError;

/// An entry of an address list that isn't an address ([`Address::parse_list`]).
///
/// `entry` is what was typed (user input, so don't log it).
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an email address: {entry}")]
pub struct AddressListError {
    pub entry: String,
}

impl fmt::Debug for AddressListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AddressListError").finish_non_exhaustive()
    }
}

impl Address {
    pub fn new(name: Option<impl Into<String>>, email: impl Into<String>) -> Self {
        Self {
            name: name.map(Into::into),
            email: email.into(),
        }
    }

    /// Parse `Name <user@host>` or a bare `user@host`. Full RFC 5322 parsing lives in
    /// the `mime` crate; this is for user input such as CLI arguments.
    pub fn parse(input: &str) -> Result<Self, AddressParseError> {
        let input = input.trim();
        let (name, email) = match (input.rfind('<'), input.rfind('>')) {
            (Some(open), Some(close)) if open < close => {
                let name = input[..open].trim().trim_matches('"').trim();
                let name = (!name.is_empty()).then(|| name.to_owned());
                (name, input[open + 1..close].trim())
            }
            (None, None) => (None, input),
            _ => return Err(AddressParseError),
        };
        let (local, domain) = email.rsplit_once('@').ok_or(AddressParseError)?;
        if local.is_empty() || domain.is_empty() || email.contains(char::is_whitespace) {
            return Err(AddressParseError);
        }
        Ok(Self {
            name,
            email: email.to_owned(),
        })
    }

    /// Parse a comma- or semicolon-separated list as typed into a To or Cc field:
    /// `anna@example.com, "Doe, John" <john@example.com>; bob@example.org`. Separators
    /// inside quotes or angle brackets don't split; empty entries are skipped.
    pub fn parse_list(input: &str) -> Result<Vec<Self>, AddressListError> {
        let mut out = Vec::new();
        let mut entry = String::new();
        let (mut quoted, mut angle) = (false, false);
        let mut finish = |entry: &mut String| -> Result<(), AddressListError> {
            let text = entry.trim();
            if !text.is_empty() {
                let address = Self::parse(text).map_err(|_| AddressListError {
                    entry: text.to_owned(),
                })?;
                out.push(address);
            }
            entry.clear();
            Ok(())
        };
        let mut escaped = false;
        for c in input.chars() {
            if escaped {
                escaped = false;
                entry.push(c);
                continue;
            }
            match c {
                '\\' if quoted => {
                    escaped = true;
                    continue;
                }
                '"' => quoted = !quoted,
                '<' if !quoted => angle = true,
                '>' if !quoted => angle = false,
                ',' | ';' | '\n' if !quoted && !angle => {
                    finish(&mut entry)?;
                    continue;
                }
                _ => {}
            }
            entry.push(c);
        }
        finish(&mut entry)?;
        Ok(out)
    }

    /// Formats addresses for a To or Cc field (`a@b.example, Anna <c@d.example>`).
    pub fn format_list(addresses: &[Self]) -> String {
        addresses
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Whether both addresses have the same mailbox (case-insensitive).
    pub fn same_mailbox(&self, other: &Self) -> bool {
        self.email.eq_ignore_ascii_case(&other.email)
    }

    /// The domain part, lowercased.
    pub fn domain(&self) -> String {
        self.email
            .rsplit_once('@')
            .map(|(_, d)| d.to_ascii_lowercase())
            .unwrap_or_default()
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) if name.contains([',', '"', '<', '>', '@']) => {
                write!(f, "\"{}\" <{}>", name.replace('"', "\\\""), self.email)
            }
            Some(name) => write!(f, "{name} <{}>", self.email),
            None => f.write_str(&self.email),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        let a = Address::parse("Anna Example <anna@example.com>").unwrap();
        assert_eq!(a.name.as_deref(), Some("Anna Example"));
        assert_eq!(a.email, "anna@example.com");
        assert_eq!(a.domain(), "example.com");

        let b = Address::parse("anna@example.com").unwrap();
        assert_eq!(b.name, None);
        assert_eq!(b.to_string(), "anna@example.com");

        assert!(Address::parse("not an address").is_err());
        assert!(Address::parse("<broken").is_err());
    }

    #[test]
    fn parses_lists_as_typed() {
        let list = Address::parse_list(
            "anna@example.com, \"Doe, John\" <john@example.com>; Bob <bob@example.org>,, ",
        )
        .unwrap();
        assert_eq!(
            list,
            vec![
                Address::new(None::<String>, "anna@example.com"),
                Address::new(Some("Doe, John"), "john@example.com"),
                Address::new(Some("Bob"), "bob@example.org"),
            ]
        );
        assert_eq!(Address::parse_list("  ").unwrap(), vec![]);
        let err = Address::parse_list("anna@example.com, nobody").unwrap_err();
        assert_eq!(err.entry, "nobody");
        assert!(!format!("{err:?}").contains("nobody"));
        let round = Address::format_list(&list);
        assert_eq!(Address::parse_list(&round).unwrap(), list);
        assert!(list[0].same_mailbox(&Address::new(None::<String>, "ANNA@example.com")));
    }

    #[test]
    fn display_quotes_special_names() {
        let a = Address::new(Some("Example, Anna"), "anna@example.com");
        assert_eq!(a.to_string(), "\"Example, Anna\" <anna@example.com>");
    }
}
