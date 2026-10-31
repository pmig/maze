//! Contact sync next to mail sync (ADR-024, PLAN-contacts §3).
//!
//! Every Google account with an OAuth sign-in gets a small task. Each round it checks
//! whether the account connected its contacts (`AccountConfig.extra.contacts`, see
//! [`ContactsOptIn`]) and whether `contacts.sync` (or the account's `contacts_sync`
//! override) is on, then syncs My Contacts (`google:<uuid>`) and Other contacts
//! (`google-other:<uuid>`) with [`contacts::sync_book`]: a full listing the first
//! time (and when a sync token expired), then only the changes. Each page is one store
//! write job. Afterwards it fetches due photos into the encrypted blob cache, slowly.
//!
//! A round runs when the core opens, every `contacts.sync_interval_minutes`, after
//! sleep, and on demand ([`SyncEngine::sync_contacts_now`]). Missing permission marks
//! the books `missing_scope` and the account's last known capabilities lose contacts;
//! mail is not affected. Offline rounds are skipped quietly.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ::contacts::{Book, BookKind, ContactError, GoogleContacts, Person};
use auth::{Capabilities, Capability, ProviderKind};
use mazemail_types::{AccountId, BlobId};
use store::{Account, Queries as _, ServerContact, Store, WriteTx};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::{SyncEngine, lock};
use crate::accounts::{AccountAuth, AccountConfig};
use crate::synced_contacts::ContactsOptIn;

/// The shortest interval between contact syncs.
const MIN_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How long a fetched photo (or "no photo") is trusted.
pub(crate) const PHOTO_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Photos fetched per round at most.
const PHOTOS_PER_ROUND: usize = 100;

/// The pause between two photo fetches (low priority, rate-limit friendly).
const PHOTO_PAUSE: Duration = Duration::from_millis(200);

/// The blob cache kind of contact photos.
pub(crate) const PHOTO_CACHE_KIND: &str = "contact-photo";

/// The format version of cached photos.
pub(crate) const PHOTO_CACHE_VERSION: u32 = 1;

/// A running contact sync task.
pub(crate) struct ContactTask {
    wake: Arc<Notify>,
    handle: JoinHandle<()>,
}

impl ContactTask {
    pub(crate) fn stop(self) {
        self.handle.abort();
    }
}

/// Milliseconds since the epoch: a sync run's id (later runs have larger ids).
pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Seconds since the epoch.
pub(crate) fn now_secs() -> i64 {
    now_millis() / 1000
}

/// What a sync round needs to know about an account.
pub(crate) fn is_google_oauth(config: &AccountConfig) -> bool {
    config.auth() == Some(AccountAuth::OAuth(ProviderKind::Google))
}

/// A synced contact as the store keeps it.
pub(crate) fn server_contact(person: &Person) -> ServerContact {
    ServerContact {
        resource_name: person.resource_name.clone(),
        etag: person.etag.clone(),
        display_name: person.fields.display_name(),
        emails: person.fields.email_addresses(),
        vcard: person.to_vcard(),
        photo_url: person.photo_url.clone(),
    }
}

/// The blob cache key of photo `url`.
pub(crate) fn photo_key(url: &str) -> BlobId {
    let mut hasher = blake3::Hasher::new_derive_key("mazemail 2026 contact photo v1");
    hasher.update(url.as_bytes());
    BlobId(*hasher.finalize().as_bytes())
}

