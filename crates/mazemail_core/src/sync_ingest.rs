//! [`CoreIngest`]: the sync engine's [`MessageIngest`] over the shared ingest pipeline
//! (`ingest::Ingestor`, PLAN §4.3, §4.5).
//!
//! - **Headers** arrive in batches (up to `header_batch`, 200 by default). They are
//!   parsed on a blocking thread and committed with [`Ingestor::commit`]: one store
//!   write job, so one transaction (and, with bodies, one round of blob flushes) per
//!   batch. Threading, Gmail labels and index documents are the pipeline's.
//! - **Bodies** ([`MessageIngest::attach_body`]) are parsed, encrypted and made durable
//!   on a blocking thread, then committed; full or text-parts-only as sync says.
//! - **Thunderbird body reuse**: when an import left a body reuse index
//!   (`import::BodyReuse`), [`MessageIngest::lookup_reused_body`] asks it with the
//!   Message-ID, size and header fingerprint before sync downloads a body.
//!
//! Index documents go to core's indexer through [`CoreIndexSink`](crate::CoreIndexSink).

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use ingest::{BodyLookup, BodySummary, GmailMeta, HeaderFingerprint, Ingestor, MessageMeta};
use mazemail_mime::thread::normalize_message_id;
use store::{BodyState, NewLocation, Queries as _, Store};
use sync::{HeaderInput, IngestError, Ingested, MessageIngest, ReuseQuery};
use time::OffsetDateTime;

/// The real ingest pipeline for the sync engine. See the [module docs](self).
#[derive(Clone)]
pub struct CoreIngest {
    ingestor: Ingestor,
    reuse: Option<Arc<dyn BodyLookup>>,
}

impl fmt::Debug for CoreIngest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreIngest")
            .field("reuse", &self.reuse.is_some())
            .finish_non_exhaustive()
    }
}

impl CoreIngest {
    /// Ingests through `ingestor`; asks `reuse` for bodies before downloading them.
    pub fn new(ingestor: Ingestor, reuse: Option<Arc<dyn BodyLookup>>) -> Self {
        Self { ingestor, reuse }
    }

    fn store(&self) -> &Store {
        self.ingestor.store()
    }
}

/// The pipeline's metadata and body summary for a header from the server.
fn meta_of(input: &HeaderInput) -> (MessageMeta, BodySummary) {
    let mut locations = vec![input.location.clone()];
    if input.gmail.is_some() {
        // Gmail: one location per label folder, with the message's flags (ADR-007).
        locations.extend(input.labels.iter().map(|folder| NewLocation {
            folder: *folder,
            uid: None,
            modseq: None,
            flags: input.location.flags,
            keywords: input.location.keywords.clone(),
        }));
    }
    let meta = MessageMeta {
        account: input.account,
        locations,
        internal_date: input.internal_date.map(OffsetDateTime::unix_timestamp),
        gmail: input.gmail.as_ref().map(|g| GmailMeta {
            msgid: g.msg_id,
            thrid: g.thread_id,
            replace_labels: true,
        }),
        auth_json: None,
        identity_hint: None,
    };
    let attachments: Vec<&imap::BodyPart> = input
        .body
        .iter()
        .flat_map(|b| b.parts.iter())
        .filter(|p| p.is_attachment)
        .collect();
    let summary = BodySummary {
        size: Some(u64::from(input.size)),
        has_attachments: !attachments.is_empty(),
        attachment_names: attachments
            .iter()
            .filter_map(|p| p.filename.as_ref().map(|f| f.expose().clone()))
            .collect(),
        bodystructure_json: None,
        snippet: None,
    };
    (meta, summary)
}

/// Prepares one header; a header block that doesn't parse is stored without headers
/// rather than failing the whole batch (the body, once fetched, fills in what it can).
fn prepare(
    ingestor: &Ingestor,
    input: &HeaderInput,
) -> Result<ingest::PreparedMessage, IngestError> {
    let (meta, summary) = meta_of(input);
    match ingestor.prepare_headers(meta.clone(), input.header.expose(), summary.clone()) {
        Ok(prepared) => Ok(prepared),
        Err(ingest::Error::Parse(_)) => {
            tracing::warn!(
                folder = %input.location.folder,
                uid = ?input.location.uid,
                "unparsable header block; storing the message without headers"
            );
            ingestor
                .prepare_headers(meta, b"\r\n", summary)
                .map_err(IngestError::new)
        }
        Err(err) => Err(IngestError::new(err)),
    }
}

fn joined(err: &tokio::task::JoinError) -> IngestError {
    IngestError::new(format!("ingest task failed: {err}"))
}

#[async_trait]
impl MessageIngest for CoreIngest {
    async fn ingest_headers(&self, input: HeaderInput) -> Result<Ingested, IngestError> {
        self.ingest_headers_batch(vec![input])
            .await?
            .pop()
            .ok_or_else(|| IngestError::new("ingest returned no result"))
    }

