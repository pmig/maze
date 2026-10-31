//! The domain index (`migrations/main/0014_domains.sql`, PLAN-contacts §3b): the
//! registrable domain of every address in mail, with message counts kept by triggers,
//! contact counts derived from the `contact` table, and each domain's favicon state.
//!
//! - New messages are indexed when they are stored ([`WriteTx::upsert_message`]);
//!   messages from before the migration by [`WriteTx::backfill_domains`], in batches,
//!   idempotently.
//! - Your own identity addresses are left out, so your own domain only shows the other
//!   people there. A message from one of your identities marks its recipients' domains
//!   as "sent to" (`sent_count`), which the favicon privacy default looks at.
//! - Domain names reveal who you correspond with: they are never logged.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use mazemail_mime::domain::address_domain;
use mazemail_types::{Address, BlobId, MessageId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::UnixTime;
use crate::{Result, WriteTx};

/// `message_domain.roles`: an address in From.
pub(crate) const ROLE_FROM: i64 = 1;
/// `message_domain.roles`: an address in To or Cc.
pub(crate) const ROLE_RECIPIENT: i64 = 2;
/// `message_domain.roles`: the message was sent by you to an address there.
pub(crate) const ROLE_SENT: i64 = 4;

/// The cache kind favicons are stored under ([`crate::BlobStore::cache_put`]).
pub const FAVICON_CACHE_KIND: &str = "favicon";
/// The version of the stored favicon format (64×64 PNG).
pub const FAVICON_CACHE_VERSION: u32 = 1;

/// What is known about a domain's favicon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaviconState {
    /// Never fetched.
    Unknown,
    /// Stored in the encrypted cache under `key` ([`FAVICON_CACHE_KIND`]).
    Stored {
        /// The random cache id.
        key: BlobId,
        /// When it was fetched.
        checked: UnixTime,
    },
    /// The domain had none (or it couldn't be fetched) at `checked`.
    Missing {
        /// When it was tried.
        checked: UnixTime,
    },
}

impl FaviconState {
    /// When the favicon was last fetched or tried.
    pub fn checked(&self) -> Option<UnixTime> {
        match self {
            Self::Unknown => None,
            Self::Stored { checked, .. } | Self::Missing { checked } => Some(*checked),
        }
    }
}

/// One row of the domain index.
#[derive(Clone, PartialEq, Eq)]
pub struct DomainStats {
    /// The registrable domain, lowercase ASCII (punycode for IDN).
    pub name: String,
    /// Messages with an address there (From, To or Cc; your own addresses aside).
    pub message_count: u64,
    /// Messages from an address there.
    pub from_count: u64,
    /// Messages you sent to an address there.
    pub sent_count: u64,
    /// Date of the newest message seen (not lowered when messages are deleted).
    pub last_seen: Option<UnixTime>,
    /// The favicon.
    pub favicon: FaviconState,
}

impl fmt::Debug for DomainStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DomainStats")
            .field("message_count", &self.message_count)
            .field("from_count", &self.from_count)
            .field("sent_count", &self.sent_count)
            .field("last_seen", &self.last_seen)
            .field("favicon", &self.favicon)
            .finish_non_exhaustive()
    }
}

/// An address book entry (any book, `collected` included) at a domain.
#[derive(Clone, PartialEq, Eq)]
pub struct DomainContact {
    /// The registrable domain of `email`.
    pub domain: String,
    /// The address book (`collected`, `google:<account uuid>`, …).
    pub book: String,
    /// The contact's name.
    pub display_name: Option<String>,
    /// The address, as stored.
    pub email: String,
}

impl fmt::Debug for DomainContact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DomainContact")
            .field("book", &self.book)
            .finish_non_exhaustive()
    }
}

/// How far [`WriteTx::backfill_domains`] got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainBackfill {
    /// Messages indexed by this call.
    pub indexed: usize,
    /// Whether every message from before the domain index is indexed now.
    pub done: bool,
}

const STATS_COLUMNS: &str = "name, message_count, from_count, sent_count, last_seen, \
     favicon_state, favicon_checked, favicon_key";

