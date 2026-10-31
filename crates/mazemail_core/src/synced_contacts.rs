//! Contacts synced from Google (ADR-024): connecting an account's contacts, the
//! write-through create and edit, details for the Address Book, and photos.
//!
//! # Books
//!
//! Synced contacts are rows of the `contact` table with `book` `google:<account
//! uuid>` (My Contacts, editable) and `google-other:<account uuid>` (Google's "Other
//! contacts", read-only), one row per email address; [`SyncedContact`] has the whole
//! person (parsed from the stored vCard) with its server id and etag.
//!
//! # Connecting
//!
//! Accounts sign in for mail only. [`CoreHandle::connect_google_contacts`] asks
//! Google for the contacts scopes with an incremental consent in the browser; the
//! answer is kept in the account's config (`AccountConfig.extra.contacts`, see
//! [`ContactsOptIn`]) and the first sync starts. [`CoreHandle::disconnect_contacts`]
//! turns it off and deletes the local copy. `contacts.sync` (and an account's
//! `contacts_sync` override) pause syncing without deleting anything.
//!
//! # Writing
//!
//! [`CoreHandle::create_contact`] and [`CoreHandle::update_contact`] (or
//! [`CoreHandle::save_contact`], the Address Book's Save) call Google first and store only Google's
//! answer: there is no local-only state and no offline queue. Offline is
//! [`ContactError::Offline`]; a contact changed elsewhere is
//! [`ContactError::Conflict`] with the server's current version, which is stored too,
//! so the UI can show it and let the user edit again.

use std::fmt;

use ::contacts::{
    Book, BookKind, ContactError, ContactFields, ContactSource as _, Labeled, Name, Organization,
    Photo,
};
use auth::{AuthorizeOptions, Capabilities, Capability};
use mazemail_types::AccountId;
use serde::{Deserialize, Serialize};
use store::{Account, Queries as _, StoredServerContact};

use crate::accounts::{AccountConfig, AccountErrorKind};
use crate::contacts::{ContactDetail, ContactEdit};
use crate::handle::CoreHandle;
use crate::sync_engine::contacts::{
    PHOTO_CACHE_KIND, PHOTO_CACHE_VERSION, PHOTO_TTL, decode_photo, fetch_photo, forget_photos,
    is_google_oauth, now_millis, now_secs, record_capability, server_contact, update_opt_in,
};
use crate::{Error, Result};

/// The key of the contacts opt-in in an account's config.
const OPT_IN_KEY: &str = "contacts";

/// An account's contacts opt-in, kept in its config (`AccountConfig.extra.contacts`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactsOptIn {
    /// The user connected the account's contacts (and hasn't disconnected them).
    #[serde(default)]
    pub connected: bool,
    /// What the account's grant covered when Maze last looked (`["mail",
    /// "contacts"]`).
    #[serde(default)]
    pub capabilities: Capabilities,
    /// When that was (Unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
}

impl ContactsOptIn {
    /// Whether the last known grant covers contacts.
    pub fn has_permission(&self) -> bool {
        self.capabilities.contains(Capability::Contacts)
    }
}

impl AccountConfig {
    /// The account's contacts opt-in (default: not connected).
    pub fn contacts_opt_in(&self) -> ContactsOptIn {
        self.extra
            .get(OPT_IN_KEY)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    }

    /// Replaces the account's contacts opt-in.
    pub fn set_contacts_opt_in(&mut self, opt_in: &ContactsOptIn) {
        match serde_json::to_value(opt_in) {
            Ok(value) => {
                self.extra.insert(OPT_IN_KEY.to_owned(), value);
            }
            Err(err) => tracing::warn!(%err, "storing the contacts opt-in"),
        }
    }
}

/// A synced contact with everything the Address Book shows and an edit needs.
#[derive(Clone, PartialEq, Eq)]
pub struct SyncedContact {
    /// The book (`google:<account uuid>`).
    pub book: String,
    /// The server's id (`people/c…`); pass it to [`CoreHandle::update_contact`].
    pub resource_name: String,
    /// The version Maze has; pass it to [`CoreHandle::update_contact`].
    pub etag: Option<String>,
    /// The fields.
    pub fields: ContactFields,
    /// The person's photo URL ([`CoreHandle::contact_photo`] loads it).
    pub photo_url: Option<String>,
    /// Whether Maze can save changes to it ([`CoreHandle::contact_book_editable`]).
    pub editable: bool,
}

