//! Evaluating a [`QueryPlan`] against the index and a metadata source.

use std::collections::{BTreeSet, HashMap};

use mazemail_types::MessageId;

use super::searcher::{IndexSearcher, RESULT_WINDOW, Scope};
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecuteOptions {
    /// At most this many ids are taken from each index lookup (best first).
    pub cap: usize,
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        Self { cap: RESULT_WINDOW }
    }
}

/// The messages matching a plan.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Executed {
    /// Matching messages with their relevance score where the text part provided one,
    /// best first, then by id.
    pub hits: Vec<(MessageId, Option<f32>)>,
    /// Whether an index lookup hit the cap, so matches may be missing.
    pub truncated: bool,
}

impl IndexSearcher {
    /// Evaluates `plan` exactly (see [`crate::plan`]):
    ///
    /// 1. `text` against the index (best `cap` hits) gives the first candidate set;
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
        let cap = options.cap.max(1);
        let mut truncated = false;
        let mut scores: HashMap<MessageId, f32> = HashMap::new();
        let mut candidates: Option<BTreeSet<MessageId>> = None;

        if let Some(text) = &plan.text {
            let (hits, complete) = self.lookup(text, None, cap)?;
            truncated |= !complete;
            scores = hits.iter().copied().collect();
            candidates = Some(hits.into_iter().map(|(id, _)| id).collect());
        }

        for clause in &plan.mixed {
            let (index_hits, complete) = self.lookup(&clause.index, candidates.as_ref(), cap)?;
            truncated |= !complete;
            let mut union = store(metadata, &clause.metadata, candidates.as_ref())?;
            for (id, score) in index_hits {
                union.insert(id);
                scores.entry(id).or_insert(score);
            }
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
        Ok(Executed { hits, truncated })
    }

    /// Index ids for `part`, best first, restricted to `within`, at most `cap`.
    /// Returns whether the result is complete.
    fn lookup(
        &self,
        part: &TantivyPart,
        within: Option<&BTreeSet<MessageId>>,
        cap: usize,
    ) -> Result<(Vec<(MessageId, f32)>, bool)> {
        let scope = Scope {
            messages: within.map(|ids| ids.iter().copied().collect()),
            accounts: None,
        };
        let (hits, total) = self.top_hits_with_count(part, &scope, cap)?;
        let complete = total <= hits.len();
        Ok((
            hits.into_iter()
                .map(|hit| (hit.message_id, hit.score))
                .collect(),
            complete,
        ))
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
