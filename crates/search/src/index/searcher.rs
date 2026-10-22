//! Searching: ranked hits, thread groups, facets and snippets.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use mazemail_types::{AccountId, MessageId, ThreadId};
use tantivy::aggregation::AggregationCollector;
use tantivy::aggregation::Key;
use tantivy::aggregation::agg_req::Aggregations;
use tantivy::aggregation::agg_result::{AggregationResult, AggregationResults, BucketResult};
use tantivy::collector::{Collector, Count, SegmentCollector, TopDocs};
use tantivy::columnar::Column;
use tantivy::query::Query;
use tantivy::{DocAddress, DocId, Score, Searcher, SegmentReader};

use super::compile::{Analyzers, Compiler};
use super::doc::{Language, id_from_u64};
use super::schema::{Fields, names};
use crate::error::Result;
use crate::plan::TantivyPart;

/// Subjects are highlighted whole up to this length.
const MAX_SUBJECT_CHARS: usize = 1000;

/// Thread grouping and facets look at this many top hits at most (PLAN §4.4).
pub const RESULT_WINDOW: usize = 10_000;

/// How hits are ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SortOrder {
    /// Best BM25 match first.
    #[default]
    Relevance,
    /// Most recent first; equal dates by relevance.
    NewestFirst,
    /// Oldest first; equal dates by relevance.
    OldestFirst,
}

/// Restricts a search before ranking.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scope {
    /// Only these messages (an id filter computed up front in SQLite for narrow scopes
    /// such as the current folder). `Some(vec![])` matches nothing.
    pub messages: Option<Vec<MessageId>>,
    /// Only these accounts.
    pub accounts: Option<Vec<AccountId>>,
}

/// Paging, order and scope of a search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOptions {
    /// At most this many hits (or thread groups).
    pub limit: usize,
    /// Skip this many first.
    pub offset: usize,
    /// The order.
    pub order: SortOrder,
    /// Restrictions applied inside the index.
    pub scope: Scope,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 50,
            offset: 0,
            order: SortOrder::Relevance,
            scope: Scope::default(),
        }
    }
}

/// One matching message.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    /// The message.
    pub message_id: MessageId,
    /// Its thread.
    pub thread_id: ThreadId,
    /// The BM25 relevance score (also computed when ordering by date).
    pub score: f32,
    /// The message date, Unix seconds.
    pub date: i64,
}

/// One thread with matching messages.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThreadHit {
    /// The thread.
    pub thread_id: ThreadId,
    /// Its first matching message in the requested order.
    pub best: Hit,
    /// How many of its messages matched (within the result window).
    pub matches: usize,
    /// The newest matching message date, Unix seconds.
    pub latest_date: i64,
}

/// Facet request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacetOptions {
    /// Restrictions applied inside the index.
    pub scope: Scope,
    /// At most this many people per list.
    pub people: usize,
    /// Width of a date histogram bucket in seconds (default 30 days).
    pub histogram_interval: i64,
    /// Facets are computed over at most this many top hits by relevance.
    pub window: usize,
}

impl Default for FacetOptions {
    fn default() -> Self {
        Self {
            scope: Scope::default(),
            people: 10,
            histogram_interval: 30 * 24 * 3600,
            window: RESULT_WINDOW,
        }
    }
}

/// Facet counts over a result set. Folder and tag facets are counted in SQLite over
/// the same ids (PLAN §4.4).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facets {
    /// All messages that matched.
    pub total: usize,
    /// How many of them the counts below cover (at most the window).
    pub counted: usize,
    /// Most frequent sender addresses (lowercased) with counts.
    pub senders: Vec<(String, u64)>,
    /// Most frequent recipient addresses (To and Cc, lowercased) with counts.
    pub recipients: Vec<(String, u64)>,
    /// Messages with at least one attachment.
    pub with_attachment: u64,
    /// Messages without attachments.
    pub without_attachment: u64,
    /// Message counts per date bucket: (bucket start in Unix seconds, count), ascending,
    /// empty buckets omitted.
    pub histogram: Vec<(i64, u64)>,
}

/// Highlighted text from a message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snippet {
    /// The excerpt.
    pub fragment: String,
    /// Byte ranges within `fragment` to highlight.
    pub highlighted: Vec<Range<usize>>,
}

impl Snippet {
    /// Whether nothing matched.
    pub fn is_empty(&self) -> bool {
        self.highlighted.is_empty()
    }
}

/// A point-in-time view of the index. Cheap to create; reuse it for the pages of one
/// result list so they are consistent.
#[derive(Clone)]
pub struct IndexSearcher {
    searcher: Searcher,
    fields: Arc<Fields>,
    analyzers: Analyzers,
}

