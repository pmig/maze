//! Resources of a message: inline parts (`cid:` images) for the original HTML mode
//! (ADR-020) and attachments, decoded from the encrypted blob, and remote images
//! fetched by Maze itself when the message's remote content decision allows it.
//! Everything stays in memory; writing an attachment to a file is the UI's decision.
//!
//! An attachment needs the full message. Large messages may only have their text
//! parts stored ([`BodyState::TextParts`]); [`CoreHandle::attachment`] then has the sync
//! engine download the whole message first ([`FetchPriority::Full`]) and waits up to
//! [`ATTACHMENT_DOWNLOAD_TIMEOUT`].
//!
//! Remote fetches use their own HTTP client: no cookies, no `Referer`, a generic
//! `User-Agent`, a timeout and a size cap, images only, and only URLs the message
//! itself references (after the sanitizer dropped tracking pixels).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use mazemail_mime::html::{ResourceOrigin, rewrite_resources};
use mazemail_mime::{AttachmentPart, InlinePart};
use mazemail_types::{MessageId, PublicId as _};
use store::{BodyState, Queries as _};

use super::CoreHandle;
use crate::message::RemoteContent;
use crate::sync_bridge::FetchPriority;
use crate::{Error, Result};

/// The most a remote image may weigh.
pub const REMOTE_MAX_BYTES: usize = 5 * 1024 * 1024;

/// How long a remote image may take, in total (below the UI's wait for the core).
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(8);

/// How long [`CoreHandle::attachment`] waits for a message's full body to download.
pub const ATTACHMENT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// A remote image fetched for a message.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteResource {
    /// MIME type, lowercase (`image/png`).
    pub content_type: String,
    /// The bytes.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for RemoteResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteResource")
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

fn client() -> Option<&'static reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                // The same for every Maze user; no version, no platform.
                .user_agent("Mozilla/5.0")
                .referer(false)
                .connect_timeout(Duration::from_secs(5))
                .timeout(REMOTE_TIMEOUT)
                .redirect(reqwest::redirect::Policy::limited(3))
                .build()
                .map_err(|err| tracing::warn!(%err, "no HTTP client for remote images"))
                .ok()
        })
        .as_ref()
}

/// Whether `content_type` is an image the engine may get.
fn is_image(content_type: &str) -> bool {
    content_type.starts_with("image/")
}

impl CoreHandle {
    /// The part of message `id` with this Content-ID, decoded. `None` when the message
    /// has no such part or its body isn't stored. Inline parts belong to the message,
    /// so the remote content policy doesn't apply.
    pub async fn inline_part(
        &self,
        id: MessageId,
        content_id: String,
    ) -> Result<Option<InlinePart>> {
        self.ensure_open()?;
        let inner = Arc::clone(&self.inner);
        self.inner
            .runtime
            .blocking(move || {
                let reader = inner.store.reader()?;
                let detail = reader
                    .message(id)?
                    .ok_or_else(|| Error::not_found("message", id.to_public()))?;
                let Some(blob) = detail.blob else {
                    return Ok(None);
                };
                let raw = match inner.store.blobs().read(&blob) {
                    Ok(raw) => raw,
                    Err(store::Error::BlobMissing(_)) => return Ok(None),
                    Err(err) => return Err(err.into()),
                };
                Ok(mazemail_mime::inline_part(&raw, &content_id))
            })
            .await
    }

    /// The attachment of message `id` at `part_path` (`AttachmentMeta::part_path`),
    /// decoded. Downloads the full message first when only its text parts (or nothing)
    /// are stored. `None` when the message has no such attachment.
    ///
    /// # Errors
    ///
    /// [`Error::BodyUnavailable`] when the message couldn't be downloaded within
    /// [`ATTACHMENT_DOWNLOAD_TIMEOUT`] (offline, no sync engine).
    pub async fn attachment(
        &self,
        id: MessageId,
        part_path: String,
    ) -> Result<Option<AttachmentPart>> {
        self.ensure_open()?;
        if self.body_state(id)? != BodyState::Full {
            // Subscribe first, so the batch with the stored body can't be missed.
            let mut events = self.events();
            self.inner.sync.fetch_body(id, FetchPriority::Full);
            let deadline = tokio::time::Instant::now() + ATTACHMENT_DOWNLOAD_TIMEOUT;
            while self.body_state(id)? != BodyState::Full {
                match tokio::time::timeout_at(deadline, events.recv()).await {
                    Ok(Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) | Err(_) => {
                        return Err(Error::BodyUnavailable(
                            "The message couldn't be downloaded. Check your connection and \
                             try again."
                                .to_owned(),
                        ));
                    }
                }
            }
        }
        let inner = Arc::clone(&self.inner);
        self.inner
            .runtime
            .blocking(move || {
                let reader = inner.store.reader()?;
                let detail = reader
                    .message(id)?
                    .ok_or_else(|| Error::not_found("message", id.to_public()))?;
                let Some(blob) = detail.blob else {
                    return Ok(None);
                };
                let raw = inner.store.blobs().read(&blob)?;
                Ok(mazemail_mime::attachment_part(&raw, &part_path))
            })
            .await
    }

    /// How much of message `id`'s body is stored.
    fn body_state(&self, id: MessageId) -> Result<BodyState> {
        let detail = self
            .inner
            .store
            .reader()?
            .message(id)?
            .ok_or_else(|| Error::not_found("message", id.to_public()))?;
        Ok(detail.body_state)
    }

    /// Fetches remote image `url` of message `id`, if the message's remote content
    /// decision allows it (`once`: the user chose "Load images" for this message) and
    /// the message references that URL. `None` when it's not allowed, not referenced,
    /// not an image, too large or unreachable.
    pub async fn remote_resource(
        &self,
        id: MessageId,
        url: String,
        once: bool,
    ) -> Result<Option<RemoteResource>> {
        let view = self.message(id).await?;
        let Some(sanitized) = view.rendered().and_then(|r| r.sanitized_html.clone()) else {
            return Ok(None);
        };
        if !view.remote_content.allows_fetch(once) {
            return Ok(None);
        }
        let referenced = ResourceOrigin::new("https://message.mazemail.localhost")
            .map(|origin| {
                rewrite_resources(&sanitized.html, &origin)
                    .remote_resources
                    .contains(&url)
            })
            .unwrap_or(false);
        if !referenced {
            return Ok(None);
        }
        Ok(fetch_image(&url).await)
    }

    /// "Always for this sender": messages from `email` load remote content without
    /// asking (unless the policy is `never`). Kept in the encrypted database.
    pub async fn allow_remote_content(&self, email: String) -> Result<()> {
        self.ensure_open()?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        self.inner
            .store
            .write(move |w| w.allow_remote_content(&email, now))
            .await?;
        Ok(())
    }

    /// The remote content decision of message `id`.
    pub async fn remote_content(&self, id: MessageId) -> Result<RemoteContent> {
        Ok(self.message(id).await?.remote_content)
    }
}

async fn fetch_image(url: &str) -> Option<RemoteResource> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let mut response = client()?
        .get(parsed)
        .header(reqwest::header::ACCEPT, "image/*")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if !is_image(&content_type)
        || response
            .content_length()
            .is_some_and(|len| len > REMOTE_MAX_BYTES as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > REMOTE_MAX_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(RemoteResource {
        content_type,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_images_pass() {
        assert!(is_image("image/png"));
        assert!(!is_image("text/html"));
        assert!(!is_image("text/css"));
        assert!(!is_image(""));
    }
}
