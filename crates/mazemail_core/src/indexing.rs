//! The indexer task and the indexing seam for `ingest` (PLAN §4.1, §4.4).
//!
//! # The indexer
//!
//! One dedicated thread (`mazemail-indexer`) owns the Tantivy writer
//! ([`search::Indexer`]). It takes adds and deletes from a channel and commits when the
//! commit policy says so: after `max_pending` operations (2,000 by default), or when
//! [`search::Indexer::commit_due`] has passed (1 s after the oldest pending change). It
//! waits on the channel with exactly that deadline, so an idle indexer sleeps without
//! a timer. Each commit is announced as [`IndexProgress`] in the event batches.
//!
//! # The `IndexSink` seam
//!
//! `ingest` (raw message → parse → thread → store → blob) produces one index document
//! per stored message and hands it to an `IndexSink` with the shape `add(IndexDoc)` /
//! `delete(MessageId)`. Core's side of that seam is [`CoreIndexSink`] (from
//! [`CoreHandle::index_sink`](crate::CoreHandle::index_sink)) and the
//! [`IndexDocument`] it accepts, which mirrors exactly what the index stores.
//! [`IndexDocument::from_parsed`] builds one from a parsed message. Wiring `ingest` in
//! is one `impl ingest::IndexSink for CoreIndexSink` that converts `ingest`'s document
//! into an [`IndexDocument`] field by field and calls [`CoreIndexSink::add`].
//!
//! Deleting messages needs no call from `ingest`: core removes the documents of every
//! message in a store change set's `messages_removed`.
//!
//! # Maintenance
//!
//! [`CoreHandle::reindex_all`](crate::CoreHandle::reindex_all) rebuilds the index
//! from the store and the blobs; [`CoreHandle::check_index`](crate::CoreHandle::check_index)
//! compares the indexed ids with the store and optionally repairs the difference.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

use mazemail_mime::{Lang, ParsedMessage};
use mazemail_types::{AccountId, Address, MessageId, Sensitive, ThreadId};
use search::{Attachment, IndexDoc, Indexer, Language, SearchIndex};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

use crate::events::CoreEvent;
use crate::{Error, Result};

/// One message as the search index sees it: only data that never changes (PLAN §4.4,
/// "Mutable fields"). Folders, flags and tags are filtered in SQLite instead.
#[derive(Clone, PartialEq, Eq)]
pub struct IndexDocument {
    /// The message.
    pub message_id: MessageId,
    /// Its thread (used to group hits).
    pub thread_id: ThreadId,
    /// Its account (scope filter).
    pub account_id: AccountId,
    /// The `Date:` header, Unix seconds.
    pub date: i64,
    /// Raw size in bytes.
    pub size: u64,
    /// Senders: display names and addresses are both indexed.
    pub from: Vec<Address>,
    /// Recipients.
    pub to: Vec<Address>,
    /// Copy recipients.
    pub cc: Vec<Address>,
    /// The decoded subject.
    pub subject: Sensitive<String>,
    /// The body as plain text.
    pub body_text: Sensitive<String>,
    /// File names of the parts that count as attachments.
    pub attachment_names: Vec<Sensitive<String>>,
    /// The `List-Id` without angle brackets.
    pub list_id: Option<String>,
    /// Whether the message has attachments (`has:attachment`).
    pub has_attachment: bool,
    /// The detected language; selects the stemmed fields.
    pub language: Option<Lang>,
}

impl fmt::Debug for IndexDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexDocument")
            .field("message_id", &self.message_id)
            .field("thread_id", &self.thread_id)
            .field("account_id", &self.account_id)
            .field("date", &self.date)
            .field("has_attachment", &self.has_attachment)
            .field("language", &self.language)
            .finish_non_exhaustive()
    }
}