    async fn ingest_headers_batch(
        &self,
        inputs: Vec<HeaderInput>,
    ) -> Result<Vec<Ingested>, IngestError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let ingestor = self.ingestor.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            inputs
                .iter()
                .map(|input| prepare(&ingestor, input))
                .collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(|err| joined(&err))??;
        // One write job for the whole batch.
        self.ingestor
            .commit(prepared)
            .await
            .into_iter()
            .map(|result| {
                result
                    .map(|done| Ingested {
                        message: done.message,
                        created: done.created,
                    })
                    .map_err(IngestError::new)
            })
            .collect()
    }

    async fn attach_body(
        &self,
        message: mazemail_types::MessageId,
        raw: Bytes,
        state: BodyState,
    ) -> Result<(), IngestError> {
        let ingestor = self.ingestor.clone();
        tokio::task::spawn_blocking(move || ingestor.attach_body_blocking_as(message, &raw, state))
            .await
            .map_err(|err| joined(&err))?
            .map(drop)
            .map_err(IngestError::new)
    }

    async fn lookup_reused_body(&self, query: &ReuseQuery) -> Result<Option<Bytes>, IngestError> {
        let Some(reuse) = self.reuse.clone() else {
            return Ok(None);
        };
        let Some(size) = query.size else {
            return Ok(None);
        };
        let query = query.clone();
        let store = self.store().clone();
        tokio::task::spawn_blocking(move || {
            let (message_id, fingerprint) = match &query.header {
                Some(header) => {
                    let Ok(headers) = mazemail_mime::parse_headers_only(header.expose()) else {
                        return Ok(None);
                    };
                    let id = headers
                        .message_id
                        .as_deref()
                        .and_then(normalize_message_id)
                        .map(str::to_owned);
                    (id, HeaderFingerprint::from_headers(&headers))
                }
                None => {
                    let Some(detail) = store.reader()?.message(query.message)? else {
                        return Ok(None);
                    };
                    let fingerprint = HeaderFingerprint::new(
                        Some(detail.date),
                        detail.from.as_ref().map(|a| a.email.as_str()),
                        detail.subject.as_deref(),
                    );
                    (query.rfc_message_id.clone(), fingerprint)
                }
            };
            let Some(message_id) = message_id else {
                return Ok(None);
            };
            Ok(reuse
                .lookup(&message_id, size, &fingerprint)
                .map(Bytes::from))
        })
        .await
        .map_err(|err| joined(&err))?
        .map_err(|err: store::Error| IngestError::new(err))
    }
}

#[cfg(test)]
mod tests {
    use imap::{BodyPart, BodySummary as ImapBody};
    use mazemail_types::{AccountId, Flags, FolderId, Sensitive};

    use super::*;

    fn input() -> HeaderInput {
        HeaderInput {
            account: AccountId(1),
            location: NewLocation::new(FolderId(2), Some(7), Flags::SEEN),
            labels: vec![FolderId(3)],
            header: Sensitive::new(Bytes::from_static(b"Subject: hi\r\n\r\n")),
            body: Some(ImapBody {
                parts: vec![BodyPart {
                    section: "2".into(),
                    mime_type: "application".into(),
                    subtype: "pdf".into(),
                    charset: None,
                    encoding: "base64".into(),
                    size: 10,
                    disposition: Some("attachment".into()),
                    filename: Some(Sensitive::new("a.pdf".into())),
                    content_id: None,
                    is_attachment: true,
                }],
            }),
            size: 1234,
            gmail: None,
            internal_date: OffsetDateTime::from_unix_timestamp(1_700_000_000).ok(),
        }
    }

    #[test]
    fn headers_map_onto_pipeline_metadata() {
        let (meta, summary) = meta_of(&input());
        // Not Gmail: labels are ignored.
        assert_eq!(meta.locations.len(), 1);
        assert_eq!(meta.internal_date, Some(1_700_000_000));
        assert_eq!(meta.gmail, None);
        assert_eq!(summary.size, Some(1234));
        assert!(summary.has_attachments);
        assert_eq!(summary.attachment_names, vec!["a.pdf".to_owned()]);

        let mut gmail = input();
        gmail.gmail = Some(imap::GmailMeta {
            msg_id: 42,
            thread_id: 43,
            labels: Vec::new(),
        });
        let (meta, _) = meta_of(&gmail);
        assert_eq!(meta.locations.len(), 2);
        assert_eq!(meta.locations[1].folder, FolderId(3));
        assert_eq!(meta.locations[1].uid, None);
        assert_eq!(meta.locations[1].flags, Flags::SEEN);
        assert_eq!(
            meta.gmail,
            Some(GmailMeta {
                msgid: 42,
                thrid: 43,
                replace_labels: true
            })
        );
    }
}
