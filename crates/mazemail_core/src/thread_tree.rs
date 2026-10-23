//! The reply tree of one thread, for the expanded thread in the message list
//! (CONCEPT §8.2 "Thread tree").
//!
//! Parent links come from `mazemail_mime`'s JWZ threader over `References` and
//! `In-Reply-To` (cycle-safe; a missing parent is replaced by the nearest ancestor
//! that is there), without the subject fallback. The messages can be a subset of the
//! thread (the copies in the open folder), so the result can have several roots; they
//! are shown in date order. [`flatten`] turns parent links into display order: depth
//! first, children by date, with each message's depth.

use mazemail_mime::thread::{SubjectFallback, ThreadInput, ThreadingOptions, thread_messages};
use mazemail_types::MessageId;
use store::TreeMessage;

/// One message of a thread's reply tree, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeNode {
    /// The message.
    pub message: MessageId,
    /// The message it replies to, if that is in the tree.
    pub parent: Option<MessageId>,
    /// 0 for roots, 1 for their replies, …
    pub depth: u16,
}

/// The reply tree of `messages` (oldest first, as
/// [`Queries::thread_tree_messages`](store::Queries::thread_tree_messages) returns
/// them), in display order.
pub fn build(messages: &[TreeMessage]) -> Vec<TreeNode> {
    let in_reply_to: Vec<Vec<String>> = messages
        .iter()
        .map(|m| m.in_reply_to.iter().cloned().collect())
        .collect();
    let inputs: Vec<ThreadInput<'_>> = messages
        .iter()
        .zip(&in_reply_to)
        .map(|(m, irt)| ThreadInput {
            message_id: m.rfc_message_id.as_deref(),
            in_reply_to: irt,
            references: &m.references,
            subject: None,
            date: Some(m.date),
        })
        .collect();
    let threads = thread_messages(
        &inputs,
        &ThreadingOptions {
            subject_fallback: SubjectFallback::Off,
        },
    );
    let order: Vec<usize> = (0..messages.len()).collect();
    flatten(&order, &threads.parent)
        .into_iter()
        .map(|(ix, parent, depth)| TreeNode {
            message: messages[ix].id,
            parent: parent.map(|p| messages[p].id),
            depth,
        })
        .collect()
}