fn stats_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DomainStats> {
    let count = |ix: usize| -> rusqlite::Result<u64> {
        Ok(u64::try_from(row.get::<_, i64>(ix)?).unwrap_or(0))
    };
    let state: i64 = row.get(5)?;
    let checked: Option<i64> = row.get(6)?;
    let key: Option<Vec<u8>> = row.get(7)?;
    let favicon = match (state, checked, key) {
        (1, Some(checked), Some(key)) => match <[u8; 32]>::try_from(key.as_slice()) {
            Ok(key) => FaviconState::Stored {
                key: BlobId(key),
                checked,
            },
            Err(_) => FaviconState::Unknown,
        },
        (2, Some(checked), _) => FaviconState::Missing { checked },
        _ => FaviconState::Unknown,
    };
    Ok(DomainStats {
        name: row.get(0)?,
        message_count: count(1)?,
        from_count: count(2)?,
        sent_count: count(3)?,
        last_seen: row.get(4)?,
        favicon,
    })
}

/// Every indexed domain (also those whose messages are all gone), by name.
pub(crate) fn domain_stats(conn: &Connection) -> Result<Vec<DomainStats>> {
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {STATS_COLUMNS} FROM domain ORDER BY name"))?;
    Ok(stmt
        .query_map([], stats_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// One domain's row.
pub(crate) fn domain_stat(conn: &Connection, name: &str) -> Result<Option<DomainStats>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {STATS_COLUMNS} FROM domain WHERE name = ?1"
        ))?
        .query_row([name], stats_row)
        .optional()?)
}

/// Every contact with a registrable domain, in id order.
pub(crate) fn contacts_with_domains(conn: &Connection) -> Result<Vec<DomainContact>> {
    let mut stmt =
        conn.prepare_cached("SELECT book, display_name, email FROM contact ORDER BY id")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (book, display_name, email) = row?;
        if let Some(domain) = address_domain(&email) {
            out.push(DomainContact {
                domain,
                book,
                display_name,
                email,
            });
        }
    }
    Ok(out)
}

/// Distinct contact addresses (case-insensitive, across all books) per domain.
pub(crate) fn domain_contact_counts(conn: &Connection) -> Result<HashMap<String, u64>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut counts: HashMap<String, u64> = HashMap::new();
    for contact in contacts_with_domains(conn)? {
        if seen.insert(contact.email.trim().to_lowercase()) {
            *counts.entry(contact.domain).or_default() += 1;
        }
    }
    Ok(counts)
}

/// The contacts at one domain.
pub(crate) fn domain_contacts(conn: &Connection, name: &str) -> Result<Vec<DomainContact>> {
    let mut contacts = contacts_with_domains(conn)?;
    contacts.retain(|c| c.domain == name);
    Ok(contacts)
}

/// Whether messages from before the domain index still wait for
/// [`WriteTx::backfill_domains`].
pub(crate) fn domain_backfill_pending(conn: &Connection) -> Result<bool> {
    let pending: Option<bool> = conn
        .prepare_cached("SELECT next_message <= last_message FROM domain_backfill WHERE id = 1")?
        .query_row([], |row| row.get(0))
        .optional()?;
    Ok(pending.unwrap_or(false))
}

