//! Microsoft contacts (Outlook.com, Microsoft 365) over Microsoft Graph
//! (`graph.microsoft.com/v1.0`), with the account's Graph token
//! (`Contacts.ReadWrite`, see [`AccessTokens`]).
//!
//! - **The book** is the default Contacts folder. Its id comes from a contact in it
//!   (`GET /me/contacts?$top=1`, `parentFolderId`); while it has no contacts there is
//!   nothing to track and the next sync looks again. Contacts in subfolders aren't
//!   synced.
//! - **Listing** is a delta query, `GET /me/contactFolders/{id}/contacts/delta` with
//!   [`json::SELECT`]: the first round lists everything, later rounds start from the
//!   stored `@odata.deltaLink` and get only the changes (deleted contacts carry
//!   `@removed`). The page token is the `@odata.nextLink`, the sync token the
//!   `@odata.deltaLink`, both full URLs that must point at the configured Graph base.
//!   A delta token Graph forgot (HTTP 410, `SyncStateNotFound`) is
//!   [`ContactError::SyncTokenExpired`].
//! - **Writes**: `POST /me/contacts`; `PATCH /me/contacts/{id}` with every field Maze
//!   manages and `If-Match` with the etag Maze read. Before an update the current
//!   contact is read: a different etag, or Graph's 412, is [`ContactError::Conflict`]
//!   with the current version. Outlook's slots limit what is written (see [`json`]).
//! - **Photos**: `GET /me/contacts/{id}/photo/$value` with the token, up to
//!   [`PHOTO_MAX_BYTES`]; the reference is [`json::PHOTO_URL_PREFIX`]`<account>:<id>`.
//!
//! - **Shared contacts**: with [`MicrosoftContacts::with_mailbox`] the client reads
//!   another mailbox's default Contacts folder (`/users/{mailbox}/…`, with the
//!   account's `Contacts.Read.Shared` token): the same delta listing, read-only, no
//!   photos.
//!
//! HTTP 401 forgets the access token and retries once; 403 `ErrorAccessDenied` is
//! [`ContactError::MissingScope`] (mail is unaffected); 429 and 5xx back off
//! ([`Backoff`], honoring `Retry-After`), for writes only 429. Requests are sequential:
//! one per account at a time.

pub mod json;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Method;
use reqwest::header::{CONTENT_TYPE, IF_MATCH, RETRY_AFTER};
use url::Url;
use uuid::Uuid;

use crate::error::ContactError;
use crate::google::{Backoff, PHOTO_MAX_BYTES};
use crate::model::{BookKind, ContactFields, Person, Photo, is_valid_mailbox};
use crate::source::{AccessTokens, ContactSource, SyncPage};

use json::{MContact, MContactWrite, MError, MPage, SELECT, is_valid_id, parse_photo_url};

/// Microsoft Graph.
pub const GRAPH_API: &str = "https://graph.microsoft.com/";

/// Contacts per delta page (`Prefer: odata.maxpagesize`).
pub const PAGE_SIZE: u32 = 200;

/// What may be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Reads: network errors, 429 and 5xx.
    Read,
    /// Writes: only 429 (the server didn't act on it).
    Write,
}

/// A failed request: already an error, or Graph's error answer to classify.
enum Failure {
    Error(ContactError),
    Api { status: u16, error: MError },
}

impl From<ContactError> for Failure {
    fn from(err: ContactError) -> Self {
        Self::Error(err)
    }
}

/// An answer: its body and content type.
struct Answer {
    bytes: Vec<u8>,
    content_type: String,
}

/// An error answer as a [`ContactError`].
fn classify(status: u16, error: &MError) -> ContactError {
    match status {
        410 => ContactError::SyncTokenExpired,
        400 if error.is_expired_delta() => ContactError::SyncTokenExpired,
        403 if error.is_access_denied() => ContactError::MissingScope,
        404 => ContactError::NotFound,
        429 => ContactError::Server { status },
        status if status >= 500 => ContactError::Server { status },
        status => ContactError::Rejected {
            status,
            message: if error.message.is_empty() {
                "no explanation".to_owned()
            } else {
                error.message.clone()
            },
        },
    }
}