/// A photo as cached: the content type, a newline, the bytes.
pub(crate) fn encode_photo(photo: &::contacts::Photo) -> Vec<u8> {
    let mut out = Vec::with_capacity(photo.content_type.len() + 1 + photo.bytes.len());
    out.extend_from_slice(photo.content_type.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(&photo.bytes);
    out
}

/// The inverse of [`encode_photo`].
pub(crate) fn decode_photo(data: &[u8]) -> Option<::contacts::Photo> {
    let newline = data.iter().position(|b| *b == b'\n')?;
    let content_type = std::str::from_utf8(&data[..newline]).ok()?.to_owned();
    Some(::contacts::Photo {
        content_type,
        bytes: data[newline + 1..].to_vec(),
    })
}

/// Removes the cache entries of photos nobody has any more.
pub(crate) fn forget_photos(store: &Store, urls: &[String]) {
    for url in urls {
        if let Err(err) =
            store
                .blobs()
                .cache_remove(&photo_key(url), PHOTO_CACHE_KIND, PHOTO_CACHE_VERSION)
        {
            tracing::debug!(%err, "removing a cached contact photo");
        }
    }
}

/// Fetches photo `url` into the blob cache and records the answer. `Err` only when
/// it's worth stopping (offline).
pub(crate) async fn fetch_photo(
    store: &Store,
    url: &str,
) -> Result<Option<::contacts::Photo>, ContactError> {
    match ::contacts::fetch_photo(url).await {
        Ok(Some(photo)) => {
            if let Err(err) = store.blobs().cache_put(
                &photo_key(url),
                PHOTO_CACHE_KIND,
                PHOTO_CACHE_VERSION,
                &encode_photo(&photo),
            ) {
                tracing::warn!(%err, "caching a contact photo");
                return Ok(Some(photo));
            }
            record_photo(store, url, true).await;
            Ok(Some(photo))
        }
        Ok(None) => {
            record_photo(store, url, false).await;
            Ok(None)
        }
        Err(err @ ContactError::Offline { .. }) => Err(err),
        Err(err) => {
            // Rate limits and server errors: try again another time.
            tracing::debug!(%err, "fetching a contact photo");
            Ok(None)
        }
    }
}

async fn record_photo(store: &Store, url: &str, found: bool) {
    let url = url.to_owned();
    if let Err(err) = store
        .write(move |w| w.record_contact_photo(&url, found))
        .await
    {
        tracing::warn!(%err, "recording a contact photo");
    }
}

/// Changes the contacts opt-in in an account's config (read and written in one
/// write job, so concurrent changes don't get lost).
pub(crate) async fn update_opt_in(
    store: &Store,
    account: AccountId,
    change: impl FnOnce(&mut ContactsOptIn) + Send + 'static,
) -> store::Result<()> {
    store
        .write(move |w| {
            let Some(row) = w.account(account)? else {
                return Ok(());
            };
            let Some(mut config) = AccountConfig::parse(&row.config_json) else {
                return Ok(());
            };
            let before = config.contacts_opt_in();
            let mut after = before.clone();
            change(&mut after);
            if after != before {
                config.set_contacts_opt_in(&after);
                w.update_account(row.id, &row.name, &config.to_json(), row.ordinal)?;
            }
            Ok(())
        })
        .await
}

/// Records whether the account's grant covers contacts, as last seen.
pub(crate) async fn record_capability(store: &Store, account: AccountId, granted: bool) {
    let result = update_opt_in(store, account, move |opt_in| {
        let caps = opt_in.capabilities.with(Capability::Mail);
        opt_in.capabilities = if granted {
            caps.with(Capability::Contacts)
        } else {
            caps.iter()
                .filter(|c| *c != Capability::Contacts)
                .collect::<Capabilities>()
        };
        opt_in.checked_at = Some(now_secs());
    })
    .await;
    if let Err(err) = result {
        tracing::warn!(%err, %account, "recording the contacts permission");
    }
}

/// Why a book's sync stopped.
#[derive(Debug, thiserror::Error)]
enum RoundError {
    #[error(transparent)]
    Contacts(#[from] ContactError),
    #[error(transparent)]
    Store(#[from] store::Error),
}

/// Stores one page; returns the photo URLs that went away (after a full listing).
fn apply_page(
    w: &mut WriteTx<'_>,
    book: &str,
    update: &::contacts::PageUpdate,
    seen: i64,
) -> store::Result<Vec<String>> {
    for person in &update.page.upserts {
        w.upsert_server_contact(book, &server_contact(person), seen)?;
    }
    for resource_name in &update.page.deletes {
        w.delete_server_contact(book, resource_name)?;
    }
    if !update.last {
        return Ok(Vec::new());
    }
    if update.full {
        let removed = w.finish_full_contact_sync(book, seen)?;
        if removed > 0 {
            tracing::debug!(removed, "contacts removed on the server");
        }
    }
    w.set_contact_sync_token(book, update.page.next_sync_token.as_deref(), update.full)?;
    if update.full {
        w.prune_contact_photos()
    } else {
        Ok(Vec::new())
    }
}

impl SyncEngine {
    /// The People API client for a Google account.
    pub(crate) fn contact_source(
        &self,
        account: &Account,
        config: &AccountConfig,
    ) -> Result<GoogleContacts, ContactError> {
        if !is_google_oauth(config) {
            return Err(ContactError::ReadOnly);
        }
        let tokens = self
            .token_manager(account, config)
            .map_err(ContactError::NotConfigured)?;
        let mut source = GoogleContacts::new(tokens)?.with_backoff(self.config.contacts_backoff);
        if let Some(base) = &self.config.contacts_api {
            source = source.with_base_url(base);
        }
        Ok(source)
    }

    /// Starts the contact sync task of `account` if it is a Google OAuth account and
    /// has none yet. Whether it syncs is decided each round.
    pub(crate) fn start_contacts(self: &Arc<Self>, account: &Account, config: &AccountConfig) {
        if !is_google_oauth(config) {
            return;
        }
        let mut tasks = lock(&self.contact_tasks);
        if tasks.contains_key(&account.id) {
            return;
        }
        let wake = Arc::new(Notify::new());
        let engine = Arc::downgrade(self);
        let id = account.id;
        let handle = self.runtime.spawn({
            let wake = Arc::clone(&wake);
            async move {
                loop {
                    let Some(engine) = engine.upgrade() else {
                        return;
                    };
                    engine.contact_round(id).await;
                    let interval = Duration::from_secs(
                        u64::from(
                            engine
                                .settings
                                .borrow()
                                .settings
                                .contacts
                                .sync_interval_minutes,
                        ) * 60,
                    )
                    .max(MIN_INTERVAL);
                    drop(engine);
                    tokio::select! {
                        () = tokio::time::sleep(interval) => {}
                        () = wake.notified() => {}
                    }
                }
            }
        });
        tasks.insert(account.id, ContactTask { wake, handle });
    }

    /// Stops the contact sync task of `account`.
    pub(crate) fn stop_contacts(&self, account: AccountId) {
        if let Some(task) = lock(&self.contact_tasks).remove(&account) {
            task.stop();
        }
    }

    /// Syncs the contacts of `account` (or of every account) now.
    pub(crate) fn sync_contacts_now(&self, account: Option<AccountId>) {
        let tasks = lock(&self.contact_tasks);
        for (id, task) in tasks.iter() {
            if account.is_none_or(|a| a == *id) {
                task.wake.notify_one();
            }
        }
    }

    /// Whether `account` syncs contacts now: a Google OAuth account that connected
    /// them, with contact sync on in the settings.
    fn contacts_enabled(&self, account: &Account, config: &AccountConfig) -> bool {
        is_google_oauth(config)
            && config.contacts_opt_in().connected
            && self
                .settings
                .borrow()
                .settings
                .account(&account.name)
                .contacts_sync
    }

    /// One sync round of `account` (see the [module docs](self)).
    async fn contact_round(&self, id: AccountId) {
        let account = match self.store.reader().and_then(|r| r.account(id)) {
            Ok(Some(account)) => account,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(%err, account = %id, "reading an account for contact sync");
                return;
            }
        };
        let Some(config) = AccountConfig::parse(&account.config_json) else {
            return;
        };
        if !self.contacts_enabled(&account, &config) {
            return;
        }
        let source = match self.contact_source(&account, &config) {
            Ok(source) => source,
            Err(err) => {
                tracing::info!(%err, account = %id, "no contact sync for this account");
                return;
            }
        };
        let mut synced = false;
        for kind in BookKind::ALL {
            let book = Book::new(kind, account.uuid).to_string();
            match self.sync_contact_book(&source, id, kind, &book).await {
                Ok(summary) => {
                    synced = true;
                    tracing::debug!(account = %id, ?kind, full = summary.full, restarted = summary.restarted, upserts = summary.upserts, deletes = summary.deletes, "contacts synced");
                }
                Err(RoundError::Contacts(err)) if err.needs_consent() => {
                    tracing::info!(%err, account = %id, "contacts need permission");
                    self.book_error(&book, "missing_scope", &err.to_string())
                        .await;
                    if matches!(err, ContactError::MissingScope) {
                        record_capability(&self.store, id, false).await;
                    }
                    return;
                }
                Err(RoundError::Contacts(err @ ContactError::Offline { .. })) => {
                    tracing::debug!(%err, account = %id, "offline; contacts sync later");
                    return;
                }
                Err(err) => {
                    tracing::warn!(%err, account = %id, ?kind, "contact sync failed");
                    self.book_error(&book, "error", &err.to_string()).await;
                }
            }
        }
        if synced
            && !config
                .contacts_opt_in()
                .capabilities
                .contains(Capability::Contacts)
        {
            record_capability(&self.store, id, true).await;
        }
        self.prefetch_photos().await;
    }

    async fn book_error(&self, book: &str, state: &'static str, message: &str) {
        let book = book.to_owned();
        let message = message.to_owned();
        if let Err(err) = self
            .store
            .write(move |w| w.set_contact_source_error(&book, state, &message))
            .await
        {
            tracing::warn!(%err, "recording a contact sync error");
        }
    }

    async fn sync_contact_book(
        &self,
        source: &GoogleContacts,
        account: AccountId,
        kind: BookKind,
        book: &str,
    ) -> Result<::contacts::SyncSummary, RoundError> {
        let store = self.store.clone();
        {
            let book = book.to_owned();
            store
                .write(move |w| w.ensure_contact_source(&book, account, kind.prefix()))
                .await?;
        }
        let token = store
            .reader()?
            .contact_source(book)?
            .and_then(|s| s.sync_token);
        let seen = now_millis();
        ::contacts::sync_book(source, kind, token, |update| {
            let store = store.clone();
            let book = book.to_owned();
            async move {
                let gone = store
                    .write(move |w| apply_page(w, &book, &update, seen))
                    .await?;
                forget_photos(&store, &gone);
                Ok::<(), RoundError>(())
            }
        })
        .await
    }

    /// Fetches photos never fetched or older than [`PHOTO_TTL`], a few at a time.
    async fn prefetch_photos(&self) {
        let cutoff = now_secs() - i64::try_from(PHOTO_TTL.as_secs()).unwrap_or(i64::MAX);
        let due = match self
            .store
            .reader()
            .and_then(|r| r.contact_photos_due(cutoff, PHOTOS_PER_ROUND))
        {
            Ok(due) => due,
            Err(err) => {
                tracing::warn!(%err, "listing contact photos to fetch");
                return;
            }
        };
        for url in due {
            if fetch_photo(&self.store, &url).await.is_err() {
                return;
            }
            tokio::time::sleep(PHOTO_PAUSE).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photos_round_trip_through_the_cache_format() {
        let photo = ::contacts::Photo {
            content_type: "image/jpeg".into(),
            bytes: b"\n\x00jpeg".to_vec(),
        };
        assert_eq!(decode_photo(&encode_photo(&photo)), Some(photo));
        assert_eq!(decode_photo(b"no newline"), None);
        assert_ne!(photo_key("https://a"), photo_key("https://b"));
    }
}
