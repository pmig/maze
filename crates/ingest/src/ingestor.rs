//! [`Ingestor`]: prepare, commit, index.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mazemail_mime::subject::parse_subject;
use mazemail_mime::thread::{
    SubjectFallback, ThreadInput, ThreadJoin, ThreadingOptions, join_thread, normalize_message_id,
    thread_link_ids,
};
use mazemail_mime::{AttachmentMeta, MessageHeaders, ParsedMessage, detect_language};
use mazemail_types::{AccountId, Address, IdentityId, MessageId, ThreadId};
use serde::Serialize;
use store::{
    BodyState, BodySummaryUpdate, NewLocation, NewMessage, Queries, StagedBlob, Store,
    ThreadAssignment, UnixTime, WriteTx,
};

use crate::doc::{IndexAddress, IndexDoc, IndexLanguage, IndexSink, NoopIndexSink};
use crate::{Error, Result};

/// Messages per write job the importers use: large enough to amortize the commit,
/// small enough to keep the writer responsive for the UI (see the crate docs).
pub const DEFAULT_BATCH: usize = 200;

/// Body text beyond this many bytes isn't indexed (same cap as attachment text,
/// PLAN §4.4).
pub const MAX_INDEX_BODY_BYTES: usize = 1 << 20;

/// Statements' worth of work per message, for the store's batch weights.
const WEIGHT_PER_MESSAGE: usize = 8;

/// Gmail's identifiers for a message (`X-GM-MSGID`, `X-GM-THRID`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GmailMeta {
    pub msgid: u64,
    pub thrid: u64,
    /// The locations list every label of the message (from `X-GM-LABELS`), so label
    /// locations not listed are removed.
    pub replace_labels: bool,
}

/// Where a message lives and what the server (or the source file) says about it,
/// besides its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageMeta {
    pub account: AccountId,
    /// At least one location (folder, UID, flags, keywords). Gmail messages list one
    /// per label folder.
    pub locations: Vec<NewLocation>,
    /// IMAP `INTERNALDATE` or the mbox envelope date; also the fallback for a missing
    /// `Date` header.
    pub internal_date: Option<UnixTime>,
    pub gmail: Option<GmailMeta>,
    /// Authentication verdicts from the `security` crate, as JSON.
    pub auth_json: Option<String>,
    pub identity_hint: Option<IdentityId>,
}

impl MessageMeta {
    /// A message in one location, without Gmail data.
    pub fn new(account: AccountId, location: NewLocation) -> Self {
        Self {
            account,
            locations: vec![location],
            internal_date: None,
            gmail: None,
            auth_json: None,
            identity_hint: None,
        }
    }

    /// Sets the internal date.
    pub fn with_internal_date(mut self, date: Option<UnixTime>) -> Self {
        self.internal_date = date;
        self
    }
}

/// What header-first sync knows about a body before downloading it (from
/// `BODYSTRUCTURE` and `RFC822.SIZE`).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct BodySummary {
    /// `RFC822.SIZE`.
    pub size: Option<u64>,
    pub has_attachments: bool,
    pub attachment_names: Vec<String>,
    /// The body structure as JSON (owned by sync), stored on the message.
    pub bodystructure_json: Option<String>,
    /// A preview, if the server provided one (`PREVIEW`, RFC 8970).
    pub snippet: Option<String>,
}

impl fmt::Debug for BodySummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BodySummary")
            .field("size", &self.size)
            .field("has_attachments", &self.has_attachments)
            .field("attachments", &self.attachment_names.len())
            .finish_non_exhaustive()
    }
}

/// One raw message for the batch functions.
#[derive(Clone, PartialEq, Eq)]
pub struct RawItem {
    pub meta: MessageMeta,
    pub raw: Vec<u8>,
}

impl fmt::Debug for RawItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawItem")
            .field("meta", &self.meta)
            .field("bytes", &self.raw.len())
            .finish()
    }
}

/// A parsed message with its body blob written, ready for [`Ingestor::commit`].
///
/// The blob stays pinned (safe from garbage collection) while this value lives and
/// until the commit that references it.
pub struct PreparedMessage {
    message: NewMessage,
    in_reply_to: Vec<String>,
    blob: Option<StagedBlob>,
    doc: IndexDoc,
    /// A reply or forward without `References`/`In-Reply-To`: may join by subject.
    subject_fallback: bool,
}

