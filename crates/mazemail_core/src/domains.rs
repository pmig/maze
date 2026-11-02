//! Domains (PLAN-contacts §3b): every registrable domain in your contacts and mail,
//! with how many contacts and messages it has, its contacts, the search for its mail
//! and its favicon (fetched from the domain itself, see [`crate::favicon`]).
//!
//! The index lives in the store (`store::Queries::domain_stats`): message counts are
//! kept up to date as mail is stored and deleted, older mail is indexed once in the
//! background after the upgrade ([`CoreHandle::open`] starts it), and contact counts
//! are derived from the address books on every call. [`CoreEventBatch::domains`]
//! (crate::CoreEventBatch::domains) says when to read them again.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use mazemail_mime::domain::{is_personal, registrable_domain, to_unicode};
use settings::FaviconPolicy;
use store::{FaviconState, Queries as _};

use crate::{CoreHandle, Result};

/// Whether a domain is an organization or a free-mail provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DomainKind {
    /// A company, school, project, …
    Organization,
    /// A free-mail or consumer provider (`gmail.com`, `outlook.com`, `web.de`, …):
    /// its addresses belong to unrelated people.
    Personal,
}

/// Which domains [`CoreHandle::domains`] lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomainFilter {
    /// Only names containing this text (case-insensitive, Unicode or punycode).
    pub text: Option<String>,
    /// Only this kind (`None`: both).
    pub kind: Option<DomainKind>,
}

/// What [`CoreHandle::domains`] sorts by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DomainSortField {
    /// The name (Unicode form), A to Z.
    #[default]
    Name,
    /// Number of contacts.
    Contacts,
    /// Number of messages.
    Messages,
    /// The newest message.
    LastSeen,
}

/// The order of [`CoreHandle::domains`]. Ties go by name, A to Z.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct DomainSort {
    /// The column.
    pub field: DomainSortField,
    /// Largest, newest or Z first.
    pub descending: bool,
}

/// One domain of the Domains list.
#[derive(Clone, PartialEq, Eq)]
pub struct DomainInfo {
    /// The registrable domain, lowercase ASCII (punycode for IDN): the key for
    /// [`CoreHandle::domain_contacts`], [`CoreHandle::domain_favicon`] and
    /// [`domain_mail_query`].
    pub name: String,
    /// The name to show (IDN decoded: `bücher.de`).
    pub display_name: String,
    /// Organization or free-mail provider.
    pub kind: DomainKind,
    /// Distinct contact addresses there (every book, addresses you wrote to included).
    pub contact_count: u64,
    /// Messages with an address there (From, To or Cc; your own addresses aside).
    pub message_count: u64,
    /// Messages from an address there.
    pub received_count: u64,
    /// Messages you sent to an address there.
    pub sent_count: u64,
    /// Date of the newest message (Unix seconds); `None` for a contact's domain
    /// without mail.
    pub last_seen: Option<i64>,
    /// Whether a favicon is stored (and the `favicons` setting isn't `off`).
    pub has_favicon: bool,
}

impl fmt::Debug for DomainInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DomainInfo")
            .field("kind", &self.kind)
            .field("contact_count", &self.contact_count)
            .field("message_count", &self.message_count)
            .field("last_seen", &self.last_seen)
            .field("has_favicon", &self.has_favicon)
            .finish_non_exhaustive()
    }
}

/// A contact at a domain: one address, with every book it is in.
#[derive(Clone, PartialEq, Eq)]
pub struct DomainContact {
    /// The address, as stored (first spelling seen).
    pub email: String,
    /// The name, from an address book before the collected addresses.
    pub display_name: Option<String>,
    /// The books (`collected`, `google:<account uuid>`, …), in the order stored.
    pub books: Vec<String>,
}

impl fmt::Debug for DomainContact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DomainContact")
            .field("books", &self.books)
            .finish_non_exhaustive()
    }
}

/// The search query for a domain's mail ("Show Mail"): `from:@northwind.example`
/// finds mail from any address there or at a subdomain (the search matches the
/// address's words in order). `name` is a registrable domain as listed.
pub fn domain_mail_query(name: &str) -> String {
    format!("from:@{name}")
}

/// The registrable domain for a name the caller passed (any case, Unicode or a
/// subdomain); `None` if it has none.
fn key(name: &str) -> Option<String> {
    registrable_domain(name)
}

