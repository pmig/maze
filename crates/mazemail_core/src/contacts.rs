//! The Address Book (PLAN-contacts §1, ADR-024): read-only views over the store's
//! `contact` table for the Address Book space, plus the hook contact sync fills in for
//! editing.
//!
//! # Books
//!
//! Every entry has a `book` (`contact.book`), and the book's id says where it comes
//! from ([`parse_book`]):
//!
//! | `book`                          | Source                    | Shown as                     |
//! | ------------------------------- | ------------------------- | ---------------------------- |
//! | `collected`                     | addresses you wrote to    | "Collected"                  |
//! | `<provider>:<account uuid>`     | an account's contacts     | under the account            |
//! | `<provider>-<kind>:<uuid>`      | another book of it        | under the account            |
//! | anything else                   | an imported address book  | the book's name              |
//!
//! Provider books name themselves after the provider ([`provider_book_name`]): Google's
//! `google:` is "My Contacts" and `google-other:` "Other Contacts", Microsoft's
//! `microsoft:` "Contacts". A provider book whose account doesn't exist (any more) is
//! listed under "Other". The Thunderbird importer stores its address books under their
//! names ("Personal Address Book", "Collected Addresses").
//!
//! # All Contacts
//!
//! [`CoreHandle::contacts`] groups the entries by address (case-insensitive): one
//! [`ContactSummary`] per address with a [`BookLabel`] per account (or collected or
//! imported book) it appears in. The name comes from an account's book first, then an
//! imported one, then the collected entry. Reading 10,000 entries, grouping, filtering
//! and sorting them takes a few milliseconds; the UI shows the rows in a virtual list.
//!
//! # Editing
//!
//! The server owns the contacts (ADR-024): an edit writes to the account's server and
//! the local copy is updated from its answer. Google's My Contacts are editable once
//! the account connected its contacts ([`CoreHandle::contact_book_editable`]);
//! [`CoreHandle::save_contact`] writes through to Google (see
//! [`crate::synced_contacts`]). Every other book answers
//! [`ContactError::NotEditable`].
//!
//! Contacts are personal data: they stay in the encrypted database, nothing here goes
//! over the network, and no address or name is logged.

use std::cmp::Ordering;
use std::collections::HashMap;

use mazemail_types::AccountId;
use store::{Account, Contact, ContactId, Queries as _, Uuid};

use crate::handle::CoreHandle;
use crate::{Error, Result};

/// The `contact.book` of addresses you wrote to.
pub const COLLECTED_BOOK: &str = "collected";

/// What a `contact.book` id says about the book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedBook<'a> {
    /// `collected`: addresses you wrote to.
    Collected,
    /// `<provider>[-<kind>]:<account uuid>`: a book of an account's contacts.
    Provider {
        /// `google`, `microsoft`, `carddav`, …
        provider: &'a str,
        /// The kind of book after the dash (`other` in `google-other:`), if any.
        kind: Option<&'a str>,
        /// The account's UUID.
        account: Uuid,
    },
    /// Any other id: an imported address book, named by its id.
    Named(&'a str),
}

/// Reads a `contact.book` id; see the [module docs](self).
pub fn parse_book(book: &str) -> ParsedBook<'_> {
    if book == COLLECTED_BOOK {
        return ParsedBook::Collected;
    }
    let Some((prefix, rest)) = book.split_once(':') else {
        return ParsedBook::Named(book);
    };
    let valid_prefix = !prefix.is_empty()
        && prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !prefix.starts_with('-');
    match (valid_prefix, Uuid::parse_str(rest)) {
        (true, Ok(account)) => {
            let (provider, kind) = match prefix.split_once('-') {
                Some((provider, kind)) if !kind.is_empty() => (provider, Some(kind)),
                Some((provider, _)) => (provider, None),
                None => (prefix, None),
            };
            ParsedBook::Provider {
                provider,
                kind,
                account,
            }
        }
        _ => ParsedBook::Named(book),
    }
}

/// The name of a provider's book: Google's main book is "My Contacts", its
/// `other` book "Other Contacts", Microsoft's "Contacts".
pub fn provider_book_name(provider: &str, kind: Option<&str>) -> String {
    match (provider, kind) {
        ("google", None) => "My Contacts".to_owned(),
        (_, None) => "Contacts".to_owned(),
        (_, Some(kind)) => {
            let mut chars = kind.chars();
            let title: String = chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default();
            format!("{} Contacts", title.replace(['-', '_'], " "))
        }
    }
}

