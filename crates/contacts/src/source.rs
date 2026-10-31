//! The provider-neutral part: where access tokens come from ([`AccessTokens`]), what
//! a contact server can do ([`ContactSource`]) and the paging loop of a sync
//! ([`sync_book`]).

use std::fmt;
use std::future::Future;

use async_trait::async_trait;
use auth::{Capability, OAuthTokenManager};
use mazemail_types::Sensitive;

use crate::error::ContactError;
use crate::model::{BookKind, ContactFields, Person, Photo};

/// Hands out access tokens for the contacts API. [`OAuthTokenManager`] implements it
/// with [`Capability::Contacts`] (Google: the account's token, which must carry the
/// contacts scopes).
#[async_trait]
pub trait AccessTokens: Send + Sync {
    /// A valid access token.
    async fn token(&self) -> Result<Sensitive<String>, auth::AuthError>;

    /// Forgets the cached token: the API rejected it (HTTP 401).
    async fn invalidate(&self);
}

#[async_trait]
impl AccessTokens for OAuthTokenManager {
    async fn token(&self) -> Result<Sensitive<String>, auth::AuthError> {
        self.access_token_for(Capability::Contacts).await
    }

    async fn invalidate(&self) {
        self.invalidate_for(Capability::Contacts).await;
    }
}

/// One page of a sync.
#[derive(Default, Clone, PartialEq, Eq)]
pub struct SyncPage {
    /// People added or changed.
    pub upserts: Vec<Person>,
    /// Resource names of people deleted on the server.
    pub deletes: Vec<String>,
    /// The next page, if any.
    pub next_page_token: Option<String>,
    /// The token for the next incremental sync; only on the last page.
    pub next_sync_token: Option<String>,
}

impl fmt::Debug for SyncPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncPage")
            .field("upserts", &self.upserts.len())
            .field("deletes", &self.deletes.len())
            .field("more", &self.next_page_token.is_some())
            .field("sync_token", &self.next_sync_token.is_some())
            .finish()
    }
}

/// A contact server: list (with sync tokens), create, update (etag-checked) and
/// photos. Implementations: [`GoogleContacts`](crate::GoogleContacts).
#[async_trait]
pub trait ContactSource: Send + Sync + fmt::Debug {
    /// One page of `book`. Without `sync_token` a full listing (asking for a sync
    /// token at the end); with it, the changes since. An expired token is
    /// [`ContactError::SyncTokenExpired`].
    async fn list(
        &self,
        book: BookKind,
        sync_token: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<SyncPage, ContactError>;

    /// Creates a contact in the writable book; returns it as the server stored it.
    async fn create(&self, fields: &ContactFields) -> Result<Person, ContactError>;

    /// Replaces the fields of contact `resource_name`, which the caller read at
    /// version `etag`. A newer version on the server is [`ContactError::Conflict`]
    /// with that version.
    async fn update(
        &self,
        resource_name: &str,
        etag: &str,
        fields: &ContactFields,
    ) -> Result<Person, ContactError>;

    /// The image behind a photo URL; `None` if there is none (any more).
    async fn photo(&self, url: &str) -> Result<Option<Photo>, ContactError>;
}

/// One page as [`sync_book`] hands it to the caller to store.
#[derive(Debug, Clone)]
pub struct PageUpdate {
    /// Which book.
    pub book: BookKind,
    /// A full listing: people of the book not in any page of it are gone (apply
    /// once `last` is set).
    pub full: bool,
    /// The first page of this listing.
    pub first: bool,
    /// The last page: `page.next_sync_token` is the token to keep.
    pub last: bool,
    /// The page.
    pub page: SyncPage,
}

/// What [`sync_book`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncSummary {
    /// It was a full listing.
    pub full: bool,
    /// The sync token had expired, so a full listing followed.
    pub restarted: bool,
    /// Pages fetched.
    pub pages: usize,
    /// People added or changed.
    pub upserts: usize,
    /// People deleted.
    pub deletes: usize,
}

/// Syncs one book: incremental from `sync_token`, or a full listing without one
/// (or when the token expired). Every page goes to `apply` (to be stored in one
/// transaction) before the next is fetched; an error from `apply` stops the sync and
/// is returned as it is.
pub async fn sync_book<F, Fut, E>(
    source: &dyn ContactSource,
    book: BookKind,
    sync_token: Option<String>,
    mut apply: F,
) -> Result<SyncSummary, E>
where
    F: FnMut(PageUpdate) -> Fut + Send,
    Fut: Future<Output = Result<(), E>> + Send,
    E: From<ContactError>,
{
    let mut token = sync_token;
    let mut summary = SyncSummary::default();
    'listing: loop {
        let full = token.is_none();
        summary.full = full;
        let mut page_token: Option<String> = None;
        let mut first = true;
        loop {
            let page = match source
                .list(book, token.as_deref(), page_token.as_deref())
                .await
            {
                Ok(page) => page,
                Err(ContactError::SyncTokenExpired) if !full => {
                    tracing::info!(?book, "the contacts sync token expired; syncing everything");
                    token = None;
                    summary.restarted = true;
                    continue 'listing;
                }
                Err(err) => return Err(err.into()),
            };
            summary.pages += 1;
            summary.upserts += page.upserts.len();
            summary.deletes += page.deletes.len();
            let next = page.next_page_token.clone().filter(|t| !t.is_empty());
            let last = next.is_none();
            apply(PageUpdate {
                book,
                full,
                first,
                last,
                page,
            })
            .await?;
            if last {
                return Ok(summary);
            }
            first = false;
            page_token = next;
        }
    }
}