impl fmt::Debug for SyncedContact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncedContact")
            .field("book", &self.book)
            .field("fields", &self.fields)
            .field("editable", &self.editable)
            .finish_non_exhaustive()
    }
}

impl SyncedContact {
    fn from_stored(stored: StoredServerContact, editable: bool) -> Self {
        Self {
            fields: ::contacts::vcard::parse(&stored.vcard).unwrap_or_default(),
            book: stored.book,
            resource_name: stored.resource_name,
            etag: stored.etag,
            photo_url: stored.photo_url,
            editable,
        }
    }

    fn from_person(book: String, person: ::contacts::Person, editable: bool) -> Self {
        Self {
            book,
            resource_name: person.resource_name,
            etag: person.etag,
            fields: person.fields,
            photo_url: person.photo_url,
            editable,
        }
    }
}

/// How a synced book is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContactBookState {
    /// Synced (or not synced yet).
    Ok,
    /// The account's grant doesn't cover contacts: "Contacts need permission"
    /// ([`CoreHandle::connect_google_contacts`] fixes it).
    NeedsPermission,
    /// The last sync failed; see [`ContactBookStatus::error`].
    Error,
}

/// One synced book of an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactBookStatus {
    /// The book (`google:<uuid>`, `google-other:<uuid>`).
    pub book: String,
    /// Which book.
    pub kind: BookKind,
    /// Whether contacts in it can be created and edited.
    pub editable: bool,
    /// How it's doing.
    pub state: ContactBookState,
    /// When it last synced (Unix seconds).
    pub last_sync: Option<i64>,
    /// The last error, for people.
    pub error: Option<String>,
}

/// Contact sync of one Google account ([`CoreHandle::contacts_status`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountContacts {
    /// The account.
    pub account: AccountId,
    /// The user connected its contacts.
    pub connected: bool,
    /// The last known grant covers contacts.
    pub permission: bool,
    /// `contacts.sync` (or the account's override) is on.
    pub sync_enabled: bool,
    /// Its synced books (empty before the first sync).
    pub books: Vec<ContactBookStatus>,
}

fn contacts_error(err: ContactError) -> Error {
    Error::Contacts(err)
}

impl CoreHandle {
    /// The account and config of a synced book's account, if it is a Google OAuth
    /// account.
    fn book_account(&self, book: &Book) -> Result<Option<(Account, AccountConfig)>> {
        let Some(account) = self.store().reader()?.account_by_uuid(book.account)? else {
            return Ok(None);
        };
        let Some(config) = AccountConfig::parse(&account.config_json) else {
            return Ok(None);
        };
        Ok(is_google_oauth(&config).then_some((account, config)))
    }

    /// Contact sync of every Google account: connected, permission, books with
    /// their state. For "Contacts need permission" and the Address Book.
    pub fn contacts_status(&self) -> Result<Vec<AccountContacts>> {
        self.ensure_open()?;
        let reader = self.store().reader()?;
        let settings = self.settings();
        let mut out = Vec::new();
        for account in reader.accounts()? {
            let Some(config) = AccountConfig::parse(&account.config_json) else {
                continue;
            };
            if !is_google_oauth(&config) {
                continue;
            }
            let opt_in = config.contacts_opt_in();
            let books = reader
                .contact_sources(account.id)?
                .into_iter()
                .filter_map(|source| {
                    let kind = source.book.parse::<Book>().ok()?.kind;
                    Some(ContactBookStatus {
                        editable: kind.is_writable() && opt_in.connected && opt_in.has_permission(),
                        kind,
                        state: match source.state.as_str() {
                            "missing_scope" => ContactBookState::NeedsPermission,
                            "error" => ContactBookState::Error,
                            _ => ContactBookState::Ok,
                        },
                        last_sync: source.last_sync,
                        error: source.error,
                        book: source.book,
                    })
                })
                .collect();
            out.push(AccountContacts {
                account: account.id,
                connected: opt_in.connected,
                permission: opt_in.has_permission(),
                sync_enabled: settings.settings.account(&account.name).contacts_sync,
                books,
            });
        }
        Ok(out)
    }

