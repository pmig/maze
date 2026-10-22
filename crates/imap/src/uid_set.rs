//! [`UidSet`]: a normalized IMAP UID set with compact formatting (`1:5,7,9:*`).

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

/// One member of a [`UidSet`]: a closed range, or a range open towards `*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UidRange {
    /// First UID of the range (≥ 1).
    pub start: u32,
    /// Last UID of the range, or `None` for `*` (the largest UID in the mailbox).
    pub end: Option<u32>,
}

impl UidRange {
    fn end_or_max(self) -> u32 {
        self.end.unwrap_or(u32::MAX)
    }
}

/// A set of message UIDs, kept sorted and merged.
///
/// Formats as an IMAP `sequence-set` using ranges wherever possible, e.g. `1:5,7,9:*`.
/// UID 0 is not a valid UID and is ignored on insertion.
///
/// ```
/// use imap::UidSet;
///
/// let set: UidSet = [7, 1, 2, 3, 4, 5, 9].into_iter().collect();
/// assert_eq!(set.to_string(), "1:5,7,9");
/// assert_eq!(UidSet::from_uid_onwards(42).to_string(), "42:*");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct UidSet {
    ranges: Vec<UidRange>,
}

/// A string that is not a valid IMAP UID set.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid UID set: {0}")]
pub struct ParseUidSetError(String);

impl UidSet {
    /// The empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// `1:*`: every message in the mailbox.
    pub fn all() -> Self {
        Self::from_uid_onwards(1)
    }

    /// `uid:*`: every message with a UID of at least `uid`.
    ///
    /// Note that per RFC 3501 `n:*` always matches the message with the highest UID,
    /// even if that UID is smaller than `n`; callers that fetch "new" mail must filter.
    pub fn from_uid_onwards(uid: u32) -> Self {
        Self {
            ranges: vec![UidRange {
                start: uid.max(1),
                end: None,
            }],
        }
    }

    /// A single UID.
    pub fn single(uid: u32) -> Self {
        let mut set = Self::new();
        set.insert(uid);
        set
    }

    /// The closed range `start:end` (order-insensitive).
    pub fn range(start: u32, end: u32) -> Self {
        let mut set = Self::new();
        set.insert_range(start, end);
        set
    }

    /// Whether the set has no members.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Whether the set contains an open range (`n:*`).
    pub fn is_open(&self) -> bool {
        self.ranges.last().is_some_and(|r| r.end.is_none())
    }

    /// The number of UIDs in the set, or `None` if it contains an open range.
    pub fn len(&self) -> Option<usize> {
        self.ranges.iter().try_fold(0usize, |acc, r| {
            let end = r.end?;
            Some(acc + (end - r.start) as usize + 1)
        })
    }

    /// The normalized ranges of this set, sorted ascending.
    pub fn ranges(&self) -> &[UidRange] {
        &self.ranges
    }

    /// Add one UID. UID 0 is ignored.
    pub fn insert(&mut self, uid: u32) {
        self.insert_range(uid, uid);
    }

    /// Add the closed range `start:end` (order-insensitive). UID 0 is ignored.
    pub fn insert_range(&mut self, start: u32, end: u32) {
        let (lo, hi) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let lo = lo.max(1);
        if hi == 0 {
            return;
        }
        self.push(UidRange {
            start: lo,
            end: Some(hi),
        });
    }

    fn push(&mut self, range: UidRange) {
        self.ranges.push(range);
        self.normalize();
    }

    fn normalize(&mut self) {
        self.ranges.sort_unstable();
        let mut merged: Vec<UidRange> = Vec::with_capacity(self.ranges.len());
        for r in self.ranges.drain(..) {
            match merged.last_mut() {
                Some(last) if r.start <= last.end_or_max().saturating_add(1) => {
                    last.end = match (last.end, r.end) {
                        (None, _) | (_, None) => None,
                        (Some(a), Some(b)) => Some(a.max(b)),
                    };
                }
                _ => merged.push(r),
            }
        }
        self.ranges = merged;
    }