/// Where an address book comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookSource {
    /// Addresses you wrote to.
    Collected,
    /// An address book imported once (Thunderbird).
    Imported,
    /// One of an account's books, kept in step with its server by contact sync.
    Account {
        /// The account.
        account: AccountId,
        /// Its stable UUID (the book id has it; the UI colors labels by it).
        uuid: Uuid,
        /// Its name.
        account_name: String,
        /// `google`, `microsoft`, …
        provider: String,
        /// The kind of book (`other`), if not the main one.
        kind: Option<String>,
    },
    /// A provider book whose account doesn't exist (any more): under "Other".
    Orphaned {
        /// `google`, `microsoft`, …
        provider: String,
    },
}

/// One address book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressBookInfo {
    /// The `contact.book` id.
    pub id: String,
    /// The book's name: "Collected", "My Contacts", "Personal Address Book".
    pub name: String,
    /// Where it comes from.
    pub source: BookSource,
    /// How many addresses it has.
    pub count: u64,
    /// Whether its contacts can be edited ([`CoreHandle::contact_book_editable`]).
    pub editable: bool,
}

impl AddressBookInfo {
    /// The label a contact of this book gets: the account's name, "Collected", or the
    /// imported book's name.
    pub fn label(&self) -> BookLabel {
        let (text, account) = match &self.source {
            BookSource::Account {
                account,
                uuid,
                account_name,
                ..
            } => (account_name.clone(), Some((*account, *uuid))),
            BookSource::Collected => ("Collected".to_owned(), None),
            BookSource::Imported | BookSource::Orphaned { .. } => (self.name.clone(), None),
        };
        BookLabel {
            text,
            account,
            book: self.id.clone(),
            collected: matches!(self.source, BookSource::Collected),
        }
    }

    /// Ranks books for picking a contact's name: accounts, then imported books, then
    /// collected addresses.
    fn name_rank(&self) -> u8 {
        match self.source {
            BookSource::Account { .. } => 0,
            BookSource::Imported => 1,
            BookSource::Orphaned { .. } => 2,
            BookSource::Collected => 3,
        }
    }
}

/// The label of a contact row: which account (or collected or imported book) it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookLabel {
    /// The account's name, "Collected", or the imported book's name.
    pub text: String,
    /// The account and its UUID, for an account's book.
    pub account: Option<(AccountId, Uuid)>,
    /// The first book of the contact that gave this label.
    pub book: String,
    /// The collected book.
    pub collected: bool,
}

/// How the contact list is sorted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ContactSort {
    /// By name (or address, without one), A to Z.
    #[default]
    Name,
    /// Most recently written to first; never written to last, by name.
    LastUsed,
}

/// What [`CoreHandle::contacts`] lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContactQuery {
    /// One book (`contact.book`), or every book (All Contacts).
    pub book: Option<String>,
    /// Only contacts whose name or address contains this (case-insensitive).
    pub text: String,
    /// The order.
    pub sort: ContactSort,
}

/// One row of the contact list: an address with the books it is in.
#[derive(Clone, PartialEq, Eq)]
pub struct ContactSummary {
    /// The address (as the entry the name came from spells it).
    pub email: String,
    /// The name: from an account's book first, then an imported one, then collected.
    pub name: Option<String>,
    /// One label per account (or collected or imported book): accounts first, then
    /// imported books, then Collected.
    pub labels: Vec<BookLabel>,
    /// How often you wrote to the address (all books).
    pub frequency: u64,
    /// When you last wrote to it (Unix seconds).
    pub last_used: Option<i64>,
}

impl std::fmt::Debug for ContactSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Names and addresses are personal data: never in logs.
        f.debug_struct("ContactSummary")
            .field("labels", &self.labels.len())
            .field("frequency", &self.frequency)
            .field("last_used", &self.last_used)
            .finish_non_exhaustive()
    }
}

/// A phone number of a contact.
#[derive(Clone, PartialEq, Eq)]
pub struct ContactPhone {
    /// What kind of number (`cell`, `work`, …) as the vCard says, if it does.
    pub kind: Option<String>,
    /// The number as written.
    pub number: String,
}