impl std::fmt::Debug for IndexSearcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexSearcher")
            .field("num_docs", &self.searcher.num_docs())
            .finish_non_exhaustive()
    }
}

/// Fast field columns of one segment, to resolve doc addresses to hits.
struct SegmentColumns {
    id: Column<u64>,
    thread: Column<u64>,
    date: Column<i64>,
}

impl SegmentColumns {
    fn open(reader: &SegmentReader) -> Result<Self> {
        let fast = reader.fast_fields();
        Ok(Self {
            id: fast.u64(names::ID)?,
            thread: fast.u64(names::THREAD)?,
            date: fast.i64(names::DATE)?,
        })
    }

    fn hit(&self, doc: DocId, score: f32) -> Hit {
        Hit {
            message_id: MessageId(id_from_u64(self.id.first(doc).unwrap_or_default())),
            thread_id: ThreadId(id_from_u64(self.thread.first(doc).unwrap_or_default())),
            score,
            date: self.date.first(doc).unwrap_or_default(),
        }
    }
}

impl IndexSearcher {
    pub(crate) fn new(searcher: Searcher, fields: Arc<Fields>, analyzers: Analyzers) -> Self {
        Self {
            searcher,
            fields,
            analyzers,
        }
    }

    /// Number of indexed messages.
    pub fn num_docs(&self) -> u64 {
        self.searcher.num_docs()
    }

    fn compile(&self, part: &TantivyPart, scope: &Scope) -> Box<dyn Query> {
        Compiler::new(&self.fields, &self.analyzers).compile_scoped(part.expr(), scope)
    }

    /// Counts matching messages.
    pub fn count(&self, part: &TantivyPart, scope: &Scope) -> Result<usize> {
        let query = self.compile(part, scope);
        Ok(self.searcher.search(&*query, &Count)?)
    }

    /// Ranked hits for `part`.
    pub fn search(&self, part: &TantivyPart, options: &SearchOptions) -> Result<Vec<Hit>> {
        let query = self.compile(part, &options.scope);
        self.top_hits(&*query, options.limit, options.offset, options.order)
    }

    /// The top `limit` hits after skipping `offset`, plus the total match count.
    pub(crate) fn top_hits_with_count(
        &self,
        part: &TantivyPart,
        scope: &Scope,
        limit: usize,
    ) -> Result<(Vec<Hit>, usize)> {
        let query = self.compile(part, scope);
        let total = self.searcher.search(&*query, &Count)?;
        let hits = self.top_hits(&*query, limit, 0, SortOrder::Relevance)?;
        Ok((hits, total))
    }