    /// Whether `uid` is a member. An open range contains every UID from its start.
    pub fn contains(&self, uid: u32) -> bool {
        self.ranges
            .iter()
            .any(|r| uid >= r.start && uid <= r.end_or_max())
    }

    /// Iterate over the UIDs of the closed ranges. Open ranges are cut at `max_uid`.
    pub fn iter_up_to(&self, max_uid: u32) -> impl Iterator<Item = u32> + '_ {
        self.ranges.iter().flat_map(move |r| {
            let end = r.end.unwrap_or(max_uid).min(max_uid);
            r.start..=end
        })
    }

    /// Split into sets of at most `max` UIDs each, ascending. Useful for batching
    /// `UID FETCH` so that one command never covers too many messages.
    ///
    /// Open ranges are cut at `max_uid`.
    pub fn chunks(&self, max: usize, max_uid: u32) -> Vec<UidSet> {
        let max = max.max(1);
        let mut out = Vec::new();
        let mut current = UidSet::new();
        let mut count = 0usize;
        for r in &self.ranges {
            let end = r.end.unwrap_or(max_uid).min(max_uid);
            let mut start = r.start;
            while start <= end {
                let room = max - count;
                let span_end = if (end - start) as usize >= room {
                    // `room` ≥ 1 and fits in the range, so this stays ≤ `end`.
                    start + u32::try_from(room - 1).unwrap_or(u32::MAX)
                } else {
                    end
                };
                current.ranges.push(UidRange {
                    start,
                    end: Some(span_end),
                });
                count += (span_end - start) as usize + 1;
                if count == max {
                    out.push(std::mem::take(&mut current));
                    count = 0;
                }
                if span_end == u32::MAX {
                    break;
                }
                start = span_end + 1;
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
        out
    }
}

impl FromIterator<u32> for UidSet {
    fn from_iter<I: IntoIterator<Item = u32>>(iter: I) -> Self {
        let mut uids: Vec<u32> = iter.into_iter().filter(|&u| u != 0).collect();
        uids.sort_unstable();
        uids.dedup();
        let mut ranges: Vec<UidRange> = Vec::new();
        for uid in uids {
            match ranges.last_mut() {
                Some(UidRange { end: Some(end), .. }) if *end + 1 == uid => *end = uid,
                _ => ranges.push(UidRange {
                    start: uid,
                    end: Some(uid),
                }),
            }
        }
        Self { ranges }
    }
}

impl Extend<u32> for UidSet {
    fn extend<I: IntoIterator<Item = u32>>(&mut self, iter: I) {
        for uid in iter {
            if uid != 0 {
                self.ranges.push(UidRange {
                    start: uid,
                    end: Some(uid),
                });
            }
        }
        self.normalize();
    }
}

impl From<u32> for UidSet {
    fn from(uid: u32) -> Self {
        Self::single(uid)
    }
}

impl From<&[u32]> for UidSet {
    fn from(uids: &[u32]) -> Self {
        uids.iter().copied().collect()
    }
}

impl fmt::Display for UidSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, r) in self.ranges.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            match r.end {
                Some(end) if end == r.start => write!(f, "{}", r.start)?,
                Some(end) => write!(f, "{}:{}", r.start, end)?,
                None => write!(f, "{}:*", r.start)?,
            }
        }
        Ok(())
    }
}

impl FromStr for UidSet {
    type Err = ParseUidSetError;