impl IndexDocument {
    /// The document for a parsed message stored as `message_id` in `thread_id`.
    /// `date` is the stored date (the `Date:` header, or the receive time when it was
    /// missing).
    pub fn from_parsed(
        message_id: MessageId,
        thread_id: ThreadId,
        account_id: AccountId,
        date: i64,
        parsed: &ParsedMessage,
    ) -> Self {
        Self {
            message_id,
            thread_id,
            account_id,
            date,
            size: parsed.size as u64,
            from: parsed.headers.from.clone(),
            to: parsed.headers.to.clone(),
            cc: parsed.headers.cc.clone(),
            subject: Sensitive::new(parsed.headers.subject.clone().unwrap_or_default()),
            body_text: Sensitive::new(parsed.body_text()),
            attachment_names: parsed
                .attachments
                .iter()
                .filter(|a| a.counts_as_attachment())
                .filter_map(|a| a.filename.clone())
                .map(Sensitive::new)
                .collect(),
            list_id: parsed.headers.list_id.clone(),
            has_attachment: parsed.has_attachments,
            language: parsed.language(),
        }
    }

    /// Converts to the `search` crate's document.
    pub fn into_search_doc(self) -> IndexDoc {
        let mut attachments: Vec<Attachment> = if self.has_attachment {
            self.attachment_names
                .into_iter()
                .map(|name| Attachment { name, text: None })
                .collect()
        } else {
            Vec::new()
        };
        // `search` derives `has:attachment` from a non-empty list.
        if self.has_attachment && attachments.is_empty() {
            attachments.push(Attachment::default());
        }
        IndexDoc {
            message_id: self.message_id,
            thread_id: self.thread_id,
            account_id: self.account_id,
            date: self.date,
            size: self.size,
            from: self.from,
            to: self.to,
            cc: self.cc,
            subject: self.subject,
            body: self.body_text,
            attachments,
            list_id: self.list_id,
            language: search_language(self.language),
        }
    }
}

/// Maps the detected language to the index's stemmed fields.
pub(crate) fn search_language(lang: Option<Lang>) -> Option<Language> {
    match lang {
        Some(Lang::De) => Some(Language::De),
        Some(Lang::En) => Some(Language::En),
        Some(Lang::Other) | None => None,
    }
}

/// Indexer state, sent with event batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexProgress {
    /// Messages in the index as of the last commit.
    pub indexed: u64,
    /// Changes waiting for the next commit.
    pub pending: usize,
    /// A running rebuild: `(done, total)` messages.
    pub rebuild: Option<(u64, u64)>,
}

pub(crate) enum IndexCommand {
    Add(Box<IndexDoc>),
    Delete(MessageId),
    DeleteAll,
    Commit(oneshot::Sender<Result<()>>),
    Progress { done: u64, total: u64 },
}

/// Handle to the indexer thread.
pub(crate) struct IndexerTask {
    tx: Mutex<Option<mpsc::Sender<IndexCommand>>>,
    thread: Mutex<Option<JoinHandle<Result<()>>>>,
}

impl fmt::Debug for IndexerTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexerTask").finish_non_exhaustive()
    }
}

impl IndexerTask {
    pub(crate) fn spawn(
        index: SearchIndex,
        indexer: Indexer,
        events: tokio_mpsc::UnboundedSender<CoreEvent>,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("mazemail-indexer".to_owned())
            .spawn(move || run(&index, indexer, &rx, &events))
            .map_err(Error::io("starting the indexer thread"))?;
        Ok(Self {
            tx: Mutex::new(Some(tx)),
            thread: Mutex::new(Some(thread)),
        })
    }

    pub(crate) fn sender(&self) -> Option<mpsc::Sender<IndexCommand>> {
        self.tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn send(&self, command: IndexCommand) -> Result<()> {
        self.sender()
            .ok_or(Error::Closed)?
            .send(command)
            .map_err(|_| Error::Closed)
    }

    /// Commits now and waits for it.
    pub(crate) async fn commit(&self) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.send(IndexCommand::Commit(reply))?;
        rx.await.map_err(|_| Error::Closed)?
    }