fn into_error(failure: Failure) -> ContactError {
    match failure {
        Failure::Error(err) => err,
        Failure::Api { status, error } => classify(status, &error),
    }
}

/// The Microsoft Graph contacts client for one account. See the [module docs](self).
pub struct MicrosoftContacts {
    http: reqwest::Client,
    tokens: Arc<dyn AccessTokens>,
    account: Uuid,
    /// Another mailbox whose shared contacts this client reads; `None` for the
    /// account's own.
    mailbox: Option<String>,
    base: String,
    backoff: Backoff,
}

impl fmt::Debug for MicrosoftContacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MicrosoftContacts")
            .field("base", &self.base)
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

impl MicrosoftContacts {
    /// A client for the account with UUID `account` that gets its Graph tokens from
    /// `tokens` and talks to [`GRAPH_API`].
    ///
    /// # Errors
    ///
    /// [`ContactError::Offline`] if no HTTP client can be built (no TLS backend).
    pub fn new(tokens: Arc<dyn AccessTokens>, account: Uuid) -> Result<Self, ContactError> {
        let http = reqwest::Client::builder()
            .user_agent("Maze")
            .referer(false)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| ContactError::Offline {
                message: format!("no HTTP client: {err}"),
            })?;
        Ok(Self {
            http,
            tokens,
            account,
            mailbox: None,
            base: GRAPH_API.to_owned(),
            backoff: Backoff::default(),
        })
    }

    /// Talks to `base` instead of [`GRAPH_API`] (tests' mock server).
    #[must_use]
    pub fn with_base_url(mut self, base: &str) -> Self {
        self.base = if base.ends_with('/') {
            base.to_owned()
        } else {
            format!("{base}/")
        };
        self
    }

    /// Retries with `backoff`.
    #[must_use]
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Reads the shared contacts of `mailbox` instead of the account's own
    /// ([`BookKind::MicrosoftShared`]); give it the account's shared-contacts tokens
    /// ([`SharedContactTokens`](crate::SharedContactTokens)).
    ///
    /// # Errors
    ///
    /// [`ContactError::NotFound`] for a mailbox that isn't a plain email address.
    pub fn with_mailbox(mut self, mailbox: &str) -> Result<Self, ContactError> {
        let mailbox = mailbox.trim().to_ascii_lowercase();
        if !is_valid_mailbox(&mailbox) {
            return Err(ContactError::NotFound);
        }
        self.mailbox = Some(mailbox);
        Ok(self)
    }

    /// Whose contacts: `me` or `users/<mailbox>`.
    fn owner(&self) -> String {
        match &self.mailbox {
            Some(mailbox) => format!("users/{mailbox}"),
            None => "me".to_owned(),
        }
    }

    /// The book this client lists.
    fn book(&self) -> BookKind {
        if self.mailbox.is_some() {
            BookKind::MicrosoftShared
        } else {
            BookKind::Microsoft
        }
    }

    /// A contact as a [`Person`]: photo references only for the account's own.
    fn person(&self, contact: MContact) -> Person {
        let mut person = contact.into_person(self.account);
        if self.mailbox.is_some() {
            person.photo_url = None;
        }
        person
    }

    /// The API root every request goes to (`<base>v1.0/`).
    fn root(&self) -> String {
        format!("{}v1.0/", self.base)
    }

    fn url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url, ContactError> {
        let mut url = Url::parse(&format!("{}{path}", self.root()))
            .map_err(|err| ContactError::InvalidResponse(format!("bad API URL: {err}")))?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }
        Ok(url)
    }

    /// A `@odata.nextLink` or `@odata.deltaLink` (a stored token or one from an
    /// answer) as a URL, only if it points at the API root: a token can't send the
    /// account's Graph token anywhere else.
    fn link(&self, link: &str) -> Result<Url, ContactError> {
        let root = self.root();
        let url = Url::parse(link)
            .map_err(|_| ContactError::InvalidResponse("a page link isn't a URL".to_owned()))?;
        if !url.as_str().starts_with(&root) || url.cannot_be_a_base() {
            return Err(ContactError::InvalidResponse(
                "a page link points away from Microsoft Graph".to_owned(),
            ));
        }
        Ok(url)
    }

    /// Sends a request with the account's token: retries a 401 once with a fresh
    /// token, and backs off on rate limits and server errors as `retry` allows.
    async fn execute(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        headers: &[(reqwest::header::HeaderName, String)],
        retry: Retry,
    ) -> Result<Answer, Failure> {
        let attempts = self.backoff.attempts.max(1);
        let mut attempt = 0;
        let mut refreshed = false;
        loop {
            attempt += 1;
            let token = self.tokens.token().await.map_err(ContactError::from_auth)?;
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .bearer_auth(token.expose());
            for (name, value) in headers {
                request = request.header(name, value);
            }
            if let Some(body) = &body {
                request = request
                    .header(CONTENT_TYPE, "application/json")
                    .body(body.clone());
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(err) => {
                    if retry == Retry::Read && attempt < attempts {
                        tokio::time::sleep(self.backoff.delay(attempt)).await;
                        continue;
                    }
                    return Err(ContactError::network(&err).into());
                }
            };
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase())
                .unwrap_or_default();
            let bytes = response
                .bytes()
                .await
                .map_err(|err| ContactError::network(&err))?;
            if (200..300).contains(&status) {
                return Ok(Answer {
                    bytes: bytes.to_vec(),
                    content_type,
                });
            }
            if status == 401 {
                if refreshed {
                    return Err(ContactError::Unauthorized.into());
                }
                tracing::debug!("Microsoft Graph rejected the access token; refreshing it");
                self.tokens.invalidate().await;
                refreshed = true;
                attempt -= 1;
                continue;
            }
            let error = MError::parse(&bytes);
            let retryable = status == 429 || (retry == Retry::Read && status >= 500);
            if retryable && attempt < attempts {
                let wait = retry_after
                    .unwrap_or_else(|| self.backoff.delay(attempt))
                    .min(self.backoff.max);
                tracing::debug!(
                    status,
                    wait_ms = wait.as_millis() as u64,
                    "Microsoft Graph busy; backing off"
                );
                tokio::time::sleep(wait).await;
                continue;
            }
            return Err(Failure::Api { status, error });
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: Url,
        headers: &[(reqwest::header::HeaderName, String)],
    ) -> Result<T, ContactError> {
        let answer = self
            .execute(Method::GET, url, None, headers, Retry::Read)
            .await
            .map_err(into_error)?;
        parse(&answer.bytes)
    }

    /// The current version of contact `id`.
    pub async fn get(&self, id: &str) -> Result<Person, ContactError> {
        if !is_valid_id(id) {
            return Err(ContactError::NotFound);
        }
        let url = self.url(
            &format!("{}/contacts/{id}", self.owner()),
            &[("$select", SELECT)],
        )?;
        let contact: MContact = self.get_json(url, &[]).await?;
        Ok(self.person(contact))
    }

    /// The id of the default Contacts folder, from a contact in it; `None` while it
    /// is empty.
    async fn default_folder(&self) -> Result<Option<String>, ContactError> {
        let url = self.url(
            &format!("{}/contacts", self.owner()),
            &[("$top", "1"), ("$select", "id,parentFolderId")],
        )?;
        let page: MPage = self.get_json(url, &[]).await?;
        Ok(page
            .value
            .into_iter()
            .find_map(|c| c.parent_folder_id)
            .filter(|id| is_valid_id(id)))
    }

    async fn conflict(&self, id: &str) -> ContactError {
        match self.get(id).await {
            Ok(current) => ContactError::Conflict(Box::new(current)),
            Err(err) => err,
        }
    }
}

fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ContactError> {
    serde_json::from_slice(bytes).map_err(|err| {
        // serde's message names a field or position, never a value.
        ContactError::InvalidResponse(err.to_string())
    })
}

#[async_trait]
impl ContactSource for MicrosoftContacts {
    async fn list(
        &self,
        book: BookKind,
        sync_token: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<SyncPage, ContactError> {
        if book != self.book() {
            return Err(ContactError::ReadOnly);
        }
        let url = match (page_token, sync_token) {
            (Some(next), _) => self.link(next)?,
            (None, Some(delta)) => self.link(delta).map_err(|err| {
                // A stored token that doesn't fit (another base): start over.
                tracing::debug!(%err, "discarding a stored delta link");
                ContactError::SyncTokenExpired
            })?,
            (None, None) => match self.default_folder().await? {
                Some(folder) => self.url(
                    &format!("{}/contactFolders/{folder}/contacts/delta", self.owner()),
                    &[("$select", SELECT)],
                )?,
                None => return Ok(SyncPage::default()),
            },
        };
        let prefer = [(
            reqwest::header::HeaderName::from_static("prefer"),
            format!("odata.maxpagesize={PAGE_SIZE}"),
        )];
        let page: MPage = self.get_json(url, &prefer).await?;
        let mut out = SyncPage::default();
        if let Some(next) = page.next_link.filter(|l| !l.is_empty()) {
            self.link(&next)?;
            out.next_page_token = Some(next);
        } else if let Some(delta) = page.delta_link.filter(|l| !l.is_empty()) {
            self.link(&delta)?;
            out.next_sync_token = Some(delta);
        }
        for contact in page.value {
            let person = self.person(contact);
            if !is_valid_id(&person.resource_name) {
                continue;
            }
            if person.deleted {
                out.deletes.push(person.resource_name);
            } else {
                out.upserts.push(person);
            }
        }
        Ok(out)
    }

    async fn create(&self, fields: &ContactFields) -> Result<Person, ContactError> {
        if self.mailbox.is_some() {
            return Err(ContactError::ReadOnly);
        }
        if fields.is_empty() {
            return Err(ContactError::Empty);
        }
        let url = self.url("me/contacts", &[])?;
        let body = serde_json::to_vec(&MContactWrite::from_fields(fields))
            .map_err(|err| ContactError::InvalidResponse(err.to_string()))?;
        let answer = self
            .execute(Method::POST, url, Some(body), &[], Retry::Write)
            .await
            .map_err(into_error)?;
        Ok(parse::<MContact>(&answer.bytes)?.into_person(self.account))
    }

    async fn update(
        &self,
        resource_name: &str,
        etag: &str,
        fields: &ContactFields,
    ) -> Result<Person, ContactError> {
        if self.mailbox.is_some() {
            return Err(ContactError::ReadOnly);
        }
        if !is_valid_id(resource_name) {
            return Err(ContactError::NotFound);
        }
        if fields.is_empty() {
            return Err(ContactError::Empty);
        }
        // The current version tells whether someone else changed the contact
        // meanwhile; If-Match guards the write itself.
        let current = self.get(resource_name).await?;
        if current.etag.as_deref() != Some(etag) {
            return Err(ContactError::Conflict(Box::new(current)));
        }
        let url = self.url(&format!("me/contacts/{resource_name}"), &[])?;
        let body = serde_json::to_vec(&MContactWrite::from_fields(fields))
            .map_err(|err| ContactError::InvalidResponse(err.to_string()))?;
        match self
            .execute(
                Method::PATCH,
                url,
                Some(body),
                &[(IF_MATCH, etag.to_owned())],
                Retry::Write,
            )
            .await
        {
            Ok(answer) => Ok(parse::<MContact>(&answer.bytes)?.into_person(self.account)),
            Err(Failure::Api { status: 412, .. }) => Err(self.conflict(resource_name).await),
            Err(Failure::Api { status: 409, error }) if error.is_conflict() => {
                Err(self.conflict(resource_name).await)
            }
            Err(failure) => Err(into_error(failure)),
        }
    }

    async fn photo(&self, url: &str) -> Result<Option<Photo>, ContactError> {
        if self.mailbox.is_some() {
            return Ok(None);
        }
        let Some((account, id)) = parse_photo_url(url) else {
            return Ok(None);
        };
        if account != self.account {
            return Ok(None);
        }
        let url = self.url(&format!("me/contacts/{id}/photo/$value"), &[])?;
        match self.execute(Method::GET, url, None, &[], Retry::Read).await {
            Ok(answer) => {
                let ok = answer.content_type.starts_with("image/")
                    && !answer.bytes.is_empty()
                    && answer.bytes.len() <= PHOTO_MAX_BYTES;
                Ok(ok.then_some(Photo {
                    content_type: answer.content_type,
                    bytes: answer.bytes,
                }))
            }
            Err(Failure::Api { status: 404, .. }) => Ok(None),
            Err(failure) => Err(into_error(failure)),
        }
    }
}
