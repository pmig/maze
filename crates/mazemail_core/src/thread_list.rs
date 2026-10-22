//! Thread lists: queries, key snapshots and the Quick Filter (PLAN §4.3, §4.4).
//!
//! A [`ThreadListSnapshot`] holds only [`ListKey`]s, 16 bytes per row, so a folder
//! with a million threads costs about 16 MB. The view asks for the rows of its visible
//! range through [`CoreHandle::rows`](crate::CoreHandle::rows).
//!
//! # Quick Filter
//!
//! - **Metadata toggles** (unread, starred, attachment, tag) go to SQL with the key
//!   query itself (one frame, CONCEPT §3).
//! - **Typed text** first matches the subject and the senders of the folder's threads
//!   in memory ([`CoreHandle::thread_list`](crate::CoreHandle::thread_list), at once);
//!   the snapshot then reports [`ThreadListSnapshot::body_matches_pending`], and
//!   [`CoreHandle::quick_filter_body`](crate::CoreHandle::quick_filter_body) adds the
//!   threads whose messages match in the index (body, recipients, attachment names),
//!   scoped to the folder, within the search budget.
//!
//! The in-memory text is a per-scope map from thread to its lowercased subject and
//! participants, built from rows on the first keystroke and kept current by store
//! change sets, so later keystrokes are a scan over memory.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mazemail_types::{FolderId, Sensitive, TagId, ThreadId};
use store::{ListFilter, ListKey, ListScope, Page, Queries, Reader, SortOrder, ThreadRow};

use crate::Result;

/// Keys fetched per keyset page when building a snapshot.
pub(crate) const KEY_PAGE: usize = 200_000;

/// Rows loaded per query when building the in-memory text of a scope.
const TEXT_ROW_CHUNK: usize = 5_000;

/// The Quick Filter bar's state. All set toggles must match.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct QuickFilter {
    /// Only threads with unread messages.
    pub unread: bool,
    /// Only starred threads.
    pub flagged: bool,
    /// Only threads with attachments.
    pub has_attachment: bool,
    /// Only threads with this tag.
    pub tag: Option<TagId>,
    /// Typed text: every word must match the subject, a participant, or (after
    /// [`CoreHandle::quick_filter_body`](crate::CoreHandle::quick_filter_body)) a
    /// message in the index.
    pub text: Option<Sensitive<String>>,
}

impl QuickFilter {
    /// The SQL part of the filter.
    pub fn list_filter(&self) -> ListFilter {
        ListFilter {
            unread: self.unread,
            flagged: self.flagged,
            has_attachment: self.has_attachment,
            tag: self.tag,
        }
    }

    /// The lowercased words of the text filter, if any.
    pub(crate) fn words(&self) -> Option<Vec<String>> {
        let words: Vec<String> = self
            .text
            .as_ref()?
            .expose()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        (!words.is_empty()).then_some(words)
    }
}

/// What a thread list shows and how.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ListQuery {
    /// A folder, a unified folder, or an account.
    pub scope: ListScope,
    /// The order.
    pub sort: SortOrder,
    /// Quick Filter toggles and text.
    pub quick_filter: QuickFilter,
}

impl ListQuery {
    /// All threads of `scope`, newest first, unfiltered.
    pub fn new(scope: ListScope) -> Self {
        Self {
            scope,
            sort: SortOrder::NewestFirst,
            quick_filter: QuickFilter::default(),
        }
    }

    /// The folder whose dates and counts the rows show (`None` for unified and
    /// account views).
    pub fn context(&self) -> Option<FolderId> {
        match self.scope {
            ListScope::Folder(folder) => Some(folder),
            ListScope::Unified(_) | ListScope::Account(_) => None,
        }
    }
}

/// The keys of a thread list at one moment.
#[derive(Debug, Clone)]
pub struct ThreadListSnapshot {
    query: ListQuery,
    keys: Arc<[ListKey]>,
    body_matches_pending: bool,
}

impl ThreadListSnapshot {
    pub(crate) fn new(query: ListQuery, keys: Vec<ListKey>, body_matches_pending: bool) -> Self {
        Self {
            query,
            keys: keys.into(),
            body_matches_pending,
        }
    }

    /// The query this snapshot answers.
    pub fn query(&self) -> &ListQuery {
        &self.query
    }

    /// Pass to [`CoreHandle::rows`](crate::CoreHandle::rows) with the visible ids.
    pub fn context(&self) -> Option<FolderId> {
        self.query.context()
    }

    /// All keys, in display order.
    pub fn keys(&self) -> &[ListKey] {
        &self.keys
    }

    /// Number of threads.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The thread ids of a range of rows (clamped), for [`CoreHandle::rows`](crate::CoreHandle::rows).
    pub fn thread_ids(&self, range: std::ops::Range<usize>) -> Vec<ThreadId> {
        let end = range.end.min(self.keys.len());
        let start = range.start.min(end);
        self.keys[start..end].iter().map(|k| k.thread).collect()
    }

    /// The row index of a thread, if it is in the list.
    pub fn position(&self, thread: ThreadId) -> Option<usize> {
        self.keys.iter().position(|k| k.thread == thread)
    }

    /// True if a text filter is active and the index-based body matches haven't been
    /// added yet (see [`CoreHandle::quick_filter_body`](crate::CoreHandle::quick_filter_body)).
    pub fn body_matches_pending(&self) -> bool {
        self.body_matches_pending
    }
}

