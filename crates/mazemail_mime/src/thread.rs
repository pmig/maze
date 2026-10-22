//! Conversation threading: JWZ over `References`/`In-Reply-To`, with a subject
//! fallback (CONCEPT §7, PLAN §4.3).
//!
//! [`thread_messages`] threads a batch (an import, a folder rebuild). [`join_thread`]
//! answers the incremental question "which existing thread does this new message
//! join?" for the store's writer.
//!
//! # Model
//!
//! - **Membership** is the connected component of the message in the graph of
//!   Message-IDs and the IDs it references. That is independent of input order and
//!   immune to contradictory or cyclic `References`: every message lands in exactly
//!   one thread, and the incremental helper computes the same components.
//! - **Structure** (who replied to whom) follows the JWZ algorithm: `References`
//!   pairs link parent → child unless that would create a cycle, and a message's own
//!   last reference wins over parents guessed from other messages. Messages are
//!   processed in date order, so the result doesn't depend on input order.
//! - **Root**: the earliest message without a parent. Other parentless messages of
//!   the same thread (their parents are missing) hang below the root, so every
//!   thread is one tree.
//! - **Subject fallback**: a message with no `References`/`In-Reply-To` whose subject
//!   carries a reply or forward prefix (`Re:`, `AW:`, `Fwd:`, …) joins the earliest
//!   thread whose root has the same normalized subject. Thunderbird's behavior with
//!   `mail.thread_without_re = false`; messages without a prefix never merge by
//!   subject.
//!
//! # Limits
//!
//! Only the last [`MAX_REFERENCES`] references of a message are used, and cycle
//! checks give up (refusing the link) after [`MAX_ANCESTOR_WALK`] steps. Both bound
//! the work hostile headers can cause; the whole batch is `O(n · MAX_REFERENCES)`
//! in the common case.

use std::cmp::Ordering;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::parse::MessageHeaders;
use crate::subject::parse_subject;

/// References beyond this many (counted from the end) are ignored.
pub const MAX_REFERENCES: usize = 64;

/// Cycle checks walk at most this many ancestors before refusing a link.
pub const MAX_ANCESTOR_WALK: usize = 4096;

/// The headers threading needs, borrowed from wherever they live.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadInput<'a> {
    /// The message's own Message-ID (angle brackets optional).
    pub message_id: Option<&'a str>,
    /// `In-Reply-To` IDs.
    pub in_reply_to: &'a [String],
    /// `References` IDs, oldest first.
    pub references: &'a [String],
    /// The raw subject.
    pub subject: Option<&'a str>,
    /// Date as Unix seconds; orders messages and picks roots.
    pub date: Option<i64>,
}

impl<'a> From<&'a MessageHeaders> for ThreadInput<'a> {
    fn from(headers: &'a MessageHeaders) -> Self {
        Self {
            message_id: headers.message_id.as_deref(),
            in_reply_to: &headers.in_reply_to,
            references: &headers.references,
            subject: headers.subject.as_deref(),
            date: headers.date.map(|d| d.unix_timestamp()),
        }
    }
}

impl<'a> ThreadInput<'a> {
    fn own_id(&self) -> Option<&'a str> {
        self.message_id.and_then(normalize_message_id)
    }

    /// The reference chain, oldest first: `References`, then the first
    /// `In-Reply-To` ID if it isn't already the last reference. Normalized, without
    /// the message's own ID, consecutive duplicates removed, capped.
    fn reference_chain(&self) -> Vec<&'a str> {
        let own = self.own_id();
        let mut chain: Vec<&'a str> = Vec::new();
        let refs = self
            .references
            .iter()
            .filter_map(|r| normalize_message_id(r));
        let irt = self
            .in_reply_to
            .iter()
            .find_map(|r| normalize_message_id(r));
        for id in refs.chain(irt) {
            if Some(id) != own && chain.last() != Some(&id) {
                chain.push(id);
            }
        }
        if chain.len() > MAX_REFERENCES {
            chain.drain(..chain.len() - MAX_REFERENCES);
        }
        chain
    }

    /// Subject key for the subject fallback, if this message may use it.
    fn fallback_subject_key(&self) -> Option<String> {
        if !self.reference_chain().is_empty() {
            return None;
        }
        let info = parse_subject(self.subject?);
        (info.is_response() && !info.base.is_empty()).then(|| info.key())
    }
}

