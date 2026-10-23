//! The message list's selection as a pure model.
//!
//! Indexes refer to the current key list. Thunderbird semantics: `f`/`b` (and the
//! arrow keys) move by one, `n`/`p` jump to the next/previous unread thread and stay
//! put when there is none. Each of those collapses the selection to one row.
//!
//! Several rows can be selected, as in Thunderbird and the Finder: `shift-up`/`down`
//! and shift-click extend a range from the anchor (the row selected last without
//! shift) to the cursor, cmd-click adds or removes one row, `cmd-a` selects every row.
//! The cursor is the row the message view follows; it is always selected. Selected
//! rows are kept as sorted ranges, so selecting all of a million rows costs one range.

use std::ops::Range;

use crate::source::Direction;

/// The selected rows of a list with `len` rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    len: usize,
    /// The row the keyboard moves from and the message view shows.
    cursor: Option<usize>,
    /// Where a shift-extended range starts.
    anchor: Option<usize>,
    /// Rows selected besides the anchor-to-cursor range (cmd-click, select all, rows
    /// kept across a reload): sorted, disjoint and not adjacent.
    rows: Vec<Range<usize>>,
}

/// Adds `add` to sorted, disjoint, non-adjacent `ranges`, merging what it touches.
fn insert(ranges: &mut Vec<Range<usize>>, add: Range<usize>) {
    if add.is_empty() {
        return;
    }
    // The ranges that touch or overlap `add`.
    let start = ranges.partition_point(|r| r.end < add.start);
    let end = ranges.partition_point(|r| r.start <= add.end);
    let merged = match ranges.get(start..end) {
        Some(touched) if !touched.is_empty() => {
            touched[0].start.min(add.start)..touched[touched.len() - 1].end.max(add.end)
        }
        _ => add,
    };
    ranges.splice(start..end, [merged]);
}

/// Takes `ix` out of sorted `ranges`.
fn remove(ranges: &mut Vec<Range<usize>>, ix: usize) {
    let at = ranges.partition_point(|r| r.end <= ix);
    let Some(range) = ranges.get(at).cloned().filter(|r| r.contains(&ix)) else {
        return;
    };
    let pieces = [range.start..ix, ix + 1..range.end];
    ranges.splice(at..=at, pieces.into_iter().filter(|r| !r.is_empty()));
}

fn contains(ranges: &[Range<usize>], ix: usize) -> bool {
    let at = ranges.partition_point(|r| r.end <= ix);
    ranges.get(at).is_some_and(|r| r.contains(&ix))
}

impl Selection {
    /// An empty selection over `len` rows.
    pub fn new(len: usize) -> Self {
        Self {
            len,
            ..Self::default()
        }
    }

