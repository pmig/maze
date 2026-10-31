//! The [`FakeMailSource`](crate::FakeMailSource)'s address books: a Google account
//! with My Contacts and Other Contacts, a Microsoft 365 account, an imported
//! Thunderbird book and collected addresses, with people in several of them, grouped
//! the way the core groups them (one row per address, a label per account, the
//! account's name before the collected one).

use std::sync::Arc;

use gpui::SharedString;
use mazemail_types::AccountId;
use time::{Duration, OffsetDateTime};

use crate::source::{
    AddressBookEntry, BookKind, ContactDetail, ContactLabel, ContactPhone, ContactQuery,
    ContactRow, ContactSort, ContactVersion,
};

/// A book: id, name, account (index into the fake's accounts, its name).
struct Book {
    id: &'static str,
    name: &'static str,
    account: Option<(i64, &'static str)>,
}

const GMAIL: (i64, &str) = (1, "Gmail (personal)");
const MICROSOFT: (i64, &str) = (3, "Microsoft 365");

const BOOKS: &[Book] = &[
    Book {
        id: "google:fake-1",
        name: "My Contacts",
        account: Some(GMAIL),
    },
    Book {
        id: "google-other:fake-1",
        name: "Other Contacts",
        account: Some(GMAIL),
    },
    Book {
        id: "microsoft:fake-3",
        name: "Contacts",
        account: Some(MICROSOFT),
    },
    Book {
        id: "collected",
        name: "Collected",
        account: None,
    },
    Book {
        id: "Personal Address Book",
        name: "Personal Address Book",
        account: None,
    },
];

/// An entry: book index, name, address, days since you last wrote, phone, org, title.
struct Entry {
    book: usize,
    name: Option<&'static str>,
    email: &'static str,
    days_ago: Option<i64>,
    phone: Option<(&'static str, &'static str)>,
    org: Option<&'static str>,
    title: Option<&'static str>,
}

const fn entry(book: usize, name: Option<&'static str>, email: &'static str) -> Entry {
    Entry {
        book,
        name,
        email,
        days_ago: None,
        phone: None,
        org: None,
        title: None,
    }
}

const fn used(mut e: Entry, days: i64) -> Entry {
    e.days_ago = Some(days);
    e
}

const fn card(
    mut e: Entry,
    phone: (&'static str, &'static str),
    org: &'static str,
    title: &'static str,
) -> Entry {
    e.phone = Some(phone);
    e.org = Some(org);
    e.title = Some(title);
    e
}

const ENTRIES: &[Entry] = &[
    // Anna is in Google, Microsoft 365 and collected: one row, three labels.
    card(
        entry(0, Some("Anna Example"), "anna@example.com"),
        ("cell", "+43 660 1234567"),
        "Example GmbH",
        "Head of Research",
    ),
    card(
        entry(2, Some("Anna Example"), "anna@example.com"),
        ("work", "+43 1 555 0100"),
        "Example GmbH",
        "Head of Research",
    ),
    used(entry(3, None, "anna@example.com"), 0),
    card(
        entry(2, Some("Jonas Berger"), "jonas.berger@work.example"),
        ("work", "+43 1 555 0142"),
        "Work Example AG",
        "Engineering Manager",
    ),
    used(entry(3, Some("Jonas"), "jonas.berger@work.example"), 1),
    card(
        entry(0, Some("Miriam Hofer"), "miriam@example.org"),
        ("cell", "+43 664 9876543"),
        "Hofer Architektur",
        "Architect",
    ),
    used(entry(3, None, "miriam@example.org"), 3),
    entry(0, Some("Lukas Steiner"), "lukas@steiner.example"),
    card(
        entry(2, Some("Sophie Wagner"), "sophie.wagner@work.example"),
        ("work", "+43 1 555 0177"),
        "Work Example AG",
        "Product Designer",
    ),
    used(entry(3, None, "sophie.wagner@work.example"), 2),
    entry(0, Some("Elena Rossi"), "elena.rossi@acme.example"),
    used(entry(3, Some("Elena Rossi"), "elena.rossi@acme.example"), 9),
    entry(4, Some("Tomás García"), "tomas@garcia.example"),
    used(entry(3, None, "tomas@garcia.example"), 30),
    entry(4, Some("Paul Huber"), "paul.huber@example.net"),
    entry(1, Some("Kathrin Maier"), "kathrin@club.example"),
    used(entry(3, None, "kathrin@club.example"), 5),
    entry(2, Some("Hannah Gruber"), "hannah.gruber@work.example"),
    entry(0, Some("Felix Bauer"), "felix@bauer.example"),
    entry(4, Some("Felix Bauer"), "felix@bauer.example"),
    entry(1, Some("Clara Novak"), "clara.novak@acme.example"),
    used(entry(3, None, "markus@pichler.example"), 12),
    entry(0, Some("Julia Lechner"), "julia.lechner@example.com"),
    used(entry(3, Some("Nina Fischer"), "nina@fischer.example"), 7),
    entry(1, Some("Club Secretary"), "secretary@club.example"),
    used(entry(3, None, "secretary@club.example"), 20),
    entry(4, Some("Oma"), "oma@familie.example"),
];

fn color(book: &Book) -> Option<u32> {
    book.account.map(|(id, _)| id as u32)
}

fn label(book: &Book) -> ContactLabel {
    ContactLabel {
        text: book.account.map_or(book.name, |(_, name)| name).into(),
        color: color(book),
    }
}

/// Accounts first, then imported books, then collected (as the core picks names).
fn rank(book: &Book) -> u8 {
    match (book.account, book.id) {
        (Some(_), _) => 0,
        (None, "collected") => 2,
        (None, _) => 1,
    }
}

/// The entries of each address, in first-seen order, best name source first.
fn groups() -> Vec<Vec<&'static Entry>> {
    let mut groups: Vec<Vec<&Entry>> = Vec::new();
    for entry in ENTRIES {
        match groups
            .iter_mut()
            .find(|g| g[0].email.eq_ignore_ascii_case(entry.email))
        {
            Some(group) => group.push(entry),
            None => groups.push(vec![entry]),
        }
    }
    for group in &mut groups {
        group.sort_by_key(|e| rank(&BOOKS[e.book]));
    }
    groups
}