/// Display order of a forest given as parent links: `order` lists the nodes in the
/// order siblings should appear (by date), `parent[i]` is node `i`'s parent. Returns
/// `(node, parent, depth)` depth first. Nodes whose parent chain loops or leaves the
/// set become roots, so every node appears exactly once.
pub fn flatten(order: &[usize], parent: &[Option<usize>]) -> Vec<(usize, Option<usize>, u16)> {
    let n = parent.len();
    let mut parent: Vec<Option<usize>> = parent
        .iter()
        .enumerate()
        .map(|(node, p)| p.filter(|p| *p < n && *p != node))
        .collect();
    // Walk each parent chain once; a chain that comes back to a node on the current
    // walk is a cycle, which is cut at that node (it becomes a root).
    const NEW: u8 = 0;
    const WALKING: u8 = 1;
    const DONE: u8 = 2;
    let mut state = vec![NEW; n];
    let mut path = Vec::new();
    for start in 0..n {
        let mut current = start;
        loop {
            match state[current] {
                DONE => break,
                WALKING => {
                    parent[current] = None;
                    break;
                }
                _ => {}
            }
            state[current] = WALKING;
            path.push(current);
            match parent[current] {
                Some(p) => current = p,
                None => break,
            }
        }
        for node in path.drain(..) {
            state[node] = DONE;
        }
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots = Vec::new();
    for &node in order {
        if node >= n {
            continue;
        }
        match parent[node] {
            Some(p) => children[p].push(node),
            None => roots.push(node),
        }
    }
    let mut out = Vec::with_capacity(n);
    let mut seen = vec![false; n];
    let mut stack: Vec<(usize, u16)> = roots.iter().rev().map(|r| (*r, 0)).collect();
    while let Some((node, depth)) = stack.pop() {
        if std::mem::replace(&mut seen[node], true) {
            continue;
        }
        out.push((node, parent[node], depth));
        for &child in children[node].iter().rev() {
            stack.push((child, depth.saturating_add(1)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: i64, date: i64, own: &str, refs: &[&str]) -> TreeMessage {
        TreeMessage {
            id: MessageId(id),
            date,
            rfc_message_id: Some(own.to_owned()),
            in_reply_to: refs.last().map(|r| (*r).to_owned()),
            references: refs.iter().map(|r| (*r).to_owned()).collect(),
        }
    }

    fn shape(nodes: &[TreeNode]) -> Vec<(i64, u16)> {
        nodes.iter().map(|n| (n.message.0, n.depth)).collect()
    }

    #[test]
    fn builds_the_reply_tree_depth_first() {
        // a ─┬─ b ── d
        //    └─ c
        let nodes = build(&[
            message(1, 10, "a@x", &[]),
            message(2, 20, "b@x", &["a@x"]),
            message(3, 30, "c@x", &["a@x"]),
            message(4, 40, "d@x", &["a@x", "b@x"]),
        ]);
        assert_eq!(shape(&nodes), vec![(1, 0), (2, 1), (4, 2), (3, 1)]);
        assert_eq!(nodes[2].parent, Some(MessageId(2)));
        assert_eq!(nodes[0].parent, None);
    }

    #[test]
    fn a_missing_parent_is_replaced_by_its_nearest_ancestor() {
        // b (not in the folder) replied to a; c replied to b.
        let nodes = build(&[
            message(1, 10, "a@x", &[]),
            message(3, 30, "c@x", &["a@x", "b@x"]),
        ]);
        assert_eq!(shape(&nodes), vec![(1, 0), (3, 1)]);
        // Without any ancestor in the set, messages are roots by date.
        let nodes = build(&[
            message(5, 50, "e@x", &["gone@x"]),
            message(6, 60, "f@x", &["other@x"]),
        ]);
        assert!(nodes.iter().all(|n| n.depth == 0 || n.parent.is_some()));
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn cyclic_references_still_list_every_message_once() {
        let nodes = build(&[
            message(1, 10, "a@x", &["b@x"]),
            message(2, 20, "b@x", &["a@x"]),
            message(3, 30, "c@x", &["c@x"]),
        ]);
        let mut ids: Vec<i64> = nodes.iter().map(|n| n.message.0).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3]);
        assert!(nodes.iter().any(|n| n.depth == 0));
    }

    #[test]
    fn flatten_breaks_cycles_and_dangling_links() {
        // 0 → 1 → 2 → 0 is a cycle; 3's parent doesn't exist; 4 is a child of 3.
        let parent = [Some(2), Some(0), Some(1), Some(99), Some(3)];
        let out = flatten(&[0, 1, 2, 3, 4], &parent);
        assert_eq!(out.len(), 5);
        let mut nodes: Vec<usize> = out.iter().map(|(n, _, _)| *n).collect();
        nodes.sort_unstable();
        assert_eq!(nodes, vec![0, 1, 2, 3, 4]);
        let depth = |node: usize| out.iter().find(|(n, _, _)| *n == node).unwrap().2;
        assert_eq!(depth(3), 0, "a dangling parent makes a root");
        assert_eq!(depth(4), 1);
        // The cycle got exactly one root and hangs below it.
        let roots = out.iter().filter(|(n, _, d)| *d == 0 && *n < 3).count();
        assert_eq!(roots, 1);
    }

    #[test]
    fn children_keep_the_given_order() {
        let parent = [None, Some(0), Some(0), Some(0)];
        let out = flatten(&[0, 3, 1, 2], &parent);
        assert_eq!(
            out.iter().map(|(n, _, _)| *n).collect::<Vec<_>>(),
            vec![0, 3, 1, 2]
        );
    }
}