/// Your identity addresses, lowercase.
fn identity_emails(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare_cached("SELECT lower(trim(email)) FROM identity")?;
    Ok(stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// The domains a message involves, with their roles.
pub(crate) fn message_domains(
    from: Option<&str>,
    to: &[Address],
    cc: &[Address],
    identities: &HashSet<String>,
) -> BTreeMap<String, i64> {
    let mine = |email: &str| identities.contains(&email.trim().to_lowercase());
    let sent = from.is_some_and(mine);
    let mut domains: BTreeMap<String, i64> = BTreeMap::new();
    if let Some(from) = from
        && !sent
        && let Some(domain) = address_domain(from)
    {
        *domains.entry(domain).or_default() |= ROLE_FROM;
    }
    let recipient_roles = if sent {
        ROLE_RECIPIENT | ROLE_SENT
    } else {
        ROLE_RECIPIENT
    };
    for address in to.iter().chain(cc) {
        if mine(&address.email) {
            continue;
        }
        if let Some(domain) = address_domain(&address.email) {
            *domains.entry(domain).or_default() |= recipient_roles;
        }
    }
    domains
}

/// Adds the `message_domain` rows of one message (existing rows stay as they are).
/// Returns how many were added.
fn insert_message_domains(
    conn: &Connection,
    message: i64,
    domains: &BTreeMap<String, i64>,
) -> Result<usize> {
    let mut added = 0;
    for (name, roles) in domains {
        conn.prepare_cached("INSERT INTO domain (name) VALUES (?1) ON CONFLICT (name) DO NOTHING")?
            .execute([name])?;
        let id: i64 = conn
            .prepare_cached("SELECT id FROM domain WHERE name = ?1")?
            .query_row([name], |row| row.get(0))?;
        added += conn
            .prepare_cached(
                "INSERT OR IGNORE INTO message_domain (domain_id, message_id, roles)
                 VALUES (?1, ?2, ?3)",
            )?
            .execute(params![id, message, roles])?;
    }
    Ok(added)
}

/// `id, from_addr, to_json, cc_json` of a message to backfill.
type BackfillRow = (i64, Option<String>, Option<String>, Option<String>);

/// A random id for a favicon's cache entry (so file names don't tell which domain it
/// belongs to).
fn random_cache_key() -> BlobId {
    let mut key = [0u8; 32];
    key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    BlobId(key)
}

impl WriteTx<'_> {
    /// Indexes the domains of a newly stored message.
    pub(crate) fn index_message_domains(
        &mut self,
        message: MessageId,
        from: Option<&str>,
        to: &[Address],
        cc: &[Address],
    ) -> Result<()> {
        if from.is_none() && to.is_empty() && cc.is_empty() {
            return Ok(());
        }
        let identities = identity_emails(self.conn)?;
        let domains = message_domains(from, to, cc, &identities);
        if insert_message_domains(self.conn, message.0, &domains)? > 0 {
            self.changes.domains = true;
        }
        Ok(())
    }

    /// Indexes up to `limit` messages stored before the domain index existed, in id
    /// order, and remembers how far it got. Safe to run again (rows that exist are
    /// kept); the core calls it in the background until it reports `done`.
    pub fn backfill_domains(&mut self, limit: usize) -> Result<DomainBackfill> {
        let range: Option<(i64, i64)> = self
            .conn
            .prepare_cached("SELECT next_message, last_message FROM domain_backfill WHERE id = 1")?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        let Some((next, last)) = range else {
            return Ok(DomainBackfill {
                indexed: 0,
                done: true,
            });
        };
        if next > last {
            return Ok(DomainBackfill {
                indexed: 0,
                done: true,
            });
        }
        let limit = limit.max(1);
        let rows: Vec<BackfillRow> = self
            .conn
            .prepare_cached(
                "SELECT id, from_addr, to_json, cc_json FROM message
                 WHERE id >= ?1 AND id <= ?2 ORDER BY id LIMIT ?3",
            )?
            .query_map(params![next, last, limit as i64], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let identities = identity_emails(self.conn)?;
        let mut added = 0;
        for (id, from, to, cc) in &rows {
            // Unreadable address lists (never written by this store) are skipped.
            let parse = |json: &Option<String>| -> Vec<Address> {
                json.as_deref()
                    .and_then(|json| serde_json::from_str(json).ok())
                    .unwrap_or_default()
            };
            let domains = message_domains(from.as_deref(), &parse(to), &parse(cc), &identities);
            added += insert_message_domains(self.conn, *id, &domains)?;
        }
        let new_next = if rows.len() < limit {
            last + 1
        } else {
            rows.last().map_or(last + 1, |row| row.0 + 1)
        };
        self.conn
            .prepare_cached("UPDATE domain_backfill SET next_message = ?1 WHERE id = 1")?
            .execute([new_next])?;
        if added > 0 {
            self.changes.domains = true;
        }
        Ok(DomainBackfill {
            indexed: rows.len(),
            done: new_next > last,
        })
    }

    /// Stores a domain's favicon (a small PNG) in the encrypted cache and marks it
    /// fetched now. The domain gets an index row if it has none (a contact's domain
    /// without mail).
    pub fn store_domain_favicon(&mut self, name: &str, png: &[u8]) -> Result<()> {
        let existing = domain_stat(self.conn, name)?;
        let key = match existing.map(|stats| stats.favicon) {
            Some(FaviconState::Stored { key, .. }) => key,
            _ => random_cache_key(),
        };
        self.blobs
            .cache_put(&key, FAVICON_CACHE_KIND, FAVICON_CACHE_VERSION, png)?;
        self.conn
            .prepare_cached(
                "INSERT INTO domain (name, favicon_state, favicon_checked, favicon_key)
                 VALUES (?1, 1, ?2, ?3)
                 ON CONFLICT (name) DO UPDATE SET favicon_state = 1,
                   favicon_checked = excluded.favicon_checked,
                   favicon_key = excluded.favicon_key",
            )?
            .execute(params![name, self.now, key.0.as_slice()])?;
        self.changes.domains = true;
        Ok(())
    }

    /// Remembers that a domain has no favicon (or it couldn't be fetched) as of now.
    /// A stored icon stays in the cache until it is replaced.
    pub fn mark_domain_favicon_missing(&mut self, name: &str) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO domain (name, favicon_state, favicon_checked) VALUES (?1, 2, ?2)
                 ON CONFLICT (name) DO UPDATE SET favicon_state = 2,
                   favicon_checked = excluded.favicon_checked",
            )?
            .execute(params![name, self.now])?;
        self.changes.domains = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mazemail_types::{AccountId, Flags, FolderId, FolderRole};

    use super::*;
    use crate::{
        AccountKind, IdentityInput, NewAccount, NewFolder, NewLocation, NewMessage, Queries, Store,
        StoreConfig, ThreadAssignment,
    };

    fn open(dir: &std::path::Path) -> Store {
        let mut config = StoreConfig::new(dir);
        config.gc_interval = None;
        let vault = vault::Vault::from_master_key(vault::MasterKey::from_bytes(
            vault::KeyId::FIRST,
            [3; vault::KEY_LEN],
        ));
        Store::open(&config, &vault).unwrap()
    }

    /// An account with an Inbox and the identity `me@home.example`.
    fn setup(store: &Store) -> (AccountId, FolderId) {
        store
            .write_blocking(|w| {
                let account = w.create_account(&NewAccount {
                    kind: AccountKind::Imap,
                    name: "Work".to_owned(),
                    config_json: "{}".to_owned(),
                    ordinal: 0,
                })?;
                let inbox = w.create_folder(
                    &NewFolder::new(account, "INBOX", "Inbox").with_role(FolderRole::Inbox),
                )?;
                w.create_identity(
                    account,
                    &IdentityInput {
                        email: "Me@home.example".to_owned(),
                        ..IdentityInput::default()
                    },
                )?;
                Ok((account, inbox))
            })
            .unwrap()
    }

    fn mail(
        account: AccountId,
        folder: FolderId,
        uid: u32,
        date: i64,
        from: &str,
        to: &[&str],
        cc: &[&str],
    ) -> NewMessage {
        let mut m = NewMessage::new(account, date);
        m.rfc_message_id = Some(format!("m{uid}@test"));
        m.from = Some(Address::new(None::<String>, from));
        m.to = to
            .iter()
            .map(|a| Address::new(None::<String>, *a))
            .collect();
        m.cc = cc
            .iter()
            .map(|a| Address::new(None::<String>, *a))
            .collect();
        m.thread = ThreadAssignment::New;
        m.locations = vec![NewLocation::new(folder, Some(uid), Flags::empty())];
        m
    }

    /// `(name, messages, from, sent, last seen)` of every domain with messages.
    fn counts(store: &Store) -> Vec<(String, u64, u64, u64, Option<i64>)> {
        store
            .reader()
            .unwrap()
            .domain_stats()
            .unwrap()
            .into_iter()
            .filter(|d| d.message_count > 0)
            .map(|d| {
                (
                    d.name,
                    d.message_count,
                    d.from_count,
                    d.sent_count,
                    d.last_seen,
                )
            })
            .collect()
    }

    fn sample(account: AccountId, inbox: FolderId) -> Vec<NewMessage> {
        vec![
            mail(
                account,
                inbox,
                1,
                100,
                "anna@mail.eu.northwind.example",
                &["me@home.example", "bob@northwind.example"],
                &["colleague@home.example"],
            ),
            mail(
                account,
                inbox,
                2,
                300,
                "ME@home.example",
                &["bob@northwind.example"],
                &["carol@gmail.com", "root@[192.0.2.1]"],
            ),
            mail(
                account,
                inbox,
                3,
                200,
                "news@shop.example.co.uk",
                &["me@home.example"],
                &[],
            ),
        ]
    }

    fn expected() -> Vec<(String, u64, u64, u64, Option<i64>)> {
        vec![
            ("example.co.uk".to_owned(), 1, 1, 0, Some(200)),
            ("gmail.com".to_owned(), 1, 0, 1, Some(300)),
            ("home.example".to_owned(), 1, 0, 0, Some(100)),
            ("northwind.example".to_owned(), 2, 1, 1, Some(300)),
        ]
    }

    #[test]
    fn new_messages_are_indexed_and_deletes_count_down() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open(tmp.path());
        let (account, inbox) = setup(&store);
        let messages = sample(account, inbox);
        let ids = store
            .write_blocking(move |w| {
                let mut ids = Vec::new();
                for m in &messages {
                    ids.push(w.upsert_message(m)?.message);
                }
                // Stored again (a re-sync): counted once.
                w.upsert_message(&messages[0])?;
                Ok(ids)
            })
            .unwrap();
        assert_eq!(counts(&store), expected());
        assert!(!store.reader().unwrap().domain_backfill_pending().unwrap());

        let first = ids[0];
        store
            .write_blocking(move |w| w.delete_message(first))
            .unwrap();
        assert_eq!(
            counts(&store),
            vec![
                ("example.co.uk".to_owned(), 1, 1, 0, Some(200)),
                ("gmail.com".to_owned(), 1, 0, 1, Some(300)),
                ("northwind.example".to_owned(), 1, 0, 1, Some(300)),
            ]
        );
        // Removing the account takes the rest through the foreign keys.
        store
            .write_blocking(move |w| w.delete_account(account))
            .unwrap();
        assert_eq!(counts(&store), vec![]);
    }

    #[test]
    fn backfill_indexes_older_messages_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open(tmp.path());
        let (account, inbox) = setup(&store);
        let messages = sample(account, inbox);
        store
            .write_blocking(move |w| {
                for m in &messages {
                    w.upsert_message(m)?;
                }
                // As if these messages were stored before the domain index existed, with
                // the first one indexed already (a backfill interrupted after its commit
                // is the same situation).
                w.conn.execute_batch(
                    "DELETE FROM message_domain WHERE message_id <> 1;
                     DELETE FROM domain WHERE message_count = 0;
                     UPDATE domain_backfill
                       SET next_message = 1, last_message = (SELECT max(id) FROM message);",
                )?;
                Ok(())
            })
            .unwrap();
        assert!(store.reader().unwrap().domain_backfill_pending().unwrap());
        let mut rounds = Vec::new();
        loop {
            let progress = store.write_blocking(|w| w.backfill_domains(2)).unwrap();
            rounds.push(progress);
            if progress.done {
                break;
            }
        }
        assert_eq!(
            rounds,
            vec![
                DomainBackfill {
                    indexed: 2,
                    done: false
                },
                DomainBackfill {
                    indexed: 1,
                    done: true
                }
            ]
        );
        assert_eq!(counts(&store), expected());
        assert!(!store.reader().unwrap().domain_backfill_pending().unwrap());
        // Done stays done.
        let again = store.write_blocking(|w| w.backfill_domains(2)).unwrap();
        assert_eq!(
            again,
            DomainBackfill {
                indexed: 0,
                done: true
            }
        );
        assert_eq!(counts(&store), expected());
    }

    #[test]
    fn contacts_count_per_domain_across_books() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open(tmp.path());
        store
            .write_blocking(|w| {
                w.bump_contact("Anna@Northwind.example", Some("Anna"), 10)?;
                w.upsert_contact("google:1", "anna@northwind.example", Some("Anna N"), None)?;
                w.upsert_contact("google:1", "bob@sales.northwind.example", None, None)?;
                w.upsert_contact("google:1", "carol@gmail.com", None, None)?;
                w.upsert_contact("google:1", "printer@localhost", None, None)?;
                Ok(())
            })
            .unwrap();
        let reader = store.reader().unwrap();
        let counts = reader.domain_contact_counts().unwrap();
        assert_eq!(counts.len(), 2);
        assert_eq!(counts["northwind.example"], 2);
        assert_eq!(counts["gmail.com"], 1);
        let contacts = reader.domain_contacts("northwind.example").unwrap();
        let summary: Vec<(&str, &str)> = contacts
            .iter()
            .map(|c| (c.book.as_str(), c.email.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("collected", "Anna@Northwind.example"),
                ("google:1", "anna@northwind.example"),
                ("google:1", "bob@sales.northwind.example"),
            ]
        );
        assert!(
            reader
                .domain_contacts("nowhere.example")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn favicons_are_cached_encrypted_and_misses_remembered() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open(tmp.path());
        store
            .write_blocking(|w| {
                w.mark_domain_favicon_missing("northwind.example")?;
                w.store_domain_favicon("shop.example", b"\x89PNG fake")?;
                Ok(())
            })
            .unwrap();
        let reader = store.reader().unwrap();
        let missing = reader.domain_stat("northwind.example").unwrap().unwrap();
        assert!(matches!(missing.favicon, FaviconState::Missing { .. }));
        assert_eq!(missing.message_count, 0);
        let stored = reader.domain_stat("shop.example").unwrap().unwrap();
        let FaviconState::Stored { key, .. } = stored.favicon else {
            panic!("not stored: {stored:?}");
        };
        assert_eq!(
            store
                .blobs()
                .cache_get(&key, FAVICON_CACHE_KIND, FAVICON_CACHE_VERSION)
                .as_deref(),
            Some(&b"\x89PNG fake"[..])
        );
        // Replacing keeps the cache id.
        store
            .write_blocking(|w| w.store_domain_favicon("shop.example", b"new"))
            .unwrap();
        let stored = reader.domain_stat("shop.example").unwrap().unwrap();
        assert!(matches!(stored.favicon, FaviconState::Stored { key: k, .. } if k == key));
    }

    fn addr(email: &str) -> Address {
        Address {
            name: None,
            email: email.to_owned(),
        }
    }

    #[test]
    fn roles_leave_out_your_own_addresses() {
        let me: HashSet<String> = ["me@home.example".to_owned()].into();
        let received = message_domains(
            Some("Anna@Mail.Northwind.example"),
            &[addr("me@home.example"), addr("bob@northwind.example")],
            &[addr("carol@home.example"), addr("x@[192.0.2.1]")],
            &me,
        );
        assert_eq!(
            received,
            BTreeMap::from([
                ("home.example".to_owned(), ROLE_RECIPIENT),
                ("northwind.example".to_owned(), ROLE_FROM | ROLE_RECIPIENT),
            ])
        );
        let sent = message_domains(
            Some("ME@home.example"),
            &[addr("anna@northwind.example")],
            &[],
            &me,
        );
        assert_eq!(
            sent,
            BTreeMap::from([("northwind.example".to_owned(), ROLE_RECIPIENT | ROLE_SENT)])
        );
    }
}