    /// Closes the channel; the thread commits, waits for merges and exits.
    pub(crate) fn stop(&self) -> Option<JoinHandle<Result<()>>> {
        self.tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

fn run(
    index: &SearchIndex,
    mut indexer: Indexer,
    rx: &mpsc::Receiver<IndexCommand>,
    events: &tokio_mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let mut rebuild: Option<(u64, u64)> = None;
    let progress = |indexer: &Indexer, rebuild: Option<(u64, u64)>| {
        let _ = events.send(CoreEvent::Index(IndexProgress {
            indexed: index.num_docs(),
            pending: indexer.pending(),
            rebuild,
        }));
    };
    loop {
        let received = match indexer.commit_due() {
            Some(due) => rx.recv_timeout(due.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let committed = match received {
            Ok(IndexCommand::Add(doc)) => indexer.replace(&doc),
            Ok(IndexCommand::Delete(id)) => indexer.delete(id),
            Ok(IndexCommand::DeleteAll) => indexer.delete_all().map(|()| false),
            Ok(IndexCommand::Commit(reply)) => {
                let result = indexer.commit().map_err(Error::from);
                let ok = result.is_ok();
                let _ = reply.send(result);
                Ok(ok)
            }
            Ok(IndexCommand::Progress { done, total }) => {
                rebuild = (done < total).then_some((done, total));
                progress(&indexer, rebuild);
                Ok(false)
            }
            Err(RecvTimeoutError::Timeout) => indexer.commit().map(|()| true),
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match committed {
            Ok(true) => progress(&indexer, rebuild),
            Ok(false) => {}
            Err(err) => tracing::error!(%err, "search indexer operation failed"),
        }
    }
    indexer.finish()?;
    progress_final(index, events);
    tracing::debug!("search indexer stopped");
    Ok(())
}

fn progress_final(index: &SearchIndex, events: &tokio_mpsc::UnboundedSender<CoreEvent>) {
    let _ = events.send(CoreEvent::Index(IndexProgress {
        indexed: index.num_docs(),
        pending: 0,
        rebuild: None,
    }));
}

/// Core's side of the `ingest::IndexSink` seam: queues documents for the indexer
/// thread. Cheap to clone; never blocks.
#[derive(Clone)]
pub struct CoreIndexSink {
    pub(crate) tx: Option<mpsc::Sender<IndexCommand>>,
}

impl fmt::Debug for CoreIndexSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreIndexSink")
            .field("open", &self.tx.is_some())
            .finish()
    }
}

impl CoreIndexSink {
    /// Adds a message (replacing any indexed version of it).
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] after shutdown.
    pub fn add(&self, document: IndexDocument) -> Result<()> {
        self.send(IndexCommand::Add(Box::new(document.into_search_doc())))
    }

    /// Removes a message from the index.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] after shutdown.
    pub fn delete(&self, message: MessageId) -> Result<()> {
        self.send(IndexCommand::Delete(message))
    }

    fn send(&self, command: IndexCommand) -> Result<()> {
        self.tx
            .as_ref()
            .ok_or(Error::Closed)?
            .send(command)
            .map_err(|_| Error::Closed)
    }
}

/// `ingest`'s documents go straight to the indexer thread. A document that arrives
/// after shutdown is dropped (and logged): the next start's
/// [`CoreHandle::check_index`](crate::CoreHandle::check_index) or rebuild picks the
/// message up from the store.
impl ingest::IndexSink for CoreIndexSink {
    fn add(&self, doc: ingest::IndexDoc) {
        if let Err(err) = CoreIndexSink::add(self, IndexDocument::from(doc)) {
            tracing::warn!(%err, "an index document arrived after shutdown");
        }
    }

    fn delete(&self, message: MessageId) {
        if let Err(err) = CoreIndexSink::delete(self, message) {
            tracing::warn!(%err, "an index delete arrived after shutdown");
        }
    }
}

/// One `ingest` participant as an [`Address`]: the display form carries the name, the
/// lowercased address is the fallback when the display form doesn't parse.
fn index_address(address: ingest::IndexAddress) -> Address {
    match Address::parse(&address.display) {
        Ok(parsed) => parsed,
        Err(_) => Address {
            name: None,
            email: address.address,
        },
    }
}

impl From<ingest::IndexDoc> for IndexDocument {
    fn from(doc: ingest::IndexDoc) -> Self {
        let addresses =
            |list: Vec<ingest::IndexAddress>| list.into_iter().map(index_address).collect();
        Self {
            message_id: doc.message,
            thread_id: doc.thread,
            account_id: doc.account,
            date: doc.date,
            size: doc.size,
            from: addresses(doc.from),
            to: addresses(doc.to),
            cc: addresses(doc.cc),
            subject: Sensitive::new(doc.subject),
            body_text: Sensitive::new(doc.body),
            attachment_names: doc
                .attachment_names
                .into_iter()
                .map(Sensitive::new)
                .collect(),
            list_id: doc.list_id,
            has_attachment: doc.has_attachment,
            language: doc.language.map(|lang| match lang {
                ingest::IndexLanguage::De => Lang::De,
                ingest::IndexLanguage::En => Lang::En,
            }),
        }
    }
}

/// What [`CoreHandle::reindex_all`](crate::CoreHandle::reindex_all) did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReindexReport {
    /// Messages indexed.
    pub indexed: u64,
    /// Of those, messages without a stored body (indexed from their headers only;
    /// `ingest` re-adds them with the body once it arrives).
    pub headers_only: u64,
    /// Messages whose body couldn't be read or parsed (indexed from their headers).
    pub unreadable: u64,
}

/// What [`CoreHandle::check_index`](crate::CoreHandle::check_index) found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexCheck {
    /// Messages in the store (with at least one location).
    pub store_messages: u64,
    /// Documents in the index.
    pub indexed_documents: u64,
    /// Messages in the store but not in the index.
    pub missing: Vec<MessageId>,
    /// Documents in the index for messages the store doesn't have (any more).
    pub stale: Vec<MessageId>,
    /// Messages indexed more than once.
    pub duplicated: Vec<MessageId>,
    /// Whether the differences were repaired.
    pub repaired: bool,
}

