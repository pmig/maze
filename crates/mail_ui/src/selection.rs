//! The message list's selection as a pure model.
//!
//! Indexes refer to the current key list. Thunderbird semantics: `f`/`b` (and the
//! arrow keys) move by one, `n`/`p` jump to the next/previous unread thread and stay
//! put when there is none.

use crate::source::Direction;

/// The selected row of a list with `len` rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Selection {
    len: usize,
    selected: Option<usize>,
}

impl Selection {
    /// An empty selection over `len` rows.
    pub fn new(len: usize) -> Self {
        Self {
            len,
            selected: None,
        }
    }

    /// The selected index.
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the list has no rows.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Replaces the list, keeping the selection only if `keep` still points into it.
    pub fn reset(&mut self, len: usize, keep: Option<usize>) {
        self.len = len;
        self.selected = keep.filter(|ix| *ix < len);
    }

    /// Selects `ix` if it exists. Returns the new selection.
    pub fn select(&mut self, ix: usize) -> Option<usize> {
        if ix < self.len {
            self.selected = Some(ix);
        }
        self.selected
    }

    /// Clears the selection.
    pub fn clear(&mut self) {
        self.selected = None;
    }

    /// Moves by `delta` rows, clamped to the list. With nothing selected, forward
    /// movement starts at the first row and backward movement at the last.
    pub fn step(&mut self, delta: isize) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let last = self.len - 1;
        let target = match self.selected {
            None if delta >= 0 => 0,
            None => last,
            Some(ix) => ix.saturating_add_signed(delta).min(last),
        };
        self.selected = Some(target);
        self.selected
    }

    /// Selects the first row.
    pub fn first(&mut self) -> Option<usize> {
        if self.len > 0 {
            self.selected = Some(0);
        }
        self.selected
    }

    /// Selects the last row.
    pub fn last(&mut self) -> Option<usize> {
        if self.len > 0 {
            self.selected = Some(self.len - 1);
        }
        self.selected
    }

    /// Jumps to the next row in `direction` that `find` reports (for example the
    /// next unread thread). `find` gets the current index and the direction and
    /// returns a match or `None`; with no match the selection stays where it is.
    /// Returns whether the selection moved.
    pub fn jump(
        &mut self,
        direction: Direction,
        find: impl FnOnce(Option<usize>, Direction) -> Option<usize>,
    ) -> bool {
        match find(self.selected, direction).filter(|ix| *ix < self.len) {
            Some(ix) if Some(ix) != self.selected => {
                self.selected = Some(ix);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
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
