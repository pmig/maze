//! Global search (PLAN §4.4, CONCEPT §13).
//!
//! [`CoreHandle::search`](crate::CoreHandle::search) parses the query, plans it with
//! the user's time zone (dates like `before:2026-01-01` mean local midnight), evaluates
//! the plan against the index and, for folders, flags, tags and accounts, against the
//! store (see the private `metadata` module), groups the matching messages by thread, and adds facets
//! and highlighted snippets. Snippet text comes from the reader cache (or the blob,
//! which then fills the cache); the index stores no bodies.

use std::collections::{BTreeSet, HashMap, HashSet};

use mazemail_types::{FolderId, MessageId, TagId, ThreadId};
use search::plan::IndexExpr;
use search::query::{ParseWarning, parse};
use search::{
    ExecuteOptions, FacetOptions, Highlighter, IndexSearcher, PlanContext, QueryPlan, Scope,
    SearchIndex, SearchOptions, Snippet, TantivyPart,
};
use store::{ListScope, MessageScope, Queries, Reader, Store};
use time::{OffsetDateTime, UtcOffset};

use crate::indexing::search_language;
use crate::message::load_rendered;
use crate::metadata::StoreMetadata;
use crate::{Error, Result};

/// Characters of body text in a snippet.
const SNIPPET_CHARS: usize = 200;

/// How search results are ordered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SearchSort {
    /// Best match first.
    #[default]
    Relevance,
    /// Newest thread first.
    NewestFirst,
    /// Oldest thread first.
    OldestFirst,
}

/// Options for [`CoreHandle::search`](crate::CoreHandle::search).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOpts {
    /// Threads per page.
    pub limit: usize,
    /// Threads to skip.
    pub offset: usize,
    /// The order.
    pub sort: SearchSort,
    /// Only search this folder, unified folder or account.
    pub scope: Option<ListScope>,
    /// Compute facets (people, attachments, dates, folders, tags).
    pub facets: bool,
    /// Highlighted subject and body snippets for the returned threads.
    pub snippets: bool,
    /// The time zone dates in the query refer to; `None`: the core's
    /// [`CoreConfig::utc_offset`](crate::CoreConfig::utc_offset).
    pub utc_offset: Option<UtcOffset>,
}

impl Default for SearchOpts {
    fn default() -> Self {
        Self {
            limit: 50,
            offset: 0,
            sort: SearchSort::Relevance,
            scope: None,
            facets: false,
            snippets: true,
            utc_offset: None,
        }
    }
}

/// One thread with matching messages.
#[derive(Clone, PartialEq)]
pub struct SearchThread {
    /// The thread.
    pub thread: ThreadId,
    /// Its best matching message (by the requested order).
    pub best_message: MessageId,
    /// Every matching message of the thread, best first.
    pub matched: Vec<MessageId>,
    /// Date of the newest matching message.
    pub latest_date: i64,
    /// Relevance of the best message, when the query had text.
    pub score: Option<f32>,
    /// The best message's subject with matches highlighted.
    pub subject: Option<Snippet>,
    /// An excerpt of the best message's body with matches highlighted.
    pub body: Option<Snippet>,
}

impl std::fmt::Debug for SearchThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchThread")
            .field("thread", &self.thread)
            .field("best_message", &self.best_message)
            .field("matched", &self.matched.len())
            .field("latest_date", &self.latest_date)
            .field("score", &self.score)
            .finish_non_exhaustive()
    }
}

/// Facet counts over all matching messages (up to the index's result window).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SearchFacets {
    /// Most frequent senders (lowercased addresses).
    pub senders: Vec<(String, u64)>,
    /// Most frequent recipients (To and Cc).
    pub recipients: Vec<(String, u64)>,
    /// Messages with attachments.
    pub with_attachment: u64,
    /// Messages without attachments.
    pub without_attachment: u64,
    /// `(bucket start, count)` per 30 days, ascending.
    pub histogram: Vec<(i64, u64)>,
    /// Matching messages per folder (counted in SQLite: folders are mutable).
    pub folders: Vec<(FolderId, u64)>,
    /// Matching messages per tag (counted in SQLite).
    pub tags: Vec<(TagId, u64)>,
}

impl std::fmt::Debug for SearchFacets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchFacets")
            .field("senders", &self.senders.len())
            .field("with_attachment", &self.with_attachment)
            .field("without_attachment", &self.without_attachment)
            .field("folders", &self.folders)
            .field("tags", &self.tags)
            .finish_non_exhaustive()
    }
}

