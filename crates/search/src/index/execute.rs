//! Evaluating a [`QueryPlan`] against the index and a metadata source.

use std::collections::{BTreeSet, HashMap};

use mazemail_types::{MessageId, ThreadId};

use super::searcher::{Hit, IndexSearcher, Scope};
use crate::error::{Error, Result};
use crate::plan::{MetadataFilter, QueryPlan, TantivyPart};

/// Evaluates [`MetadataFilter`]s; implemented by the store over SQLite.
pub trait MetadataSource {
    /// The store's error type.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Ids of messages matching `filter`, restricted to `within` when given. `None`
    /// means every message the store knows.
    fn matching(
        &self,
        filter: &MetadataFilter,
        within: Option<&BTreeSet<MessageId>>,
    ) -> Result<BTreeSet<MessageId>, Self::Error>;
}

/// Limits for [`IndexSearcher::execute`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecuteOptions {
    /// At most this many ids are taken from each index lookup (best first). `None`
    /// (the default): every match, so the result is exact whatever order it's shown
    /// in. A cap suits only results shown best first: a list by date would lose
    /// whatever ranked below the cap, however new.
    pub cap: Option<usize>,
}

/// The messages matching a plan.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Executed {
    /// Matching messages with their relevance score where the text part provided one,
    /// best first, then by id.
    pub hits: Vec<(MessageId, Option<f32>)>,
    /// Whether an index lookup hit the cap, so matches may be missing.
    pub truncated: bool,
    /// Thread and date (Unix seconds) of the hits the index found, read along with
    /// them. Hits only the store found (metadata terms) aren't in it.
    pub placement: HashMap<MessageId, (ThreadId, i64)>,
}

impl IndexSearcher {
    /// Evaluates `plan` exactly (see [`crate::plan`]):
    ///
    /// 1. `text` against the index (every hit, or the best `cap`) gives the first
    ///    candidate set;
    /// 2. each mixed clause narrows it to `index ids ∪ metadata ids` within the
    ///    candidates (or, without a text part, forms the first candidate set);
    /// 3. `metadata` filters the rest in the store.
    ///
    /// Without any index work the whole query runs in the store.
    pub fn execute<M: MetadataSource>(
        &self,
        plan: &QueryPlan,
        metadata: &M,
        options: &ExecuteOptions,
    ) -> Result<Executed> {
        let cap = options.cap.map(|cap| cap.max(1));
        let mut truncated = false;
        let mut scores: HashMap<MessageId, f32> = HashMap::new();
        let mut placement: HashMap<MessageId, (ThreadId, i64)> = HashMap::new();
        let mut candidates: Option<BTreeSet<MessageId>> = None;
        let mut found = |hits: &[Hit]| {
            for hit in hits {
                scores.entry(hit.message_id).or_insert(hit.score);
                placement.insert(hit.message_id, (hit.thread_id, hit.date));
            }
        };

        if let Some(text) = &plan.text {
            let (hits, complete) = self.lookup(text, None, cap)?;
            truncated |= !complete;
            found(&hits);
            candidates = Some(hits.into_iter().map(|hit| hit.message_id).collect());
        }

        for clause in &plan.mixed {
            let (index_hits, complete) = self.lookup(&clause.index, candidates.as_ref(), cap)?;
            truncated |= !complete;
            found(&index_hits);
            let mut union = store(metadata, &clause.metadata, candidates.as_ref())?;
            union.extend(index_hits.iter().map(|hit| hit.message_id));
            if let Some(within) = &candidates {
                union.retain(|id| within.contains(id));
            }
            candidates = Some(union);
        }

        let result = match candidates {
            Some(candidates) if plan.metadata.is_all() => candidates,
            candidates => store(metadata, &plan.metadata, candidates.as_ref())?,
        };

        let mut hits: Vec<(MessageId, Option<f32>)> = result
            .into_iter()
            .map(|id| (id, scores.get(&id).copied()))
            .collect();
        hits.sort_by(|a, b| {
            let score = |hit: &(MessageId, Option<f32>)| hit.1.unwrap_or(f32::NEG_INFINITY);
            score(b).total_cmp(&score(a)).then_with(|| a.0.cmp(&b.0))
        });
        Ok(Executed {
            hits,
            truncated,
            placement,
        })
    }

    /// Index hits for `part`, restricted to `within`: all of them, or the best `cap`.
    /// Returns whether the result is complete.
    fn lookup(
        &self,
        part: &TantivyPart,
        within: Option<&BTreeSet<MessageId>>,
        cap: Option<usize>,
    ) -> Result<(Vec<Hit>, bool)> {
        let scope = Scope {
            messages: within.map(|ids| ids.iter().copied().collect()),
            accounts: None,
        };
        let mut hits = self.all_hits(part, &scope)?;
        match cap {
            Some(cap) if hits.len() > cap => {
                hits.sort_unstable_by(|a, b| {
                    b.score
                        .total_cmp(&a.score)
                        .then_with(|| a.message_id.cmp(&b.message_id))
                });
                hits.truncate(cap);
                Ok((hits, false))
            }
            _ => Ok((hits, true)),
        }
    }
}

fn store<M: MetadataSource>(
    metadata: &M,
    filter: &MetadataFilter,
    within: Option<&BTreeSet<MessageId>>,
) -> Result<BTreeSet<MessageId>> {
    if let Some(within) = within
        && filter.is_all()
    {
        return Ok(within.clone());
    }
    metadata
        .matching(filter, within)
        .map_err(|err| Error::Metadata(Box::new(err)))
}
