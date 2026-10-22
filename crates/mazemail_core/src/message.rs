//! Opening a message: [`MessageView`] (PLAN §4.7, §4.8, CONCEPT §8.4).
//!
//! # Rendering and the reader cache
//!
//! The body is rendered once per blob and renderer version: parse
//! ([`mazemail_mime::parse`]), sanitize and build the reader document
//! ([`mazemail_mime::html::prepare`], or the plain-text reader), detect the language.
//! The result is stored encrypted in `mail/cache/` under the blob id, the kind
//! `"reader"` and [`RENDERER_VERSION`], so the next open only decrypts and decodes it
//! (CONCEPT §3: a cached message opens in < 30 ms). Bump [`RENDERER_VERSION`] whenever
//! the rendering changes; old entries are then simply never read again.
//!
//! # Missing bodies
//!
//! A message without a stored body (headers only, or a blob file that went missing
//! after a rollback) opens with [`Body::Pending`]; core asks the sync seam to fetch it
//! with [`FetchPriority::Interactive`](crate::FetchPriority::Interactive). The arrival
//! shows up as a message change in the event batches.
//!
//! # Sender verification
//!
//! The [`AuthReport`] comes from `message.auth_json` (written by sync from trusted
//! provider headers, or by an earlier check), else from the encrypted cache, else it is
//! computed lazily in the background with [`security::Verifier`] and persisted, and the
//! view says [`AuthStatus::Pending`] until then. Reports with temporary errors or an
//! older schema are recomputed.
//!
//! The store has no setter for `auth_json` yet, so a computed report is written through
//! [`store::WriteTx::upsert_message`] for messages the store can identify (a Gmail
//! message id or a server UID) and to the cache (kind `"auth"`) otherwise.

use std::sync::Arc;

use mazemail_mime::html::{SanitizeOptions, SanitizedHtml};
use mazemail_mime::{AttachmentMeta, Lang, ReaderDoc};
use mazemail_types::{AccountId, Address, BlobId, Flags, FolderId, MessageId, TagId, ThreadId};
use security::AuthReport;
use security::phishing::{KnownContacts, Warning, normalize_display_name};
use serde::{Deserialize, Serialize};
use settings::RemoteContentPolicy;
use store::{
    BlobStore, MessageDetail, NewLocation, NewMessage, Queries, Reader, Store, ThreadAssignment,
};

use crate::{Error, Result};

/// Version of the rendering pipeline; part of the reader cache key.
pub const RENDERER_VERSION: u32 = 1;

/// Cache kind of rendered bodies.
pub(crate) const READER_CACHE_KIND: &str = "reader";

/// Cache kind of sender verification reports that couldn't go into the database.
pub(crate) const AUTH_CACHE_KIND: &str = "auth";

/// A rendered body, as cached.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedBody {
    /// The reader document (from the HTML body if there is one, else the text body).
    pub reader: ReaderDoc,
    /// The sanitized HTML, for "Open in browser"; `None` for plain-text messages.
    pub sanitized_html: Option<SanitizedHtml>,
    /// Every non-body MIME part.
    pub attachments: Vec<AttachmentMeta>,
    /// `Reply-To` addresses.
    pub reply_to: Vec<Address>,
    /// `List-Unsubscribe` URIs.
    pub list_unsubscribe: Vec<String>,
    /// The detected language.
    pub language: Option<Lang>,
}

impl std::fmt::Debug for RenderedBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderedBody")
            .field("blocks", &self.reader.blocks.len())
            .field("html", &self.sanitized_html.is_some())
            .field("attachments", &self.attachments.len())
            .field("language", &self.language)
            .finish_non_exhaustive()
    }
}

