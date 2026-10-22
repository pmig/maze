//! Display-name spoofing.

use std::collections::HashMap;

use mazemail_types::Address;

use super::confusables::skeleton;
use super::{Severity, Warning, WarningKind};

/// Normalized names shorter than this aren't compared; initials and one-letter
/// names would match far too often.
const MIN_NAME_CHARS: usize = 3;

/// Looks up the addresses of known contacts by display name.
pub trait KnownContacts {
    /// The addresses of contacts whose display name normalizes to
    /// `normalized_name` (see [`normalize_display_name`]). Empty if none.
    fn addresses_for(&self, normalized_name: &str) -> Vec<String>;
}

/// An in-memory [`KnownContacts`] built from address book entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContactIndex {
    by_name: HashMap<String, Vec<String>>,
}

impl ContactIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a contact. Entries without a usable name are ignored.
    pub fn add(&mut self, address: &Address) {
        let Some(name) = address.name.as_deref() else {
            return;
        };
        let key = normalize_display_name(name);
        if key.chars().count() < MIN_NAME_CHARS {
            return;
        }
        let email = address.email.trim().to_lowercase();
        let entry = self.by_name.entry(key).or_default();
        if !entry.contains(&email) {
            entry.push(email);
        }
    }

    /// The number of distinct names.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

impl<'a> FromIterator<&'a Address> for ContactIndex {
    fn from_iter<I: IntoIterator<Item = &'a Address>>(iter: I) -> Self {
        let mut index = Self::new();
        for address in iter {
            index.add(address);
        }
        index
    }
}

impl KnownContacts for ContactIndex {
    fn addresses_for(&self, normalized_name: &str) -> Vec<String> {
        self.by_name
            .get(normalized_name)
            .cloned()
            .unwrap_or_default()
    }
}

/// Normalize a display name for comparison: confusable characters mapped to their
/// [skeleton], case folded, everything but letters and digits collapsed to single
/// spaces. `"Anna Exаmple"` (Cyrillic `а`) and `anna  example` normalize the same.
pub fn normalize_display_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_space = false;
    for c in skeleton(name).chars() {
        if c.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(c);
        } else {
            pending_space = true;
        }
    }
    out
}

/// Check the `From` display name.
///
/// - The name is a known contact's, but the address isn't one of theirs
///   ([`Severity::Caution`]: people do write from a second address).
/// - The name contains an email address that isn't the sender's
///   ([`Severity::Danger`]).
pub fn check_display_name(from: &Address, contacts: Option<&dyn KnownContacts>) -> Vec<Warning> {
    let Some(name) = from
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    else {
        return Vec::new();
    };
    let email = from.email.trim().to_lowercase();
    let mut warnings = Vec::new();

    if let Some(shown) = address_in_name(name)
        && shown != email
    {
        warnings.push(Warning {
            message: format!("The sender name shows {shown}, but the message is from {email}."),
            kind: WarningKind::DisplayNameAddressMismatch {
                display_name: name.to_owned(),
                address: email.clone(),
            },
            severity: Severity::Danger,
        });
    }

    let key = normalize_display_name(name);
    if let Some(contacts) = contacts
        && key.chars().count() >= MIN_NAME_CHARS
    {
        let known = contacts.addresses_for(&key);
        if !known.is_empty() && !known.iter().any(|a| a.eq_ignore_ascii_case(&email)) {
            warnings.push(Warning {
                message: format!(
                    "{name} is in your contacts as {}, but this message is from {email}.",
                    known.join(", ")
                ),
                kind: WarningKind::DisplayNameSpoofing {
                    display_name: name.to_owned(),
                    address: email,
                    known_addresses: known,
                },
                severity: Severity::Caution,
            });
        }
    }
    warnings
}

/// The first `local@domain.tld` in a display name, lowercased.
fn address_in_name(name: &str) -> Option<String> {
    name.split(|c: char| {
        c.is_whitespace() || matches!(c, '<' | '>' | '(' | ')' | '"' | '\'' | ',' | ';')
    })
    .map(|token| token.trim_matches(|c| matches!(c, '.' | ':' | '[' | ']')))
    .find(|token| {
        token.rsplit_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.ends_with('.')
        })
    })
    .map(str::to_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contacts() -> ContactIndex {
        let book = [
            Address::new(Some("Anna Example"), "anna@example.com"),
            Address::new(Some("Anna Example"), "Anna.Private@mail.example"),
            Address::new(Some("Al"), "al@example.com"),
            Address::new(None::<String>, "nobody@example.com"),
        ];
        book.iter().collect()
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize_display_name("  Anna   Example "), "anna exarnple");
        assert_eq!(
            normalize_display_name("\"Anna Exаmple\""),
            normalize_display_name("anna example")
        );
        assert_eq!(
            normalize_display_name("Example, Anna (Work)"),
            "exarnple anna work"
        );
    }

    #[test]
    fn index_ignores_short_or_missing_names() {
        let index = contacts();
        assert_eq!(index.len(), 1);
        assert_eq!(
            index.addresses_for(&normalize_display_name("Anna Example")),
            ["anna@example.com", "anna.private@mail.example"]
        );
        assert!(ContactIndex::new().is_empty());
    }

    #[test]
    fn flags_known_name_with_unknown_address() {
        let index = contacts();
        let from = Address::new(Some("Anna Exаmple"), "anna.example@freemail.example");
        let warnings = check_display_name(&from, Some(&index));
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].severity, Severity::Caution);
        assert!(matches!(
            &warnings[0].kind,
            WarningKind::DisplayNameSpoofing { known_addresses, .. } if known_addresses.len() == 2
        ));

        for email in ["anna@example.com", "ANNA.PRIVATE@mail.example"] {
            let from = Address::new(Some("Anna Example"), email);
            assert!(
                check_display_name(&from, Some(&index)).is_empty(),
                "{email}"
            );
        }
        let stranger = Address::new(Some("Bob Builder"), "bob@example.org");
        assert!(check_display_name(&stranger, Some(&index)).is_empty());
        let no_name = Address::new(None::<String>, "x@evil.example");
        assert!(check_display_name(&no_name, Some(&index)).is_empty());
    }

    #[test]
    fn flags_address_in_display_name() {
        let from = Address::new(Some("ceo@company.example"), "attacker@evil.example");
        let warnings = check_display_name(&from, None);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].severity, Severity::Danger);
        assert_eq!(
            warnings[0].message,
            "The sender name shows ceo@company.example, but the message is from attacker@evil.example."
        );

        let honest = Address::new(Some("Noreply@GitHub.com"), "noreply@github.com");
        assert!(check_display_name(&honest, None).is_empty());
        let not_an_address = Address::new(Some("Team @ Example"), "team@example.com");
        assert!(check_display_name(&not_an_address, None).is_empty());
    }
}