    /// Connects the contacts of Google account `account`: opens Google's consent page
    /// for the contacts scopes (an incremental consent: mail stays as it is), keeps
    /// the answer in the account's config and starts syncing. Returns whether Google
    /// granted contacts (the user may untick them).
    ///
    /// # Errors
    ///
    /// [`Error::Account`] for an account that doesn't sign in with Google
    /// ([`AccountErrorKind::InvalidInput`]), a missing client ID
    /// ([`AccountErrorKind::OAuthNotConfigured`]) or a failed or cancelled sign-in
    /// ([`AccountErrorKind::SignInFailed`]); [`Error::NoSyncEngine`].
    pub async fn connect_google_contacts(&self, account: AccountId) -> Result<bool> {
        let core = self.clone();
        self.run(async move {
            let engine = core.engine_or_err()?;
            let row = core
                .store()
                .reader()?
                .account(account)?
                .ok_or_else(|| Error::not_found("account", account))?;
            let config = AccountConfig::parse(&row.config_json)
                .filter(is_google_oauth)
                .ok_or_else(|| Error::Account {
                    kind: AccountErrorKind::InvalidInput,
                    message: "contacts can be connected for accounts that sign in with Google"
                        .to_owned(),
                })?;
            let manager =
                engine
                    .token_manager(&row, &config)
                    .map_err(|message| Error::Account {
                        kind: AccountErrorKind::OAuthNotConfigured,
                        message,
                    })?;
            let hint = config.email.clone().unwrap_or_else(|| config.login());
            let granted = manager
                .grant(
                    engine.browser().as_ref(),
                    Capabilities::CONTACTS,
                    AuthorizeOptions::new().login_hint(hint),
                )
                .await
                .map_err(|err| Error::Account {
                    kind: AccountErrorKind::SignInFailed,
                    message: format!("the Google sign-in failed: {err}"),
                })?;
            let has = granted.contains(Capability::Contacts);
            let known = manager
                .known_capabilities()
                .await
                .union(granted)
                .with(Capability::Mail);
            update_opt_in(core.store(), account, move |opt_in| {
                opt_in.connected = has;
                opt_in.capabilities = if has {
                    known
                } else {
                    known
                        .iter()
                        .filter(|c| *c != Capability::Contacts)
                        .collect()
                };
                opt_in.checked_at = Some(now_secs());
            })
            .await?;
            engine.start_contacts(&row, &config);
            engine.sync_contacts_now(Some(account));
            tracing::info!(%account, granted = has, "contacts connected");
            Ok(has)
        })
        .await
    }

    /// Disconnects the contacts of `account`: stops syncing them and deletes the
    /// local copy (books, people, photos). Nothing changes on Google, and the grant
    /// stays (connecting again needs no new consent).
    pub async fn disconnect_contacts(&self, account: AccountId) -> Result<()> {
        let core = self.clone();
        self.run(async move {
            update_opt_in(core.store(), account, |opt_in| opt_in.connected = false).await?;
            let urls = core
                .store()
                .write(move |w| w.delete_contact_books(account))
                .await?;
            forget_photos(core.store(), &urls);
            tracing::info!(%account, "contacts disconnected");
            Ok(())
        })
        .await
    }

    /// Syncs the contacts of `account` (or of every account) now, in the background.
    pub fn sync_contacts_now(&self, account: Option<AccountId>) {
        if let Some(engine) = self.engine() {
            engine.sync_contacts_now(account);
        }
    }