fn compare(a: &DomainInfo, b: &DomainInfo, sort: DomainSort) -> Ordering {
    let by_name = || a.display_name.cmp(&b.display_name);
    let primary = match sort.field {
        DomainSortField::Name => by_name(),
        DomainSortField::Contacts => a.contact_count.cmp(&b.contact_count),
        DomainSortField::Messages => a.message_count.cmp(&b.message_count),
        DomainSortField::LastSeen => a.last_seen.cmp(&b.last_seen),
    };
    let primary = if sort.descending {
        primary.reverse()
    } else {
        primary
    };
    primary.then_with(by_name)
}

impl CoreHandle {
    /// The domains of your contacts and mail that match `filter`, in `sort`'s order.
    /// Reads the store (no network); the contact counts scan the address books, so
    /// call it off the UI thread for very large ones.
    pub fn domains(&self, filter: &DomainFilter, sort: DomainSort) -> Result<Vec<DomainInfo>> {
        self.ensure_open()?;
        let reader = self.store().reader()?;
        let mut contacts = reader.domain_contact_counts()?;
        let favicons_on = self.favicons().policy() != FaviconPolicy::Off;
        let text = filter
            .text
            .as_deref()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty());
        let mut out = Vec::new();
        let mut push = |name: String, contact_count: u64, stats: Option<&store::DomainStats>| {
            let display_name = to_unicode(&name);
            if let Some(text) = &text
                && !name.contains(text.as_str())
                && !display_name.to_lowercase().contains(text.as_str())
            {
                return;
            }
            let kind = if is_personal(&name) {
                DomainKind::Personal
            } else {
                DomainKind::Organization
            };
            if filter.kind.is_some_and(|k| k != kind) {
                return;
            }
            out.push(DomainInfo {
                display_name,
                kind,
                contact_count,
                message_count: stats.map_or(0, |s| s.message_count),
                received_count: stats.map_or(0, |s| s.from_count),
                sent_count: stats.map_or(0, |s| s.sent_count),
                last_seen: stats.and_then(|s| s.last_seen),
                has_favicon: favicons_on
                    && stats.is_some_and(|s| matches!(s.favicon, FaviconState::Stored { .. })),
                name,
            });
        };
        for stats in reader.domain_stats()? {
            let contact_count = contacts.remove(&stats.name).unwrap_or(0);
            if stats.message_count == 0 && contact_count == 0 {
                continue;
            }
            push(stats.name.clone(), contact_count, Some(&stats));
        }
        for (name, contact_count) in contacts {
            push(name, contact_count, None);
        }
        out.sort_by(|a, b| compare(a, b, sort));
        Ok(out)
    }

    /// The contacts at a domain (every book), one entry per address, by address.
    pub fn domain_contacts(&self, name: &str) -> Result<Vec<DomainContact>> {
        self.ensure_open()?;
        let Some(name) = key(name) else {
            return Ok(Vec::new());
        };
        let reader = self.store().reader()?;
        // Per address: the entry, and the first name from an address book and from the
        // collected addresses.
        let mut by_email: BTreeMap<String, (DomainContact, Option<String>, Option<String>)> =
            BTreeMap::new();
        for contact in reader.domain_contacts(&name)? {
            let (entry, book_name, collected_name) = by_email
                .entry(contact.email.trim().to_lowercase())
                .or_insert_with(|| {
                    (
                        DomainContact {
                            email: contact.email.trim().to_owned(),
                            display_name: None,
                            books: Vec::new(),
                        },
                        None,
                        None,
                    )
                });
            let display_name = contact.display_name.filter(|n| !n.trim().is_empty());
            let slot = if contact.book == "collected" {
                collected_name
            } else {
                book_name
            };
            if slot.is_none() {
                *slot = display_name;
            }
            if !entry.books.contains(&contact.book) {
                entry.books.push(contact.book);
            }
        }
        Ok(by_email
            .into_values()
            .map(|(mut entry, book_name, collected_name)| {
                entry.display_name = book_name.or(collected_name);
                entry
            })
            .collect())
    }

    /// The favicon of a domain as a 64×64 PNG, when one is stored; `None` otherwise or
    /// with the `favicons` setting `off`. Never waits for the network: a missing or old
    /// favicon is fetched in the background if the setting allows it, and
    /// [`CoreEventBatch::domains`](crate::CoreEventBatch::domains) says when it
    /// arrived.
    pub fn domain_favicon(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.ensure_open()?;
        let Some(name) = key(name) else {
            return Ok(None);
        };
        self.favicons().favicon(&name)
    }
}
