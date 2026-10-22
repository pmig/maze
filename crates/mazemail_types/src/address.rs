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
    fn display_quotes_special_names() {
        let a = Address::new(Some("Example, Anna"), "anna@example.com");
        assert_eq!(a.to_string(), "\"Example, Anna\" <anna@example.com>");
    }
}