/// Strip whitespace and angle brackets from a Message-ID. `None` if nothing is left.
pub fn normalize_message_id(id: &str) -> Option<&str> {
    let id = id
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    (!id.is_empty()).then_some(id)
}

/// When messages may be grouped by subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectFallback {
    /// Only `References`/`In-Reply-To` link messages.
    Off,
    /// Messages without references whose subject has a reply/forward prefix join a
    /// thread with the same normalized subject.
    #[default]
    RepliesOnly,
}

/// Threading options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ThreadingOptions {
    pub subject_fallback: SubjectFallback,
}

/// One conversation in a [`Threads`] result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    /// Index (into the input slice) of the root message.
    pub root: usize,
    /// Indices of all messages in the thread, by date (input index breaks ties).
    pub messages: Vec<usize>,
}

/// The result of [`thread_messages`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Threads {
    /// Threads ordered by their root's date.
    pub threads: Vec<Thread>,
    /// For each input message, the index of its thread in `threads`.
    pub thread_of: Vec<usize>,
    /// For each input message, its parent message, `None` for thread roots.
    pub parent: Vec<Option<usize>>,
}

/// A container in the JWZ id table: one per distinct Message-ID, plus one per
/// message that has no (or a duplicate) ID.
#[derive(Debug, Clone, Default)]
struct Container {
    message: Option<usize>,
    parent: Option<usize>,
    /// Some container has (or had) this one as parent. Containers that never had
    /// children can't be anyone's ancestor, which skips the cycle walk in the common
    /// case of a new reply.
    has_children: bool,
}

/// Union-find with path halving and union by size.
struct Components {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl Components {
    fn new() -> Self {
        Self {
            parent: Vec::new(),
            size: Vec::new(),
        }
    }

    fn add(&mut self) -> usize {
        self.parent.push(self.parent.len());
        self.size.push(1);
        self.parent.len() - 1
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a == b {
            return;
        }
        let (big, small) = if self.size[a] >= self.size[b] {
            (a, b)
        } else {
            (b, a)
        };
        self.parent[small] = big;
        self.size[big] += self.size[small];
    }
}

struct Table<'a> {
    containers: Vec<Container>,
    ids: HashMap<&'a str, usize>,
    components: Components,
}

impl<'a> Table<'a> {
    fn new(capacity: usize) -> Self {
        Self {
            containers: Vec::with_capacity(capacity),
            ids: HashMap::with_capacity(capacity),
            components: Components::new(),
        }
    }

    fn add(&mut self) -> usize {
        self.containers.push(Container::default());
        self.components.add()
    }

    fn container_for(&mut self, id: &'a str) -> usize {
        if let Some(&c) = self.ids.get(id) {
            return c;
        }
        let c = self.add();
        self.ids.insert(id, c);
        c
    }

    /// Whether making `parent` the parent of `child` would close a cycle (or can't
    /// be ruled out within the walk limit).
    fn would_cycle(&self, parent: usize, child: usize) -> bool {
        if !self.containers[child].has_children {
            return false;
        }
        let mut current = Some(parent);
        for _ in 0..MAX_ANCESTOR_WALK {
            match current {
                None => return false,
                Some(c) if c == child => return true,
                Some(c) => current = self.containers[c].parent,
            }
        }
        true
    }

    fn link(&mut self, parent: usize, child: usize) -> bool {
        if parent == child || self.would_cycle(parent, child) {
            return false;
        }
        self.containers[child].parent = Some(parent);
        self.containers[parent].has_children = true;
        true
    }
}

