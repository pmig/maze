//! Google contacts over the People API (`people.googleapis.com/v1`).
//!
//! - **My Contacts**: `people.connections.list` with
//!   `personFields=names,emailAddresses,phoneNumbers,organizations,addresses,urls,biographies,photos,metadata`,
//!   `pageSize=1000` and `requestSyncToken=true`; later calls pass the `syncToken` and
//!   get only the changes (deleted people with `metadata.deleted`). An expired token
//!   (HTTP 400 `EXPIRED_SYNC_TOKEN`, or 410) is [`ContactError::SyncTokenExpired`].
//! - **Other contacts**: `otherContacts.list` with
//!   `readMask=names,emailAddresses,phoneNumbers,photos,metadata`, its own sync token.
//!   Read-only.
//! - **Writes**: `people:createContact`; `people/…:updateContact` with the person's
//!   `etag`, its contact source and `updatePersonFields`. Before an update the current
//!   person is read: a different etag, or the server's `FAILED_PRECONDITION`, is
//!   [`ContactError::Conflict`] with the current version.
//! - **Photos**: the photo URL with `=s96`, fetched without credentials, cookies or
//!   `Referer`, images up to [`PHOTO_MAX_BYTES`].
//!
//! HTTP 401 forgets the access token ([`AccessTokens::invalidate`]) and retries once;
//! 403 for missing scopes is [`ContactError::MissingScope`] (mail is unaffected);
//! 429 and 5xx back off ([`Backoff`], honoring `Retry-After`), for writes only 429
//! (a create that may have reached the server is never sent twice). Requests are
//! sequential: one per account at a time.

pub(crate) mod json;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Method;
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use url::Url;

use crate::error::ContactError;
use crate::model::{BookKind, ContactFields, Person, Photo};
use crate::source::{AccessTokens, ContactSource, SyncPage};

use json::{GConnections, GError, GOtherContacts, GPerson, GPersonMetadata, GSource};

/// The People API.
pub const PEOPLE_API: &str = "https://people.googleapis.com/";

/// The fields read for My Contacts.
pub const PERSON_FIELDS: &str =
    "names,emailAddresses,phoneNumbers,organizations,addresses,urls,biographies,photos,metadata";

/// The fields read for Other contacts (all `otherContacts.list` allows that Maze uses).
pub const OTHER_CONTACTS_READ_MASK: &str = "names,emailAddresses,phoneNumbers,photos,metadata";

/// The fields an update replaces.
pub const UPDATE_PERSON_FIELDS: &str =
    "names,emailAddresses,phoneNumbers,organizations,addresses,urls,biographies";

/// People per page (the API's maximum).
pub const PAGE_SIZE: u32 = 1000;

/// The most a contact photo may weigh.
pub const PHOTO_MAX_BYTES: usize = 1024 * 1024;

/// How rate limits and server errors are retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Attempts in total (at least 1).
    pub attempts: u32,
    /// The wait after the first failure; doubled each time.
    pub initial: Duration,
    /// The longest wait (also caps `Retry-After`).
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            attempts: 4,
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

impl Backoff {
    fn delay(&self, attempt: u32) -> Duration {
        let factor = 2u32.saturating_pow(attempt.saturating_sub(1));
        self.initial.saturating_mul(factor).min(self.max)
    }
}

/// What may be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Reads: network errors, 429 and 5xx.
    Read,
    /// Writes: only 429 (the server didn't act on it).
    Write,
}

/// A failed request: already an error, or Google's error answer to classify.
enum Failure {
    Error(ContactError),
    Api { status: u16, error: GError },
}

impl From<ContactError> for Failure {
    fn from(err: ContactError) -> Self {
        Self::Error(err)
    }
}

