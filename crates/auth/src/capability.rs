//! What an account's OAuth grant can be used for ([`Capability`], [`Capabilities`])
//! and which API an access token is for ([`TokenResource`]).
//!
//! Every capability needs its own scopes ([`Provider::scopes_for`](crate::Provider::scopes_for)).
//! Mail is what every OAuth account signs in for; contacts are opt-in per account and
//! can be granted later with an incremental consent
//! ([`AuthorizeOptions::incremental_consent`](crate::AuthorizeOptions::incremental_consent)).
//! A missing contacts grant never affects mail.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Something Maze does with an account's OAuth grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    /// IMAP and SMTP (`https://mail.google.com/`; Outlook's `IMAP.AccessAsUser.All`
    /// and `SMTP.Send`).
    Mail,
    /// Reading and writing the account's contacts (Google People API; Microsoft
    /// Graph `Contacts.ReadWrite`).
    Contacts,
}

impl Capability {
    /// Every capability, in the order scopes are requested.
    pub const ALL: [Self; 2] = [Self::Mail, Self::Contacts];

    /// The technical name, `mail` or `contacts`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mail => "mail",
            Self::Contacts => "contacts",
        }
    }

    /// The name people read, e.g. in "Contacts need permission".
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Mail => "Mail",
            Self::Contacts => "Contacts",
        }
    }

    const fn bit(self) -> u8 {
        match self {
            Self::Mail => 1,
            Self::Contacts => 1 << 1,
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of [`Capability`]s, e.g. what an account's grant covers. Serializes as a
/// list (`["mail", "contacts"]`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "Vec<Capability>", from = "Vec<Capability>")]
pub struct Capabilities(u8);

impl Capabilities {
    /// No capability.
    pub const NONE: Self = Self(0);
    /// Mail only: what every OAuth account signs in for.
    pub const MAIL: Self = Self::only(Capability::Mail);
    /// Contacts only, for an incremental consent on an account that has mail.
    pub const CONTACTS: Self = Self::only(Capability::Contacts);

    /// The set with just `capability`.
    pub const fn only(capability: Capability) -> Self {
        Self(capability.bit())
    }

    /// This set plus `capability`.
    #[must_use]
    pub const fn with(self, capability: Capability) -> Self {
        Self(self.0 | capability.bit())
    }

    /// Both sets together.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether `capability` is in the set.
    pub const fn contains(self, capability: Capability) -> bool {
        self.0 & capability.bit() != 0
    }

    /// Whether the set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The capabilities in the set, in [`Capability::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = Capability> {
        Capability::ALL
            .into_iter()
            .filter(move |capability| self.contains(*capability))
    }
}

impl From<Capability> for Capabilities {
    fn from(capability: Capability) -> Self {
        Self::only(capability)
    }
}

impl FromIterator<Capability> for Capabilities {
    fn from_iter<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        iter.into_iter().fold(Self::NONE, Self::with)
    }
}

impl From<Vec<Capability>> for Capabilities {
    fn from(capabilities: Vec<Capability>) -> Self {
        capabilities.into_iter().collect()
    }
}

impl From<Capabilities> for Vec<Capability> {
    fn from(capabilities: Capabilities) -> Self {
        capabilities.iter().collect()
    }
}

impl fmt::Debug for Capabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// The API an access token is issued for.
///
/// Google issues one access token for all granted scopes, so every capability uses
/// [`TokenResource::Primary`]. Microsoft issues one access token per resource: IMAP
/// and SMTP (`outlook.office.com`) use the primary token, contacts a separate
/// [`TokenResource::Graph`] token (`graph.microsoft.com`), both from the same
/// refresh token. See [`Provider::resource_for`](crate::Provider::resource_for).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenResource {
    /// The token mail uses: Google's only token, Microsoft's `outlook.office.com`.
    Primary,
    /// Microsoft Graph (`graph.microsoft.com`).
    Graph,
}

impl TokenResource {
    /// Every resource.
    pub const ALL: [Self; 2] = [Self::Primary, Self::Graph];

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Primary => 0,
            Self::Graph => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn set_operations() {
        let set = Capabilities::MAIL.with(Capability::Contacts);
        assert!(set.contains(Capability::Mail) && set.contains(Capability::Contacts));
        assert!(!Capabilities::MAIL.contains(Capability::Contacts));
        assert!(Capabilities::NONE.is_empty());
        assert_eq!(Capabilities::MAIL.union(Capabilities::CONTACTS), set);
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            [Capability::Mail, Capability::Contacts]
        );
        assert_eq!(format!("{set:?}"), "{Mail, Contacts}");
    }

    #[test]
    fn serializes_as_a_list() {
        let set = Capabilities::MAIL.with(Capability::Contacts);
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(json, r#"["mail","contacts"]"#);
        assert_eq!(serde_json::from_str::<Capabilities>(&json).unwrap(), set);
        assert_eq!(
            serde_json::from_str::<Capabilities>("[]").unwrap(),
            Capabilities::NONE
        );
    }
}