    /// Creates a contact in the My Contacts of `account`: sends it to Google and
    /// stores Google's answer (`google:<uuid>`). Returns the stored contact.
    ///
    /// # Errors
    ///
    /// [`Error::Contacts`] with [`ContactError::Offline`] without a connection (nothing
    /// is queued), [`ContactError::MissingScope`] when the account hasn't connected
    /// its contacts (or the permission is gone), [`ContactError::Empty`],
    /// [`ContactError::Rejected`] for what Google refuses; [`Error::NotFound`] for an
    /// unknown account; [`Error::NoSyncEngine`].
    pub async fn create_contact(
        &self,
        account: AccountId,
        fields: ContactFields,
    ) -> Result<SyncedContact> {
        let core = self.clone();
        self.run(async move {
            let engine = core.engine_or_err()?;
            let row = core
                .store()
                .reader()?
                .account(account)?
                .ok_or_else(|| Error::not_found("account", account))?;
            let config = writable_config(&row)?;
            let source = engine
                .contact_source(&row, &config)
                .map_err(contacts_error)?;
            let person = match source.create(&fields).await {
                Ok(person) => person,
                Err(err) => return Err(core.write_failed(account, err).await),
            };
            let book = Book::new(BookKind::Google, row.uuid).to_string();
            core.store_person(account, &book, &person).await?;
            tracing::info!(%account, "contact created");
            Ok(SyncedContact::from_person(book, person, true))
        })
        .await
    }

    /// Saves changes to synced contact `resource_name` of `book`, which the user
    /// edited at version `etag`: sends all `fields` to Google (etag-checked) and
    /// stores Google's answer. Returns the stored contact.
    ///
    /// # Errors
    ///
    /// As [`CoreHandle::create_contact`], plus [`ContactError::Conflict`] with the
    /// current version when the contact changed elsewhere (that version is stored
    /// too), [`ContactError::NotFound`] when it was deleted on the server (it is
    /// removed here too), and [`ContactError::ReadOnly`] for books that can't be
    /// edited (`google-other:`, local books).
    pub async fn update_contact(
        &self,
        book: String,
        resource_name: String,
        fields: ContactFields,
        etag: String,
    ) -> Result<SyncedContact> {
        let core = self.clone();
        self.run(async move {
            let engine = core.engine_or_err()?;
            let parsed = book
                .parse::<Book>()
                .ok()
                .filter(|b| b.kind.is_writable())
                .ok_or(Error::Contacts(ContactError::ReadOnly))?;
            let (row, _) = core
                .book_account(&parsed)?
                .ok_or(Error::Contacts(ContactError::ReadOnly))?;
            let config = writable_config(&row)?;
            let source = engine
                .contact_source(&row, &config)
                .map_err(contacts_error)?;
            match source.update(&resource_name, &etag, &fields).await {
                Ok(person) => {
                    core.store_person(row.id, &book, &person).await?;
                    tracing::info!(account = %row.id, "contact updated");
                    Ok(SyncedContact::from_person(book, person, true))
                }
                Err(ContactError::Conflict(current)) => {
                    // The server's version is the truth: keep it, show it.
                    core.store_person(row.id, &book, &current).await?;
                    Err(Error::Contacts(ContactError::Conflict(current)))
                }
                Err(ContactError::NotFound) => {
                    let gone = book.clone();
                    core.store()
                        .write(move |w| w.delete_server_contact(&gone, &resource_name))
                        .await?;
                    Err(Error::Contacts(ContactError::NotFound))
                }
                Err(err) => Err(core.write_failed(row.id, err).await),
            }
        })
        .await
    }

    /// [`CoreHandle::save_contact`] for synced books: a new contact (`edit.contact`
    /// `None`) goes to [`CoreHandle::create_contact`], a change to
    /// [`CoreHandle::update_contact`] with the stored etag; the fields the editor
    /// doesn't show are kept. Books that aren't editable answer
    /// [`ContactError::NotEditable`](crate::contacts::ContactError::NotEditable).
    pub(crate) async fn save_contact_edit(
        &self,
        edit: ContactEdit,
    ) -> std::result::Result<ContactDetail, crate::contacts::ContactError> {
        use crate::contacts::ContactError as EditError;
        self.ensure_open()?;
        if !self.contact_book_editable(&edit.book) {
            return Err(EditError::NotEditable { book: edit.book });
        }
        let saved = match edit.contact {
            None => {
                let parsed = edit
                    .book
                    .parse::<Book>()
                    .map_err(|_| EditError::NotEditable {
                        book: edit.book.clone(),
                    })?;
                let account = self
                    .store()
                    .reader()
                    .map_err(Error::from)?
                    .account_by_uuid(parsed.account)
                    .map_err(Error::from)?
                    .ok_or_else(|| Error::not_found("account", parsed.account))?;
                self.create_contact(account.id, apply_edit(ContactFields::default(), &edit))
                    .await?
            }
            Some(id) => {
                let stored = self
                    .store()
                    .reader()
                    .map_err(Error::from)?
                    .server_contact_of_entry(id)
                    .map_err(Error::from)?
                    .filter(|s| s.book == edit.book)
                    .ok_or_else(|| Error::not_found("contact", id.0))?;
                let existing = ::contacts::vcard::parse(&stored.vcard).unwrap_or_default();
                let fields = apply_edit(existing, &edit);
                self.update_contact(
                    stored.book,
                    stored.resource_name,
                    fields,
                    stored.etag.unwrap_or_default(),
                )
                .await?
            }
        };
        let email = saved
            .fields
            .email_addresses()
            .into_iter()
            .next()
            .ok_or_else(|| Error::not_found("contact", &saved.resource_name))?;
        Ok(self
            .contact(&email)?
            .ok_or_else(|| Error::not_found("contact", email))?)
    }