/// The result of a search.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResults {
    /// The requested page of threads.
    pub threads: Vec<SearchThread>,
    /// Threads with matches in total.
    pub total_threads: usize,
    /// Matching messages in total.
    pub total_messages: usize,
    /// Whether an index lookup hit the result window, so matches may be missing.
    pub truncated: bool,
    /// Facets, if requested.
    pub facets: Option<SearchFacets>,
    /// Parser warnings to show under the search box.
    pub warnings: Vec<ParseWarning>,
}

/// Runs a search. Blocking: call it on a blocking thread.
pub(crate) fn run(
    store: &Store,
    index: &SearchIndex,
    query: &str,
    opts: &SearchOpts,
    utc_offset: UtcOffset,
) -> Result<SearchResults> {
    let parsed = parse(query)?;
    let context = PlanContext::new(
        OffsetDateTime::now_utc(),
        opts.utc_offset.unwrap_or(utc_offset),
    );
    let plan = QueryPlan::new(&parsed.query, &context).map_err(search::Error::Plan)?;
    let searcher = index.searcher();
    let reader = store.reader()?;
    let metadata = StoreMetadata::new(&reader)?;
    let executed = searcher.execute(&plan, &metadata, &ExecuteOptions::default())?;

    let mut hits = executed.hits;
    if let Some(scope) = opts.scope {
        let in_scope: HashSet<MessageId> = reader
            .message_ids_in_scope(&message_scope(scope))?
            .into_iter()
            .collect();
        hits.retain(|(id, _)| in_scope.contains(id));
    }
    let ids: Vec<MessageId> = hits.iter().map(|(id, _)| *id).collect();
    let placement = thread_and_date(&searcher, &reader, &ids)?;
    let mut threads = group(&hits, &placement);
    match opts.sort {
        SearchSort::Relevance => {}
        SearchSort::NewestFirst => {
            threads.sort_by(|a, b| {
                b.latest_date
                    .cmp(&a.latest_date)
                    .then(a.thread.cmp(&b.thread))
            });
        }
        SearchSort::OldestFirst => {
            threads.sort_by(|a, b| {
                a.latest_date
                    .cmp(&b.latest_date)
                    .then(a.thread.cmp(&b.thread))
            });
        }
    }
    let total_threads = threads.len();
    let mut page: Vec<SearchThread> = threads
        .into_iter()
        .skip(opts.offset)
        .take(opts.limit)
        .collect();

    if opts.snippets {
        let highlighter = match highlight_part(&plan) {
            Some(part) => Some(searcher.highlighter(&part, SNIPPET_CHARS)?),
            None => None,
        };
        for thread in &mut page {
            add_snippets(store, &reader, highlighter.as_ref(), thread)?;
        }
    }

    let facets = if opts.facets {
        Some(facets(&searcher, &reader, &ids)?)
    } else {
        None
    };

    Ok(SearchResults {
        threads: page,
        total_threads,
        total_messages: ids.len(),
        truncated: executed.truncated,
        facets,
        warnings: parsed.warnings,
    })
}

pub(crate) fn message_scope(scope: ListScope) -> MessageScope {
    match scope {
        ListScope::Folder(folder) => MessageScope::Folder(folder),
        ListScope::Unified(role) => MessageScope::Role(role),
        ListScope::Account(account) => MessageScope::Account(account),
    }
}

/// Thread and date of each id: from the index's fast fields, else from the store
/// (messages not indexed yet).
fn thread_and_date(
    searcher: &IndexSearcher,
    reader: &Reader,
    ids: &[MessageId],
) -> Result<HashMap<MessageId, (ThreadId, i64)>> {
    let mut out = HashMap::with_capacity(ids.len());
    if ids.is_empty() {
        return Ok(out);
    }
    let hits = searcher.search(
        &TantivyPart::everything(),
        &SearchOptions {
            limit: ids.len(),
            scope: Scope {
                messages: Some(ids.to_vec()),
                accounts: None,
            },
            ..SearchOptions::default()
        },
    )?;
    for hit in hits {
        out.insert(hit.message_id, (hit.thread_id, hit.date));
    }
    for id in ids {
        if !out.contains_key(id)
            && let Some(detail) = reader.message(*id)?
        {
            out.insert(*id, (detail.thread, detail.date));
        }
    }
    Ok(out)
}

