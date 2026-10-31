//! Domains for the Address Book space's Domains list (PLAN-contacts §3b): the types
//! [`MailSource::domains`](crate::source::MailSource::domains) and its siblings return,
//! and the synthetic domains of [`crate::FakeMailSource`].

use std::cmp::Ordering;

use gpui::SharedString;
use mazemail_types::Sensitive;
use time::OffsetDateTime;

/// Which domains the list shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct DomainFilter {
    /// Only names containing this text (case-insensitive).
    pub text: Option<String>,
    /// Hide free-mail providers (`gmail.com`, `outlook.com`, …): they are people, not
    /// organizations.
    pub organizations_only: bool,
}

/// The column the list is sorted by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DomainSortBy {
    /// The name, A to Z.
    #[default]
    Name,
    /// Number of contacts.
    Contacts,
    /// Number of messages.
    Messages,
    /// The newest message.
    LastSeen,
}

/// The list's order. Ties go by name, A to Z.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct DomainSort {
    /// The column.
    pub by: DomainSortBy,
    /// Largest, newest or Z first.
    pub descending: bool,
}

/// One row of the Domains list.
#[derive(Clone, PartialEq, Eq)]
pub struct DomainEntry {
    /// The registrable domain, lowercase ASCII (punycode for IDN): the key for
    /// [`MailSource::domain_contacts`](crate::source::MailSource::domain_contacts),
    /// [`MailSource::domain_favicon`](crate::source::MailSource::domain_favicon) and
    /// [`MailSource::domain_mail_query`](crate::source::MailSource::domain_mail_query).
    pub name: SharedString,
    /// The name to show (`bücher.de`).
    pub display_name: SharedString,
    /// `false` for free-mail providers.
    pub organization: bool,
    /// Distinct contact addresses there (every book, addresses you wrote to included).
    pub contacts: u64,
    /// Messages with an address there.
    pub messages: u64,
    /// The newest of those messages; `None` for a contact's domain without mail.
    pub last_seen: Option<OffsetDateTime>,
    /// Whether [`MailSource::domain_favicon`](crate::source::MailSource::domain_favicon)
    /// has an icon right now.
    pub has_favicon: bool,
}

impl std::fmt::Debug for DomainEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomainEntry")
            .field("organization", &self.organization)
            .field("contacts", &self.contacts)
            .field("messages", &self.messages)
            .finish_non_exhaustive()
    }
}

/// A contact at a domain: one address, with the books it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainContactEntry {
    /// The address.
    pub email: Sensitive<SharedString>,
    /// The name, if any.
    pub name: Option<Sensitive<SharedString>>,
    /// The address books (`collected`, `google:<account uuid>`, …).
    pub books: Vec<SharedString>,
}

/// `entries` after `filter`, in `sort`'s order (what a backend without its own
/// sorting returns).
pub fn filter_and_sort(
    mut entries: Vec<DomainEntry>,
    filter: &DomainFilter,
    sort: DomainSort,
) -> Vec<DomainEntry> {
    let text = filter
        .text
        .as_deref()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty());
    entries.retain(|entry| {
        (!filter.organizations_only || entry.organization)
            && text.as_deref().is_none_or(|text| {
                entry.name.contains(text) || entry.display_name.to_lowercase().contains(text)
            })
    });
    entries.sort_by(|a, b| {
        let primary = match sort.by {
            DomainSortBy::Name => Ordering::Equal,
            DomainSortBy::Contacts => a.contacts.cmp(&b.contacts),
            DomainSortBy::Messages => a.messages.cmp(&b.messages),
            DomainSortBy::LastSeen => a.last_seen.cmp(&b.last_seen),
        };
        let by_name = a.display_name.cmp(&b.display_name);
        let primary = if sort.by == DomainSortBy::Name {
            by_name
        } else {
            primary
        };
        let primary = if sort.descending {
            primary.reverse()
        } else {
            primary
        };
        primary.then(by_name)
    });
    entries
}

/// `(name, display name, organization, contacts, messages, days since the newest
/// message)`.
type FakeDomain = (&'static str, &'static str, bool, u64, u64, Option<i64>);

/// The domains of [`crate::FakeMailSource`]: `(name, display name, organization,
/// contacts, messages, days since the newest message)`.
const FAKE_DOMAINS: &[FakeDomain] = &[
    (
        "northwind.example",
        "northwind.example",
        true,
        4,
        128,
        Some(0),
    ),
    ("fabrikam.example", "fabrikam.example", true, 2, 37, Some(3)),
    ("contoso.example", "contoso.example", true, 1, 0, None),
    (
        "xn--bcher-kva.example",
        "bücher.example",
        true,
        0,
        5,
        Some(40),
    ),
    ("gmail.com", "gmail.com", false, 6, 64, Some(1)),
];

/// The synthetic Domains list, relative to `now`.
pub(crate) fn fake_domains(now: OffsetDateTime) -> Vec<DomainEntry> {
    FAKE_DOMAINS
        .iter()
        .map(
            |&(name, display_name, organization, contacts, messages, days)| DomainEntry {
                name: name.into(),
                display_name: display_name.into(),
                organization,
                contacts,
                messages,
                last_seen: days.map(|d| now - time::Duration::days(d)),
                has_favicon: false,
            },
        )
        .collect()
}

/// The synthetic contacts of a domain: `contacts` people named after it.
pub(crate) fn fake_domain_contacts(domain: &str) -> Vec<DomainContactEntry> {
    let count = FAKE_DOMAINS
        .iter()
        .find(|d| d.0 == domain)
        .map_or(0, |d| d.3);
    const NAMES: &[&str] = &["Anna", "Bob", "Carla", "Dan", "Eve", "Finn"];
    NAMES
        .iter()
        .take(count as usize)
        .map(|name| DomainContactEntry {
            email: Sensitive::new(format!("{}@{domain}", name.to_lowercase()).into()),
            name: Some(Sensitive::new((*name).into())),
            books: vec!["collected".into()],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_and_sorts() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(1000);
        let names = |filter: DomainFilter, sort: DomainSort| -> Vec<SharedString> {
            filter_and_sort(fake_domains(now), &filter, sort)
                .into_iter()
                .map(|d| d.name)
                .collect()
        };
        assert_eq!(
            names(DomainFilter::default(), DomainSort::default()),
            [
                "xn--bcher-kva.example",
                "contoso.example",
                "fabrikam.example",
                "gmail.com",
                "northwind.example"
            ]
        );
        assert_eq!(
            names(
                DomainFilter {
                    organizations_only: true,
                    ..DomainFilter::default()
                },
                DomainSort {
                    by: DomainSortBy::Messages,
                    descending: true
                }
            ),
            [
                "northwind.example",
                "fabrikam.example",
                "xn--bcher-kva.example",
                "contoso.example"
            ]
        );
        assert_eq!(
            names(
                DomainFilter {
                    text: Some("BÜCH".into()),
                    ..DomainFilter::default()
                },
                DomainSort::default()
            ),
            ["xn--bcher-kva.example"]
        );
        assert_eq!(fake_domain_contacts("fabrikam.example").len(), 2);
    }
}
