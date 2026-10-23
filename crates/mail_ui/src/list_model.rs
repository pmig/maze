//! The index math of the message list's thread tree, as pure functions.
//!
//! # Keys only, with expansions spliced in
//!
//! The list holds the keys of its folder (thread ids, 8 bytes each) and never a row per
//! visible line. Expanding a thread splices its messages in after the thread row
//! *virtually*: [`Expansions`] keeps one [`Span`] per expanded thread (its base index
//! and how many child rows it adds) with the number of child rows before it, so the
//! visible row `v` resolves to "thread `b`" or "child `o` of thread `b`" with one
//! binary search, and the list length is `threads + child rows`. Nothing is copied and
//! the 1M-thread key list stays shared.
//!
//! # Cards: two slots per card
//!
//! GPUI's `uniform_list` needs one row height. In cards mode a card (thread or
//! message) takes two 26 px slots (its two lines) and a child row of an expanded
//! thread one slot, so children are compact single lines. [`Expansions::slot_of`] and
//! [`Expansions::resolve_slot`] map between visible rows and slots with a base weight
//! of 2 (cards) or 1 (table).
//!
//! # Expand all
//!
//! "Expand all" on a folder with a million threads needs every thread's message count,
//! and nothing else: [`Expansions::from_counts`] builds the spans from the counts (only
//! threads with replies get one, about 0.3 M spans for 1 M threads, 24 bytes each), and
//! the reply trees themselves are fetched lazily for the threads that scroll into view.
//!
//! [`tree_rows`] adds the guide lines of a reply tree (Thunderbird's tree lines).

use crate::source::TreeNode;

/// What a visible row shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowPosition {
    /// The base key at this index (a thread, or a message of an unthreaded list).
    Base(usize),
    /// Child `offset` (in tree order) of the expanded thread at base index `base`.
    Child {
        /// The thread's base index.
        base: usize,
        /// The child's position in the thread's reply tree.
        offset: usize,
    },
}

impl RowPosition {
    /// The base index of the row's thread (or message).
    pub fn base(self) -> usize {
        match self {
            Self::Base(base) | Self::Child { base, .. } => base,
        }
    }
}

/// One expanded thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// The thread's base index.
    pub base: usize,
    /// Child rows it adds.
    pub children: usize,
    /// Child rows of all spans before this one.
    pub before: usize,
}

impl Span {
    /// The visible index of the thread row.
    fn visible(&self) -> usize {
        self.base + self.before
    }

    /// The first slot of the thread row, with base rows `weight` slots tall.
    fn slot(&self, weight: usize) -> usize {
        self.base * weight + self.before
    }
}

/// The expanded threads of a list. See the [module docs](self).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expansions {
    spans: Vec<Span>,
    children: usize,
}

impl Expansions {
    /// No thread expanded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Spans for `(base, children)` pairs in ascending base order; zero counts are
    /// skipped.
    pub fn from_counts(counts: impl IntoIterator<Item = (usize, usize)>) -> Self {
        let mut spans = Vec::new();
        let mut before = 0;
        for (base, children) in counts {
            if children == 0 {
                continue;
            }
            debug_assert!(spans.last().is_none_or(|s: &Span| s.base < base));
            spans.push(Span {
                base,
                children,
                before,
            });
            before += children;
        }
        Self {
            spans,
            children: before,
        }
    }

    /// Whether no thread is expanded.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Number of expanded threads.
    pub fn count(&self) -> usize {
        self.spans.len()
    }

    /// All child rows.
    pub fn total_children(&self) -> usize {
        self.children
    }

    /// Visible rows of a list with `base_len` base keys.
    pub fn len(&self, base_len: usize) -> usize {
        base_len + self.children
    }

    /// The expanded threads, by base index.
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    fn find(&self, base: usize) -> Result<usize, usize> {
        self.spans.binary_search_by_key(&base, |s| s.base)
    }

    /// Child rows of the thread at `base` (0 if collapsed).
    pub fn children_of(&self, base: usize) -> usize {
        self.find(base).map_or(0, |ix| self.spans[ix].children)
    }

    /// Expands the thread at `base` to `children` rows; 0 collapses it. O(expanded
    /// threads after it).
    pub fn set(&mut self, base: usize, children: usize) {
        let from = match (self.find(base), children) {
            (Ok(ix), 0) => {
                self.spans.remove(ix);
                ix
            }
            (Ok(ix), _) => {
                self.spans[ix].children = children;
                ix
            }
            (Err(_), 0) => return,
            (Err(ix), _) => {
                self.spans.insert(
                    ix,
                    Span {
                        base,
                        children,
                        before: 0,
                    },
                );
                ix
            }
        };
        let mut before = if from == 0 {
            0
        } else {
            let prev = self.spans[from - 1];
            prev.before + prev.children
        };
        for span in &mut self.spans[from..] {
            span.before = before;
            before += span.children;
        }
        self.children = before;
    }