    /// The synced contact `resource_name` of `book`.
    pub fn synced_contact(&self, book: &str, resource_name: &str) -> Result<Option<SyncedContact>> {
        self.ensure_open()?;
        let stored = self.store().reader()?.server_contact(book, resource_name)?;
        let editable = stored.is_some() && self.contact_book_editable(book);
        Ok(stored.map(|s| SyncedContact::from_stored(s, editable)))
    }

    /// The synced contact whose row in `book` has address `email` (the row the
    /// Address Book lists).
    pub fn synced_contact_for_email(
        &self,
        book: &str,
        email: &str,
    ) -> Result<Option<SyncedContact>> {
        self.ensure_open()?;
        let stored = self
            .store()
            .reader()?
            .server_contact_by_email(book, email)?;
        let editable = stored.is_some() && self.contact_book_editable(book);
        Ok(stored.map(|s| SyncedContact::from_stored(s, editable)))
    }

    /// The photo of the synced contact with address `email` (in `book`, or in any
    /// synced book, My Contacts first): from the encrypted cache, else fetched now
    /// (and cached for 7 days). `None` when there is none or Maze is offline.
    pub async fn contact_photo(
        &self,
        email: String,
        book: Option<String>,
    ) -> Result<Option<Photo>> {
        let core = self.clone();
        self.run(async move {
            let reader = core.store().reader()?;
            let Some(url) = reader.contact_photo_url(&email, book.as_deref())? else {
                return Ok(None);
            };
            let state = reader.contact_photo(&url)?;
            drop(reader);
            if let Some(state) = state {
                let ttl = i64::try_from(PHOTO_TTL.as_secs()).unwrap_or(i64::MAX);
                let fresh = now_secs().saturating_sub(state.fetched_at) < ttl;
                if state.found {
                    let key = crate::sync_engine::contacts::photo_key(&url);
                    if let Some(photo) = core
                        .store()
                        .blobs()
                        .cache_get(&key, PHOTO_CACHE_KIND, PHOTO_CACHE_VERSION)
                        .and_then(|data| decode_photo(&data))
                        && fresh
                    {
                        return Ok(Some(photo));
                    }
                } else if fresh {
                    return Ok(None);
                }
            }
            Ok(fetch_photo(core.store(), &url).await.unwrap_or(None))
        })
        .await
    }

    /// Stores a person Google sent back after a write.
    async fn store_person(
        &self,
        account: AccountId,
        book: &str,
        person: &::contacts::Person,
    ) -> Result<()> {
        let book = book.to_owned();
        let stored = server_contact(person);
        let seen = now_millis();
        self.store()
            .write(move |w| {
                w.ensure_contact_source(&book, account, BookKind::Google.prefix())?;
                w.upsert_server_contact(&book, &stored, seen)
            })
            .await?;
        Ok(())
    }

    /// A write that Google refused: a missing permission is remembered.
    async fn write_failed(&self, account: AccountId, err: ContactError) -> Error {
        if matches!(err, ContactError::MissingScope) {
            record_capability(self.store(), account, false).await;
        }
        Error::Contacts(err)
    }
}