impl fmt::Debug for PreparedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedMessage")
            .field("message", &self.message)
            .field("has_body", &self.blob.is_some())
            .finish_non_exhaustive()
    }
}

impl PreparedMessage {
    /// The store input this will upsert (thread assignment is decided at commit).
    pub fn message(&self) -> &NewMessage {
        &self.message
    }

    /// The body blob, if the full message was given.
    pub fn blob(&self) -> Option<&StagedBlob> {
        self.blob.as_ref()
    }
}

/// What ingesting one message did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingested {
    pub message: MessageId,
    pub thread: ThreadId,
    /// False if the message was already stored (same Gmail id, UID, or a detached
    /// copy) and got its locations updated instead.
    pub created: bool,
    /// The document sent to the index sink.
    pub doc: IndexDoc,
}

/// Turns raw messages and headers into stored state (see the crate docs).
///
/// Cheap to clone; clones share the store and the sink.
#[derive(Clone)]
pub struct Ingestor {
    store: Store,
    sink: Arc<dyn IndexSink>,
}

impl fmt::Debug for Ingestor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ingestor")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl Ingestor {
    /// An ingestor that doesn't index.
    pub fn new(store: Store) -> Self {
        Self::with_sink(store, Arc::new(NoopIndexSink))
    }

    /// An ingestor that sends documents to `sink` after each commit.
    pub fn with_sink(store: Store, sink: Arc<dyn IndexSink>) -> Self {
        Self { store, sink }
    }

    /// The store.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The index sink.
    pub fn sink(&self) -> &Arc<dyn IndexSink> {
        &self.sink
    }

    // ---- prepare (any thread) ------------------------------------------------------

    /// Parses a full raw message and stages its encrypted body blob
    /// ([`BlobStore::stage`](store::BlobStore::stage): written, not yet durable; the
    /// commit makes the whole batch's blobs durable at once).
    ///
    /// Blocking (CPU and file IO); call it off async worker threads
    /// (`spawn_blocking`) or from a thread pool.
    pub fn prepare_raw(&self, meta: MessageMeta, raw: &[u8]) -> Result<PreparedMessage> {
        check_meta(&meta)?;
        let parsed = mazemail_mime::parse(raw)?;
        let body = capped(parsed.body_text(), MAX_INDEX_BODY_BYTES);
        let language = language_of(parsed.headers.subject.as_deref(), &body);
        let attachment_names = attachment_names(&parsed.attachments);
        let summary = BodySummary {
            size: Some(raw.len() as u64),
            has_attachments: parsed.has_attachments,
            bodystructure_json: bodystructure_json(&parsed),
            snippet: (!parsed.snippet.is_empty()).then(|| parsed.snippet.clone()),
            attachment_names,
        };
        let blob = self.store.blobs().stage(raw)?;
        Ok(build(
            meta,
            &parsed.headers,
            summary,
            body,
            language,
            Some(blob),
        ))
    }

    /// Parses a header block for header-first sync; the body comes later through
    /// [`Ingestor::attach_body`].
    pub fn prepare_headers(
        &self,
        meta: MessageMeta,
        headers: &[u8],
        summary: BodySummary,
    ) -> Result<PreparedMessage> {
        check_meta(&meta)?;
        let parsed = mazemail_mime::parse_headers_only(headers)?;
        let language = language_of(parsed.subject.as_deref(), "");
        Ok(build(meta, &parsed, summary, String::new(), language, None))
    }

    // ---- commit (store writer) -----------------------------------------------------

    /// Stores a batch in one write job and indexes it. Results are in input order; if
    /// the batch job fails, every message is retried alone, so only bad messages fail.
    ///
    /// The store's writer makes the batch's blobs durable before it commits (no
    /// blocking file IO here), together with those of other jobs in the same
    /// transaction.
    pub async fn commit(&self, batch: Vec<PreparedMessage>) -> Vec<Result<Ingested>> {
        let items = Arc::new(batch);
        let all = 0..items.len();
        let first = self
            .store
            .write_weighted(weight(&all), commit_job(Arc::clone(&items), all.clone()))
            .await;
        match first {
            Ok(done) => self.finish(&items, all, Ok(done)),
            Err(err) if items.len() <= 1 => self.finish(&items, all, Err(err)),
            Err(err) => {
                tracing::warn!(error = %err, count = items.len(), "ingest batch failed; retrying one by one");
                let mut out = Vec::with_capacity(items.len());
                for i in 0..items.len() {
                    let range = i..i + 1;
                    let result = self
                        .store
                        .write_weighted(
                            weight(&range),
                            commit_job(Arc::clone(&items), range.clone()),
                        )
                        .await;
                    out.extend(self.finish(&items, range, result));
                }
                out
            }
        }
    }

    /// Blocking variant of [`Ingestor::commit`] for synchronous callers (importers).
    /// Must not be called from an async runtime's worker thread.
    ///
    /// Makes the batch's blobs durable on the calling thread first, so the store's
    /// writer doesn't have to.
    pub fn commit_blocking(&self, batch: Vec<PreparedMessage>) -> Vec<Result<Ingested>> {
        self.persist_blobs(batch.iter().filter_map(|p| p.blob.as_ref()));
        let items = Arc::new(batch);
        let all = 0..items.len();
        match self.write_blocking(&items, all.clone()) {
            Ok(done) => self.finish(&items, all, Ok(done)),
            Err(err) if items.len() <= 1 => self.finish(&items, all, Err(err)),
            Err(err) => {
                tracing::warn!(error = %err, count = items.len(), "ingest batch failed; retrying one by one");
                (0..items.len())
                    .flat_map(|i| {
                        let range = i..i + 1;
                        let result = self.write_blocking(&items, range.clone());
                        self.finish(&items, range, result)
                    })
                    .collect()
            }
        }
    }

    /// Makes staged blobs durable on this thread, sharing one round of device flushes.
    /// A failure is only logged: the writer persists whatever is left before it
    /// commits, and fails exactly the jobs it can't (then the batch is retried message
    /// by message as usual).
    fn persist_blobs<'a>(&self, blobs: impl IntoIterator<Item = &'a StagedBlob>) {
        if let Err(err) = self.store.blobs().persist(blobs) {
            tracing::warn!(error = %err, "making staged blobs durable failed; the writer retries");
        }
    }

    fn write_blocking(
        &self,
        items: &Arc<Vec<PreparedMessage>>,
        range: Range<usize>,
    ) -> store::Result<Vec<Committed>> {
        // `write_blocking` has no weight parameter; the job is heavy enough to run in a
        // transaction of its own, which is what a weighted submit would do as well.
        self.store
            .write_blocking(commit_job(Arc::clone(items), range))
    }

    /// Turns committed rows into results and feeds the sink.
    fn finish(
        &self,
        items: &[PreparedMessage],
        range: Range<usize>,
        result: store::Result<Vec<Committed>>,
    ) -> Vec<Result<Ingested>> {
        match result {
            Ok(done) => items[range]
                .iter()
                .zip(done)
                .map(|(item, committed)| {
                    let mut doc = item.doc.clone();
                    doc.message = committed.message;
                    doc.thread = committed.thread;
                    self.sink.add(doc.clone());
                    Ok(Ingested {
                        message: committed.message,
                        thread: committed.thread,
                        created: committed.created,
                        doc,
                    })
                })
                .collect(),
            // Only single-message jobs end up here (a failed batch is retried one by
            // one), so the error belongs to that message.
            Err(err) => {
                let mut out = Vec::with_capacity(range.len());
                let mut err = Some(Error::Store(err));
                for _ in range {
                    out.push(Err(err.take().unwrap_or(Error::Invalid(
                        "the batch this message was in failed",
                    ))));
                }
                out
            }
        }
    }

    // ---- convenience ---------------------------------------------------------------

    /// Prepares and commits one raw message.
    pub async fn ingest_raw(&self, meta: MessageMeta, raw: &[u8]) -> Result<Ingested> {
        let prepared = self.prepare_raw(meta, raw)?;
        single(self.commit(vec![prepared]).await)
    }

    /// Blocking variant of [`Ingestor::ingest_raw`].
    pub fn ingest_raw_blocking(&self, meta: MessageMeta, raw: &[u8]) -> Result<Ingested> {
        let prepared = self.prepare_raw(meta, raw)?;
        single(self.commit_blocking(vec![prepared]))
    }

    /// Prepares and commits raw messages in batches of up to [`DEFAULT_BATCH`], one
    /// write job per batch. Results are in input order.
    pub async fn ingest_raw_batch(&self, items: Vec<RawItem>) -> Vec<Result<Ingested>> {
        let mut out = Vec::with_capacity(items.len());
        let mut items = items.into_iter().peekable();
        while items.peek().is_some() {
            let (prepared, slots) = self.prepare_chunk(items.by_ref().take(DEFAULT_BATCH));
            let committed = self.commit(prepared).await;
            out.extend(merge_slots(slots, committed));
        }
        out
    }

    /// Blocking variant of [`Ingestor::ingest_raw_batch`].
    pub fn ingest_raw_batch_blocking(&self, items: Vec<RawItem>) -> Vec<Result<Ingested>> {
        let mut out = Vec::with_capacity(items.len());
        let mut items = items.into_iter().peekable();
        while items.peek().is_some() {
            let (prepared, slots) = self.prepare_chunk(items.by_ref().take(DEFAULT_BATCH));
            let committed = self.commit_blocking(prepared);
            out.extend(merge_slots(slots, committed));
        }
        out
    }

    fn prepare_chunk(
        &self,
        chunk: impl Iterator<Item = RawItem>,
    ) -> (Vec<PreparedMessage>, Vec<Option<Error>>) {
        let mut prepared = Vec::with_capacity(DEFAULT_BATCH);
        let mut slots = Vec::with_capacity(DEFAULT_BATCH);
        for item in chunk {
            match self.prepare_raw(item.meta, &item.raw) {
                Ok(p) => {
                    prepared.push(p);
                    slots.push(None);
                }
                Err(err) => slots.push(Some(err)),
            }
        }
        (prepared, slots)
    }

    /// Prepares and commits one header-only message.
    pub async fn ingest_headers(
        &self,
        meta: MessageMeta,
        headers: &[u8],
        summary: BodySummary,
    ) -> Result<Ingested> {
        let prepared = self.prepare_headers(meta, headers, summary)?;
        single(self.commit(vec![prepared]).await)
    }

    /// Blocking variant of [`Ingestor::ingest_headers`].
    pub fn ingest_headers_blocking(
        &self,
        meta: MessageMeta,
        headers: &[u8],
        summary: BodySummary,
    ) -> Result<Ingested> {
        let prepared = self.prepare_headers(meta, headers, summary)?;
        single(self.commit_blocking(vec![prepared]))
    }

    /// Fills in the body of a stored (header-only) message: writes the blob, sets the
    /// body state to full, takes size, snippet and the attachment flag from the parsed
    /// body ([`WriteTx::update_body_summary`], so thread and folder attachment counts
    /// follow), fills the attachment structure if sync stored none, and sends a full
    /// index document.
    pub async fn attach_body(&self, message: MessageId, raw: &[u8]) -> Result<Ingested> {
        self.attach_body_as(message, raw, BodyState::Full).await
    }

    /// Like [`Ingestor::attach_body`], with the body state to record: header-first
    /// sync stores a synthetic message with only the text parts of a large message as
    /// [`BodyState::TextParts`], and the full message later.
    pub async fn attach_body_as(
        &self,
        message: MessageId,
        raw: &[u8],
        state: BodyState,
    ) -> Result<Ingested> {
        let prepared = self.prepare_body(raw)?;
        let job_input = prepared.clone();
        let detail = self
            .store
            .write_weighted(WEIGHT_PER_MESSAGE, move |w| {
                attach_job(w, message, &job_input, state)
            })
            .await?;
        Ok(self.finish_attach(message, detail, prepared))
    }

    /// Blocking variant of [`Ingestor::attach_body`].
    pub fn attach_body_blocking(&self, message: MessageId, raw: &[u8]) -> Result<Ingested> {
        self.attach_body_blocking_as(message, raw, BodyState::Full)
    }

    /// Blocking variant of [`Ingestor::attach_body_as`]: parses, encrypts and makes the
    /// blob durable on the calling thread (call it from a blocking thread, not an async
    /// worker), then waits for the store's commit.
    pub fn attach_body_blocking_as(
        &self,
        message: MessageId,
        raw: &[u8],
        state: BodyState,
    ) -> Result<Ingested> {
        let prepared = self.prepare_body(raw)?;
        self.persist_blobs([&prepared.blob]);
        let job_input = prepared.clone();
        let detail = self
            .store
            .write_blocking(move |w| attach_job(w, message, &job_input, state))?;
        Ok(self.finish_attach(message, detail, prepared))
    }

    fn prepare_body(&self, raw: &[u8]) -> Result<PreparedBody> {
        let parsed = mazemail_mime::parse(raw)?;
        let body = capped(parsed.body_text(), MAX_INDEX_BODY_BYTES);
        let blob = self.store.blobs().stage(raw)?;
        Ok(PreparedBody {
            language: language_of(parsed.headers.subject.as_deref(), &body),
            attachment_names: attachment_names(&parsed.attachments),
            snippet: (!parsed.snippet.is_empty()).then(|| parsed.snippet.clone()),
            bodystructure_json: bodystructure_json(&parsed),
            has_attachments: parsed.has_attachments,
            size: raw.len() as u64,
            body,
            blob,
        })
    }

    fn finish_attach(
        &self,
        message: MessageId,
        detail: AttachedDetail,
        body: PreparedBody,
    ) -> Ingested {
        let doc = IndexDoc {
            message,
            thread: detail.thread,
            account: detail.account,
            date: detail.date,
            size: body.size,
            from: detail.from.iter().map(IndexAddress::from_address).collect(),
            to: detail.to.iter().map(IndexAddress::from_address).collect(),
            cc: detail.cc.iter().map(IndexAddress::from_address).collect(),
            subject: detail.subject.unwrap_or_default(),
            body: body.body,
            attachment_names: body.attachment_names,
            list_id: detail.list_id,
            has_attachment: body.has_attachments,
            language: body.language,
        };
        self.sink.add(doc.clone());
        Ingested {
            message,
            thread: detail.thread,
            created: false,
            doc,
        }
    }
}

