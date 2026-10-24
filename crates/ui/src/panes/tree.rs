//! The split layout of the center as plain data: panes in horizontal and vertical
//! splits, each split with the relative sizes of its members. Modelled on Zed's
//! `PaneGroup` (`crates/workspace/src/pane_group.rs`, GPL-3.0): a split in the
//! direction of the parent split joins it; otherwise the pane becomes a new split.
//!
//! The tree is generic over the pane type, so its logic is tested without GPUI
//! entities; [`PaneGroup`](super::PaneGroup) stores `Entity<Pane>`s in it.

use gpui::{Axis, Bounds, Pixels, Point, point, px};
use serde::{Deserialize, Serialize};

/// Where a new pane goes relative to an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    /// Above.
    Up,
    /// Below.
    Down,
    /// To the left.
    Left,
    /// To the right.
    Right,
}

impl SplitDirection {
    /// Every direction.
    pub const ALL: [Self; 4] = [Self::Up, Self::Down, Self::Left, Self::Right];

    /// The axis a split in this direction lays panes out on.
    pub fn axis(self) -> Axis {
        match self {
            Self::Up | Self::Down => Axis::Vertical,
            Self::Left | Self::Right => Axis::Horizontal,
        }
    }

    /// Whether the new pane comes after the existing one (right of it, or below).
    pub fn increasing(self) -> bool {
        matches!(self, Self::Down | Self::Right)
    }
}

/// Identifies a split across edits (resize handles refer to it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SplitId(pub u64);

/// A pane or a split.
#[derive(Debug, Clone, PartialEq)]
pub enum Member<P> {
    /// One pane.
    Pane(P),
    /// Panes (or further splits) side by side or stacked.
    Split(Split<P>),
}

/// Members laid out along an axis.
#[derive(Debug, Clone, PartialEq)]
pub struct Split<P> {
    /// Stable id.
    pub id: SplitId,
    /// Horizontal: members side by side; vertical: stacked.
    pub axis: Axis,
    /// At least two members.
    pub members: Vec<Member<P>>,
    /// Relative sizes, one per member (any positive scale).
    pub flexes: Vec<f32>,
}

impl<P> Split<P> {
    /// The share of the split member `ix` takes, `0.0..=1.0`.
    pub fn fraction(&self, ix: usize) -> f32 {
        let total: f32 = self.flexes.iter().sum();
        if total <= 0. {
            return 1. / self.members.len().max(1) as f32;
        }
        self.flexes.get(ix).copied().unwrap_or(0.) / total
    }
}

/// The smallest share a resize leaves a split member.
pub const MIN_FRACTION: f32 = 0.1;

/// The center's layout: a tree of splits with panes as leaves. Never empty.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneTree<P> {
    root: Member<P>,
    next_split: u64,
}

impl<P: Clone + PartialEq> PaneTree<P> {
    /// A layout with one pane.
    pub fn new(pane: P) -> Self {
        Self {
            root: Member::Pane(pane),
            next_split: 1,
        }
    }

    /// Builds a tree from a member (deserialized layouts). Splits get fresh ids; splits
    /// with fewer than two members collapse, and broken sizes are reset. `None` when
    /// there is no pane at all.
    pub fn from_member(root: Member<P>) -> Option<Self> {
        let mut next_split = 1;
        let root = renumber(root, &mut next_split)?;
        Some(Self { root, next_split })
    }

    /// The root member.
    pub fn root(&self) -> &Member<P> {
        &self.root
    }

    /// Every pane, depth first (left to right, top to bottom).
    pub fn panes(&self) -> Vec<&P> {
        fn walk<'a, P>(member: &'a Member<P>, out: &mut Vec<&'a P>) {
            match member {
                Member::Pane(pane) => out.push(pane),
                Member::Split(split) => split.members.iter().for_each(|m| walk(m, out)),
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &mut out);
        out
    }

    /// Whether `pane` is in the tree.
    pub fn contains(&self, pane: &P) -> bool {
        self.panes().into_iter().any(|p| p == pane)
    }

