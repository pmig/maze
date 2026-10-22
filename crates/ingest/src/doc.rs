//! The search contract: [`IndexDoc`] and [`IndexSink`].

use std::fmt;
use std::sync::{Mutex, PoisonError};

use mazemail_types::{AccountId, Address, MessageId, ThreadId};
use serde::{Deserialize, Serialize};

/// The language-specific (stemmed) fields a document fills (PLAN §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexLanguage {
    /// German.
    De,
    /// English.
    En,
}

impl IndexLanguage {
    /// Maps the `mazemail_mime` language hint; other languages get only the neutral
    /// fields.
    pub fn from_hint(lang: Option<mazemail_mime::Lang>) -> Option<Self> {
        match lang? {
            mazemail_mime::Lang::De => Some(Self::De),
            mazemail_mime::Lang::En => Some(Self::En),
            mazemail_mime::Lang::Other => None,
        }
    }
}

/// One participant as the index sees it.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IndexAddress {
    /// What the UI shows: `Name <email>` or the bare address.
    pub display: String,
    /// The address, lowercased.
    pub address: String,
}

impl IndexAddress {
    /// From a parsed address.
    pub fn from_address(address: &Address) -> Self {
        Self {
            display: address.to_string(),
            address: address.email.trim().to_lowercase(),
        }
    }
}

impl fmt::Debug for IndexAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IndexAddress(<redacted>)")
    }
}

/// Everything the search index stores about a message: only fields that never change
/// (PLAN §4.4 "Mutable fields"); folders, flags and tags are filtered in SQLite.
///
/// `body` is empty for header-only messages; [`Ingestor::attach_body`] sends a full
/// document later, which replaces this one.
///
/// [`Ingestor::attach_body`]: crate::Ingestor::attach_body
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDoc {
    pub message: MessageId,
    pub thread: ThreadId,
    pub account: AccountId,
    /// Unix seconds.
    pub date: i64,
    /// Raw size in bytes.
    pub size: u64,
    pub from: Vec<IndexAddress>,
    pub to: Vec<IndexAddress>,
    pub cc: Vec<IndexAddress>,
    pub subject: String,
    /// Body text (plain text, or the text of the HTML body), capped at
    /// [`MAX_INDEX_BODY_BYTES`](crate::MAX_INDEX_BODY_BYTES).
    pub body: String,
    pub attachment_names: Vec<String>,
    pub list_id: Option<String>,
    pub has_attachment: bool,
    pub language: Option<IndexLanguage>,
}

impl fmt::Debug for IndexDoc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexDoc")
            .field("message", &self.message)
            .field("thread", &self.thread)
            .field("account", &self.account)
            .field("date", &self.date)
            .field("size", &self.size)
            .field("body_bytes", &self.body.len())
            .field("attachments", &self.attachment_names.len())
            .field("has_attachment", &self.has_attachment)
            .field("language", &self.language)
            .finish_non_exhaustive()
    }
}

/// Receives index documents after the store committed them.
///
/// Implementations queue the work (the search writer batches commits); both methods
/// are called on the ingesting thread and should return quickly.
pub trait IndexSink: Send + Sync {
    /// Adds a document, replacing any document with the same message id.
    fn add(&self, doc: IndexDoc);

    /// Removes the document of a message.
    fn delete(&self, message: MessageId);
}

/// A sink that drops everything (tools that don't index, benchmarks).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopIndexSink;

impl IndexSink for NoopIndexSink {
    fn add(&self, _doc: IndexDoc) {}

    fn delete(&self, _message: MessageId) {}
}

/// A sink that keeps the latest document per message in memory (tests).
#[derive(Debug, Default)]
pub struct MemoryIndexSink {
    docs: Mutex<Vec<IndexDoc>>,
}

impl MemoryIndexSink {
    /// An empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current documents, ordered by message id.
    pub fn docs(&self) -> Vec<IndexDoc> {
        let mut docs = self
            .docs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        docs.sort_by_key(|d| d.message);
        docs
    }

    /// The document of one message.
    pub fn get(&self, message: MessageId) -> Option<IndexDoc> {
        self.docs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|d| d.message == message)
            .cloned()
    }

    /// How many documents the sink holds.
    pub fn len(&self) -> usize {
        self.docs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether the sink holds no documents.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl IndexSink for MemoryIndexSink {
    fn add(&self, doc: IndexDoc) {
        let mut docs = self.docs.lock().unwrap_or_else(PoisonError::into_inner);
        docs.retain(|d| d.message != doc.message);
        docs.push(doc);
    }

    fn delete(&self, message: MessageId) {
        self.docs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|d| d.message != message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: i64) -> IndexDoc {
        IndexDoc {
            message: MessageId(id),
            thread: ThreadId(1),
            account: AccountId(1),
            date: 0,
            size: 10,
            from: vec![IndexAddress::from_address(&Address::new(
                Some("Anna"),
                "Anna@Example.com",
            ))],
            to: Vec::new(),
            cc: Vec::new(),
            subject: "Quarterly numbers".to_owned(),
            body: "secret body".to_owned(),
            attachment_names: Vec::new(),
            list_id: None,
            has_attachment: false,
            language: None,
        }
    }

    #[test]
    fn memory_sink_replaces_and_deletes() {
        let sink = MemoryIndexSink::new();
        sink.add(doc(2));
        sink.add(doc(1));
        let mut changed = doc(2);
        changed.body = "new".to_owned();
        sink.add(changed);
        assert_eq!(sink.len(), 2);
        assert_eq!(sink.get(MessageId(2)).unwrap().body, "new");
        assert_eq!(sink.docs()[0].message, MessageId(1));
        sink.delete(MessageId(1));
        assert_eq!(sink.len(), 1);
    }

    #[test]
    fn addresses_are_lowercased_and_debug_is_redacted() {
        let d = doc(1);
        assert_eq!(d.from[0].address, "anna@example.com");
        assert_eq!(d.from[0].display, "Anna <Anna@Example.com>");
        let debug = format!("{d:?}");
        assert!(!debug.contains("Quarterly"), "{debug}");
        assert!(!debug.contains("anna"), "{debug}");
        assert!(!debug.contains("secret"), "{debug}");
    }

    #[test]
    fn languages_map_from_hints() {
        use mazemail_mime::Lang;
        assert_eq!(
            IndexLanguage::from_hint(Some(Lang::De)),
            Some(IndexLanguage::De)
        );
        assert_eq!(IndexLanguage::from_hint(Some(Lang::Other)), None);
        assert_eq!(IndexLanguage::from_hint(None), None);
    }
}
