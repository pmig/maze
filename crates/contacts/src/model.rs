//! The contact model shared by every source: the fields a person has
//! ([`ContactFields`], what the UI edits), a person as a server has it ([`Person`]),
//! and the address books Maze keeps per account ([`Book`]).
//!
//! Nothing here implements `Debug` with contact data: names, addresses and phone
//! numbers are other people's data and never end up in logs.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `Debug` without the value: contact data never reaches logs.
macro_rules! redacted_debug {
    ($($ty:ident),*) => {
        $(impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($ty), "(<redacted>)"))
            }
        })*
    };
}

redacted_debug!(Labeled, Name, Organization, PostalAddress);

/// A value with an optional label, e.g. an address `work` or a phone `mobile`.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Labeled {
    /// The value (an address, a phone number, a URL).
    pub value: String,
    /// The label, lowercase (`home`, `work`, `mobile`, `other`, or the user's own).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

impl Labeled {
    /// A value without a label.
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            kind: None,
        }
    }

    /// A value with the label `kind`.
    pub fn with_kind(value: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            kind: Some(kind.into()),
        }
    }
}

/// A person's name. `display` is the full name as shown; the parts are optional.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Name {
    /// The full name ("Anna Maria Example"). Servers compute it from the parts when
    /// they are set; when only this is set, it is sent as the unstructured name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// Given name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub given: Option<String>,
    /// Family name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Middle name(s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub middle: Option<String>,
    /// Honorific prefix ("Dr.").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Honorific suffix ("Jr.").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
}

impl Name {
    /// Whether no part is set.
    pub fn is_empty(&self) -> bool {
        [
            &self.display,
            &self.given,
            &self.family,
            &self.middle,
            &self.prefix,
            &self.suffix,
        ]
        .iter()
        .all(|part| part.as_deref().is_none_or(|p| p.trim().is_empty()))
    }

    /// The full name: `display`, else the parts joined with spaces.
    pub fn full(&self) -> Option<String> {
        if let Some(display) = non_empty(self.display.as_deref()) {
            return Some(display.to_owned());
        }
        let parts: Vec<&str> = [
            &self.prefix,
            &self.given,
            &self.middle,
            &self.family,
            &self.suffix,
        ]
        .iter()
        .filter_map(|part| non_empty(part.as_deref()))
        .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    }

    /// Whether any structured part (not `display`) is set.
    pub fn has_parts(&self) -> bool {
        [
            &self.given,
            &self.family,
            &self.middle,
            &self.prefix,
            &self.suffix,
        ]
        .iter()
        .any(|part| non_empty(part.as_deref()).is_some())
    }
}

/// An organization a person belongs to.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Organization {
    /// The company or organization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The department.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub department: Option<String>,
    /// The person's job title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// A postal address.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PostalAddress {
    /// The whole address as one text, as the server formats it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formatted: Option<String>,
    /// Post office box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub po_box: Option<String>,
    /// Extended address (apartment, suite).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extended: Option<String>,
    /// Street and number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub street: Option<String>,
    /// City.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    /// Region, state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Postal code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postal_code: Option<String>,
    /// Country.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// Label (`home`, `work`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// What a contact consists of: the part people read and edit. Creating and editing a
/// contact sends these to the server.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContactFields {
    /// The name.
    #[serde(default)]
    pub name: Name,
    /// Email addresses, the primary one first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emails: Vec<Labeled>,
    /// Phone numbers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phones: Vec<Labeled>,
    /// Organizations (company, department, title).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub organizations: Vec<Organization>,
    /// Postal addresses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<PostalAddress>,
    /// Web sites.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<Labeled>,
    /// A free-text note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl fmt::Debug for ContactFields {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContactFields")
            .field("emails", &self.emails.len())
            .field("phones", &self.phones.len())
            .field("organizations", &self.organizations.len())
            .field("addresses", &self.addresses.len())
            .field("urls", &self.urls.len())
            .finish_non_exhaustive()
    }
}

impl ContactFields {
    /// The name to show: the full name, else the first email address.
    pub fn display_name(&self) -> Option<String> {
        self.name.full()
    }

    /// The email addresses, trimmed, without empty ones and without duplicates
    /// (case-insensitive), in order.
    pub fn email_addresses(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for email in &self.emails {
            let value = email.value.trim();
            if value.is_empty() || out.iter().any(|e| e.eq_ignore_ascii_case(value)) {
                continue;
            }
            out.push(value.to_owned());
        }
        out
    }

    /// Whether there is nothing to save: no name, no address, no phone number.
    pub fn is_empty(&self) -> bool {
        self.name.is_empty()
            && self.email_addresses().is_empty()
            && self.phones.iter().all(|p| p.value.trim().is_empty())
    }
}

/// A server's identifier of one source of a person (Google: `metadata.sources`). An
/// update has to name the contact source it changes, with that source's etag.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceRef {
    /// The source type (`CONTACT`, `PROFILE`, `OTHER_CONTACT`, …).
    pub kind: String,
    /// The source's id.
    pub id: String,
    /// The source's etag, if the server sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