// ---- prepare helpers ---------------------------------------------------------------

fn check_meta(meta: &MessageMeta) -> Result<()> {
    if meta.locations.is_empty() {
        return Err(Error::Invalid("a message needs at least one location"));
    }
    Ok(())
}

fn now() -> UnixTime {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Truncates `text` to at most `max` bytes on a char boundary.
fn capped(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn language_of(subject: Option<&str>, body: &str) -> Option<IndexLanguage> {
    let mut text = subject.unwrap_or_default().to_owned();
    text.push('\n');
    text.push_str(body);
    IndexLanguage::from_hint(detect_language(&text))
}

fn attachment_names(attachments: &[AttachmentMeta]) -> Vec<String> {
    attachments
        .iter()
        .filter(|a| a.counts_as_attachment())
        .filter_map(|a| a.filename.clone())
        .collect()
}

/// The part structure stored on the message (`bodystructure_json`), versioned.
#[derive(Serialize)]
struct BodyStructure<'a> {
    v: u32,
    source: &'static str,
    attachments: &'a [AttachmentMeta],
}

fn bodystructure_json(parsed: &ParsedMessage) -> Option<String> {
    if parsed.attachments.is_empty() {
        return None;
    }
    serde_json::to_string(&BodyStructure {
        v: 1,
        source: "mime",
        attachments: &parsed.attachments,
    })
    .ok()
}

fn first_normalized(ids: &[String]) -> Option<String> {
    ids.iter()
        .find_map(|id| normalize_message_id(id))
        .map(str::to_owned)
}

fn build(
    meta: MessageMeta,
    headers: &MessageHeaders,
    summary: BodySummary,
    body: String,
    language: Option<IndexLanguage>,
    blob: Option<StagedBlob>,
) -> PreparedMessage {
    let date = headers
        .date
        .map(|d| d.unix_timestamp())
        .or(meta.internal_date)
        .unwrap_or_else(now);
    let mut m = NewMessage::new(meta.account, date);
    m.rfc_message_id = headers
        .message_id
        .as_deref()
        .and_then(normalize_message_id)
        .map(str::to_owned);
    let in_reply_to: Vec<String> = headers
        .in_reply_to
        .iter()
        .filter_map(|id| normalize_message_id(id))
        .map(str::to_owned)
        .collect();
    m.in_reply_to = first_normalized(&in_reply_to);
    m.references = headers
        .references
        .iter()
        .filter_map(|id| normalize_message_id(id))
        .map(str::to_owned)
        .collect();
    m.received = meta.internal_date;
    m.from = headers
        .from
        .first()
        .cloned()
        .or_else(|| headers.sender.clone());
    m.to.clone_from(&headers.to);
    m.cc.clone_from(&headers.cc);
    m.delivered_to = headers
        .delivered_to
        .first()
        .or_else(|| headers.x_original_to.first())
        .or_else(|| headers.envelope_to.first())
        .cloned();
    m.subject.clone_from(&headers.subject);
    let subject_info = headers.subject.as_deref().map(parse_subject);
    m.subject_key = subject_info
        .as_ref()
        .map(|s| s.key())
        .filter(|k| !k.is_empty());
    m.snippet = summary.snippet;
    m.size = summary.size;
    m.has_attachments = summary.has_attachments;
    m.list_id.clone_from(&headers.list_id);
    m.bodystructure_json = summary.bodystructure_json;
    if let Some(gmail) = meta.gmail {
        m.gm_msgid = Some(gmail.msgid);
        m.gm_thrid = Some(gmail.thrid);
        m.replace_labels = gmail.replace_labels;
    }
    m.auth_json = meta.auth_json;
    m.identity_hint = meta.identity_hint;
    m.locations = meta.locations;

    let subject_fallback = m.references.is_empty()
        && in_reply_to.is_empty()
        && subject_info.as_ref().is_some_and(|s| s.is_response());

    let addresses = |list: &[Address]| list.iter().map(IndexAddress::from_address).collect();
    let doc = IndexDoc {
        message: MessageId(0),
        thread: ThreadId(0),
        account: meta.account,
        date,
        size: m.size.unwrap_or_default(),
        from: m.from.iter().map(IndexAddress::from_address).collect(),
        to: addresses(&m.to),
        cc: addresses(&m.cc),
        subject: m.subject.clone().unwrap_or_default(),
        body,
        attachment_names: summary.attachment_names,
        list_id: m.list_id.clone(),
        has_attachment: m.has_attachments,
        language,
    };
    PreparedMessage {
        message: m,
        in_reply_to,
        blob,
        doc,
        subject_fallback,
    }
}

// ---- commit helpers ----------------------------------------------------------------

/// What the write job reports per message.
#[derive(Debug, Clone, Copy)]
struct Committed {
    message: MessageId,
    thread: ThreadId,
    created: bool,
}

fn weight(range: &Range<usize>) -> usize {
    range.len().max(1) * WEIGHT_PER_MESSAGE
}

fn commit_job(
    items: Arc<Vec<PreparedMessage>>,
    range: Range<usize>,
) -> impl FnOnce(&mut WriteTx<'_>) -> store::Result<Vec<Committed>> + Send + 'static {
    move |w| {
        items[range]
            .iter()
            .map(|item| commit_one(w, item))
            .collect()
    }
}

fn commit_one(w: &mut WriteTx<'_>, item: &PreparedMessage) -> store::Result<Committed> {
    let (assignment, merges) = decide_thread(w, item)?;
    let mut message = item.message.clone();
    message.thread = assignment;
    let outcome = w.upsert_message(&message)?;
    if outcome.created {
        for other in merges {
            if other != outcome.thread {
                w.merge_threads(outcome.thread, other)?;
            }
        }
    }
    if let Some(blob) = &item.blob {
        w.set_message_body(outcome.message, blob, BodyState::Full)?;
    }
    Ok(Committed {
        message: outcome.message,
        thread: outcome.thread,
        created: outcome.created,
    })
}

/// Picks the thread of a new message: Gmail's thread id decides for Gmail; otherwise
/// the threads that the message's own and referenced Message-IDs belong to, through
/// `join_thread` (merging several), then the subject fallback for replies without
/// references, else a new thread.
fn decide_thread(
    w: &WriteTx<'_>,
    item: &PreparedMessage,
) -> store::Result<(ThreadAssignment, Vec<ThreadId>)> {
    let m = &item.message;
    if let Some(thrid) = m.gm_thrid {
        return Ok(match w.thread_for_gm_thrid(m.account, thrid)? {
            Some(thread) => (ThreadAssignment::Existing(thread), Vec::new()),
            None => (ThreadAssignment::New, Vec::new()),
        });
    }
    let input = ThreadInput {
        message_id: m.rfc_message_id.as_deref(),
        in_reply_to: &item.in_reply_to,
        references: &m.references,
        subject: m.subject.as_deref(),
        date: Some(m.date),
    };
    let ids = thread_link_ids(&input);
    // One query for all ids; most new messages start a thread and stop here.
    let any = w.threads_for_rfc_ids(m.account, &ids)?;
    let mut lookup: HashMap<String, ThreadId> = HashMap::new();
    if any.len() == 1 {
        if let Some(&thread) = any.first() {
            for id in &ids {
                lookup.insert((*id).to_owned(), thread);
            }
        }
    } else if any.len() > 1 {
        for id in &ids {
            if let Some(&thread) = w.threads_for_rfc_ids(m.account, &[id])?.first() {
                lookup.insert((*id).to_owned(), thread);
            }
        }
    }
    let options = ThreadingOptions {
        subject_fallback: SubjectFallback::Off,
    };
    Ok(match join_thread(&lookup, &input, &options) {
        ThreadJoin::Join(thread) => (ThreadAssignment::Existing(thread), Vec::new()),
        ThreadJoin::Merge { into, merged } => (ThreadAssignment::Existing(into), merged),
        ThreadJoin::New if item.subject_fallback => {
            (ThreadAssignment::Resolve { by_subject: true }, Vec::new())
        }
        ThreadJoin::New => (ThreadAssignment::New, Vec::new()),
    })
}

/// A body ready for [`Ingestor::attach_body`].
#[derive(Clone)]
struct PreparedBody {
    body: String,
    language: Option<IndexLanguage>,
    attachment_names: Vec<String>,
    snippet: Option<String>,
    bodystructure_json: Option<String>,
    has_attachments: bool,
    size: u64,
    blob: StagedBlob,
}

/// The stored fields the full index document needs.
struct AttachedDetail {
    account: AccountId,
    thread: ThreadId,
    date: i64,
    from: Option<Address>,
    to: Vec<Address>,
    cc: Vec<Address>,
    subject: Option<String>,
    list_id: Option<String>,
}

fn attach_job(
    w: &mut WriteTx<'_>,
    message: MessageId,
    body: &PreparedBody,
    state: BodyState,
) -> store::Result<AttachedDetail> {
    let detail = w.message(message)?.ok_or_else(|| store::Error::NotFound {
        entity: "message",
        id: message.to_string(),
    })?;
    // The parsed body is authoritative for size, attachments and the snippet; the
    // body structure belongs to sync, so it is only filled in if sync stored none.
    w.update_body_summary(
        message,
        &BodySummaryUpdate {
            snippet: body.snippet.clone(),
            size: Some(body.size),
            has_attachments: Some(body.has_attachments),
            bodystructure_json: if detail.bodystructure_json.is_none() {
                body.bodystructure_json.clone()
            } else {
                None
            },
            blob: Some(body.blob.clone()),
            body_state: Some(state),
        },
    )?;
    Ok(AttachedDetail {
        account: detail.account,
        thread: detail.thread,
        date: detail.date,
        from: detail.from,
        to: detail.to,
        cc: detail.cc,
        subject: detail.subject,
        list_id: detail.list_id,
    })
}

// ---- small utilities ---------------------------------------------------------------

fn single(mut results: Vec<Result<Ingested>>) -> Result<Ingested> {
    results
        .pop()
        .unwrap_or(Err(Error::Invalid("the commit returned no result")))
}

fn merge_slots(
    slots: Vec<Option<Error>>,
    committed: Vec<Result<Ingested>>,
) -> Vec<Result<Ingested>> {
    let mut committed = committed.into_iter();
    slots
        .into_iter()
        .map(|slot| match slot {
            Some(err) => Err(err),
            None => committed
                .next()
                .unwrap_or(Err(Error::Invalid("the commit returned too few results"))),
        })
        .collect()
}