    /// Puts `new` next to `old` in `direction`. Joins `old`'s split when it runs along
    /// the same axis (`new` takes half of `old`'s share); otherwise `old` becomes a
    /// split of the two. Returns `false` when `old` isn't in the tree.
    pub fn split(&mut self, old: &P, new: P, direction: SplitDirection) -> bool {
        let id = SplitId(self.next_split);
        if split_member(&mut self.root, old, new, direction, id) {
            self.next_split += 1;
            true
        } else {
            false
        }
    }

    /// Removes `pane`; a split left with one member is replaced by it. The last pane
    /// can't be removed (returns `false`, as for a pane that isn't in the tree).
    pub fn remove(&mut self, pane: &P) -> bool {
        if matches!(&self.root, Member::Pane(_)) {
            return false;
        }
        remove_member(&mut self.root, pane)
    }

    /// The split with this id.
    pub fn split_mut(&mut self, id: SplitId) -> Option<&mut Split<P>> {
        fn find<P>(member: &mut Member<P>, id: SplitId) -> Option<&mut Split<P>> {
            let Member::Split(split) = member else {
                return None;
            };
            if split.id == id {
                return Some(split);
            }
            split.members.iter_mut().find_map(|m| find(m, id))
        }
        find(&mut self.root, id)
    }

    /// Moves the boundary after member `ix` of split `id` to `position` (`0.0..=1.0`
    /// along the split). Both neighbors keep at least [`MIN_FRACTION`] of the split;
    /// the other members keep their size.
    pub fn resize(&mut self, id: SplitId, ix: usize, position: f32) -> bool {
        let Some(split) = self.split_mut(id) else {
            return false;
        };
        if ix + 1 >= split.members.len() || !position.is_finite() {
            return false;
        }
        let total: f32 = split.flexes.iter().sum();
        if total <= 0. {
            return false;
        }
        let start: f32 = split.flexes[..ix].iter().sum::<f32>() / total;
        let pair = (split.flexes[ix] + split.flexes[ix + 1]) / total;
        let left = (position - start).clamp(
            MIN_FRACTION.min(pair / 2.),
            pair - MIN_FRACTION.min(pair / 2.),
        );
        split.flexes[ix] = left * total;
        split.flexes[ix + 1] = (pair - left) * total;
        true
    }

    /// The pane in `direction` of `from`: the one containing a point just past the
    /// edge of `from`, level with its center (Zed's approach). `bounds` gives each
    /// pane's last drawn bounds.
    pub fn neighbor(
        &self,
        from: &P,
        direction: SplitDirection,
        bounds: impl Fn(&P) -> Option<Bounds<Pixels>>,
    ) -> Option<&P> {
        let edge = bounds(from)?;
        let center = edge.center();
        let gap = px(8.);
        let target: Point<Pixels> = match direction {
            SplitDirection::Left => point(edge.left() - gap, center.y),
            SplitDirection::Right => point(edge.right() + gap, center.y),
            SplitDirection::Up => point(center.x, edge.top() - gap),
            SplitDirection::Down => point(center.x, edge.bottom() + gap),
        };
        self.panes()
            .into_iter()
            .filter(|pane| *pane != from)
            .find(|pane| bounds(pane).is_some_and(|b| b.contains(&target)))
    }

    /// The same layout over other pane values.
    pub fn map<Q>(&self, mut f: impl FnMut(&P) -> Q) -> Member<Q> {
        fn walk<P, Q>(member: &Member<P>, f: &mut impl FnMut(&P) -> Q) -> Member<Q> {
            match member {
                Member::Pane(pane) => Member::Pane(f(pane)),
                Member::Split(split) => Member::Split(Split {
                    id: split.id,
                    axis: split.axis,
                    members: split.members.iter().map(|m| walk(m, f)).collect(),
                    flexes: split.flexes.clone(),
                }),
            }
        }
        walk(&self.root, &mut f)
    }
}