impl RenderedBody {
    /// Renders a raw message.
    pub fn render(raw: &[u8]) -> Result<Self, mazemail_mime::ParseError> {
        let parsed = mazemail_mime::parse(raw)?;
        let (reader, sanitized_html) = match parsed.prepare_html(&SanitizeOptions::default()) {
            Some(prepared) => (prepared.reader, Some(prepared.sanitized)),
            None => (parsed.reader_doc(), None),
        };
        Ok(Self {
            reader,
            sanitized_html,
            language: parsed.language(),
            reply_to: parsed.headers.reply_to,
            list_unsubscribe: parsed.headers.list_unsubscribe,
            attachments: parsed.attachments,
        })
    }
}

/// The body of an opened message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// Rendered and ready.
    Ready(Arc<RenderedBody>),
    /// Not stored yet; a fetch has been requested.
    Pending,
    /// The stored body can't be read (damaged or not a message).
    Unreadable,
}

/// Sender verification of an opened message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStatus {
    /// The report.
    Ready(Box<AuthReport>),
    /// Being computed (or waiting for the body); a message change event follows.
    Pending,
    /// Can't be checked (no DNS resolver available).
    Unavailable,
}

/// Remote content of an opened message (CONCEPT §15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteContent {
    /// The policy for this account.
    pub policy: RemoteContentPolicy,
    /// Whether the message references remote resources.
    pub has_remote_resources: bool,
    /// Tracking pixels removed by the sanitizer.
    pub trackers_removed: usize,
    /// Whether remote resources stay blocked (the UI shows "load remote content" for
    /// [`RemoteContentPolicy::Ask`]).
    pub blocked: bool,
}

/// Everything the message view shows.
#[derive(Clone, PartialEq, Eq)]
pub struct MessageView {
    pub id: MessageId,
    pub thread: ThreadId,
    pub account: AccountId,
    pub subject: Option<String>,
    pub from: Option<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// `Date:` header, Unix seconds.
    pub date: i64,
    /// Server receive time, Unix seconds.
    pub received: Option<i64>,
    pub rfc_message_id: Option<String>,
    pub list_id: Option<String>,
    /// Effective flags (`\Seen` only if every copy is seen).
    pub flags: Flags,
    pub tags: Vec<TagId>,
    /// Folders (and Gmail labels) the message is in.
    pub folders: Vec<FolderId>,
    pub has_attachments: bool,
    pub body: Body,
    pub auth: AuthStatus,
    /// Phishing warnings, most serious first (empty until the body is ready).
    pub warnings: Vec<Warning>,
    pub remote_content: RemoteContent,
}

impl std::fmt::Debug for MessageView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageView")
            .field("id", &self.id)
            .field("thread", &self.thread)
            .field("account", &self.account)
            .field("flags", &self.flags)
            .field("body", &self.body)
            .field("auth", &self.auth)
            .field("warnings", &self.warnings)
            .finish_non_exhaustive()
    }
}

impl MessageView {
    /// The rendered body, if ready.
    pub fn rendered(&self) -> Option<&RenderedBody> {
        match &self.body {
            Body::Ready(body) => Some(body),
            Body::Pending | Body::Unreadable => None,
        }
    }
}

/// Where a rendered body came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderSource {
    Cache,
    Rendered,
}

/// The rendered body of `blob`: from the cache, or rendered and cached. `None` if the
/// blob isn't a readable message.
pub(crate) fn load_rendered(
    blobs: &BlobStore,
    blob: &BlobId,
) -> Result<Option<(RenderedBody, RenderSource)>> {
    if let Some(bytes) = blobs.cache_get(blob, READER_CACHE_KIND, RENDERER_VERSION) {
        match serde_json::from_slice::<RenderedBody>(&bytes) {
            Ok(body) => return Ok(Some((body, RenderSource::Cache))),
            Err(err) => tracing::warn!(%err, "ignoring an undecodable reader cache entry"),
        }
    }
    let raw = blobs.read(blob)?;
    let Ok(body) = RenderedBody::render(&raw) else {
        return Ok(None);
    };
    match serde_json::to_vec(&body) {
        Ok(bytes) => {
            if let Err(err) = blobs.cache_put(blob, READER_CACHE_KIND, RENDERER_VERSION, &bytes) {
                tracing::warn!(%err, "writing the reader cache failed");
            }
        }
        Err(err) => tracing::warn!(%err, "encoding a reader cache entry failed"),
    }
    Ok(Some((body, RenderSource::Rendered)))
}