/// Groups hits (best first) by thread, in order of each thread's best hit.
fn group(
    hits: &[(MessageId, Option<f32>)],
    placement: &HashMap<MessageId, (ThreadId, i64)>,
) -> Vec<SearchThread> {
    let mut order: Vec<ThreadId> = Vec::new();
    let mut groups: HashMap<ThreadId, SearchThread> = HashMap::new();
    for (id, score) in hits {
        let Some((thread, date)) = placement.get(id).copied() else {
            continue;
        };
        groups
            .entry(thread)
            .and_modify(|group| {
                group.matched.push(*id);
                group.latest_date = group.latest_date.max(date);
            })
            .or_insert_with(|| {
                order.push(thread);
                SearchThread {
                    thread,
                    best_message: *id,
                    matched: vec![*id],
                    latest_date: date,
                    score: *score,
                    subject: None,
                    body: None,
                }
            });
    }
    order
        .into_iter()
        .filter_map(|thread| groups.remove(&thread))
        .collect()
}

/// The index terms of a plan, for highlighting.
fn highlight_part(plan: &QueryPlan) -> Option<TantivyPart> {
    let mut exprs: Vec<IndexExpr> = Vec::new();
    if let Some(text) = &plan.text {
        exprs.push(text.expr().clone());
    }
    for clause in &plan.mixed {
        exprs.push(clause.index.expr().clone());
    }
    (!exprs.is_empty()).then(|| TantivyPart::new(IndexExpr::Or(exprs)))
}

fn add_snippets(
    store: &Store,
    reader: &Reader,
    highlighter: Option<&Highlighter>,
    thread: &mut SearchThread,
) -> Result<()> {
    let Some(detail) = reader.message(thread.best_message)? else {
        return Ok(());
    };
    let rendered = match detail.blob {
        Some(blob) => match load_rendered(store.blobs(), &blob) {
            Ok(rendered) => rendered.map(|(body, _)| body),
            Err(Error::Store(store::Error::BlobMissing(_))) => None,
            Err(err) => return Err(err),
        },
        None => None,
    };
    let language = search_language(rendered.as_ref().and_then(|r| r.language));
    let subject = detail.subject.clone().unwrap_or_default();
    let body = rendered
        .as_ref()
        .map(|r| r.reader.plain_text())
        .or_else(|| detail.snippet.clone())
        .unwrap_or_default();
    match highlighter {
        Some(highlighter) => {
            thread.subject = Some(highlighter.subject(&subject, language));
            thread.body = Some(highlighter.body(&body, language));
        }
        None => {
            thread.subject = Some(Snippet {
                fragment: subject,
                highlighted: Vec::new(),
            });
            let end = body
                .char_indices()
                .nth(SNIPPET_CHARS)
                .map_or(body.len(), |(i, _)| i);
            thread.body = Some(Snippet {
                fragment: body[..end].trim_end().to_owned(),
                highlighted: Vec::new(),
            });
        }
    }
    Ok(())
}

fn facets(searcher: &IndexSearcher, reader: &Reader, ids: &[MessageId]) -> Result<SearchFacets> {
    if ids.is_empty() {
        return Ok(SearchFacets::default());
    }
    let index = searcher.facets(
        &TantivyPart::everything(),
        &FacetOptions {
            scope: Scope {
                messages: Some(ids.to_vec()),
                accounts: None,
            },
            ..FacetOptions::default()
        },
    )?;
    Ok(SearchFacets {
        senders: index.senders,
        recipients: index.recipients,
        with_attachment: index.with_attachment,
        without_attachment: index.without_attachment,
        histogram: index.histogram,
        folders: reader.folder_counts(ids)?,
        tags: reader.tag_counts(ids)?,
    })
}

/// Threads in `scope` with a message matching `text` in the index (Quick Filter body
/// matches).
pub(crate) fn body_matching_threads(
    index: &SearchIndex,
    reader: &Reader,
    scope: &MessageScope,
    text: &str,
) -> Result<BTreeSet<ThreadId>> {
    let ids = reader.message_ids_in_scope(scope)?;
    if ids.is_empty() {
        return Ok(BTreeSet::new());
    }
    let hits = index.searcher().search(
        &TantivyPart::text(text),
        &SearchOptions {
            limit: search::RESULT_WINDOW,
            scope: Scope {
                messages: Some(ids),
                accounts: None,
            },
            ..SearchOptions::default()
        },
    )?;
    Ok(hits.into_iter().map(|hit| hit.thread_id).collect())
}