/// Every key of `scope`, page by page.
pub(crate) fn all_keys(
    reader: &Reader,
    scope: ListScope,
    sort: SortOrder,
    filter: &ListFilter,
) -> Result<Vec<ListKey>> {
    let mut keys: Vec<ListKey> = Vec::new();
    let mut page = Page::first(KEY_PAGE);
    // Unified scopes can repeat a thread on a later page (see `thread_list_keys`).
    let mut seen: Option<HashSet<ThreadId>> =
        matches!(scope, ListScope::Unified(_)).then(HashSet::new);
    loop {
        let batch = reader.thread_list_keys(scope, sort, filter, page)?;
        let full = batch.len() == page.limit;
        let last = batch.last().copied();
        match &mut seen {
            Some(seen) => keys.extend(batch.into_iter().filter(|k| seen.insert(k.thread))),
            None => keys.extend(batch),
        }
        match (full, last) {
            (true, Some(last)) => page = Page::after(last, KEY_PAGE),
            _ => break,
        }
    }
    Ok(keys)
}

/// The lowercased text a Quick Filter word is matched against.
fn haystack(row: &ThreadRow) -> Box<str> {
    let mut text = row.subject.clone().unwrap_or_default();
    for participant in &row.participants {
        text.push('\u{1f}');
        if let Some(name) = &participant.name {
            text.push_str(name);
            text.push(' ');
        }
        text.push_str(&participant.email);
    }
    text.to_lowercase().into_boxed_str()
}

#[derive(Default)]
struct TextState {
    scope: Option<ListScope>,
    entries: HashMap<ThreadId, Box<str>>,
}

/// The in-memory subject and sender text of the threads of one scope.
#[derive(Default)]
pub(crate) struct TextCache {
    state: Mutex<TextState>,
}

impl std::fmt::Debug for TextCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("TextCache")
            .field("scope", &state.scope)
            .field("entries", &state.entries.len())
            .finish()
    }
}

impl TextCache {
    fn lock(&self) -> MutexGuard<'_, TextState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The threads among `keys` whose subject or participants contain every word.
    pub(crate) fn matching(
        &self,
        reader: &Reader,
        scope: ListScope,
        keys: &[ListKey],
        words: &[String],
    ) -> Result<HashSet<ThreadId>> {
        let mut state = self.lock();
        if state.scope != Some(scope) {
            state.scope = Some(scope);
            state.entries.clear();
        }
        let missing: Vec<ThreadId> = keys
            .iter()
            .map(|k| k.thread)
            .filter(|t| !state.entries.contains_key(t))
            .collect();
        for chunk in missing.chunks(TEXT_ROW_CHUNK) {
            for row in reader.thread_rows(chunk, None)? {
                let text = haystack(&row);
                state.entries.insert(row.id, text);
            }
        }
        Ok(keys
            .iter()
            .filter(|k| {
                state
                    .entries
                    .get(&k.thread)
                    .is_some_and(|text| words.iter().all(|w| text.contains(w.as_str())))
            })
            .map(|k| k.thread)
            .collect())
    }

    /// Forgets the text of threads that changed.
    pub(crate) fn invalidate(&self, threads: impl IntoIterator<Item = ThreadId>) {
        let mut state = self.lock();
        if state.entries.is_empty() {
            return;
        }
        for thread in threads {
            state.entries.remove(&thread);
        }
    }

    /// Forgets everything.
    pub(crate) fn clear(&self) {
        let mut state = self.lock();
        state.scope = None;
        state.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use mazemail_types::{AccountId, FolderRole};
    use store::Participant;

    use super::*;

    #[test]
    fn list_keys_are_16_bytes() {
        assert_eq!(std::mem::size_of::<ListKey>(), 16);
    }

    #[test]
    fn words_are_lowercased_and_blank_text_is_no_filter() {
        let mut filter = QuickFilter {
            text: Some(Sensitive::new("  Anna INVOICE ".to_owned())),
            ..QuickFilter::default()
        };
        assert_eq!(
            filter.words(),
            Some(vec!["anna".to_owned(), "invoice".to_owned()])
        );
        filter.text = Some(Sensitive::new("   ".to_owned()));
        assert_eq!(filter.words(), None);
        let debug = format!("{filter:?}");
        assert!(!debug.contains("Anna"));
    }

    #[test]
    fn haystack_holds_subject_and_participants() {
        let row = ThreadRow {
            id: ThreadId(1),
            account: AccountId(1),
            subject: Some("Quarterly Numbers".to_owned()),
            participants: vec![Participant {
                name: Some("Anna Example".to_owned()),
                email: "Anna@Example.com".to_owned(),
            }],
            date: 0,
            message_count: 1,
            unread_count: 0,
            flagged: false,
            has_attachments: false,
            tags: Vec::new(),
            snippet: None,
            last_message: None,
        };
        let text = haystack(&row);
        assert!(text.contains("quarterly"));
        assert!(text.contains("anna example"));
        assert!(text.contains("anna@example.com"));
    }

    #[test]
    fn context_follows_the_scope() {
        assert_eq!(
            ListQuery::new(ListScope::Folder(FolderId(3))).context(),
            Some(FolderId(3))
        );
        assert_eq!(
            ListQuery::new(ListScope::Unified(FolderRole::Inbox)).context(),
            None
        );
    }
}