/// An error answer as a [`ContactError`].
fn classify(status: u16, error: &GError) -> ContactError {
    match status {
        410 => ContactError::SyncTokenExpired,
        400 if error.is_expired_sync_token() => ContactError::SyncTokenExpired,
        403 if error.is_missing_scope() => ContactError::MissingScope,
        403 if error.is_api_disabled() => ContactError::ApiDisabled,
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

/// The Google People API client for one account. See the [module docs](self).
pub struct GoogleContacts {
    http: reqwest::Client,
    photos: reqwest::Client,
    tokens: Arc<dyn AccessTokens>,
    base: String,
    backoff: Backoff,
}

impl fmt::Debug for GoogleContacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleContacts")
            .field("base", &self.base)
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

impl GoogleContacts {
    /// A client that gets its tokens from `tokens` and talks to [`PEOPLE_API`].
    ///
    /// # Errors
    ///
    /// [`ContactError::Offline`] if no HTTP client can be built (no TLS backend).
    pub fn new(tokens: Arc<dyn AccessTokens>) -> Result<Self, ContactError> {
        let no_client = |err: reqwest::Error| ContactError::Offline {
            message: format!("no HTTP client: {err}"),
        };
        let http = reqwest::Client::builder()
            .user_agent("Maze")
            .referer(false)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(no_client)?;
        let photos = photo_client()?;
        Ok(Self {
            http,
            photos,
            tokens,
            base: PEOPLE_API.to_owned(),
            backoff: Backoff::default(),
        })
    }

    /// Talks to `base` instead of [`PEOPLE_API`] (tests' mock server).
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

    fn url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url, ContactError> {
        let mut url = Url::parse(&format!("{}v1/{path}", self.base))
            .map_err(|err| ContactError::InvalidResponse(format!("bad API URL: {err}")))?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
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
        retry: Retry,
    ) -> Result<Vec<u8>, Failure> {
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
            let bytes = response
                .bytes()
                .await
                .map_err(|err| ContactError::network(&err))?;
            if (200..300).contains(&status) {
                return Ok(bytes.to_vec());
            }
            if status == 401 {
                if refreshed {
                    return Err(ContactError::Unauthorized.into());
                }
                tracing::debug!("the People API rejected the access token; refreshing it");
                self.tokens.invalidate().await;
                refreshed = true;
                attempt -= 1;
                continue;
            }
            let error = GError::parse(&bytes);
            let rate_limited = status == 429 || (status == 403 && error.is_rate_limit());
            let retryable = rate_limited || (retry == Retry::Read && status >= 500);
            if retryable && attempt < attempts {
                let wait = retry_after
                    .unwrap_or_else(|| self.backoff.delay(attempt))
                    .min(self.backoff.max);
                tracing::debug!(
                    status,
                    wait_ms = wait.as_millis() as u64,
                    "People API busy; backing off"
                );
                tokio::time::sleep(wait).await;
                continue;
            }
            if rate_limited {
                return Err(ContactError::Server { status: 429 }.into());
            }
            return Err(Failure::Api { status, error });
        }
    }

    fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ContactError> {
        serde_json::from_slice(bytes).map_err(|err| {
            // serde's message names a field or position, never a value.
            ContactError::InvalidResponse(err.to_string())
        })
    }

    /// The current version of contact `resource_name`.
    pub async fn get(&self, resource_name: &str) -> Result<Person, ContactError> {
        check_resource_name(resource_name)?;
        let url = self.url(resource_name, &[("personFields", PERSON_FIELDS)])?;
        let bytes = self
            .execute(Method::GET, url, None, Retry::Read)
            .await
            .map_err(into_error)?;
        Ok(Self::parse::<GPerson>(&bytes)?.into_person())
    }

    async fn list_connections(
        &self,
        sync_token: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<SyncPage, ContactError> {
        let page_size = PAGE_SIZE.to_string();
        let mut query = vec![
            ("personFields", PERSON_FIELDS),
            ("pageSize", page_size.as_str()),
            ("requestSyncToken", "true"),
        ];
        if let Some(token) = sync_token {
            query.push(("syncToken", token));
        }
        if let Some(token) = page_token {
            query.push(("pageToken", token));
        }
        let url = self.url("people/me/connections", &query)?;
        let bytes = self
            .execute(Method::GET, url, None, Retry::Read)
            .await
            .map_err(into_error)?;
        let list: GConnections = Self::parse(&bytes)?;
        Ok(page(
            list.connections,
            list.next_page_token,
            list.next_sync_token,
        ))
    }

    async fn list_other(
        &self,
        sync_token: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<SyncPage, ContactError> {
        let page_size = PAGE_SIZE.to_string();
        let mut query = vec![
            ("readMask", OTHER_CONTACTS_READ_MASK),
            ("pageSize", page_size.as_str()),
            ("requestSyncToken", "true"),
        ];
        if let Some(token) = sync_token {
            query.push(("syncToken", token));
        }
        if let Some(token) = page_token {
            query.push(("pageToken", token));
        }
        let url = self.url("otherContacts", &query)?;
        let bytes = self
            .execute(Method::GET, url, None, Retry::Read)
            .await
            .map_err(into_error)?;
        let list: GOtherContacts = Self::parse(&bytes)?;
        Ok(page(
            list.other_contacts,
            list.next_page_token,
            list.next_sync_token,
        ))
    }

    async fn conflict(&self, resource_name: &str) -> ContactError {
        match self.get(resource_name).await {
            Ok(current) => ContactError::Conflict(Box::new(current)),
            Err(err) => err,
        }
    }
}

/// A list response as a [`SyncPage`]: deleted people apart.
fn page(
    people: Vec<GPerson>,
    next_page_token: Option<String>,
    next_sync_token: Option<String>,
) -> SyncPage {
    let mut out = SyncPage {
        next_page_token: next_page_token.filter(|t| !t.is_empty()),
        next_sync_token: next_sync_token.filter(|t| !t.is_empty()),
        ..SyncPage::default()
    };
    for person in people {
        let person = person.into_person();
        if person.resource_name.is_empty() {
            continue;
        }
        if person.deleted {
            out.deletes.push(person.resource_name);
        } else {
            out.upserts.push(person);
        }
    }
    out
}

/// Only `people/<id>` names are accepted for reads and writes of single contacts,
/// so a stored value can't point a request elsewhere.
fn check_resource_name(resource_name: &str) -> Result<(), ContactError> {
    let ok = resource_name.strip_prefix("people/").is_some_and(|id| {
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    });
    if ok {
        Ok(())
    } else {
        Err(ContactError::NotFound)
    }
}

#[async_trait]
impl ContactSource for GoogleContacts {
    async fn list(
        &self,
        book: BookKind,
        sync_token: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<SyncPage, ContactError> {
        match book {
            BookKind::Google => self.list_connections(sync_token, page_token).await,
            BookKind::GoogleOther => self.list_other(sync_token, page_token).await,
        }
    }

    async fn create(&self, fields: &ContactFields) -> Result<Person, ContactError> {
        if fields.is_empty() {
            return Err(ContactError::Empty);
        }
        let url = self.url("people:createContact", &[("personFields", PERSON_FIELDS)])?;
        let body = serde_json::to_vec(&GPerson::from_fields(fields))
            .map_err(|err| ContactError::InvalidResponse(err.to_string()))?;
        let bytes = self
            .execute(Method::POST, url, Some(body), Retry::Write)
            .await
            .map_err(into_error)?;
        Ok(Self::parse::<GPerson>(&bytes)?.into_person())
    }

    async fn update(
        &self,
        resource_name: &str,
        etag: &str,
        fields: &ContactFields,
    ) -> Result<Person, ContactError> {
        check_resource_name(resource_name)?;
        if fields.is_empty() {
            return Err(ContactError::Empty);
        }
        // The update has to name the contact source (with its etag); the current
        // version also tells whether someone else changed the contact meanwhile.
        let current = self.get(resource_name).await?;
        if current.etag.as_deref() != Some(etag) {
            return Err(ContactError::Conflict(Box::new(current)));
        }
        let sources: Vec<GSource> = current
            .sources
            .iter()
            .filter(|s| s.kind == "CONTACT")
            .map(|s| GSource {
                kind: s.kind.clone(),
                id: s.id.clone(),
                etag: s.etag.clone(),
            })
            .collect();
        if sources.is_empty() {
            // A profile-only person (no contact source) can't be edited.
            return Err(ContactError::ReadOnly);
        }
        let mut person = GPerson::from_fields(fields);
        person.resource_name = Some(resource_name.to_owned());
        person.etag = Some(etag.to_owned());
        person.metadata = Some(GPersonMetadata {
            sources,
            deleted: None,
        });
        let url = self.url(
            &format!("{resource_name}:updateContact"),
            &[
                ("updatePersonFields", UPDATE_PERSON_FIELDS),
                ("personFields", PERSON_FIELDS),
            ],
        )?;
        let body = serde_json::to_vec(&person)
            .map_err(|err| ContactError::InvalidResponse(err.to_string()))?;
        match self
            .execute(Method::PATCH, url, Some(body), Retry::Write)
            .await
        {
            Ok(bytes) => Ok(Self::parse::<GPerson>(&bytes)?.into_person()),
            Err(Failure::Api { status: 400, error }) if error.is_failed_precondition() => {
                Err(self.conflict(resource_name).await)
            }
            Err(failure) => Err(into_error(failure)),
        }
    }

    async fn photo(&self, url: &str) -> Result<Option<Photo>, ContactError> {
        fetch_photo_with(&self.photos, url).await
    }
}

fn photo_client() -> Result<reqwest::Client, ContactError> {
    // The same for every Maze user, no credentials, no cookies, no Referer.
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .referer(false)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .map_err(|err| ContactError::Offline {
            message: format!("no HTTP client: {err}"),
        })
}

/// Fetches a contact photo (a URL from [`Person::photo_url`]) without credentials,
/// cookies or `Referer`: `None` if there is no image behind it (404, not an image,
/// larger than [`PHOTO_MAX_BYTES`]). Google's photo URLs need no sign-in.
pub async fn fetch_photo(url: &str) -> Result<Option<Photo>, ContactError> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| photo_client().ok()).as_ref();
    match client {
        Some(client) => fetch_photo_with(client, url).await,
        None => Err(ContactError::Offline {
            message: "no HTTP client".to_owned(),
        }),
    }
}

async fn fetch_photo_with(
    client: &reqwest::Client,
    url: &str,
) -> Result<Option<Photo>, ContactError> {
    let Ok(url) = Url::parse(url) else {
        return Ok(None);
    };
    if !matches!(url.scheme(), "https" | "http") {
        return Ok(None);
    }
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| ContactError::network(&err))?;
    let status = response.status().as_u16();
    match status {
        200..=299 => {}
        403 | 404 | 410 => return Ok(None),
        429 | 500.. => return Err(ContactError::Server { status }),
        status => {
            return Err(ContactError::Rejected {
                status,
                message: "the photo couldn't be loaded".to_owned(),
            });
        }
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase())
        .unwrap_or_default();
    if !content_type.starts_with("image/") {
        return Ok(None);
    }
    if response
        .content_length()
        .is_some_and(|len| len > PHOTO_MAX_BYTES as u64)
    {
        return Ok(None);
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|err| ContactError::network(&err))?;
    if bytes.is_empty() || bytes.len() > PHOTO_MAX_BYTES {
        return Ok(None);
    }
    Ok(Some(Photo {
        content_type,
        bytes: bytes.to_vec(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_names_are_checked() {
        assert!(check_resource_name("people/c123").is_ok());
        assert!(check_resource_name("people/c1_2-3").is_ok());
        assert!(check_resource_name("people/").is_err());
        assert!(check_resource_name("people/../x").is_err());
        assert!(check_resource_name("otherContacts/c1").is_err());
        assert!(check_resource_name("people/c1?x=1").is_err());
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let backoff = Backoff {
            attempts: 5,
            initial: Duration::from_secs(1),
            max: Duration::from_secs(5),
        };
        assert_eq!(backoff.delay(1), Duration::from_secs(1));
        assert_eq!(backoff.delay(2), Duration::from_secs(2));
        assert_eq!(backoff.delay(3), Duration::from_secs(4));
        assert_eq!(backoff.delay(4), Duration::from_secs(5));
    }
}