/// Order in which messages are processed and roots are chosen: by date (undated
/// last), then Message-ID, then input index.
fn canonical_order(messages: &[ThreadInput<'_>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..messages.len()).collect();
    order.sort_by(|&a, &b| compare_messages(messages, a, b));
    order
}

fn compare_messages(messages: &[ThreadInput<'_>], a: usize, b: usize) -> Ordering {
    let key = |i: usize| {
        let m = &messages[i];
        (m.date.is_none(), m.date, m.own_id())
    };
    key(a).cmp(&key(b)).then(a.cmp(&b))
}

/// Thread a batch of messages.
///
/// See the [module documentation](self) for the model. Runs in
/// `O(n log n + n · MAX_REFERENCES)` for well-formed headers.
pub fn thread_messages(messages: &[ThreadInput<'_>], options: &ThreadingOptions) -> Threads {
    let n = messages.len();
    let order = canonical_order(messages);
    let mut rank = vec![0; n];
    for (r, &i) in order.iter().enumerate() {
        rank[i] = r;
    }

    // 1. JWZ id table and parent links, plus connected components.
    let mut table = Table::new(n * 2);
    let mut own_container = vec![0; n];
    for &i in &order {
        let message = &messages[i];
        let (own, duplicate_of) = match message.own_id() {
            Some(id) => {
                let c = table.container_for(id);
                if table.containers[c].message.is_none() {
                    (c, None)
                } else {
                    (table.add(), Some(c))
                }
            }
            None => (table.add(), None),
        };
        table.containers[own].message = Some(i);
        own_container[i] = own;

        let chain: Vec<usize> = message
            .reference_chain()
            .into_iter()
            .map(|id| table.container_for(id))
            .collect();
        for pair in chain.windows(2) {
            let (parent, child) = (pair[0], pair[1]);
            if table.containers[child].parent.is_none() {
                table.link(parent, child);
            }
            table.components.union(parent, child);
        }
        if let Some(original) = duplicate_of {
            // A second copy of the same message sits below the first one.
            table.link(original, own);
            table.components.union(original, own);
        } else if let Some(&last) = chain.last() {
            // The message's own references are authoritative for its parent.
            let previous = table.containers[own].parent.take();
            if !table.link(last, own) {
                table.containers[own].parent = previous;
            }
        }
        if let Some(&first) = chain.first() {
            table.components.union(first, own);
        }
    }

    // 2. Message-level parents: the nearest ancestor container holding a message.
    let mut parent = nearest_message_parents(&table, &own_container, n);

    // 3. Subject fallback merges whole components into an earlier one.
    if options.subject_fallback == SubjectFallback::RepliesOnly {
        merge_by_subject(messages, &order, &own_container, &mut table, &mut parent);
    }

    // 4. Group by component, pick roots, make every thread a single tree.
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for &i in &order {
        let component = table.components.find(own_container[i]);
        groups.entry(component).or_default().push(i);
    }
    let mut threads: Vec<Thread> = groups
        .into_values()
        .map(|members| {
            // `members` is in canonical order, so the first parentless one is the root.
            let root = members
                .iter()
                .copied()
                .find(|&m| parent[m].is_none())
                .unwrap_or(members[0]);
            for &m in &members {
                if m != root && parent[m].is_none() {
                    parent[m] = Some(root);
                }
            }
            parent[root] = None;
            Thread {
                root,
                messages: members,
            }
        })
        .collect();
    threads.sort_by_key(|t| rank[t.root]);
    let mut thread_of = vec![0; n];
    for (t, thread) in threads.iter().enumerate() {
        for &m in &thread.messages {
            thread_of[m] = t;
        }
    }
    Threads {
        threads,
        thread_of,
        parent,
    }
}

/// For every message, the message in the nearest ancestor container, skipping
/// empty (missing-message) containers. Memoized, so linear overall.
fn nearest_message_parents(table: &Table<'_>, own: &[usize], n: usize) -> Vec<Option<usize>> {
    // For each container: the nearest message at or above it, once known.
    let mut memo: Vec<Option<Option<usize>>> = vec![None; table.containers.len()];
    let mut path = Vec::new();
    let mut resolve = |start: Option<usize>, memo: &mut Vec<Option<Option<usize>>>| {
        let mut current = start;
        let answer = loop {
            match current {
                None => break None,
                Some(c) => {
                    if let Some(known) = memo[c] {
                        break known;
                    }
                    if let Some(m) = table.containers[c].message {
                        break Some(m);
                    }
                    path.push(c);
                    current = table.containers[c].parent;
                }
            }
        };
        for c in path.drain(..) {
            memo[c] = Some(answer);
        }
        answer
    };
    (0..n)
        .map(|i| resolve(table.containers[own[i]].parent, &mut memo))
        .collect()
}

fn merge_by_subject(
    messages: &[ThreadInput<'_>],
    order: &[usize],
    own: &[usize],
    table: &mut Table<'_>,
    parent: &mut [Option<usize>],
) {
    // Root (earliest parentless message) of every component, in canonical order.
    let mut roots: HashMap<usize, usize> = HashMap::new();
    for &i in order {
        if parent[i].is_none() {
            let component = table.components.find(own[i]);
            roots.entry(component).or_insert(i);
        }
    }
    let mut by_subject: HashMap<String, usize> = HashMap::new();
    for &i in order {
        let component = table.components.find(own[i]);
        if roots.get(&component) != Some(&i) {
            continue;
        }
        let Some(subject) = messages[i].subject else {
            continue;
        };
        let key = parse_subject(subject).key();
        if key.is_empty() {
            continue;
        }
        match by_subject.get(&key) {
            Some(&target) if messages[i].fallback_subject_key().is_some() => {
                parent[i] = Some(target);
                table.components.union(own[target], own[i]);
            }
            Some(_) => {}
            None => {
                by_subject.insert(key, i);
            }
        }
    }
}

/// Lookups the store provides for [`join_thread`].
pub trait ThreadLookup {
    /// Thread identifier, e.g. the store's thread row id. When a message links several
    /// threads, the smallest key survives.
    type Key: Ord + Clone;

    /// The thread that a Message-ID belongs to: either a message with that ID, or a
    /// message whose references mention it (see [`thread_link_ids`]).
    fn thread_by_message_id(&self, message_id: &str) -> Option<Self::Key>;

    /// The thread whose root has this subject key ([`subject_thread_key`]).
    fn thread_by_subject(&self, _subject_key: &str) -> Option<Self::Key> {
        None
    }
}

impl<K: Ord + Clone, S: std::hash::BuildHasher> ThreadLookup for HashMap<String, K, S> {
    type Key = K;

    fn thread_by_message_id(&self, message_id: &str) -> Option<K> {
        self.get(message_id).cloned()
    }
}

/// What to do with a new message, from [`join_thread`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadJoin<K> {
    /// Start a new thread.
    New,
    /// Add the message to this thread.
    Join(K),
    /// The message connects several threads: merge `merged` into `into` and add the
    /// message there.
    Merge { into: K, merged: Vec<K> },
}

/// Decide which existing thread a new message joins.
///
/// Looks up the message's own ID and every referenced ID. One hit joins that thread,
/// several hits merge them (a reply that arrived before its parent, or a message that
/// references two conversations). Without hits, the subject fallback applies as in
/// [`thread_messages`]. After applying the result, register [`thread_link_ids`]
/// (and, for new threads, [`subject_thread_key`]) under the thread.
pub fn join_thread<L: ThreadLookup>(
    lookup: &L,
    message: &ThreadInput<'_>,
    options: &ThreadingOptions,
) -> ThreadJoin<L::Key> {
    let mut keys: Vec<L::Key> = thread_link_ids(message)
        .into_iter()
        .filter_map(|id| lookup.thread_by_message_id(id))
        .collect();
    keys.sort();
    keys.dedup();
    let mut keys = keys.into_iter();
    match keys.next() {
        Some(into) => {
            let merged: Vec<L::Key> = keys.collect();
            if merged.is_empty() {
                ThreadJoin::Join(into)
            } else {
                ThreadJoin::Merge { into, merged }
            }
        }
        None => {
            let by_subject = (options.subject_fallback == SubjectFallback::RepliesOnly)
                .then(|| message.fallback_subject_key())
                .flatten()
                .and_then(|key| lookup.thread_by_subject(&key));
            by_subject.map_or(ThreadJoin::New, ThreadJoin::Join)
        }
    }
}

/// The IDs to register for a message's thread: its own ID and every ID it
/// references (normalized, capped like [`thread_messages`]).
pub fn thread_link_ids<'a>(message: &ThreadInput<'a>) -> Vec<&'a str> {
    let mut ids = message.reference_chain();
    if let Some(own) = message.own_id() {
        ids.push(own);
    }
    ids
}

/// The subject key a thread root registers for the subject fallback.
pub fn subject_thread_key(message: &ThreadInput<'_>) -> Option<String> {
    let key = parse_subject(message.subject?).key();
    (!key.is_empty()).then_some(key)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashSet};

    use proptest::prelude::*;

    use super::*;

    /// Owned test message.
    #[derive(Debug, Clone)]
    struct Msg {
        id: Option<String>,
        irt: Vec<String>,
        refs: Vec<String>,
        subject: Option<String>,
        date: Option<i64>,
    }

    fn msg(id: &str, refs: &[&str], subject: &str, date: i64) -> Msg {
        Msg {
            id: Some(id.to_owned()),
            irt: Vec::new(),
            refs: refs.iter().map(|&r| r.to_owned()).collect(),
            subject: Some(subject.to_owned()),
            date: Some(date),
        }
    }

    fn inputs(msgs: &[Msg]) -> Vec<ThreadInput<'_>> {
        msgs.iter()
            .map(|m| ThreadInput {
                message_id: m.id.as_deref(),
                in_reply_to: &m.irt,
                references: &m.refs,
                subject: m.subject.as_deref(),
                date: m.date,
            })
            .collect()
    }

    fn run(msgs: &[Msg]) -> Threads {
        thread_messages(&inputs(msgs), &ThreadingOptions::default())
    }

    /// Threads as sets of message ids, plus root ids, for order-free comparison.
    fn shape(msgs: &[Msg], threads: &Threads) -> BTreeSet<(String, BTreeSet<String>)> {
        threads
            .threads
            .iter()
            .map(|t| {
                let id = |i: usize| msgs[i].id.clone().unwrap_or_default();
                (id(t.root), t.messages.iter().map(|&m| id(m)).collect())
            })
            .collect()
    }

    fn check_invariants(n: usize, threads: &Threads) -> Result<(), TestCaseError> {
        let mut seen = HashSet::new();
        for (t, thread) in threads.threads.iter().enumerate() {
            prop_assert!(thread.messages.contains(&thread.root));
            prop_assert_eq!(threads.parent[thread.root], None);
            for &m in &thread.messages {
                prop_assert!(seen.insert(m), "message {} in two threads", m);
                prop_assert_eq!(threads.thread_of[m], t);
                // Walking up terminates at the root, inside the thread.
                let mut current = m;
                let mut steps = 0;
                while let Some(p) = threads.parent[current] {
                    prop_assert_eq!(threads.thread_of[p], t);
                    current = p;
                    steps += 1;
                    prop_assert!(steps <= n, "cycle through message {}", m);
                }
                prop_assert_eq!(current, thread.root);
            }
        }
        prop_assert_eq!(seen.len(), n);
        Ok(())
    }

    #[test]
    fn threads_a_simple_conversation() {
        let msgs = vec![
            msg("c", &["a", "b"], "Re: Lunch", 3),
            msg("a", &[], "Lunch", 1),
            msg("b", &["a"], "Re: Lunch", 2),
            msg("x", &[], "Other", 4),
        ];
        let threads = run(&msgs);
        assert_eq!(threads.threads.len(), 2);
        assert_eq!(threads.threads[0].root, 1);
        assert_eq!(threads.threads[0].messages, vec![1, 2, 0]);
        assert_eq!(threads.parent, vec![Some(2), None, Some(1), None]);
    }

    #[test]
    fn missing_parents_keep_the_thread_together() {
        // "b" was never seen; both replies reference it.
        let msgs = vec![
            msg("c1", &["a", "b"], "Re: x", 3),
            msg("c2", &["a", "b"], "Re: x", 2),
        ];
        let threads = run(&msgs);
        assert_eq!(threads.threads.len(), 1);
        assert_eq!(threads.threads[0].root, 1);
        assert_eq!(threads.parent, vec![Some(1), None]);
    }

    #[test]
    fn in_reply_to_without_references() {
        let mut reply = msg("b", &[], "Re: x", 2);
        reply.irt = vec!["<a>".into()];
        let msgs = vec![msg("<a>", &[], "x", 1), reply];
        let threads = run(&msgs);
        assert_eq!(threads.parent, vec![None, Some(0)]);
    }

    #[test]
    fn subject_fallback_only_for_replies_without_references() {
        let msgs = vec![
            msg("a", &[], "Quarterly report", 1),
            msg("b", &[], "AW: Quarterly report", 2),
            msg("c", &[], "Quarterly report", 3),
            msg("d", &["zzz"], "Re: Quarterly report", 4),
        ];
        let threads = run(&msgs);
        let groups: Vec<Vec<usize>> = threads.threads.iter().map(|t| t.messages.clone()).collect();
        assert_eq!(groups, vec![vec![0, 1], vec![2], vec![3]]);
        assert_eq!(threads.parent[1], Some(0));

        let off = thread_messages(
            &inputs(&msgs),
            &ThreadingOptions {
                subject_fallback: SubjectFallback::Off,
            },
        );
        assert_eq!(off.threads.len(), 4);
    }

    #[test]
    fn duplicates_share_a_thread() {
        let msgs = vec![
            msg("a", &[], "x", 1),
            msg("a", &[], "x", 1),
            msg("b", &["a"], "Re: x", 2),
        ];
        let threads = run(&msgs);
        assert_eq!(threads.threads.len(), 1);
        assert_eq!(threads.parent, vec![None, Some(0), Some(0)]);
    }

    #[test]
    fn cyclic_references_do_not_loop() {
        let msgs = vec![
            msg("a", &["b"], "x", 1),
            msg("b", &["a"], "x", 2),
            msg("c", &["c", "a", "c"], "x", 3),
        ];
        let threads = run(&msgs);
        check_invariants(msgs.len(), &threads).unwrap();
        assert_eq!(threads.threads.len(), 1);
    }

    #[test]
    fn messages_without_ids_or_dates() {
        let msgs = vec![
            Msg {
                id: None,
                irt: Vec::new(),
                refs: Vec::new(),
                subject: None,
                date: None,
            },
            Msg {
                id: Some("  ".into()),
                irt: vec!["".into()],
                refs: vec!["<>".into()],
                subject: Some("Re: ".into()),
                date: None,
            },
        ];
        let threads = run(&msgs);
        assert_eq!(threads.threads.len(), 2);
        check_invariants(2, &threads).unwrap();
    }

    #[test]
    fn long_chains_scale_linearly() {
        // 20k messages, each referencing up to its last 20 ancestors.
        let n = 20_000;
        let ids: Vec<String> = (0..n).map(|i| format!("m{i}@example.com")).collect();
        let msgs: Vec<Msg> = (0..n)
            .map(|i| Msg {
                id: Some(ids[i].clone()),
                irt: Vec::new(),
                refs: ids[i.saturating_sub(20)..i].to_vec(),
                subject: Some("Re: long".into()),
                date: Some(i64::try_from(i).unwrap()),
            })
            .collect();
        let start = std::time::Instant::now();
        let threads = run(&msgs);
        assert_eq!(threads.threads.len(), 1);
        assert_eq!(threads.parent[n - 1], Some(n - 2));
        assert!(start.elapsed().as_secs() < 10, "took {:?}", start.elapsed());
    }

    #[test]
    fn join_thread_cases() {
        let mut index: HashMap<String, u32> = HashMap::new();
        index.insert("a".into(), 1);
        index.insert("b".into(), 2);
        let options = ThreadingOptions::default();

        let fresh = msg("z", &[], "Hello", 1);
        assert_eq!(
            join_thread(&index, &inputs(&[fresh])[0], &options),
            ThreadJoin::New
        );

        let reply = msg("c", &["a"], "Re: x", 1);
        assert_eq!(
            join_thread(&index, &inputs(&[reply])[0], &options),
            ThreadJoin::Join(1)
        );

        // Its own id was referenced by an existing thread (child arrived first).
        let parent = msg("b", &[], "x", 1);
        assert_eq!(
            join_thread(&index, &inputs(&[parent])[0], &options),
            ThreadJoin::Join(2)
        );

        let bridge = msg("d", &["b", "a"], "Re: x", 1);
        assert_eq!(
            join_thread(&index, &inputs(&[bridge])[0], &options),
            ThreadJoin::Merge {
                into: 1,
                merged: vec![2]
            }
        );
    }

    #[test]
    fn join_thread_subject_fallback() {
        struct Lookup;
        impl ThreadLookup for Lookup {
            type Key = u32;
            fn thread_by_message_id(&self, _: &str) -> Option<u32> {
                None
            }
            fn thread_by_subject(&self, key: &str) -> Option<u32> {
                (key == "quarterly report").then_some(7)
            }
        }
        let options = ThreadingOptions::default();
        let reply = msg("r", &[], "AW: Quarterly  Report", 1);
        assert_eq!(
            join_thread(&Lookup, &inputs(&[reply])[0], &options),
            ThreadJoin::Join(7)
        );
        let plain = msg("p", &[], "Quarterly Report", 1);
        assert_eq!(
            join_thread(&Lookup, &inputs(&[plain])[0], &options),
            ThreadJoin::New
        );
        assert_eq!(
            subject_thread_key(&inputs(&[msg("p", &[], "[list] Quarterly Report", 1)])[0]),
            Some("quarterly report".into())
        );
    }

    /// Up to 24 messages over a tiny id universe, so references collide, cycle and
    /// point at missing or duplicate messages.
    fn hostile_messages() -> impl Strategy<Value = Vec<Msg>> {
        let id = prop::sample::select(vec!["a", "b", "c", "d", "e", "f", "<a>", " b "]);
        let subject = prop::sample::select(vec!["x", "Re: x", "AW: x", "y", "Fwd: y", ""]);
        let one = (
            prop::option::weighted(0.9, id.clone()),
            prop::collection::vec(id.clone(), 0..2),
            prop::collection::vec(id, 0..6),
            prop::option::of(subject),
            prop::option::weighted(0.9, 0i64..5),
        )
            .prop_map(|(id, irt, refs, subject, date)| Msg {
                id: id.map(str::to_owned),
                irt: irt.into_iter().map(str::to_owned).collect(),
                refs: refs.into_iter().map(str::to_owned).collect(),
                subject: subject.map(str::to_owned),
                date,
            });
        prop::collection::vec(one, 0..24)
    }

    /// Unique ids and distinct dates, with references only to earlier messages
    /// (well-formed mail), shuffled.
    fn wellformed_messages() -> impl Strategy<Value = (Vec<Msg>, Vec<usize>)> {
        (1usize..30)
            .prop_flat_map(|n| {
                let parents = prop::collection::vec(prop::option::of(0usize..n), n);
                let missing = prop::collection::vec(prop::bool::weighted(0.2), n);
                (
                    parents,
                    missing,
                    Just((0..n).collect::<Vec<_>>()).prop_shuffle(),
                )
            })
            .prop_map(|(parents, missing, perm)| {
                let n = parents.len();
                let mut msgs: Vec<Msg> = Vec::with_capacity(n);
                for i in 0..n {
                    let parent = parents[i].filter(|&p| p < i);
                    let refs = match parent {
                        Some(p) => {
                            let mut refs = msgs[p].refs.clone();
                            refs.push(format!("m{p}"));
                            refs
                        }
                        None if missing[i] => vec![format!("gone{}", i % 3)],
                        None => Vec::new(),
                    };
                    let subject = if parent.is_some() {
                        "Re: topic"
                    } else {
                        "topic"
                    };
                    msgs.push(Msg {
                        id: Some(format!("m{i}")),
                        irt: Vec::new(),
                        refs,
                        subject: Some(format!("{subject} {}", i % 4)),
                        date: Some(i64::try_from(i).unwrap() * 60),
                    });
                }
                (msgs, perm)
            })
    }

    proptest! {
        #[test]
        fn hostile_headers_keep_invariants(msgs in hostile_messages()) {
            let threads = run(&msgs);
            check_invariants(msgs.len(), &threads)?;
        }

        #[test]
        fn result_does_not_depend_on_input_order((msgs, perm) in wellformed_messages()) {
            let shuffled: Vec<Msg> = perm.iter().map(|&i| msgs[i].clone()).collect();
            let a = run(&msgs);
            let b = run(&shuffled);
            check_invariants(msgs.len(), &a)?;
            prop_assert_eq!(shape(&msgs, &a), shape(&shuffled, &b));
        }

        #[test]
        fn incremental_joins_match_the_batch((msgs, _) in wellformed_messages()) {
            let options = ThreadingOptions { subject_fallback: SubjectFallback::Off };
            // Simulated store: id -> thread key, thread key -> member indices.
            let mut index: HashMap<String, usize> = HashMap::new();
            let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
            let inputs = inputs(&msgs);
            for (i, input) in inputs.iter().enumerate() {
                let key = match join_thread(&index, input, &options) {
                    ThreadJoin::New => i,
                    ThreadJoin::Join(k) => k,
                    ThreadJoin::Merge { into, merged } => {
                        for k in merged {
                            let moved = members.remove(&k).unwrap_or_default();
                            for v in index.values_mut() {
                                if *v == k {
                                    *v = into;
                                }
                            }
                            members.entry(into).or_default().extend(moved);
                        }
                        into
                    }
                };
                members.entry(key).or_default().push(i);
                for id in thread_link_ids(input) {
                    index.insert(id.to_owned(), key);
                }
            }
            let incremental: BTreeSet<BTreeSet<usize>> =
                members.into_values().map(|m| m.into_iter().collect()).collect();
            let batch: BTreeSet<BTreeSet<usize>> = thread_messages(&inputs, &options)
                .threads
                .into_iter()
                .map(|t| t.messages.into_iter().collect())
                .collect();
            prop_assert_eq!(incremental, batch);
        }
    }
}