impl std::fmt::Debug for ContactPhone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContactPhone")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// One book's version of a contact.
#[derive(Clone, PartialEq, Eq)]
pub struct ContactVersion {
    /// The entry.
    pub id: ContactId,
    /// Its book.
    pub book: AddressBookInfo,
    /// The name in this book.
    pub name: Option<String>,
    /// The entry's address first, then the vCard's other addresses.
    pub emails: Vec<String>,
    /// Phone numbers from the vCard.
    pub phones: Vec<ContactPhone>,
    /// Organization (`ORG`), if the vCard has one.
    pub organization: Option<String>,
    /// Title (`TITLE`), if the vCard has one.
    pub title: Option<String>,
    /// How often you wrote to the address (collected entries).
    pub frequency: u32,
    /// When you last wrote to it (Unix seconds).
    pub last_used: Option<i64>,
}

impl std::fmt::Debug for ContactVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContactVersion")
            .field("id", &self.id)
            .field("book", &self.book.id)
            .finish_non_exhaustive()
    }
}

/// Everything known about an address: each book's version, and their fields merged.
#[derive(Clone, PartialEq, Eq)]
pub struct ContactDetail {
    /// The address.
    pub email: String,
    /// The name, chosen like [`ContactSummary::name`].
    pub name: Option<String>,
    /// Every address of every version, the contact's own first (no duplicates).
    pub emails: Vec<String>,
    /// Every phone number of every version (no duplicates).
    pub phones: Vec<ContactPhone>,
    /// The first organization a version has.
    pub organization: Option<String>,
    /// The first title a version has.
    pub title: Option<String>,
    /// One version per book, in book order.
    pub versions: Vec<ContactVersion>,
}

impl std::fmt::Debug for ContactDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContactDetail")
            .field("versions", &self.versions)
            .finish_non_exhaustive()
    }
}

/// A change to one book's version of a contact (or a new contact), for
/// [`CoreHandle::save_contact`]. The account's server gets it first.
#[derive(Clone, PartialEq, Eq)]
pub struct ContactEdit {
    /// The book to write to.
    pub book: String,
    /// The entry changed, or `None` for a new contact.
    pub contact: Option<ContactId>,
    /// The name.
    pub name: Option<String>,
    /// The addresses, primary first.
    pub emails: Vec<String>,
    /// The phone numbers.
    pub phones: Vec<ContactPhone>,
    /// Organization.
    pub organization: Option<String>,
    /// Title.
    pub title: Option<String>,
}

impl std::fmt::Debug for ContactEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContactEdit")
            .field("book", &self.book)
            .field("contact", &self.contact)
            .finish_non_exhaustive()
    }
}

/// Why a contact couldn't be saved.
#[derive(Debug, thiserror::Error)]
pub enum ContactError {
    /// The book is read-only in Maze: collected addresses, imported books, and every
    /// book until contact sync can write to its server.
    #[error("contacts in {book} can't be edited in Maze")]
    NotEditable {
        /// The `contact.book` id.
        book: String,
    },
    /// The core failed.
    #[error(transparent)]
    Core(#[from] Error),
}

/// The books of `entries` with their counts, in display order: each account's books
/// (accounts by ordinal), Collected, imported books by name, then books of accounts
/// that don't exist.
fn books_of(entries: &[Contact], accounts: &[Account]) -> Vec<AddressBookInfo> {
    let mut counts: HashMap<&str, u64> = HashMap::new();
    for entry in entries {
        *counts.entry(entry.book.as_str()).or_default() += 1;
    }
    let mut books: Vec<(usize, AddressBookInfo)> = counts
        .into_iter()
        .map(|(id, count)| {
            let (order, name, source) = match parse_book(id) {
                ParsedBook::Collected => (
                    accounts.len(),
                    "Collected".to_owned(),
                    BookSource::Collected,
                ),
                ParsedBook::Named(name) => {
                    (accounts.len() + 1, name.to_owned(), BookSource::Imported)
                }
                ParsedBook::Provider {
                    provider,
                    kind,
                    account,
                } => {
                    let name = provider_book_name(provider, kind);
                    match accounts.iter().position(|a| a.uuid == account) {
                        Some(ix) => (
                            ix,
                            name,
                            BookSource::Account {
                                account: accounts[ix].id,
                                uuid: account,
                                account_name: accounts[ix].name.clone(),
                                provider: provider.to_owned(),
                                kind: kind.map(str::to_owned),
                            },
                        ),
                        None => (
                            accounts.len() + 2,
                            name,
                            BookSource::Orphaned {
                                provider: provider.to_owned(),
                            },
                        ),
                    }
                }
            };
            let info = AddressBookInfo {
                id: id.to_owned(),
                name,
                source,
                count,
                editable: book_editable(id, accounts),
            };
            (order, info)
        })
        .collect();
    books.sort_by(|(a_order, a), (b_order, b)| {
        let main_first = |book: &AddressBookInfo| match &book.source {
            BookSource::Account { kind, .. } => kind.is_some(),
            _ => false,
        };
        a_order
            .cmp(b_order)
            .then_with(|| main_first(a).cmp(&main_first(b)))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.id.cmp(&b.id))
    });
    books.into_iter().map(|(_, book)| book).collect()
}

