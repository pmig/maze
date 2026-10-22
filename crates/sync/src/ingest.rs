//! The coupling to the shared ingest pipeline: [`MessageIngest`].
//!
//! New messages from every source (sync, import, local drafts) go through one
//! pipeline (crate `ingest`): parse, thread, security checks, index, store. The sync
//! engine only fetches; it hands every header (and later every body) to a
//! [`MessageIngest`]. `mazemail_core` plugs in the real pipeline; [`StoreIngest`] is a
//! small reference implementation that parses with `mazemail_mime` and writes to the
//! store directly, used by this crate's tests.

use std::fmt;

use async_trait::async_trait;
use bytes::Bytes;
use imap::{BodySummary, GmailMeta};
use mazemail_types::{AccountId, FolderId, MessageId, Sensitive};
use store::{BodyState, NewLocation, NewMessage, Store, ThreadAssignment};
use time::OffsetDateTime;

/// An error from the ingest pipeline. Must not contain message content.
#[derive(Debug)]
pub struct IngestError(Box<dyn std::error::Error + Send + Sync>);

impl IngestError {
    /// Wraps any error.
    pub fn new(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(err.into())
    }
}

impl fmt::Display for IngestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for IngestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

impl From<store::Error> for IngestError {
    fn from(err: store::Error) -> Self {
        Self::new(err)
    }
}

/// One message header from the server, to be ingested.
#[derive(Clone)]
pub struct HeaderInput {
    /// The account.
    pub account: AccountId,
    /// The mailbox copy the header came from: folder, UID, mod-sequence, flags and
    /// keywords.
    pub location: NewLocation,
    /// Gmail: the label folders from `X-GM-LABELS`, already resolved. The message's
    /// label locations are replaced by exactly these (with the location's flags and
    /// keywords). Empty and ignored for other servers (see `gmail`).
    pub labels: Vec<FolderId>,
    /// The raw RFC 5322 header block.
    pub header: Sensitive<Bytes>,
    /// MIME structure from `BODYSTRUCTURE`.
    pub body: Option<BodySummary>,
    /// `RFC822.SIZE`.
    pub size: u32,
    /// Gmail message id, thread id and labels (`None` on other servers). One message
    /// per `X-GM-MSGID`: ingesting it again from another mailbox adds a location.
    pub gmail: Option<GmailMeta>,
    /// `INTERNALDATE`.
    pub internal_date: Option<OffsetDateTime>,
}

impl fmt::Debug for HeaderInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeaderInput")
            .field("account", &self.account)
            .field("location", &self.location)
            .field("labels", &self.labels)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// The result of ingesting one header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ingested {
    /// The message (new or existing).
    pub message: MessageId,
    /// Whether a new message was created (false: a Gmail duplicate, a known UID or a
    /// detached message that was reattached).
    pub created: bool,
}

/// A request to reuse a body that is already on disk (Thunderbird's offline store,
/// PLAN §4.14) instead of downloading it.
#[derive(Clone)]
pub struct ReuseQuery {
    /// The message.
    pub message: MessageId,
    /// Its `Message-ID` (without angle brackets), if known.
    pub rfc_message_id: Option<String>,
    /// `RFC822.SIZE` as the server reports it.
    pub size: Option<u64>,
    /// The raw header block when the query is made during header sync; `None` when it
    /// is made on opening a message. The implementation computes its header
    /// fingerprint (Date, From, Subject) from this, or from the stored message.
    pub header: Option<Sensitive<Bytes>>,
}

impl fmt::Debug for ReuseQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReuseQuery")
            .field("message", &self.message)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// The ingest pipeline as the sync engine sees it.
///
/// All methods are async because the store's write API is: an implementation submits
/// store write jobs and awaits their commit. An `Ok` means the data is committed.
#[async_trait]
pub trait MessageIngest: Send + Sync + 'static {
    /// Ingests one header: parse, thread, store the message with its location(s).
    async fn ingest_headers(&self, input: HeaderInput) -> Result<Ingested, IngestError>;

    /// Ingests a batch of headers (up to [`AccountSyncConfig::header_batch`](crate::AccountSyncConfig::header_batch)),
    /// ideally in one store transaction. Returns one result per input, in order.
    ///
    /// The default calls [`ingest_headers`](Self::ingest_headers) for each input.
    async fn ingest_headers_batch(
        &self,
        inputs: Vec<HeaderInput>,
    ) -> Result<Vec<Ingested>, IngestError> {
        let mut out = Vec::with_capacity(inputs.len());
        for input in inputs {
            out.push(self.ingest_headers(input).await?);
        }
        Ok(out)
    }

    /// Stores a fetched body: the full raw message ([`BodyState::Full`]) or a
    /// synthetic MIME message holding only the text parts ([`BodyState::TextParts`]).
    async fn attach_body(
        &self,
        message: MessageId,
        raw: Bytes,
        state: BodyState,
    ) -> Result<(), IngestError>;

    /// Thunderbird body reuse (PLAN §4.14): the raw message if a local copy matches
    /// Message-ID, size and header fingerprint exactly. The default finds nothing.
    async fn lookup_reused_body(&self, query: &ReuseQuery) -> Result<Option<Bytes>, IngestError> {
        let _ = query;
        Ok(None)
    }
}

/// A simple [`MessageIngest`]: parses headers with `mazemail_mime` and writes to the
/// store directly. No security checks, no indexing.
///
/// It is what the sync engine's tests run against, and a reference for the real
/// pipeline: how a [`HeaderInput`] maps onto [`NewMessage`].
#[derive(Debug, Clone)]
pub struct StoreIngest {
    store: Store,
}