    /// Child rows before the thread at `base`.
    fn children_before(&self, base: usize) -> usize {
        let ix = self.spans.partition_point(|s| s.base < base);
        self.spans.get(ix).map_or(self.children, |s| s.before)
    }

    /// The visible index of the row of base key `base`.
    pub fn visible_of(&self, base: usize) -> usize {
        base + self.children_before(base)
    }

    /// The visible index of child `offset` of the thread at `base`.
    pub fn visible_of_child(&self, base: usize, offset: usize) -> usize {
        self.visible_of(base) + 1 + offset
    }

    /// The visible index of a position.
    pub fn visible_of_position(&self, position: RowPosition) -> usize {
        match position {
            RowPosition::Base(base) => self.visible_of(base),
            RowPosition::Child { base, offset } => self.visible_of_child(base, offset),
        }
    }

    /// What visible row `visible` shows.
    pub fn resolve(&self, visible: usize) -> RowPosition {
        let ix = self.spans.partition_point(|s| s.visible() <= visible);
        let Some(span) = ix.checked_sub(1).map(|ix| self.spans[ix]) else {
            return RowPosition::Base(visible);
        };
        let thread_row = span.visible();
        if visible == thread_row {
            RowPosition::Base(span.base)
        } else if visible <= thread_row + span.children {
            RowPosition::Child {
                base: span.base,
                offset: visible - thread_row - 1,
            }
        } else {
            RowPosition::Base(visible - span.before - span.children)
        }
    }

    /// Slots of a list with `base_len` base keys, base rows `weight` slots tall.
    pub fn slot_len(&self, base_len: usize, weight: usize) -> usize {
        base_len * weight + self.children
    }

    /// The first slot of visible row `visible`.
    pub fn slot_of(&self, visible: usize, weight: usize) -> usize {
        match self.resolve(visible) {
            RowPosition::Base(base) => base * weight + self.children_before(base),
            RowPosition::Child { base, offset } => {
                base * weight + self.children_before(base) + weight + offset
            }
        }
    }

    /// The visible row in slot `slot` and which of its slots it is (0 for children).
    pub fn resolve_slot(&self, slot: usize, weight: usize) -> (usize, usize) {
        let weight = weight.max(1);
        let ix = self.spans.partition_point(|s| s.slot(weight) <= slot);
        let Some(span) = ix.checked_sub(1).map(|ix| self.spans[ix]) else {
            return (slot / weight, slot % weight);
        };
        let thread_slot = span.slot(weight);
        if slot < thread_slot + weight {
            (span.visible(), slot - thread_slot)
        } else if slot < thread_slot + weight + span.children {
            (span.visible() + 1 + (slot - thread_slot - weight), 0)
        } else {
            let rel = slot - span.before - span.children;
            let base = rel / weight;
            (base + span.before + span.children, rel % weight)
        }
    }
}

/// A row of a reply tree with its guide lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeRow {
    /// The message and its depth.
    pub node: TreeNode,
    /// Another message at the same depth follows below this one's replies (draw `├`
    /// rather than `└`).
    pub has_next_sibling: bool,
    /// Bit `d` set: the ancestor at depth `d` has a later sibling, so a vertical guide
    /// passes through this row at that depth.
    pub lines: u64,
}

