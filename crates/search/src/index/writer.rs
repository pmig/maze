//! The indexer: an `IndexWriter` with a commit policy.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mazemail_types::MessageId;
use tantivy::{IndexReader, IndexWriter, TantivyDocument, Term};

use super::doc::{IndexDoc, id_to_u64};
use super::schema::Fields;
use crate::error::Result;

/// When the [`Indexer`] commits on its own.
///
/// A commit makes changes durable and visible to new searchers. Committing too often
/// creates many tiny segments; too rarely delays search freshness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitPolicy {
    /// Commit once this many operations are pending (default 2000).
    pub max_pending: usize,
    /// Commit once the oldest pending operation is this old (default 1 s).
    pub max_delay: Duration,
}

impl Default for CommitPolicy {
    fn default() -> Self {
        Self {
            max_pending: 2000,
            max_delay: Duration::from_secs(1),
        }
    }
}

/// Adds, replaces and deletes messages in the index. Only one indexer can exist per
/// index at a time (Tantivy's writer lock).
///
/// The policy is checked after every operation. There is no background timer: call
/// [`Indexer::maybe_commit`] when [`Indexer::commit_due`] says so (or at the end of a
/// sync batch call [`Indexer::commit`]) so that a trickle of changes still becomes
/// visible within `max_delay`.
pub struct Indexer {
    writer: IndexWriter,
    reader: IndexReader,
    fields: Arc<Fields>,
    policy: CommitPolicy,
    pending: usize,
    oldest_pending: Option<Instant>,
}

impl std::fmt::Debug for Indexer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Indexer")
            .field("policy", &self.policy)
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

impl Indexer {
    pub(crate) fn new(
        writer: IndexWriter,
        reader: IndexReader,
        fields: Arc<Fields>,
        policy: CommitPolicy,
    ) -> Self {
        Self {
            writer,
            reader,
            fields,
            policy,
            pending: 0,
            oldest_pending: None,
        }
    }

    fn id_term(&self, id: MessageId) -> Term {
        Term::from_field_u64(self.fields.id, id_to_u64(id.0))
    }

    fn touched(&mut self) -> Result<bool> {
        self.pending += 1;
        self.oldest_pending.get_or_insert_with(Instant::now);
        self.maybe_commit()
    }

    /// Adds a message that is not in the index yet. Returns whether this committed.
    ///
    /// Adding a message twice indexes it twice; use [`Indexer::replace`] when unsure.
    pub fn add(&mut self, doc: &IndexDoc) -> Result<bool> {
        let document: TantivyDocument = doc.to_document(&self.fields);
        self.writer.add_document(document)?;
        self.touched()
    }

    /// Adds a message, replacing any indexed version of it. Returns whether this
    /// committed.
    pub fn replace(&mut self, doc: &IndexDoc) -> Result<bool> {
        self.writer.delete_term(self.id_term(doc.message_id));
        let document: TantivyDocument = doc.to_document(&self.fields);
        self.writer.add_document(document)?;
        self.touched()
    }

    /// Removes a message. Returns whether this committed.
    pub fn delete(&mut self, message_id: MessageId) -> Result<bool> {
        self.writer.delete_term(self.id_term(message_id));
        self.touched()
    }

    /// Removes every message (before a rebuild), discarding uncommitted changes too.
    /// Takes effect on the next commit.
    pub fn delete_all(&mut self) -> Result<()> {
        // Tantivy's delete-all only covers committed segments, so drop pending adds.
        self.writer.rollback()?;
        self.writer.delete_all_documents()?;
        self.pending += 1;
        self.oldest_pending.get_or_insert_with(Instant::now);
        Ok(())
    }

    /// Operations not yet committed.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// When the policy wants the next commit, if anything is pending.
    pub fn commit_due(&self) -> Option<Instant> {
        self.oldest_pending
            .map(|oldest| oldest + self.policy.max_delay)
    }

    /// Commits if the policy says so. Returns whether it committed.
    pub fn maybe_commit(&mut self) -> Result<bool> {
        let due = self.pending >= self.policy.max_pending
            || self
                .oldest_pending
                .is_some_and(|oldest| oldest.elapsed() >= self.policy.max_delay);
        if due {
            self.commit()?;
        }
        Ok(due)
    }

    /// Commits now and makes the changes visible to new searchers.
    pub fn commit(&mut self) -> Result<()> {
        if self.pending > 0 {
            self.writer.commit()?;
            self.pending = 0;
            self.oldest_pending = None;
        }
        self.reader.reload()?;
        tracing::debug!("search index committed");
        Ok(())
    }

    /// Drops uncommitted changes.
    pub fn rollback(&mut self) -> Result<()> {
        self.writer.rollback()?;
        self.pending = 0;
        self.oldest_pending = None;
        Ok(())
    }

    /// Commits and waits for background merges, releasing the writer lock.
    pub fn finish(mut self) -> Result<()> {
        self.commit()?;
        self.writer.wait_merging_threads()?;
        Ok(())
    }
}