    fn top_hits(
        &self,
        query: &dyn Query,
        limit: usize,
        offset: usize,
        order: SortOrder,
    ) -> Result<Vec<Hit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let window = TopDocs::with_limit(limit).and_offset(offset);
        let docs: Vec<(f32, DocAddress)> = match order {
            SortOrder::Relevance => self.searcher.search(query, &window.order_by_score())?,
            SortOrder::NewestFirst | SortOrder::OldestFirst => {
                let newest = order == SortOrder::NewestFirst;
                let collector = window.tweak_score(move |reader: &SegmentReader| {
                    let dates = reader
                        .fast_fields()
                        .i64(names::DATE)
                        .ok()
                        .map(|column| column.first_or_default_col(0));
                    move |doc: DocId, score: Score| {
                        let date = dates.as_ref().map_or(0, |dates| dates.get_val(doc));
                        let key = if newest { date } else { date.saturating_neg() };
                        (key, score)
                    }
                });
                self.searcher
                    .search(query, &collector)?
                    .into_iter()
                    .map(|((_, score), address)| (score, address))
                    .collect()
            }
        };
        self.resolve(&docs)
    }

    fn resolve(&self, docs: &[(f32, DocAddress)]) -> Result<Vec<Hit>> {
        let mut columns: HashMap<u32, SegmentColumns> = HashMap::new();
        let mut hits = Vec::with_capacity(docs.len());
        for (score, address) in docs {
            let segment = match columns.entry(address.segment_ord) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => entry.insert(
                    SegmentColumns::open(self.searcher.segment_reader(address.segment_ord))?,
                ),
            };
            hits.push(segment.hit(address.doc_id, *score));
        }
        Ok(hits)
    }

    /// Hits grouped by thread, in the requested order of each thread's first hit. Groups
    /// are built from the top [`RESULT_WINDOW`] hits; `limit` and `offset` page over
    /// groups.
    pub fn search_threads(
        &self,
        part: &TantivyPart,
        options: &SearchOptions,
    ) -> Result<Vec<ThreadHit>> {
        let query = self.compile(part, &options.scope);
        let hits = self.top_hits(&*query, RESULT_WINDOW, 0, options.order)?;
        let mut order: Vec<ThreadId> = Vec::new();
        let mut groups: HashMap<ThreadId, ThreadHit> = HashMap::new();
        for hit in hits {
            groups
                .entry(hit.thread_id)
                .and_modify(|group| {
                    group.matches += 1;
                    group.latest_date = group.latest_date.max(hit.date);
                })
                .or_insert_with(|| {
                    order.push(hit.thread_id);
                    ThreadHit {
                        thread_id: hit.thread_id,
                        best: hit,
                        matches: 1,
                        latest_date: hit.date,
                    }
                });
        }
        Ok(order
            .into_iter()
            .skip(options.offset)
            .take(options.limit)
            .filter_map(|thread| groups.remove(&thread))
            .collect())
    }

    /// People, attachment and date facets over the top hits (Tantivy aggregations).
    pub fn facets(&self, part: &TantivyPart, options: &FacetOptions) -> Result<Facets> {
        let query = self.compile(part, &options.scope);
        let window = options.window.max(1);
        let (total, docs) = self.searcher.search(
            &*query,
            &(Count, TopDocs::with_limit(window).order_by_score()),
        )?;
        let mut facets = Facets {
            total,
            counted: docs.len(),
            ..Facets::default()
        };
        if docs.is_empty() {
            return Ok(facets);
        }
        let people = options.people.max(1);
        let request: Aggregations = serde_json::from_value(serde_json::json!({
            "senders": { "terms": { "field": names::FROM_ADDR, "size": people } },
            "to": { "terms": { "field": names::TO_ADDR, "size": people } },
            "cc": { "terms": { "field": names::CC_ADDR, "size": people } },
            "attachments": { "terms": { "field": names::HAS_ATTACHMENT, "size": 2 } },
            "dates": {
                "histogram": {
                    "field": names::DATE,
                    "interval": options.histogram_interval.max(1),
                    "min_doc_count": 1
                }
            }
        }))
        .map_err(|err| tantivy::TantivyError::InvalidArgument(err.to_string()))?;
        let collector = AggregationCollector::from_aggs(request, Default::default());

        // Feed only the top window to the aggregation, segment by segment.
        let mut by_segment: HashMap<u32, Vec<DocId>> = HashMap::new();
        for (_, address) in &docs {
            by_segment
                .entry(address.segment_ord)
                .or_default()
                .push(address.doc_id);
        }
        let mut fruits = Vec::with_capacity(by_segment.len());
        for (segment_ord, mut segment_docs) in by_segment {
            segment_docs.sort_unstable();
            let reader = self.searcher.segment_reader(segment_ord);
            let mut segment = collector.for_segment(segment_ord, reader)?;
            for doc in segment_docs {
                segment.collect(doc, 0.0);
            }
            fruits.push(segment.harvest());
        }
        let results = collector.merge_fruits(fruits)?;

        facets.senders = terms(&results, "senders");
        let mut recipients: HashMap<String, u64> = HashMap::new();
        for (address, count) in terms(&results, "to")
            .into_iter()
            .chain(terms(&results, "cc"))
        {
            *recipients.entry(address).or_default() += count;
        }
        let mut recipients: Vec<(String, u64)> = recipients.into_iter().collect();
        recipients.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        recipients.truncate(people);
        facets.recipients = recipients;
        for (key, count) in bucket_keys(&results, "attachments") {
            match key {
                Key::U64(1) | Key::I64(1) => facets.with_attachment = count,
                Key::F64(value) if value >= 0.5 => facets.with_attachment = count,
                _ => facets.without_attachment = count,
            }
        }
        facets.histogram = bucket_keys(&results, "dates")
            .into_iter()
            .filter_map(|(key, count)| match key {
                Key::F64(start) => Some((start as i64, count)),
                Key::I64(start) => Some((start, count)),
                Key::U64(start) => i64::try_from(start).ok().map(|start| (start, count)),
                _ => None,
            })
            .collect();
        facets.histogram.sort_unstable();
        Ok(facets)
    }

    /// A highlighter for `part`'s terms. Create it once per result list and call it for
    /// each hit with the text the caller loaded from the store.
    pub fn highlighter(&self, part: &TantivyPart, max_chars: usize) -> Result<Highlighter> {
        let query = self.compile(part, &Scope::default());
        let generator = |field, max_chars| -> Result<tantivy::snippet::SnippetGenerator> {
            let mut generator =
                tantivy::snippet::SnippetGenerator::create(&self.searcher, &*query, field)?;
            generator.set_max_num_chars(max_chars);
            Ok(generator)
        };
        let fields = &self.fields;
        Ok(Highlighter {
            max_chars,
            body: generator(fields.body, max_chars)?,
            body_de: generator(fields.body_de, max_chars)?,
            body_en: generator(fields.body_en, max_chars)?,
            subject: generator(fields.subject, MAX_SUBJECT_CHARS)?,
            subject_de: generator(fields.subject_de, MAX_SUBJECT_CHARS)?,
            subject_en: generator(fields.subject_en, MAX_SUBJECT_CHARS)?,
        })
    }
}