impl IndexCheck {
    /// Whether index and store agree.
    pub fn is_consistent(&self) -> bool {
        self.missing.is_empty() && self.stale.is_empty() && self.duplicated.is_empty()
    }
}

/// Ids in the index, with duplicates reported separately.
pub(crate) fn indexed_ids(index: &SearchIndex) -> Result<(BTreeSet<MessageId>, Vec<MessageId>)> {
    let searcher = index.searcher();
    let total = usize::try_from(searcher.num_docs()).unwrap_or(usize::MAX);
    let hits = searcher.search(
        &search::TantivyPart::everything(),
        &search::SearchOptions {
            limit: total,
            ..search::SearchOptions::default()
        },
    )?;
    let mut ids = BTreeSet::new();
    let mut duplicated = BTreeSet::new();
    for hit in hits {
        if !ids.insert(hit.message_id) {
            duplicated.insert(hit.message_id);
        }
    }
    Ok((ids, duplicated.into_iter().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(has_attachment: bool, names: &[&str]) -> IndexDocument {
        IndexDocument {
            message_id: MessageId(1),
            thread_id: ThreadId(2),
            account_id: AccountId(3),
            date: 4,
            size: 5,
            from: vec![Address::new(Some("Anna"), "anna@example.com")],
            to: Vec::new(),
            cc: Vec::new(),
            subject: Sensitive::new("Quarterly numbers".to_owned()),
            body_text: Sensitive::new("Body".to_owned()),
            attachment_names: names
                .iter()
                .map(|n| Sensitive::new((*n).to_owned()))
                .collect(),
            list_id: None,
            has_attachment,
            language: Some(Lang::Other),
        }
    }

    #[test]
    fn conversion_keeps_has_attachment_exact() {
        let doc = document(true, &[]).into_search_doc();
        assert_eq!(doc.attachments.len(), 1);
        let doc = document(true, &["a.pdf", "b.pdf"]).into_search_doc();
        assert_eq!(doc.attachments.len(), 2);
        let doc = document(false, &["inline.png"]).into_search_doc();
        assert!(doc.attachments.is_empty());
        assert_eq!(doc.language, None);
        assert_eq!(doc.message_id, MessageId(1));
        assert_eq!(doc.body.expose(), "Body");
    }

    #[test]
    fn debug_is_redacted() {
        let debug = format!("{:?}", document(false, &[]));
        assert!(!debug.contains("Quarterly"));
        assert!(!debug.contains("anna@"));
    }
}