/// The books with their counts.
pub(crate) fn books() -> Vec<AddressBookEntry> {
    BOOKS
        .iter()
        .enumerate()
        .map(|(ix, book)| AddressBookEntry {
            id: book.id.into(),
            name: book.name.into(),
            kind: match book.account {
                Some((id, name)) => BookKind::Account {
                    account: AccountId(id),
                    name: name.into(),
                },
                None if book.id == "collected" => BookKind::Collected,
                None => BookKind::Imported,
            },
            count: ENTRIES.iter().filter(|e| e.book == ix).count() as u64,
            color: color(book),
        })
        .collect()
}

/// The contact list for `query`.
pub(crate) fn contacts(query: &ContactQuery, now: OffsetDateTime) -> Arc<[ContactRow]> {
    let needle = query.text.trim().to_lowercase();
    let mut rows: Vec<ContactRow> = groups()
        .into_iter()
        .filter(|group| {
            query
                .book
                .as_ref()
                .is_none_or(|book| group.iter().any(|e| BOOKS[e.book].id == book.as_ref()))
        })
        .filter(|group| {
            needle.is_empty()
                || group.iter().any(|e| {
                    e.email.to_lowercase().contains(&needle)
                        || e.name.is_some_and(|n| n.to_lowercase().contains(&needle))
                })
        })
        .map(|group| {
            let mut labels: Vec<ContactLabel> = Vec::new();
            for e in &group {
                let label = label(&BOOKS[e.book]);
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
            ContactRow {
                email: group[0].email.into(),
                name: group.iter().find_map(|e| e.name).map(SharedString::from),
                labels,
                last_used: group
                    .iter()
                    .filter_map(|e| e.days_ago)
                    .min()
                    .map(|days| now - Duration::days(days) - Duration::hours(1)),
            }
        })
        .collect();
    let name_key = |row: &ContactRow| row.name.as_ref().unwrap_or(&row.email).to_lowercase();
    match query.sort {
        ContactSort::Name => rows.sort_by_key(name_key),
        ContactSort::LastUsed => rows.sort_by(|a, b| {
            b.last_used
                .cmp(&a.last_used)
                .then_with(|| name_key(a).cmp(&name_key(b)))
        }),
    }
    Arc::from(rows)
}

/// The detail of `email`.
pub(crate) fn contact(email: &str, now: OffsetDateTime) -> Option<ContactDetail> {
    let group = groups()
        .into_iter()
        .find(|g| g[0].email.eq_ignore_ascii_case(email))?;
    let versions: Vec<ContactVersion> = group
        .iter()
        .map(|e| {
            let book = &BOOKS[e.book];
            ContactVersion {
                id: ENTRIES
                    .iter()
                    .position(|x| std::ptr::eq(x, *e))
                    .map_or(0, |ix| ix as i64 + 1),
                book: book.id.into(),
                book_name: book.name.into(),
                label: label(book),
                name: e.name.map(SharedString::from),
                emails: vec![e.email.into()],
                phones: e
                    .phone
                    .map(|(kind, number)| ContactPhone {
                        kind: Some(kind.into()),
                        number: number.into(),
                    })
                    .into_iter()
                    .collect(),
                organization: e.org.map(SharedString::from),
                title: e.title.map(SharedString::from),
                frequency: e.days_ago.map_or(0, |days| 1 + (12 / (days as u32 + 1))),
                last_used: e
                    .days_ago
                    .map(|days| now - Duration::days(days) - Duration::hours(1)),
                editable: false,
            }
        })
        .collect();
    let mut phones: Vec<ContactPhone> = Vec::new();
    for version in &versions {
        for phone in &version.phones {
            if !phones.iter().any(|p| p.number == phone.number) {
                phones.push(phone.clone());
            }
        }
    }
    Some(ContactDetail {
        email: group[0].email.into(),
        name: group.iter().find_map(|e| e.name).map(SharedString::from),
        emails: vec![group[0].email.into()],
        phones,
        organization: versions.iter().find_map(|v| v.organization.clone()),
        title: versions.iter().find_map(|v| v.title.clone()),
        versions,
    })
}