impl fmt::Debug for SourceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceRef")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// A person as the server has it.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Person {
    /// The server's id (Google: `people/c…` or `otherContacts/c…`).
    pub resource_name: String,
    /// The version the server sent; an update must name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// The fields.
    #[serde(default)]
    pub fields: ContactFields,
    /// The URL of the person's own photo (not a generated placeholder), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub photo_url: Option<String>,
    /// The sources the server merged into this person.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceRef>,
    /// The person was deleted on the server (in an incremental sync).
    #[serde(default)]
    pub deleted: bool,
}

impl fmt::Debug for Person {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Person")
            .field("fields", &self.fields)
            .field("photo", &self.photo_url.is_some())
            .field("deleted", &self.deleted)
            .finish_non_exhaustive()
    }
}

impl Person {
    /// The person as a vCard 4.0 ([`crate::vcard::to_vcard`]).
    pub fn to_vcard(&self) -> String {
        crate::vcard::to_vcard(&self.fields, Some(&self.resource_name))
    }
}

/// Which of an account's address books.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[non_exhaustive]
pub enum BookKind {
    /// Google "My Contacts" (`connections.list`): readable and writable.
    Google,
    /// Google "Other contacts" (`otherContacts.list`): people you wrote to, saved
    /// by Google. Read-only.
    GoogleOther,
}

impl BookKind {
    /// Every kind.
    pub const ALL: [Self; 2] = [Self::Google, Self::GoogleOther];

    /// The prefix of the book's name (`google`, `google-other`).
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::GoogleOther => "google-other",
        }
    }

    /// The name people read.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Google => "My Contacts",
            Self::GoogleOther => "Other Contacts",
        }
    }

    /// Whether Maze can create and edit contacts in books of this kind.
    pub const fn is_writable(self) -> bool {
        matches!(self, Self::Google)
    }

    fn from_prefix(prefix: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.prefix() == prefix)
    }
}

/// An account's address book on a server, named `<provider>[-<kind>]:<account uuid>`
/// in the `contact` table's `book` column (`google:6f…`, `google-other:6f…`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Book {
    /// Which book.
    pub kind: BookKind,
    /// The account's UUID.
    pub account: Uuid,
}

impl Book {
    /// The book of `kind` of `account`.
    pub const fn new(kind: BookKind, account: Uuid) -> Self {
        Self { kind, account }
    }
}

impl fmt::Display for Book {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind.prefix(), self.account)
    }
}

/// A `book` value that isn't a server book (`collected`, an imported book).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a synced address book")]
pub struct NotASyncedBook;

impl FromStr for Book {
    type Err = NotASyncedBook;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (prefix, uuid) = s.split_once(':').ok_or(NotASyncedBook)?;
        let kind = BookKind::from_prefix(prefix).ok_or(NotASyncedBook)?;
        let account = Uuid::parse_str(uuid).map_err(|_| NotASyncedBook)?;
        Ok(Self { kind, account })
    }
}

/// A contact photo.
#[derive(Clone, PartialEq, Eq)]
pub struct Photo {
    /// MIME type, lowercase (`image/jpeg`).
    pub content_type: String,
    /// The image.
    pub bytes: Vec<u8>,
}

impl fmt::Debug for Photo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Photo")
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

pub(crate) fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn book_names_round_trip() {
        let account = Uuid::from_u128(0x6f00_0000_0000_4000_8000_0000_0000_0001);
        let book = Book::new(BookKind::Google, account);
        assert_eq!(
            book.to_string(),
            "google:6f000000-0000-4000-8000-000000000001"
        );
        assert_eq!(book.to_string().parse::<Book>(), Ok(book));
        let other = Book::new(BookKind::GoogleOther, account);
        assert_eq!(
            other.to_string(),
            "google-other:6f000000-0000-4000-8000-000000000001"
        );
        assert_eq!(other.to_string().parse::<Book>(), Ok(other));
        assert_eq!("collected".parse::<Book>(), Err(NotASyncedBook));
        assert_eq!("google:nope".parse::<Book>(), Err(NotASyncedBook));
        assert_eq!(
            "carddav:6f000000-0000-4000-8000-000000000001".parse::<Book>(),
            Err(NotASyncedBook)
        );
        assert!(BookKind::Google.is_writable() && !BookKind::GoogleOther.is_writable());
    }

    #[test]
    fn names_and_addresses() {
        let name = Name {
            given: Some("Anna".into()),
            family: Some("Example".into()),
            ..Name::default()
        };
        assert_eq!(name.full().as_deref(), Some("Anna Example"));
        assert!(name.has_parts());
        assert!(Name::default().is_empty());
        let fields = ContactFields {
            name,
            emails: vec![
                Labeled::new(" anna@example.com "),
                Labeled::new("ANNA@example.com"),
                Labeled::new(""),
                Labeled::with_kind("anna@work.example", "work"),
            ],
            ..ContactFields::default()
        };
        assert_eq!(
            fields.email_addresses(),
            ["anna@example.com", "anna@work.example"]
        );
        let debug = format!("{fields:?}");
        assert!(
            !debug.contains("anna") && !debug.contains("Anna"),
            "{debug}"
        );
    }
}