/// Whether contacts in `book` can be edited: Google's My Contacts (`google:<uuid>`) of
/// an account that connected its contacts (contact sync, see
/// [`crate::synced_contacts`]).
fn book_editable(book: &str, accounts: &[Account]) -> bool {
    crate::synced_contacts::synced_book_editable(book, accounts)
}

/// The address an entry groups by.
fn group_key(email: &str) -> String {
    email.trim().to_lowercase()
}

fn non_empty(name: Option<&str>) -> Option<&str> {
    name.map(str::trim).filter(|n| !n.is_empty())
}

/// Groups `entries` into contact rows (see [`CoreHandle::contacts`]).
pub fn summarize(
    entries: &[Contact],
    books: &[AddressBookInfo],
    query: &ContactQuery,
) -> Vec<ContactSummary> {
    let book_ix: HashMap<&str, usize> = books
        .iter()
        .enumerate()
        .map(|(ix, book)| (book.id.as_str(), ix))
        .collect();
    // Entry indexes per address, in first-seen order.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::with_capacity(entries.len());
    for (ix, entry) in entries.iter().enumerate() {
        let slot = *by_key.entry(group_key(&entry.email)).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[slot].push(ix);
    }
    let needle = query.text.trim().to_lowercase();
    let rank = |entry: &Contact| {
        book_ix
            .get(entry.book.as_str())
            .map_or((u8::MAX, usize::MAX), |&ix| (books[ix].name_rank(), ix))
    };
    let mut rows: Vec<ContactSummary> = groups
        .into_iter()
        .filter_map(|mut members| {
            if let Some(book) = &query.book
                && !members.iter().any(|&ix| &entries[ix].book == book)
            {
                return None;
            }
            members.sort_by_key(|&ix| rank(&entries[ix]));
            if !needle.is_empty() {
                let found = members.iter().any(|&ix| {
                    let entry = &entries[ix];
                    entry.email.to_lowercase().contains(&needle)
                        || non_empty(entry.display_name.as_deref())
                            .is_some_and(|name| name.to_lowercase().contains(&needle))
                });
                if !found {
                    return None;
                }
            }
            // In one book's list its own entry's name comes first.
            let named = query
                .book
                .as_ref()
                .and_then(|book| {
                    members.iter().copied().find(|&ix| {
                        &entries[ix].book == book
                            && non_empty(entries[ix].display_name.as_deref()).is_some()
                    })
                })
                .or_else(|| {
                    members
                        .iter()
                        .copied()
                        .find(|&ix| non_empty(entries[ix].display_name.as_deref()).is_some())
                });
            let first = members[0];
            let source = named.unwrap_or(first);
            let mut labels: Vec<BookLabel> = Vec::new();
            for &ix in &members {
                let Some(&book) = book_ix.get(entries[ix].book.as_str()) else {
                    continue;
                };
                let label = books[book].label();
                let duplicate = labels.iter().any(|l| match (l.account, label.account) {
                    (Some((a, _)), Some((b, _))) => a == b,
                    _ => l.book == label.book,
                });
                if !duplicate {
                    labels.push(label);
                }
            }
            Some(ContactSummary {
                email: entries[source].email.trim().to_owned(),
                name: named
                    .and_then(|ix| non_empty(entries[ix].display_name.as_deref()))
                    .map(str::to_owned),
                labels,
                frequency: members
                    .iter()
                    .map(|&ix| u64::from(entries[ix].frequency))
                    .sum(),
                last_used: members.iter().filter_map(|&ix| entries[ix].last_used).max(),
            })
        })
        .collect();
    sort_contacts(&mut rows, query.sort);
    rows
}