    /// The cursor: the selected row the message view shows.
    pub fn selected(&self) -> Option<usize> {
        self.cursor
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the list has no rows.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The anchor-to-cursor range.
    fn range(&self) -> Option<Range<usize>> {
        let cursor = self.cursor?;
        let anchor = self.anchor.unwrap_or(cursor);
        Some(anchor.min(cursor)..anchor.max(cursor) + 1)
    }

    /// Every selected row as sorted, disjoint ranges.
    pub fn ranges(&self) -> Vec<Range<usize>> {
        let mut ranges = self.rows.clone();
        if let Some(range) = self.range() {
            insert(&mut ranges, range);
        }
        ranges
    }

    /// The selected rows in order.
    pub fn indices(&self) -> impl Iterator<Item = usize> + use<> {
        self.ranges().into_iter().flatten()
    }

    /// Number of selected rows.
    pub fn count(&self) -> usize {
        self.ranges().iter().map(ExactSizeIterator::len).sum()
    }

    /// Whether more than one row is selected.
    pub fn is_multiple(&self) -> bool {
        self.count() > 1
    }

    /// Whether every row is selected.
    pub fn is_all(&self) -> bool {
        matches!(self.ranges().as_slice(), [all] if self.len > 0 && *all == (0..self.len))
    }

    /// Whether row `ix` is selected.
    pub fn is_selected(&self, ix: usize) -> bool {
        self.range().is_some_and(|r| r.contains(&ix)) || contains(&self.rows, ix)
    }

    /// Selects only `ix` (when it exists) and makes it the anchor.
    fn collapse(&mut self, ix: Option<usize>) {
        self.cursor = ix;
        self.anchor = ix;
        self.rows.clear();
    }

    /// Replaces the list, keeping one row selected if `keep` still points into it.
    pub fn reset(&mut self, len: usize, keep: Option<usize>) {
        self.len = len;
        self.collapse(keep.filter(|ix| *ix < len));
    }

    /// Replaces the list, selecting `rows` (those still in it) around `cursor`.
    pub fn reset_rows(
        &mut self,
        len: usize,
        cursor: Option<usize>,
        rows: impl IntoIterator<Item = usize>,
    ) {
        self.reset(len, cursor);
        let mut rows: Vec<usize> = rows.into_iter().filter(|ix| *ix < len).collect();
        rows.sort_unstable();
        rows.dedup();
        for ix in rows {
            insert(&mut self.rows, ix..ix + 1);
        }
        if self.cursor.is_none() {
            let first = self.rows.first().map(|r| r.start);
            self.cursor = first;
            self.anchor = first;
        }
    }

    /// Selects only `ix` if it exists. Returns the new cursor.
    pub fn select(&mut self, ix: usize) -> Option<usize> {
        if ix < self.len {
            self.collapse(Some(ix));
        }
        self.cursor
    }

    /// Clears the selection.
    pub fn clear(&mut self) {
        self.collapse(None);
    }

    /// The row `delta` rows from the cursor, clamped to the list. With nothing
    /// selected, forward movement starts at the first row and backward at the last.
    fn target(&self, delta: isize) -> Option<usize> {
        let last = self.len.checked_sub(1)?;
        Some(match self.cursor {
            None if delta >= 0 => 0,
            None => last,
            Some(ix) => ix.saturating_add_signed(delta).min(last),
        })
    }

    /// Moves by `delta` rows and selects only that row (see [`Self::target`]).
    pub fn step(&mut self, delta: isize) -> Option<usize> {
        let target = self.target(delta)?;
        self.collapse(Some(target));
        self.cursor
    }

    /// `shift-down`/`shift-up`: moves the cursor by `delta` rows, selecting the range
    /// from the anchor to it.
    pub fn extend_by(&mut self, delta: isize) -> Option<usize> {
        let target = self.target(delta)?;
        self.extend_to(target)
    }

    /// Shift-click: moves the cursor to `ix`, selecting the range from the anchor.
    pub fn extend_to(&mut self, ix: usize) -> Option<usize> {
        if ix < self.len {
            self.anchor = self.anchor.or(self.cursor).or(Some(ix));
            self.cursor = Some(ix);
        }
        self.cursor
    }

    /// Cmd-click: adds `ix` to the selection, or takes it out. The cursor and anchor
    /// go to `ix`, or to the nearest selected row when `ix` was taken out; the last
    /// selected row can't be taken out.
    pub fn toggle(&mut self, ix: usize) -> Option<usize> {
        if ix >= self.len {
            return self.cursor;
        }
        let mut rows = self.ranges();
        if contains(&rows, ix) {
            remove(&mut rows, ix);
            let below = rows.iter().find(|r| r.start > ix).map(|r| r.start);
            let above = rows.iter().rev().find(|r| r.end <= ix).map(|r| r.end - 1);
            let nearest = match (above, below) {
                (Some(above), Some(below)) if ix - above <= below - ix => Some(above),
                (_, Some(below)) => Some(below),
                (above, None) => above,
            };
            let Some(next) = nearest else {
                return self.cursor;
            };
            self.rows = rows;
            self.cursor = Some(next);
            self.anchor = Some(next);
        } else {
            self.rows = rows;
            insert(&mut self.rows, ix..ix + 1);
            self.cursor = Some(ix);
            self.anchor = Some(ix);
        }
        self.cursor
    }

    /// `cmd-a`: selects every row, keeping the cursor.
    pub fn select_all(&mut self) {
        if self.len == 0 {
            return;
        }
        self.rows.clear();
        self.rows.push(0..self.len);
        self.cursor = self.cursor.or(Some(0));
        self.anchor = self.cursor;
    }

    /// Selects only the first row.
    pub fn first(&mut self) -> Option<usize> {
        if self.len > 0 {
            self.collapse(Some(0));
        }
        self.cursor
    }

    /// Selects only the last row.
    pub fn last(&mut self) -> Option<usize> {
        if self.len > 0 {
            self.collapse(Some(self.len - 1));
        }
        self.cursor
    }

    /// Jumps to the next row in `direction` that `find` reports (for example the
    /// next unread thread) and selects only it. `find` gets the cursor and the
    /// direction and returns a match or `None`; with no match the selection stays
    /// where it is. Returns whether the selection moved.
    pub fn jump(
        &mut self,
        direction: Direction,
        find: impl FnOnce(Option<usize>, Direction) -> Option<usize>,
    ) -> bool {
        match find(self.cursor, direction).filter(|ix| *ix < self.len) {
            Some(ix) if Some(ix) != self.cursor || self.is_multiple() => {
                self.collapse(Some(ix));
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init, reason = "rows as ranges")]
mod tests {
    use super::*;

    #[test]
    fn steps_and_clamps() {
        let mut s = Selection::new(3);
        assert_eq!(s.step(1), Some(0), "forward from nothing starts at the top");
        assert_eq!(s.step(1), Some(1));
        assert_eq!(s.step(10), Some(2));
        assert_eq!(s.step(-1), Some(1));
        assert_eq!(s.step(-10), Some(0));
        let mut s = Selection::new(3);
        assert_eq!(
            s.step(-1),
            Some(2),
            "backward from nothing starts at the bottom"
        );
        assert_eq!(Selection::new(0).step(1), None);
    }

    #[test]
    fn first_last_and_select() {
        let mut s = Selection::new(5);
        assert_eq!(s.last(), Some(4));
        assert_eq!(s.first(), Some(0));
        assert_eq!(s.select(3), Some(3));
        assert_eq!(s.select(9), Some(3), "out of range keeps the selection");
        s.clear();
        assert_eq!(s.selected(), None);
        assert_eq!(s.count(), 0);
    }

    #[test]
    fn reset_keeps_valid_selection_only() {
        let mut s = Selection::new(10);
        s.select(8);
        s.reset(20, Some(8));
        assert_eq!(s.selected(), Some(8));
        s.reset(5, Some(8));
        assert_eq!(s.selected(), None);
        assert_eq!(s.len(), 5);
    }

    #[test]
    fn shift_extends_from_the_anchor_and_a_plain_move_collapses() {
        let mut s = Selection::new(10);
        s.select(4);
        assert_eq!(s.extend_by(1), Some(5));
        assert_eq!(s.extend_by(1), Some(6));
        assert_eq!(s.ranges(), [4..7]);
        assert!(s.is_multiple());
        // Back over the anchor: the range flips to the other side.
        s.extend_by(-4);
        assert_eq!(s.selected(), Some(2));
        assert_eq!(s.ranges(), [2..5]);
        assert!(s.is_selected(3) && !s.is_selected(5));
        // Shift-click from the same anchor.
        s.extend_to(8);
        assert_eq!(s.ranges(), [4..9]);
        assert_eq!(s.count(), 5);
        assert_eq!(s.indices().collect::<Vec<_>>(), [4, 5, 6, 7, 8]);
        // Clamped at the edges.
        s.extend_by(10);
        assert_eq!(s.ranges(), [4..10]);
        // A plain step collapses to one row.
        assert_eq!(s.step(-1), Some(8));
        assert_eq!(s.ranges(), [8..9]);
        assert!(!s.is_multiple());
        // Extending from nothing starts at the top.
        let mut s = Selection::new(3);
        assert_eq!(s.extend_by(1), Some(0));
        assert_eq!(s.ranges(), [0..1]);
    }

    #[test]
    fn cmd_click_adds_and_removes_rows() {
        let mut s = Selection::new(10);
        s.select(1);
        s.extend_to(3);
        assert_eq!(s.toggle(6), Some(6));
        assert_eq!(s.ranges(), [1..4, 6..7]);
        // Shift extends from the toggled row, keeping the earlier rows.
        s.extend_to(8);
        assert_eq!(s.ranges(), [1..4, 6..9]);
        // Taking a row out moves the cursor to the nearest selected row.
        assert_eq!(s.toggle(2), Some(1));
        assert_eq!(s.ranges(), [1..2, 3..4, 6..9]);
        assert_eq!(s.toggle(7), Some(6));
        assert_eq!(s.ranges(), [1..2, 3..4, 6..7, 8..9]);
        // Adjacent rows merge.
        s.toggle(7);
        assert_eq!(s.ranges(), [1..2, 3..4, 6..9]);
        // The last selected row stays.
        let mut s = Selection::new(3);
        s.select(1);
        assert_eq!(s.toggle(1), Some(1));
        assert_eq!(s.ranges(), [1..2]);
    }

    #[test]
    fn select_all_is_one_range() {
        let mut s = Selection::new(1_000_000);
        s.select(42);
        s.select_all();
        assert_eq!(s.ranges(), [0..1_000_000]);
        assert_eq!(s.count(), 1_000_000);
        assert!(s.is_all());
        assert_eq!(s.selected(), Some(42));
        s.step(1);
        assert_eq!(s.count(), 1);
        Selection::new(0).select_all();
    }

    #[test]
    fn reset_rows_keeps_the_rows_that_still_exist() {
        let mut s = Selection::new(10);
        s.reset_rows(6, Some(2), [9, 4, 3, 2, 4]);
        assert_eq!(s.ranges(), [2..5]);
        assert_eq!(s.selected(), Some(2));
        // Extending works from the kept cursor.
        s.extend_by(2);
        assert_eq!(s.ranges(), [2..5]);
        assert_eq!(s.selected(), Some(4));
        // Without the cursor the first kept row takes its place.
        s.reset_rows(6, None, [5, 1]);
        assert_eq!(s.selected(), Some(1));
        assert_eq!(s.ranges(), [1..2, 5..6]);
    }

    #[test]
    fn jumps_to_the_next_match_or_stays() {
        let unread = [false, true, false, false, true, false];
        let find = |from: Option<usize>, direction: Direction| match direction {
            Direction::Forward => {
                (from.map_or(0, |ix| ix + 1)..unread.len()).find(|ix| unread[*ix])
            }
            Direction::Backward => (0..from.unwrap_or(unread.len()))
                .rev()
                .find(|ix| unread[*ix]),
        };
        let mut s = Selection::new(unread.len());
        assert!(s.jump(Direction::Forward, find));
        assert_eq!(s.selected(), Some(1));
        assert!(s.jump(Direction::Forward, find));
        assert_eq!(s.selected(), Some(4));
        assert!(!s.jump(Direction::Forward, find), "no unread below: stay");
        assert_eq!(s.selected(), Some(4));
        assert!(s.jump(Direction::Backward, find));
        assert_eq!(s.selected(), Some(1));
        assert!(!s.jump(Direction::Backward, find));
    }
}