    /// Parse an IMAP sequence set such as `1:5,7,9:*`. A bare `*` is rejected because
    /// its meaning depends on the mailbox.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseUidSetError(s.to_owned());
        let mut set = UidSet::new();
        for part in s.split(',') {
            let parse = |n: &str| n.parse::<u32>().ok().filter(|&n| n > 0);
            match part.split_once(':') {
                Some((a, "*")) | Some(("*", a)) => {
                    let start = parse(a).ok_or_else(err)?;
                    set.ranges.push(UidRange { start, end: None });
                }
                Some((a, b)) => {
                    let (a, b) = (parse(a).ok_or_else(err)?, parse(b).ok_or_else(err)?);
                    set.ranges.push(UidRange {
                        start: a.min(b),
                        end: Some(a.max(b)),
                    });
                }
                None => {
                    let uid = parse(part).ok_or_else(err)?;
                    set.ranges.push(UidRange {
                        start: uid,
                        end: Some(uid),
                    });
                }
            }
        }
        set.normalize();
        Ok(set)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn compact_formatting() {
        let set: UidSet = [9, 1, 2, 3, 4, 5, 7, 3].into_iter().collect();
        assert_eq!(set.to_string(), "1:5,7,9");
        assert_eq!(set.len(), Some(7));
        let mut open = set;
        open.push(UidRange {
            start: 9,
            end: None,
        });
        assert_eq!(open.to_string(), "1:5,7,9:*");
        assert!(open.is_open());
        assert_eq!(open.len(), None);
        assert_eq!(UidSet::all().to_string(), "1:*");
        assert_eq!(UidSet::new().to_string(), "");
    }

    #[test]
    fn insert_merges_adjacent_ranges() {
        let mut set = UidSet::range(10, 5);
        set.insert(11);
        set.insert(4);
        set.insert(20);
        set.insert(0);
        assert_eq!(set.to_string(), "4:11,20");
        set.insert_range(12, 19);
        assert_eq!(set.to_string(), "4:20");
        assert!(set.contains(4) && set.contains(20) && !set.contains(21));
    }

    #[test]
    fn parse_accepts_imap_syntax() {
        let set: UidSet = "7,1:5,9:*".parse().unwrap();
        assert_eq!(set.to_string(), "1:5,7,9:*");
        assert_eq!("5:2".parse::<UidSet>().unwrap().to_string(), "2:5");
        assert!("".parse::<UidSet>().is_err());
        assert!("0".parse::<UidSet>().is_err());
        assert!("1,,2".parse::<UidSet>().is_err());
        assert!("a:b".parse::<UidSet>().is_err());
    }

    #[test]
    fn chunks_split_by_count() {
        let set: UidSet = "1:5,7,9:12".parse().unwrap();
        let chunks: Vec<String> = set.chunks(4, 100).iter().map(ToString::to_string).collect();
        assert_eq!(chunks, ["1:4", "5,7,9:10", "11:12"]);
        let open = UidSet::from_uid_onwards(98);
        let chunks: Vec<String> = open
            .chunks(2, 100)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(chunks, ["98:99", "100"]);
        let edge = UidSet::range(u32::MAX - 1, u32::MAX);
        assert_eq!(edge.chunks(10, u32::MAX).len(), 1);
    }

    #[test]
    fn iter_cuts_open_ranges() {
        let set: UidSet = "1:2,5:*".parse().unwrap();
        assert_eq!(set.iter_up_to(7).collect::<Vec<_>>(), [1, 2, 5, 6, 7]);
    }

    proptest! {
        #[test]
        fn display_parse_round_trip(uids in proptest::collection::vec(1u32..500, 1..60)) {
            let set: UidSet = uids.iter().copied().collect();
            let parsed: UidSet = set.to_string().parse().unwrap();
            prop_assert_eq!(&parsed, &set);
            let mut expected = uids;
            expected.sort_unstable();
            expected.dedup();
            prop_assert_eq!(set.iter_up_to(u32::MAX).collect::<Vec<_>>(), expected);
        }

        #[test]
        fn chunks_cover_everything(uids in proptest::collection::vec(1u32..500, 1..60), max in 1usize..20) {
            let set: UidSet = uids.iter().copied().collect();
            let chunks = set.chunks(max, u32::MAX);
            let mut all = Vec::new();
            for c in &chunks {
                prop_assert!(c.len().unwrap() <= max);
                all.extend(c.iter_up_to(u32::MAX));
            }
            prop_assert_eq!(all, set.iter_up_to(u32::MAX).collect::<Vec<_>>());
        }
    }
}
