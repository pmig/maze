//! [`MicrosoftTenant`]: which Microsoft Entra directory "Sign in with Microsoft" uses.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The tenant part of Microsoft's sign-in endpoints
/// (`https://login.microsoftonline.com/<tenant>/oauth2/v2.0/…`).
///
/// One of `common` (work, school and personal accounts; the default),
/// `organizations` (work and school accounts), `consumers` (personal accounts), a
/// tenant ID (a GUID) or a verified domain of the tenant (`contoso.com`). An app
/// registration that only allows its own organization (single-tenant) needs the
/// tenant ID or domain: `common` fails with `AADSTS50194`.
///
/// Stored lowercase; parsing trims and lowercases.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MicrosoftTenant(String);

/// Why a text isn't a [`MicrosoftTenant`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{0:?} is not a Microsoft tenant: use \"common\", \"organizations\", \"consumers\", a \
     tenant ID (a GUID) or a domain like \"contoso.com\""
)]
pub struct InvalidTenant(pub String);

impl MicrosoftTenant {
    /// Work, school and personal accounts (multi-tenant registrations).
    pub const COMMON: &str = "common";
    /// Work and school accounts of any organization.
    pub const ORGANIZATIONS: &str = "organizations";
    /// Personal Microsoft accounts (Outlook.com).
    pub const CONSUMERS: &str = "consumers";

    /// `common`, the default.
    pub fn common() -> Self {
        Self(Self::COMMON.to_owned())
    }

    /// Parses a tenant (see the [type docs](Self)).
    ///
    /// # Errors
    ///
    /// [`InvalidTenant`] for anything else: blanks, URLs, paths, spaces.
    pub fn parse(text: &str) -> Result<Self, InvalidTenant> {
        let tenant = text.trim().to_ascii_lowercase();
        let valid = matches!(
            tenant.as_str(),
            Self::COMMON | Self::ORGANIZATIONS | Self::CONSUMERS
        ) || is_guid(&tenant)
            || is_domain(&tenant);
        if valid {
            Ok(Self(tenant))
        } else {
            Err(InvalidTenant(text.to_owned()))
        }
    }

    /// The tenant as it goes into the URL.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is `common`.
    pub fn is_common(&self) -> bool {
        self.0 == Self::COMMON
    }
}

impl Default for MicrosoftTenant {
    fn default() -> Self {
        Self::common()
    }
}

impl fmt::Display for MicrosoftTenant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for MicrosoftTenant {
    type Err = InvalidTenant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for MicrosoftTenant {
    type Error = InvalidTenant;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<MicrosoftTenant> for String {
    fn from(value: MicrosoftTenant) -> Self {
        value.0
    }
}

/// `8-4-4-4-12` hex digits.
fn is_guid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A DNS name with at least two labels and an alphabetic top-level label
/// (`contoso.com`, `contoso.onmicrosoft.com`).
fn is_domain(text: &str) -> bool {
    if text.len() > 253 {
        return false;
    }
    let labels: Vec<&str> = text.split('.').collect();
    let label_ok = |label: &&str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    labels.len() >= 2
        && labels.iter().all(label_ok)
        && labels
            .last()
            .is_some_and(|tld| tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_keywords_guids_and_domains() {
        for (text, parsed) in [
            ("common", "common"),
            ("Organizations", "organizations"),
            (" consumers ", "consumers"),
            (
                "72F988BF-86F1-41AF-91AB-2D7CD011DB47",
                "72f988bf-86f1-41af-91ab-2d7cd011db47",
            ),
            ("contoso.com", "contoso.com"),
            ("Contoso.onmicrosoft.com", "contoso.onmicrosoft.com"),
            ("my-org.co.uk", "my-org.co.uk"),
        ] {
            assert_eq!(MicrosoftTenant::parse(text).unwrap().as_str(), parsed);
        }
        assert!(MicrosoftTenant::default().is_common());
    }

    #[test]
    fn rejects_everything_else() {
        for text in [
            "",
            "  ",
            "contoso",
            "common/oauth2",
            "https://login.microsoftonline.com/contoso.com",
            "contoso.com/",
            "con toso.com",
            "-contoso.com",
            "contoso.c",
            "contoso.123",
            "72f988bf-86f1-41af-91ab",
            "72f988bf-86f1-41af-91ab-2d7cd011db4g",
            "a..b",
            "contoso.com?x=1",
        ] {
            assert!(MicrosoftTenant::parse(text).is_err(), "{text:?}");
        }
        let err = MicrosoftTenant::parse("x y").unwrap_err();
        assert!(err.to_string().contains("contoso.com"), "{err}");
    }

    #[test]
    fn serializes_as_a_string() {
        let tenant: MicrosoftTenant = serde_json::from_str("\"Contoso.com\"").unwrap();
        assert_eq!(tenant.as_str(), "contoso.com");
        assert_eq!(serde_json::to_string(&tenant).unwrap(), "\"contoso.com\"");
        assert!(serde_json::from_str::<MicrosoftTenant>("\"no way\"").is_err());
    }
}