fn renumber<P>(member: Member<P>, next: &mut u64) -> Option<Member<P>> {
    let Member::Split(split) = member else {
        return Some(member);
    };
    let broken = split.flexes.len() != split.members.len()
        || split.flexes.iter().any(|f| !f.is_finite() || *f <= 0.);
    let (mut members, mut flexes): (Vec<_>, Vec<_>) = split
        .members
        .into_iter()
        .zip(split.flexes.into_iter().chain(std::iter::repeat(1.)))
        .filter_map(|(m, f)| Some((renumber(m, next)?, f)))
        .unzip();
    if broken {
        flexes = vec![1.; members.len()];
    }
    match members.len() {
        0 => None,
        1 => members.pop(),
        _ => {
            let id = SplitId(*next);
            *next += 1;
            Some(Member::Split(Split {
                id,
                axis: split.axis,
                members,
                flexes,
            }))
        }
    }
}

fn split_member<P: Clone + PartialEq>(
    member: &mut Member<P>,
    old: &P,
    new: P,
    direction: SplitDirection,
    id: SplitId,
) -> bool {
    match member {
        Member::Pane(pane) if pane == old => {
            let old = pane.clone();
            let members = if direction.increasing() {
                vec![Member::Pane(old), Member::Pane(new)]
            } else {
                vec![Member::Pane(new), Member::Pane(old)]
            };
            *member = Member::Split(Split {
                id,
                axis: direction.axis(),
                members,
                flexes: vec![1., 1.],
            });
            true
        }
        Member::Pane(_) => false,
        Member::Split(split) => {
            let Some(ix) = split
                .members
                .iter()
                .position(|m| matches!(m, Member::Pane(p) if p == old))
            else {
                return split
                    .members
                    .iter_mut()
                    .any(|m| contains(m, old) && split_member(m, old, new.clone(), direction, id));
            };
            if split.axis != direction.axis() {
                return split_member(&mut split.members[ix], old, new, direction, id);
            }
            let half = split.flexes[ix] / 2.;
            split.flexes[ix] = half;
            let at = if direction.increasing() { ix + 1 } else { ix };
            split.members.insert(at, Member::Pane(new));
            split.flexes.insert(at, half);
            // The id wasn't used: the caller still counts it, which only skips one.
            true
        }
    }
}

fn contains<P: PartialEq>(member: &Member<P>, pane: &P) -> bool {
    match member {
        Member::Pane(p) => p == pane,
        Member::Split(split) => split.members.iter().any(|m| contains(m, pane)),
    }
}