/// The best stored verification report: the database's, else the cached one, unless
/// it should be recomputed.
pub(crate) fn stored_auth(blobs: &BlobStore, detail: &MessageDetail) -> Option<AuthReport> {
    let from_db = detail
        .auth_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<AuthReport>(json).ok())
        .filter(|report| !report.needs_recheck());
    from_db.or_else(|| {
        let blob = detail.blob?;
        let bytes = blobs.cache_get(&blob, AUTH_CACHE_KIND, security::AUTH_REPORT_SCHEMA)?;
        serde_json::from_slice::<AuthReport>(&bytes)
            .ok()
            .filter(|report| !report.needs_recheck())
    })
}

/// Persists a computed report: into `message.auth_json` when the store can identify the
/// message by Gmail id or UID, else into the cache. Returns whether it went into the
/// database.
pub(crate) async fn persist_auth(
    store: &Store,
    detail: &MessageDetail,
    report: &AuthReport,
) -> Result<bool> {
    let json = serde_json::to_string(report)
        .map_err(|err| Error::io("encoding a verification report")(err.into()))?;
    let id = detail.id;
    let db_json = json.clone();
    let stored = store
        .write(move |w| {
            let Some(current) = w.message(id)? else {
                return Ok(false);
            };
            if current.auth_json.is_some() {
                // `upsert_message` only fills empty fields; a stale report stays and the
                // fresh one goes to the cache.
                return Ok(false);
            }
            let location = current.locations.iter().find(|l| l.uid.is_some());
            if current.gm_msgid.is_none() && location.is_none() {
                return Ok(false);
            }
            let mut update = NewMessage::new(current.account, current.date);
            update.rfc_message_id = current.rfc_message_id.clone();
            update.gm_msgid = current.gm_msgid;
            update.auth_json = Some(db_json);
            update.thread = ThreadAssignment::Existing(current.thread);
            update.locations = location
                .map(|l| {
                    vec![NewLocation {
                        folder: l.folder,
                        uid: l.uid,
                        modseq: l.modseq,
                        flags: l.flags,
                        keywords: l.keywords.clone(),
                    }]
                })
                .unwrap_or_default();
            let outcome = w.upsert_message(&update)?;
            if outcome.created || outcome.message != id {
                // Rolls the job back: this must only ever update the same message.
                return Err(store::Error::Invalid(
                    "verification report matched another message",
                ));
            }
            Ok(true)
        })
        .await?;
    if !stored && let Some(blob) = detail.blob {
        store.blobs().cache_put(
            &blob,
            AUTH_CACHE_KIND,
            security::AUTH_REPORT_SCHEMA,
            json.as_bytes(),
        )?;
    }
    Ok(stored)
}

/// Known contacts from the address book, for display-name spoofing checks.
pub(crate) struct StoreContacts<'r> {
    pub(crate) reader: &'r Reader,
}

impl KnownContacts for StoreContacts<'_> {
    fn addresses_for(&self, normalized_name: &str) -> Vec<String> {
        let Some(first_word) = normalized_name.split_whitespace().next() else {
            return Vec::new();
        };
        match self.reader.search_contacts(first_word, 50) {
            Ok(contacts) => contacts
                .into_iter()
                .filter(|c| {
                    c.display_name
                        .as_deref()
                        .is_some_and(|name| normalize_display_name(name) == normalized_name)
                })
                .map(|c| c.email)
                .collect(),
            Err(err) => {
                tracing::warn!(%err, "looking up contacts for a phishing check");
                Vec::new()
            }
        }
    }
}