/// Guide lines for a reply tree in display order (depth first). Depths beyond 63
/// share the last bit.
pub fn tree_rows(nodes: &[TreeNode]) -> Vec<TreeRow> {
    // Backwards: a node has a next sibling if a node of the same depth comes later
    // before any shallower one.
    let mut next = vec![false; nodes.len()];
    let mut seen: Vec<bool> = Vec::new();
    for (ix, node) in nodes.iter().enumerate().rev() {
        let depth = usize::from(node.depth);
        next[ix] = seen.get(depth).copied().unwrap_or(false);
        seen.truncate(depth + 1);
        seen.resize(depth + 1, false);
        seen[depth] = true;
    }
    // Forwards: the lines of the ancestors.
    let mut ancestors: Vec<bool> = Vec::new();
    nodes
        .iter()
        .zip(next)
        .map(|(node, has_next_sibling)| {
            let depth = usize::from(node.depth);
            ancestors.truncate(depth);
            ancestors.resize(depth, false);
            let lines = ancestors
                .iter()
                .enumerate()
                .filter(|(_, line)| **line)
                .fold(0u64, |bits, (d, _)| bits | 1 << d.min(63));
            ancestors.push(has_next_sibling);
            TreeRow {
                node: *node,
                has_next_sibling,
                lines,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use mazemail_types::MessageId;

    use super::*;

    /// The rows of a list as positions, the slow way.
    fn naive(base_len: usize, expanded: &[(usize, usize)]) -> Vec<RowPosition> {
        let mut out = Vec::new();
        for base in 0..base_len {
            out.push(RowPosition::Base(base));
            if let Some((_, n)) = expanded.iter().find(|(b, _)| *b == base) {
                for offset in 0..*n {
                    out.push(RowPosition::Child { base, offset });
                }
            }
        }
        out
    }

    fn check(base_len: usize, expanded: &[(usize, usize)], e: &Expansions) {
        let rows = naive(base_len, expanded);
        assert_eq!(e.len(base_len), rows.len());
        for (v, position) in rows.iter().enumerate() {
            assert_eq!(e.resolve(v), *position, "row {v}");
            assert_eq!(e.visible_of_position(*position), v, "{position:?}");
        }
        for weight in [1, 2] {
            let mut slot = 0;
            for (v, position) in rows.iter().enumerate() {
                assert_eq!(e.slot_of(v, weight), slot, "row {v} weight {weight}");
                let height = match position {
                    RowPosition::Base(_) => weight,
                    RowPosition::Child { .. } => 1,
                };
                for half in 0..height {
                    assert_eq!(e.resolve_slot(slot + half, weight), (v, half));
                }
                slot += height;
            }
            assert_eq!(e.slot_len(base_len, weight), slot);
        }
    }

    #[test]
    fn splices_children_after_their_thread() {
        let mut e = Expansions::new();
        check(6, &[], &e);
        e.set(2, 3);
        check(6, &[(2, 3)], &e);
        e.set(0, 1);
        check(6, &[(0, 1), (2, 3)], &e);
        e.set(5, 2);
        check(6, &[(0, 1), (2, 3), (5, 2)], &e);
        e.set(2, 1);
        check(6, &[(0, 1), (2, 1), (5, 2)], &e);
        e.set(0, 0);
        check(6, &[(2, 1), (5, 2)], &e);
        e.set(4, 0);
        check(6, &[(2, 1), (5, 2)], &e);
        assert_eq!(e.count(), 2);
        assert_eq!(e.children_of(5), 2);
        assert_eq!(e.children_of(4), 0);
        assert_eq!(e.total_children(), 3);
    }

    #[test]
    fn expand_all_builds_spans_from_counts() {
        let counts = [3, 1, 1, 2, 0, 4, 1];
        let e = Expansions::from_counts(
            counts
                .iter()
                .enumerate()
                .map(|(base, n)| (base, (*n as usize).saturating_sub(1))),
        );
        check(7, &[(0, 2), (3, 1), (5, 3)], &e);
        assert_eq!(e.count(), 3);
    }

    #[test]
    fn a_million_threads_resolve_in_log_time() {
        let base_len = 1_000_000;
        let e = Expansions::from_counts((0..base_len).filter(|b| b % 3 == 0).map(|b| (b, 2)));
        assert_eq!(e.len(base_len), base_len + 2 * 333_334);
        let last = e.len(base_len) - 1;
        // The last thread is expanded: its last child is the last row.
        assert_eq!(
            e.resolve(last),
            RowPosition::Child {
                base: base_len - 1,
                offset: 1
            }
        );
        assert_eq!(e.resolve(e.visible_of(999_998)), RowPosition::Base(999_998));
        let v = e.visible_of_child(999_999, 1);
        assert_eq!(
            e.resolve(v),
            RowPosition::Child {
                base: 999_999,
                offset: 1
            }
        );
        assert_eq!(e.resolve_slot(e.slot_of(v, 2), 2), (v, 0));
    }

    fn node(id: i64, depth: u16) -> TreeNode {
        TreeNode {
            message: MessageId(id),
            parent: None,
            depth,
        }
    }

    #[test]
    fn tree_rows_draw_thunderbird_guides() {
        // 1
        // ├ 2
        // │ └ 3
        // └ 4
        //   └ 5
        // 6
        let rows = tree_rows(&[
            node(1, 0),
            node(2, 1),
            node(3, 2),
            node(4, 1),
            node(5, 2),
            node(6, 0),
        ]);
        let next: Vec<bool> = rows.iter().map(|r| r.has_next_sibling).collect();
        assert_eq!(next, vec![true, true, false, false, false, false]);
        let lines: Vec<u64> = rows.iter().map(|r| r.lines).collect();
        assert_eq!(lines, vec![0, 0b01, 0b11, 0b01, 0b01, 0]);
    }
}