fn remove_member<P: PartialEq>(member: &mut Member<P>, pane: &P) -> bool {
    let Member::Split(split) = member else {
        return false;
    };
    if let Some(ix) = split
        .members
        .iter()
        .position(|m| matches!(m, Member::Pane(p) if p == pane))
    {
        split.members.remove(ix);
        split.flexes.remove(ix);
        if split.members.len() == 1 {
            let last = split.members.remove(0);
            *member = last;
        }
        return true;
    }
    split
        .members
        .iter_mut()
        .any(|m| contains(m, pane) && remove_member(m, pane))
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, point, px, size};

    use super::*;

    fn shape(tree: &PaneTree<u32>) -> String {
        fn walk(member: &Member<u32>) -> String {
            match member {
                Member::Pane(p) => p.to_string(),
                Member::Split(split) => {
                    let sep = match split.axis {
                        Axis::Horizontal => " | ",
                        Axis::Vertical => " / ",
                    };
                    let inner: Vec<_> = split.members.iter().map(walk).collect();
                    format!("({})", inner.join(sep))
                }
            }
        }
        walk(tree.root())
    }

    fn flexes(tree: &PaneTree<u32>) -> Vec<f32> {
        match tree.root() {
            Member::Split(split) => split.flexes.clone(),
            Member::Pane(_) => vec![],
        }
    }

    #[test]
    fn splits_join_a_split_on_the_same_axis() {
        let mut tree = PaneTree::new(1);
        assert!(tree.split(&1, 2, SplitDirection::Right));
        assert_eq!(shape(&tree), "(1 | 2)");
        assert!(tree.split(&1, 3, SplitDirection::Right));
        assert_eq!(shape(&tree), "(1 | 3 | 2)");
        assert_eq!(flexes(&tree), [0.5, 0.5, 1.], "3 takes half of 1's share");
        assert!(tree.split(&2, 4, SplitDirection::Left));
        assert_eq!(shape(&tree), "(1 | 3 | 4 | 2)");
        assert_eq!(tree.panes(), [&1, &3, &4, &2]);
    }

    #[test]
    fn a_split_across_the_axis_nests() {
        let mut tree = PaneTree::new(1);
        tree.split(&1, 2, SplitDirection::Right);
        tree.split(&2, 3, SplitDirection::Down);
        assert_eq!(shape(&tree), "(1 | (2 / 3))");
        tree.split(&3, 4, SplitDirection::Up);
        assert_eq!(shape(&tree), "(1 | (2 / 4 / 3))");
        assert!(!tree.split(&9, 5, SplitDirection::Up), "unknown pane");
    }

    #[test]
    fn removing_collapses_single_member_splits() {
        let mut tree = PaneTree::new(1);
        assert!(!tree.remove(&1), "the last pane stays");
        tree.split(&1, 2, SplitDirection::Right);
        tree.split(&2, 3, SplitDirection::Down);
        assert!(tree.remove(&2));
        assert_eq!(shape(&tree), "(1 | 3)");
        assert!(tree.remove(&1));
        assert_eq!(shape(&tree), "3");
        assert!(!tree.remove(&3));
        assert!(!tree.contains(&1) && tree.contains(&3));
    }

    #[test]
    fn resizing_moves_one_boundary_within_limits() {
        let mut tree = PaneTree::new(1);
        tree.split(&1, 2, SplitDirection::Right);
        tree.split(&2, 3, SplitDirection::Right);
        let Member::Split(split) = tree.root().clone() else {
            panic!("split");
        };
        // 1: 50 %, 2: 25 %, 3: 25 %. Move the 2|3 boundary to 90 %.
        assert!(tree.resize(split.id, 1, 0.9));
        let Member::Split(split) = tree.root().clone() else {
            panic!("split");
        };
        assert!((split.fraction(0) - 0.5).abs() < 1e-4);
        assert!((split.fraction(1) - 0.4).abs() < 1e-4);
        assert!((split.fraction(2) - 0.1).abs() < 1e-4);
        // Past the edge: clamped to the minimum.
        assert!(tree.resize(split.id, 0, -1.));
        let Member::Split(split) = tree.root().clone() else {
            panic!("split");
        };
        assert!((split.fraction(0) - MIN_FRACTION).abs() < 1e-4);
        assert!(!tree.resize(split.id, 2, 0.5), "no boundary after the last");
        assert!(!tree.resize(SplitId(99), 0, 0.5));
    }

    #[test]
    fn neighbors_follow_the_drawn_bounds() {
        // 1 on the left half; 2 above 3 on the right.
        let mut tree = PaneTree::new(1);
        tree.split(&1, 2, SplitDirection::Right);
        tree.split(&2, 3, SplitDirection::Down);
        let bounds = |pane: &u32| {
            let (x, y, w, h) = match pane {
                1 => (0., 0., 500., 800.),
                2 => (500., 0., 500., 450.),
                3 => (500., 450., 500., 350.),
                _ => return None,
            };
            Some(Bounds::new(point(px(x), px(y)), size(px(w), px(h))))
        };
        assert_eq!(tree.neighbor(&1, SplitDirection::Right, bounds), Some(&2));
        assert_eq!(tree.neighbor(&3, SplitDirection::Left, bounds), Some(&1));
        assert_eq!(tree.neighbor(&2, SplitDirection::Down, bounds), Some(&3));
        assert_eq!(tree.neighbor(&3, SplitDirection::Up, bounds), Some(&2));
        assert_eq!(tree.neighbor(&1, SplitDirection::Left, bounds), None);
    }

    #[test]
    fn deserialized_members_are_cleaned_up() {
        let member = Member::Split(Split {
            id: SplitId(0),
            axis: Axis::Horizontal,
            members: vec![
                Member::Pane(1),
                Member::Split(Split {
                    id: SplitId(0),
                    axis: Axis::Vertical,
                    members: vec![Member::Pane(2)],
                    flexes: vec![1.],
                }),
            ],
            flexes: vec![1., f32::NAN],
        });
        let tree = PaneTree::from_member(member).unwrap();
        assert_eq!(shape(&tree), "(1 | 2)");
        assert_eq!(flexes(&tree), [1., 1.], "broken flexes are reset");
    }
}