/// What a row sorts by in name order: the name, else the address, ignoring case.
fn sort_name(row: &ContactSummary) -> String {
    row.name.as_deref().unwrap_or(&row.email).to_lowercase()
}

fn sort_contacts(rows: &mut [ContactSummary], sort: ContactSort) {
    let mut keyed: Vec<(String, usize)> = rows
        .iter()
        .enumerate()
        .map(|(ix, row)| (sort_name(row), ix))
        .collect();
    let by_name = |a: &(String, usize), b: &(String, usize)| {
        a.0.cmp(&b.0).then_with(|| {
            rows[a.1]
                .email
                .to_lowercase()
                .cmp(&rows[b.1].email.to_lowercase())
        })
    };
    match sort {
        ContactSort::Name => keyed.sort_by(by_name),
        ContactSort::LastUsed => keyed.sort_by(|a, b| {
            match (rows[a.1].last_used, rows[b.1].last_used) {
                (Some(x), Some(y)) => y.cmp(&x),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
            .then_with(|| by_name(a, b))
        }),
    }
    let order: Vec<usize> = keyed.into_iter().map(|(_, ix)| ix).collect();
    let sorted: Vec<ContactSummary> = order.iter().map(|&ix| rows[ix].clone()).collect();
    rows.clone_from_slice(&sorted);
}

/// The detail of the entries of one address (see [`CoreHandle::contact`]).
fn detail_of(
    mut entries: Vec<Contact>,
    books: &[AddressBookInfo],
    accounts: &[Account],
) -> Option<ContactDetail> {
    let position = |book: &str| books.iter().position(|b| b.id == book);
    entries.sort_by_key(|entry| position(&entry.book).unwrap_or(usize::MAX));
    let versions: Vec<ContactVersion> = entries
        .into_iter()
        .map(|entry| {
            let book = position(&entry.book).map_or_else(
                // A book the list didn't have (a write in between): describe it alone.
                || {
                    let single = Contact {
                        vcard: None,
                        ..entry.clone()
                    };
                    books_of(std::slice::from_ref(&single), accounts)
                        .into_iter()
                        .next()
                },
                |ix| Some(books[ix].clone()),
            );
            let card = entry.vcard.as_deref().map(vcard::parse).unwrap_or_default();
            let mut emails = vec![entry.email.trim().to_owned()];
            for email in card.emails {
                if !emails.iter().any(|e| e.eq_ignore_ascii_case(&email)) {
                    emails.push(email);
                }
            }
            let name = non_empty(entry.display_name.as_deref())
                .map(str::to_owned)
                .or(card.name);
            (entry, book, name, emails, card.phones, card.org, card.title)
        })
        .filter_map(|(entry, book, name, emails, phones, organization, title)| {
            Some(ContactVersion {
                id: entry.id,
                book: book?,
                name,
                emails,
                phones,
                organization,
                title,
                frequency: entry.frequency,
                last_used: entry.last_used,
            })
        })
        .collect();
    let first = versions.first()?;
    let named = versions
        .iter()
        .filter(|v| v.name.is_some())
        .min_by_key(|v| v.book.name_rank());
    let mut emails: Vec<String> = Vec::new();
    let mut phones: Vec<ContactPhone> = Vec::new();
    for version in &versions {
        for email in &version.emails {
            if !emails.iter().any(|e| e.eq_ignore_ascii_case(email)) {
                emails.push(email.clone());
            }
        }
        for phone in &version.phones {
            if !phones.iter().any(|p| p.number == phone.number) {
                phones.push(phone.clone());
            }
        }
    }
    Some(ContactDetail {
        email: named.unwrap_or(first).emails[0].clone(),
        name: named.and_then(|v| v.name.clone()),
        emails,
        phones,
        organization: versions.iter().find_map(|v| v.organization.clone()),
        title: versions.iter().find_map(|v| v.title.clone()),
        versions,
    })
}

impl CoreHandle {
    /// Every address book with its count, in the Address Book's order: each account's
    /// books (accounts by ordinal), Collected, imported books by name, then books of
    /// accounts that no longer exist. Books without entries aren't listed.
    pub fn address_books(&self) -> Result<Vec<AddressBookInfo>> {
        self.ensure_open()?;
        let reader = self.store().reader()?;
        let entries = reader.contact_list()?;
        let accounts = reader.accounts()?;
        Ok(books_of(&entries, &accounts))
    }

    /// The contact list: one row per address (case-insensitive) across books, with a
    /// label per account it is in; one book's addresses with `query.book`. Filtered by
    /// name or address and sorted as asked.
    pub fn contacts(&self, query: &ContactQuery) -> Result<Vec<ContactSummary>> {
        self.ensure_open()?;
        let reader = self.store().reader()?;
        let entries = reader.contact_list()?;
        let accounts = reader.accounts()?;
        let books = books_of(&entries, &accounts);
        Ok(summarize(&entries, &books, query))
    }

    /// Everything known about `email`: each book's version with the fields its vCard
    /// has (name, addresses, phone numbers, organization, title). `None` if no book
    /// has the address.
    pub fn contact(&self, email: &str) -> Result<Option<ContactDetail>> {
        self.ensure_open()?;
        let reader = self.store().reader()?;
        let entries = reader.contacts_by_email(email)?;
        if entries.is_empty() {
            return Ok(None);
        }
        let accounts = reader.accounts()?;
        let books = books_of(&reader.contact_list()?, &accounts);
        Ok(detail_of(entries, &books, &accounts))
    }

    /// Whether contacts in `book` can be edited in Maze: Google's My Contacts
    /// (`google:<uuid>`) of an account that connected its contacts and whose grant
    /// covers them. Collected addresses, imported books and Google's Other contacts
    /// are read-only. No network.
    pub fn contact_book_editable(&self, book: &str) -> bool {
        self.store()
            .reader()
            .and_then(|reader| reader.accounts())
            .is_ok_and(|accounts| book_editable(book, &accounts))
    }

    /// Saves a contact: writes it to the book's server, then updates the local copy
    /// from the server's answer (ADR-024; see [`crate::synced_contacts`]). A new
    /// contact has `edit.contact` `None`. Books that can't be edited answer
    /// [`ContactError::NotEditable`]; offline, a missing permission or a contact
    /// changed elsewhere are [`ContactError::Core`] with
    /// [`Error::Contacts`](crate::Error::Contacts) (a conflict carries Google's
    /// current version, which is stored too).
    pub async fn save_contact(
        &self,
        edit: ContactEdit,
    ) -> std::result::Result<ContactDetail, ContactError> {
        self.save_contact_edit(edit).await
    }
}

/// Just enough vCard (RFC 6350, and the 2.1/3.0 that Thunderbird writes) for the
/// detail pane: the formatted name, addresses, phone numbers, organization and title.
pub mod vcard {
    use super::ContactPhone;

    /// The fields read from a vCard.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct Card {
        /// `FN`.
        pub name: Option<String>,
        /// `EMAIL`, in order.
        pub emails: Vec<String>,
        /// `TEL`, in order, with the first `TYPE`.
        pub phones: Vec<ContactPhone>,
        /// The first component of `ORG`.
        pub org: Option<String>,
        /// `TITLE`.
        pub title: Option<String>,
    }

    /// Undoes vCard text escaping (`\,` `\;` `\n` `\\`).
    fn unescape(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n' | 'N') => out.push('\n'),
                    Some(other) => out.push(other),
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Splits a structured value at unescaped `;`.
    fn components(value: &str) -> Vec<String> {
        let mut parts = Vec::new();
        let mut current = String::new();
        let mut escaped = false;
        for c in value.chars() {
            if escaped {
                current.push('\\');
                current.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == ';' {
                parts.push(unescape(&current));
                current.clear();
            } else {
                current.push(c);
            }
        }
        parts.push(unescape(&current));
        parts
    }

    /// The first `TYPE` of a property's parameters (`TYPE=cell`, `TYPE="work,voice"`,
    /// vCard 2.1's bare `CELL`), lowercase, `pref` and `voice` skipped.
    fn kind(params: &[&str]) -> Option<String> {
        params
            .iter()
            .flat_map(|param| {
                let values = match param.split_once('=') {
                    Some((name, values)) if name.eq_ignore_ascii_case("type") => values,
                    Some(_) => "",
                    None => param,
                };
                values
                    .trim_matches('"')
                    .split(',')
                    .map(|v| v.trim().to_lowercase())
                    .collect::<Vec<_>>()
            })
            .find(|v| !v.is_empty() && v != "pref" && v != "voice" && v != "internet")
    }

    /// Reads the fields of `text`. Anything it doesn't understand is skipped.
    pub fn parse(text: &str) -> Card {
        // Unfold: a line starting with a space or tab continues the previous one.
        let mut lines: Vec<String> = Vec::new();
        for raw in text.split('\n') {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            match line.strip_prefix([' ', '\t']) {
                Some(rest) if !lines.is_empty() => {
                    if let Some(last) = lines.last_mut() {
                        last.push_str(rest);
                    }
                }
                _ => lines.push(line.to_owned()),
            }
        }
        let mut card = Card::default();
        for line in &lines {
            let Some((head, value)) = line.split_once(':') else {
                continue;
            };
            let mut params = head.split(';');
            let name = params.next().unwrap_or_default();
            // Apple's grouping prefix: `item1.EMAIL`.
            let name = name.rsplit('.').next().unwrap_or(name).to_ascii_uppercase();
            let params: Vec<&str> = params.collect();
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            match name.as_str() {
                "FN" if card.name.is_none() => card.name = Some(unescape(value)),
                "EMAIL" => {
                    let email = unescape(value);
                    if !card.emails.iter().any(|e| e.eq_ignore_ascii_case(&email)) {
                        card.emails.push(email);
                    }
                }
                "TEL" => {
                    let number = unescape(value);
                    let number = number.strip_prefix("tel:").unwrap_or(&number).to_owned();
                    card.phones.push(ContactPhone {
                        kind: kind(&params),
                        number,
                    });
                }
                "ORG" if card.org.is_none() => {
                    card.org = components(value)
                        .into_iter()
                        .map(|c| c.trim().to_owned())
                        .find(|c| !c.is_empty());
                }
                "TITLE" if card.title.is_none() => card.title = Some(unescape(value)),
                _ => {}
            }
        }
        card
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "3f2b8c1e-4d5a-4b6c-8d7e-9f0a1b2c3d4e";

    #[test]
    fn book_ids_say_where_a_book_comes_from() {
        assert_eq!(parse_book("collected"), ParsedBook::Collected);
        let uuid = Uuid::parse_str(UUID).unwrap();
        assert_eq!(
            parse_book(&format!("google:{UUID}")),
            ParsedBook::Provider {
                provider: "google",
                kind: None,
                account: uuid
            }
        );
        assert_eq!(
            parse_book(&format!("google-other:{UUID}")),
            ParsedBook::Provider {
                provider: "google",
                kind: Some("other"),
                account: uuid
            }
        );
        // Imported books are named by their id, colons and all.
        for named in [
            "Personal Address Book",
            "thunderbird:abook",
            "Work: Team",
            "google:not-a-uuid",
        ] {
            assert_eq!(parse_book(named), ParsedBook::Named(named));
        }
        assert_eq!(provider_book_name("google", None), "My Contacts");
        assert_eq!(
            provider_book_name("google", Some("other")),
            "Other Contacts"
        );
        assert_eq!(provider_book_name("microsoft", None), "Contacts");
    }

    #[test]
    fn reads_vcards_of_every_version() {
        let card = vcard::parse(
            "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Anna Example\r\nN:Example;Anna;;;\r\n\
             EMAIL;PREF=1:anna@example.com\r\nitem1.EMAIL;TYPE=work:anna@work.example\r\n\
             TEL;TYPE=\"cell,voice\":+43 660 123\r\n 4567\r\nTEL;VALUE=uri:tel:+1-555-0100\r\n\
             ORG:Example GmbH;Research\r\nTITLE:Head of Research\\, Vienna\r\nEND:VCARD\r\n",
        );
        assert_eq!(card.name.as_deref(), Some("Anna Example"));
        assert_eq!(card.emails, ["anna@example.com", "anna@work.example"]);
        assert_eq!(
            card.phones,
            [
                ContactPhone {
                    kind: Some("cell".into()),
                    number: "+43 660 1234567".into()
                },
                ContactPhone {
                    kind: None,
                    number: "+1-555-0100".into()
                }
            ]
        );
        assert_eq!(card.org.as_deref(), Some("Example GmbH"));
        assert_eq!(card.title.as_deref(), Some("Head of Research, Vienna"));
        // vCard 2.1 (Thunderbird): bare type parameters.
        let old = vcard::parse("BEGIN:VCARD\nVERSION:2.1\nTEL;CELL:0660 1\nEND:VCARD\n");
        assert_eq!(old.phones[0].kind.as_deref(), Some("cell"));
        assert_eq!(vcard::parse("garbage"), vcard::Card::default());
    }
}