impl StoreIngest {
    /// Ingests into `store`.
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

/// Builds the store's [`NewMessage`] from a header input (parsing the header block).
pub fn new_message(input: &HeaderInput) -> NewMessage {
    let headers = mazemail_mime::parse_headers_only(input.header.expose()).ok();
    let internal = input.internal_date.map(OffsetDateTime::unix_timestamp);
    let date = headers
        .as_ref()
        .and_then(|h| h.date)
        .map(OffsetDateTime::unix_timestamp)
        .or(internal)
        .unwrap_or_default();
    let mut m = NewMessage::new(input.account, date);
    m.received = internal;
    m.size = Some(u64::from(input.size));
    m.has_attachments = input
        .body
        .as_ref()
        .is_some_and(BodySummary::has_attachments);
    if let Some(h) = headers {
        let subject_info = h
            .subject
            .as_deref()
            .map(mazemail_mime::subject::parse_subject);
        m.rfc_message_id = h.message_id;
        m.in_reply_to = h.in_reply_to.into_iter().next();
        m.references = h.references;
        m.from = h.from.into_iter().next();
        m.to = h.to;
        m.cc = h.cc;
        m.delivered_to = h.delivered_to.into_iter().next();
        m.list_id = h.list_id;
        m.subject_key = subject_info.as_ref().map(|s| s.key());
        m.thread = ThreadAssignment::Resolve {
            by_subject: subject_info.as_ref().is_some_and(|s| s.is_response()),
        };
        m.subject = h.subject;
    }
    if let Some(gmail) = &input.gmail {
        m.gm_msgid = Some(gmail.msg_id);
        m.gm_thrid = Some(gmail.thread_id);
        m.replace_labels = true;
    }
    m.locations.push(input.location.clone());
    if input.gmail.is_some() {
        for folder in &input.labels {
            m.locations.push(NewLocation {
                folder: *folder,
                uid: None,
                modseq: None,
                flags: input.location.flags,
                keywords: input.location.keywords.clone(),
            });
        }
    }
    m
}

#[async_trait]
impl MessageIngest for StoreIngest {
    async fn ingest_headers(&self, input: HeaderInput) -> Result<Ingested, IngestError> {
        let mut out = self.ingest_headers_batch(vec![input]).await?;
        out.pop()
            .ok_or_else(|| IngestError::new("ingest returned no result"))
    }

    async fn ingest_headers_batch(
        &self,
        inputs: Vec<HeaderInput>,
    ) -> Result<Vec<Ingested>, IngestError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let weight = inputs.len() * 4;
        let messages =
            tokio::task::spawn_blocking(move || inputs.iter().map(new_message).collect::<Vec<_>>())
                .await
                .map_err(IngestError::new)?;
        let outcomes = self
            .store
            .write_weighted(weight, move |w| {
                messages
                    .iter()
                    .map(|m| {
                        w.upsert_message(m).map(|o| Ingested {
                            message: o.message,
                            created: o.created,
                        })
                    })
                    .collect::<store::Result<Vec<_>>>()
            })
            .await?;
        Ok(outcomes)
    }

    async fn attach_body(
        &self,
        message: MessageId,
        raw: Bytes,
        state: BodyState,
    ) -> Result<(), IngestError> {
        let store = self.store.clone();
        let staged = tokio::task::spawn_blocking(move || store.blobs().put(&raw))
            .await
            .map_err(IngestError::new)??;
        self.store
            .write(move |w| w.set_message_body(message, &staged, state))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mazemail_types::Flags;

    use super::*;

    fn input(header: &str) -> HeaderInput {
        HeaderInput {
            account: AccountId(1),
            location: NewLocation::new(FolderId(2), Some(7), Flags::SEEN),
            labels: vec![FolderId(3)],
            header: Sensitive::new(Bytes::from(header.to_owned())),
            body: None,
            size: 1234,
            gmail: None,
            internal_date: OffsetDateTime::from_unix_timestamp(1_700_000_000).ok(),
        }
    }

    #[test]
    fn maps_headers_onto_a_new_message() {
        let m = new_message(&input(
            "Message-ID: <a@x>\r\nIn-Reply-To: <b@x>\r\nReferences: <c@x> <b@x>\r\n\
             From: Anna <anna@example.com>\r\nTo: bob@example.com\r\nSubject: Re: Plans\r\n\
             Date: Thu, 22 Oct 2026 10:00:00 +0000\r\n\r\n",
        ));
        assert_eq!(m.rfc_message_id.as_deref(), Some("a@x"));
        assert_eq!(m.in_reply_to.as_deref(), Some("b@x"));
        assert_eq!(m.references, vec!["c@x".to_owned(), "b@x".to_owned()]);
        assert_eq!(m.subject_key.as_deref(), Some("plans"));
        assert_eq!(m.thread, ThreadAssignment::Resolve { by_subject: true });
        assert_eq!(m.received, Some(1_700_000_000));
        assert_ne!(m.date, 1_700_000_000);
        assert_eq!(m.size, Some(1234));
        // Not Gmail: labels are ignored.
        assert_eq!(m.locations.len(), 1);
        assert!(!m.replace_labels);
    }

    #[test]
    fn gmail_labels_become_locations() {
        let mut i = input("Subject: hi\r\n\r\n");
        i.gmail = Some(GmailMeta {
            msg_id: 42,
            thread_id: 43,
            labels: Vec::new(),
        });
        let m = new_message(&i);
        assert_eq!(m.gm_msgid, Some(42));
        assert!(m.replace_labels);
        assert_eq!(m.locations.len(), 2);
        assert_eq!(m.locations[1].uid, None);
        assert_eq!(m.locations[1].flags, Flags::SEEN);
        // No Date header: the internal date is used.
        assert_eq!(m.date, 1_700_000_000);
    }
}
