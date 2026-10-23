//! Message lists for the unthreaded view (CONCEPT §8.2: "with threads off, the list
//! shows one row per message"), and message rows for those lists and for the messages
//! of an expanded thread.
//!
//! A [`MessageListSnapshot`] holds only [`MessageKey`]s, 16 bytes per row, like a
//! [`ThreadListSnapshot`](crate::ThreadListSnapshot); the view asks for the rows of
//! its visible range through [`CoreHandle::message_rows`](crate::CoreHandle::message_rows).
//! Rows come straight from one batched store query (0.07 ms for 60 rows at 1M
//! messages), so they don't go through the thread row LRU.
//!
//! The Quick Filter's toggles go to SQL with the key query. Typed text keeps the
//! messages of the threads that match (subject and senders in memory, plus body
//! matches with [`CoreHandle::message_quick_filter_body`](crate::CoreHandle::message_quick_filter_body)):
//! the same set of conversations the threaded view shows, one row per message.

use std::collections::HashSet;
use std::sync::Arc;

use mazemail_types::{FolderId, MessageId};
use store::{
    ListFilter, ListScope, MessageKey, MessagePage, MessageRow, Queries, Reader, SortOrder,
};

use crate::Result;
use crate::correspondents::{Correspondents, correspondents};
use crate::thread_list::{KEY_PAGE, ListQuery};

/// The keys of a message list at one moment.
#[derive(Debug, Clone)]
pub struct MessageListSnapshot {
    query: ListQuery,
    keys: Arc<[MessageKey]>,
}

impl MessageListSnapshot {
    pub(crate) fn new(query: ListQuery, keys: Vec<MessageKey>) -> Self {
        Self {
            query,
            keys: keys.into(),
        }
    }

    /// The query this snapshot answers.
    pub fn query(&self) -> &ListQuery {
        &self.query
    }

    /// Pass to [`CoreHandle::message_rows`](crate::CoreHandle::message_rows).
    pub fn context(&self) -> Option<FolderId> {
        self.query.context()
    }

    /// All keys, in display order.
    pub fn keys(&self) -> &[MessageKey] {
        &self.keys
    }

    /// Number of messages.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The message ids of a range of rows (clamped).
    pub fn message_ids(&self, range: std::ops::Range<usize>) -> Vec<MessageId> {
        let end = range.end.min(self.keys.len());
        let start = range.start.min(end);
        self.keys[start..end].iter().map(|k| k.message).collect()
    }

    /// The row index of a message, if it is in the list.
    pub fn position(&self, message: MessageId) -> Option<usize> {
        self.keys.iter().position(|k| k.message == message)
    }
}

/// Every message key of `scope`, page by page.
pub(crate) fn all_message_keys(
    reader: &Reader,
    scope: ListScope,
    sort: SortOrder,
    filter: &ListFilter,
) -> Result<Vec<MessageKey>> {
    let mut keys: Vec<MessageKey> = Vec::new();
    let mut page = MessagePage::first(KEY_PAGE);
    // Unified scopes can repeat a message on a later page.
    let mut seen: Option<HashSet<MessageId>> =
        matches!(scope, ListScope::Unified(_)).then(HashSet::new);
    loop {
        let batch = reader.message_list_keys(scope, sort, filter, page)?;
        let full = batch.len() == page.limit;
        let last = batch.last().copied();
        match &mut seen {
            Some(seen) => keys.extend(batch.into_iter().filter(|k| seen.insert(k.message))),
            None => keys.extend(batch),
        }
        match (full, last) {
            (true, Some(last)) => page = MessagePage::after(last, KEY_PAGE),
            _ => break,
        }
    }
    Ok(keys)
}

/// A message row with what the Correspondents column needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageListRow {
    /// The row data.
    pub row: MessageRow,
    /// Mail you sent (see [`crate::correspondents::is_outgoing`]).
    pub outgoing: bool,
}

impl MessageListRow {
    /// What the Correspondents column shows.
    pub fn correspondents(&self) -> Correspondents<'_> {
        correspondents(
            self.row.from.as_ref(),
            &self.row.to,
            &self.row.cc,
            self.outgoing,
        )
    }
}