fn bucket_keys(results: &AggregationResults, name: &str) -> Vec<(Key, u64)> {
    match results.0.get(name) {
        Some(AggregationResult::BucketResult(BucketResult::Terms { buckets, .. })) => buckets
            .iter()
            .map(|bucket| (bucket.key.clone(), bucket.doc_count))
            .collect(),
        Some(AggregationResult::BucketResult(BucketResult::Histogram { buckets })) => buckets
            .iter()
            .map(|bucket| (bucket.key.clone(), bucket.doc_count))
            .collect(),
        _ => Vec::new(),
    }
}

fn terms(results: &AggregationResults, name: &str) -> Vec<(String, u64)> {
    bucket_keys(results, name)
        .into_iter()
        .filter_map(|(key, count)| match key {
            Key::Str(text) => Some((text, count)),
            _ => None,
        })
        .collect()
}

/// Builds highlighted snippets for one query (Tantivy's `SnippetGenerator`).
///
/// It tries the neutral analyzer and, when the caller passes the message language, the
/// stemmed one, keeping whichever highlights more: a search for `Rechnungen` then
/// highlights `Rechnung` in a German message.
pub struct Highlighter {
    max_chars: usize,
    body: tantivy::snippet::SnippetGenerator,
    body_de: tantivy::snippet::SnippetGenerator,
    body_en: tantivy::snippet::SnippetGenerator,
    subject: tantivy::snippet::SnippetGenerator,
    subject_de: tantivy::snippet::SnippetGenerator,
    subject_en: tantivy::snippet::SnippetGenerator,
}

impl std::fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Highlighter").finish_non_exhaustive()
    }
}

impl Highlighter {
    /// The best excerpt of a message body, or its beginning when nothing matches.
    pub fn body(&self, text: &str, language: Option<Language>) -> Snippet {
        let snippet = best(
            text,
            &self.body,
            match language {
                Some(Language::De) => Some(&self.body_de),
                Some(Language::En) => Some(&self.body_en),
                None => None,
            },
        );
        if snippet.is_empty() {
            let end = text
                .char_indices()
                .nth(self.max_chars)
                .map_or(text.len(), |(index, _)| index);
            return Snippet {
                fragment: text[..end].trim_end().to_owned(),
                highlighted: Vec::new(),
            };
        }
        snippet
    }

    /// The whole subject with matches highlighted.
    pub fn subject(&self, text: &str, language: Option<Language>) -> Snippet {
        let snippet = best(
            text,
            &self.subject,
            match language {
                Some(Language::De) => Some(&self.subject_de),
                Some(Language::En) => Some(&self.subject_en),
                None => None,
            },
        );
        // Re-anchor the highlights on the whole subject instead of a fragment.
        let offset = if snippet.fragment.is_empty() {
            None
        } else {
            text.find(snippet.fragment.as_str())
        };
        Snippet {
            fragment: text.to_owned(),
            highlighted: match offset {
                Some(offset) => snippet
                    .highlighted
                    .into_iter()
                    .map(|range| range.start + offset..range.end + offset)
                    .collect(),
                None => Vec::new(),
            },
        }
    }
}

fn best(
    text: &str,
    neutral: &tantivy::snippet::SnippetGenerator,
    stemmed: Option<&tantivy::snippet::SnippetGenerator>,
) -> Snippet {
    let convert = |snippet: tantivy::snippet::Snippet| Snippet {
        fragment: snippet.fragment().to_owned(),
        highlighted: snippet.highlighted().to_vec(),
    };
    let exact = convert(neutral.snippet(text));
    match stemmed.map(|generator| convert(generator.snippet(text))) {
        Some(stemmed) if stemmed.highlighted.len() > exact.highlighted.len() => stemmed,
        _ => exact,
    }
}