/// Whether contacts in `book` can be created and edited in Maze: a `google:` book
/// (My Contacts) of an account (among `accounts`) that signs in with Google, connected
/// its contacts and whose last known grant covers them. `google-other:` books,
/// `collected` and imported books are read-only. No network. Backs
/// [`CoreHandle::contact_book_editable`].
pub(crate) fn synced_book_editable(book: &str, accounts: &[Account]) -> bool {
    let Ok(parsed) = book.parse::<Book>() else {
        return false;
    };
    if !parsed.kind.is_writable() {
        return false;
    }
    accounts
        .iter()
        .find(|a| a.uuid == parsed.account)
        .and_then(|a| AccountConfig::parse(&a.config_json))
        .filter(is_google_oauth)
        .is_some_and(|config| {
            let opt_in = config.contacts_opt_in();
            opt_in.connected && opt_in.has_permission()
        })
}

/// `edit` applied to the fields a synced contact has: name, addresses, phone numbers,
/// organization and title as the editor has them; labels of addresses and numbers that
/// stay, postal addresses, web sites and the note are kept.
fn apply_edit(existing: ContactFields, edit: &ContactEdit) -> ContactFields {
    let text = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let mut fields = existing;
    let name = text(edit.name.as_deref());
    if name != fields.name.full() {
        fields.name = Name {
            display: name,
            ..Name::default()
        };
    }
    let old_emails = std::mem::take(&mut fields.emails);
    for email in &edit.emails {
        let Some(value) = text(Some(email)) else {
            continue;
        };
        if fields
            .emails
            .iter()
            .any(|e| e.value.eq_ignore_ascii_case(&value))
        {
            continue;
        }
        let kind = old_emails
            .iter()
            .find(|e| e.value.eq_ignore_ascii_case(&value))
            .and_then(|e| e.kind.clone());
        fields.emails.push(Labeled { value, kind });
    }
    let old_phones = std::mem::take(&mut fields.phones);
    for phone in &edit.phones {
        let Some(value) = text(Some(&phone.number)) else {
            continue;
        };
        let kind = text(phone.kind.as_deref()).or_else(|| {
            old_phones
                .iter()
                .find(|p| p.value == value)
                .and_then(|p| p.kind.clone())
        });
        fields.phones.push(Labeled { value, kind });
    }
    let organization = text(edit.organization.as_deref());
    let title = text(edit.title.as_deref());
    match fields.organizations.first_mut() {
        Some(first) => {
            first.name = organization;
            first.title = title;
            if first.name.is_none() && first.title.is_none() && first.department.is_none() {
                fields.organizations.remove(0);
            }
        }
        None if organization.is_some() || title.is_some() => {
            fields.organizations.push(Organization {
                name: organization,
                department: None,
                title,
            });
        }
        None => {}
    }
    fields
}

/// The config of an account whose contacts can be written: Google, connected.
fn writable_config(account: &Account) -> Result<AccountConfig> {
    let config = AccountConfig::parse(&account.config_json)
        .filter(is_google_oauth)
        .ok_or(Error::Contacts(ContactError::ReadOnly))?;
    if !config.contacts_opt_in().connected {
        return Err(Error::Contacts(ContactError::MissingScope));
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn the_opt_in_lives_in_the_account_config() {
        let mut config = AccountConfig::parse(
            r#"{"email":"a@gmail.com","host":"imap.gmail.com","auth":"oauth2","oauth_provider":"google","import":{"x":1}}"#,
        )
        .unwrap();
        assert_eq!(config.contacts_opt_in(), ContactsOptIn::default());
        assert!(is_google_oauth(&config));
        let opt_in = ContactsOptIn {
            connected: true,
            capabilities: Capabilities::MAIL.with(Capability::Contacts),
            checked_at: Some(5),
        };
        config.set_contacts_opt_in(&opt_in);
        let json = config.to_json();
        assert!(
            json.contains(
                r#""contacts":{"connected":true,"capabilities":["mail","contacts"],"checked_at":5}"#
            ),
            "{json}"
        );
        let parsed = AccountConfig::parse(&json).unwrap();
        assert_eq!(parsed.contacts_opt_in(), opt_in);
        assert!(parsed.contacts_opt_in().has_permission());
        // Other keys survive.
        assert!(parsed.extra.contains_key("import"));
    }
}
