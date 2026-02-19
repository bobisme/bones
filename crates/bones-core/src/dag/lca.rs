//! Lowest Common Ancestor (LCA) finding for the event DAG.
//!
//! Given two tip events in a DAG, the LCA is the most recent event that is
//! an ancestor of **both** tips. Finding the LCA identifies the point where
//! two branches diverged, which is the key input for divergent-branch replay.
//!
//! # Algorithm
//!
//! We use a bidirectional BFS: walk upward from both tips simultaneously,
//! alternating between them. The first node visited by **both** walks is
//! the LCA. This runs in O(divergent) — proportional to the events since
//! divergence, not the total DAG size.
//!
//! # Edge Cases
//!
//! - If one tip is an ancestor of the other, the ancestor tip **is** the LCA.
//! - If both tips are the same event, that event is the LCA.
//! - If the tips have no common ancestor (disjoint roots), returns `None`.

use std::collections::{HashSet, VecDeque};

use super::graph::EventDag;

/// Errors from LCA computation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LcaError {
    /// One or both tip hashes were not found in the DAG.
    #[error("event not found in DAG: {0}")]
    EventNotFound(String),
}

/// Find the Lowest Common Ancestor of two events in the DAG.
///
/// Returns `Ok(Some(hash))` with the LCA event hash, or `Ok(None)` if the
/// two events share no common ancestor (disjoint roots).
///
/// # Special cases
///
/// - If `tip_a == tip_b`, returns `Ok(Some(tip_a))`.
/// - If `tip_a` is an ancestor of `tip_b`, returns `Ok(Some(tip_a))`.
/// - If `tip_b` is an ancestor of `tip_a`, returns `Ok(Some(tip_b))`.
///
/// # Errors
///
/// Returns [`LcaError::EventNotFound`] if either tip hash is not in the DAG.
///
/// # Performance
///
/// Runs in O(D) where D is the number of events between the tips and their
/// LCA — proportional to the divergent portion, not the entire DAG.
pub fn find_lca(dag: &EventDag, tip_a: &str, tip_b: &str) -> Result<Option<String>, LcaError> {
    // Validate both tips exist.
    if !dag.contains(tip_a) {
        return Err(LcaError::EventNotFound(tip_a.to_string()));
    }
    if !dag.contains(tip_b) {
        return Err(LcaError::EventNotFound(tip_b.to_string()));
    }

    // Same tip → trivial LCA.
    if tip_a == tip_b {
        return Ok(Some(tip_a.to_string()));
    }

    // Bidirectional BFS: walk upward from both tips.
    // visited_a and visited_b track which nodes each walk has seen.
    let mut visited_a: HashSet<String> = HashSet::new();
    let mut visited_b: HashSet<String> = HashSet::new();
    let mut queue_a: VecDeque<String> = VecDeque::new();
    let mut queue_b: VecDeque<String> = VecDeque::new();

    // Seed both queues with the tips themselves.
    visited_a.insert(tip_a.to_string());
    visited_b.insert(tip_b.to_string());
    queue_a.push_back(tip_a.to_string());
    queue_b.push_back(tip_b.to_string());

    // Check initial cross-containment (one tip is ancestor of the other).
    if visited_b.contains(tip_a) {
        return Ok(Some(tip_a.to_string()));
    }
    if visited_a.contains(tip_b) {
        return Ok(Some(tip_b.to_string()));
    }

    // Alternate BFS steps between the two walks.
    loop {
        let a_done = queue_a.is_empty();
        let b_done = queue_b.is_empty();

        if a_done && b_done {
            // Both walks exhausted — no common ancestor.
            return Ok(None);
        }

        // Step walk A.
        if !a_done {
            if let Some(lca) = bfs_step(dag, &mut queue_a, &mut visited_a, &visited_b) {
                return Ok(Some(lca));
            }
        }

        // Step walk B.
        if !b_done {
            if let Some(lca) = bfs_step(dag, &mut queue_b, &mut visited_b, &visited_a) {
                return Ok(Some(lca));
            }
        }
    }
}

/// Perform one BFS step: dequeue a node, enqueue its parents.
/// Returns `Some(hash)` if any newly visited node is in `other_visited`.
fn bfs_step(
    dag: &EventDag,
    queue: &mut VecDeque<String>,
    visited: &mut HashSet<String>,
    other_visited: &HashSet<String>,
) -> Option<String> {
    let current = queue.pop_front()?;

    if let Some(node) = dag.get(&current) {
        for parent_hash in &node.parents {
            if visited.insert(parent_hash.clone()) {
                // First time this walk sees this node.
                if other_visited.contains(parent_hash) {
                    // Both walks have now visited this node → LCA found.
                    return Some(parent_hash.clone());
                }
                queue.push_back(parent_hash.clone());
            }
        }
    }

    None
}

/// Find **all** LCAs (there can be multiple in a DAG with diamond merges).
///
/// An LCA is a common ancestor of both tips such that no descendant of it
/// is also a common ancestor. In practice, most divergent branches have a
/// single LCA, but complex merge histories can produce multiple.
///
/// Returns the hashes in no particular order. Returns an empty vec if the
/// tips share no common ancestor.
///
/// # Errors
///
/// Returns [`LcaError::EventNotFound`] if either tip is not in the DAG.
pub fn find_all_lcas(dag: &EventDag, tip_a: &str, tip_b: &str) -> Result<Vec<String>, LcaError> {
    if !dag.contains(tip_a) {
        return Err(LcaError::EventNotFound(tip_a.to_string()));
    }
    if !dag.contains(tip_b) {
        return Err(LcaError::EventNotFound(tip_b.to_string()));
    }

    if tip_a == tip_b {
        return Ok(vec![tip_a.to_string()]);
    }

    // Compute full ancestor sets for both tips (including the tips themselves).
    let mut ancestors_a = dag.ancestors(tip_a);
    ancestors_a.insert(tip_a.to_string());
    let mut ancestors_b = dag.ancestors(tip_b);
    ancestors_b.insert(tip_b.to_string());

    // Common ancestors = intersection.
    let common: HashSet<&String> = ancestors_a.intersection(&ancestors_b).collect();

    if common.is_empty() {
        return Ok(vec![]);
    }

    // LCAs are common ancestors that have no descendants that are also common ancestors.
    // A common ancestor C is an LCA iff no child of C (transitively) is also a common ancestor.
    let mut lcas: Vec<String> = Vec::new();
    for &ca in &common {
        let desc = dag.descendants(ca);
        let has_common_descendant = desc.iter().any(|d| common.contains(d));
        if !has_common_descendant {
            lcas.push(ca.clone());
        }
    }

    lcas.sort(); // deterministic order
    Ok(lcas)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::graph::EventDag;
    use crate::event::data::{CreateData, EventData, MoveData, UpdateData};
    use crate::event::types::EventType;
    use crate::event::writer::write_event;
    use crate::event::Event;
    use crate::model::item::{Kind, State, Urgency};
    use crate::model::item_id::ItemId;
    use std::collections::BTreeMap;

    // -------------------------------------------------------------------
    // Helpers (same pattern as graph.rs tests)
    // -------------------------------------------------------------------

    fn make_root(ts: i64, agent: &str) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: agent.into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-test"),
            data: EventData::Create(CreateData {
                title: format!("Root by {agent}"),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        write_event(&mut event).unwrap();
        event
    }

    fn make_child(ts: i64, parents: &[&str], agent: &str) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: agent.into(),
            itc: format!("itc:AQ.{ts}"),
            parents: parents.iter().map(|s| (*s).to_string()).collect(),
            event_type: EventType::Move,
            item_id: ItemId::new_unchecked("bn-test"),
            data: EventData::Move(MoveData {
                state: State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        write_event(&mut event).unwrap();
        event
    }

    fn make_update(ts: i64, parents: &[&str], field: &str, agent: &str) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: agent.into(),
            itc: format!("itc:AQ.{ts}"),
            parents: parents.iter().map(|s| (*s).to_string()).collect(),
            event_type: EventType::Update,
            item_id: ItemId::new_unchecked("bn-test"),
            data: EventData::Update(UpdateData {
                field: field.into(),
                value: serde_json::json!("new-value"),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        write_event(&mut event).unwrap();
        event
    }

    // ===================================================================
    // find_lca tests
    // ===================================================================

    #[test]
    fn lca_same_tip() {
        let root = make_root(1_000, "agent-a");
        let dag = EventDag::from_events(&[root.clone()]);

        let lca = find_lca(&dag, &root.event_hash, &root.event_hash).unwrap();
        assert_eq!(lca, Some(root.event_hash));
    }

    #[test]
    fn lca_event_not_found() {
        let dag = EventDag::new();
        let err = find_lca(&dag, "blake3:nope", "blake3:also-nope").unwrap_err();
        assert!(matches!(err, LcaError::EventNotFound(_)));
    }

    #[test]
    fn lca_one_is_ancestor_of_other() {
        // root → child
        let root = make_root(1_000, "agent-a");
        let child = make_child(2_000, &[&root.event_hash], "agent-a");
        let dag = EventDag::from_events(&[root.clone(), child.clone()]);

        // LCA(root, child) = root
        let lca = find_lca(&dag, &root.event_hash, &child.event_hash).unwrap();
        assert_eq!(lca, Some(root.event_hash.clone()));

        // LCA(child, root) = root (symmetric)
        let lca2 = find_lca(&dag, &child.event_hash, &root.event_hash).unwrap();
        assert_eq!(lca2, Some(root.event_hash));
    }

    #[test]
    fn lca_simple_fork() {
        //      root
        //     /    \
        //   left   right
        let root = make_root(1_000, "agent-a");
        let left = make_child(2_000, &[&root.event_hash], "agent-a");
        let right = make_child(2_100, &[&root.event_hash], "agent-b");
        let dag = EventDag::from_events(&[root.clone(), left.clone(), right.clone()]);

        let lca = find_lca(&dag, &left.event_hash, &right.event_hash).unwrap();
        assert_eq!(lca, Some(root.event_hash));
    }

    #[test]
    fn lca_deep_fork() {
        //  root → a → b → left
        //               \→ right
        let root = make_root(1_000, "agent-a");
        let a = make_child(2_000, &[&root.event_hash], "agent-a");
        let b = make_child(3_000, &[&a.event_hash], "agent-a");
        let left = make_child(4_000, &[&b.event_hash], "agent-a");
        let right = make_child(4_100, &[&b.event_hash], "agent-b");

        let dag = EventDag::from_events(&[
            root.clone(),
            a.clone(),
            b.clone(),
            left.clone(),
            right.clone(),
        ]);

        let lca = find_lca(&dag, &left.event_hash, &right.event_hash).unwrap();
        assert_eq!(lca, Some(b.event_hash));
    }

    #[test]
    fn lca_asymmetric_depth() {
        //  root → a → b → c → left
        //       \→ right
        let root = make_root(1_000, "agent-a");
        let a = make_child(2_000, &[&root.event_hash], "agent-a");
        let b = make_child(3_000, &[&a.event_hash], "agent-a");
        let c = make_child(4_000, &[&b.event_hash], "agent-a");
        let left = make_child(5_000, &[&c.event_hash], "agent-a");
        let right = make_child(2_100, &[&root.event_hash], "agent-b");

        let dag = EventDag::from_events(&[
            root.clone(),
            a.clone(),
            b.clone(),
            c.clone(),
            left.clone(),
            right.clone(),
        ]);

        let lca = find_lca(&dag, &left.event_hash, &right.event_hash).unwrap();
        assert_eq!(lca, Some(root.event_hash));
    }

    #[test]
    fn lca_diamond_after_fork() {
        //     root
        //    /    \
        //  a1      b1
        //    \    /
        //    merge
        //    /    \
        //  a2      b2
        let root = make_root(1_000, "agent-a");
        let a1 = make_child(2_000, &[&root.event_hash], "agent-a");
        let b1 = make_child(2_100, &[&root.event_hash], "agent-b");
        let merge = make_child(3_000, &[&a1.event_hash, &b1.event_hash], "agent-a");
        let a2 = make_child(4_000, &[&merge.event_hash], "agent-a");
        let b2 = make_child(4_100, &[&merge.event_hash], "agent-b");

        let dag = EventDag::from_events(&[
            root.clone(),
            a1.clone(),
            b1.clone(),
            merge.clone(),
            a2.clone(),
            b2.clone(),
        ]);

        // LCA of the second fork should be the merge point
        let lca = find_lca(&dag, &a2.event_hash, &b2.event_hash).unwrap();
        assert_eq!(lca, Some(merge.event_hash));
    }

    #[test]
    fn lca_disjoint_roots_returns_none() {
        // Two independent roots with no common ancestor
        let root_a = make_root(1_000, "agent-a");
        let root_b = make_root(1_100, "agent-b");
        let dag = EventDag::from_events(&[root_a.clone(), root_b.clone()]);

        let lca = find_lca(&dag, &root_a.event_hash, &root_b.event_hash).unwrap();
        assert_eq!(lca, None);
    }

    #[test]
    fn lca_is_symmetric() {
        let root = make_root(1_000, "agent-a");
        let left = make_child(2_000, &[&root.event_hash], "agent-a");
        let right = make_child(2_100, &[&root.event_hash], "agent-b");
        let dag = EventDag::from_events(&[root.clone(), left.clone(), right.clone()]);

        let lca_ab = find_lca(&dag, &left.event_hash, &right.event_hash).unwrap();
        let lca_ba = find_lca(&dag, &right.event_hash, &left.event_hash).unwrap();
        assert_eq!(lca_ab, lca_ba, "LCA must be symmetric");
    }

    // ===================================================================
    // find_all_lcas tests
    // ===================================================================

    #[test]
    fn all_lcas_simple_fork() {
        let root = make_root(1_000, "agent-a");
        let left = make_child(2_000, &[&root.event_hash], "agent-a");
        let right = make_child(2_100, &[&root.event_hash], "agent-b");
        let dag = EventDag::from_events(&[root.clone(), left.clone(), right.clone()]);

        let lcas = find_all_lcas(&dag, &left.event_hash, &right.event_hash).unwrap();
        assert_eq!(lcas, vec![root.event_hash]);
    }

    #[test]
    fn all_lcas_criss_cross_merge() {
        // Criss-cross produces TWO LCAs:
        //
        //     root
        //    /    \
        //  a1      b1
        //  |  \  / |
        //  | merge1 |
        //  |  /  \  |
        //  a2      b2
        //
        // Actually, a true criss-cross:
        //     root
        //    /    \
        //  a1      b1
        //  | \    / |
        //  |  m1    |     (m1 = merge of a1+b1)
        //  |    \   |
        //  |     m2 |     (m2 = merge of a1+b1 from other side)
        //  |    /   |
        //  a2      b2
        //
        // For simplicity, test the diamond-with-two-paths case:
        //     root
        //    /    \
        //  a1      b1
        //    \    /
        //    merge
        // This has exactly 1 LCA (merge for tips on different branches after merge).
        //
        // A real 2-LCA case requires parallel merge points:
        //     root
        //    /    \
        //  a1      b1
        //   |\ /|  
        //   | X |
        //   |/ \|
        //  ma    mb    (ma merges a1+b1, mb merges a1+b1 independently)
        //   |    |
        //  a2   b2
        let root = make_root(1_000, "agent-a");
        let a1 = make_child(2_000, &[&root.event_hash], "agent-a");
        let b1 = make_child(2_100, &[&root.event_hash], "agent-b");
        // Two independent merges of a1+b1
        let ma = make_child(3_000, &[&a1.event_hash, &b1.event_hash], "agent-a");
        let mb = make_update(3_100, &[&a1.event_hash, &b1.event_hash], "title", "agent-b");
        // Further work on each merge
        let a2 = make_child(4_000, &[&ma.event_hash], "agent-a");
        let b2 = make_update(4_100, &[&mb.event_hash], "desc", "agent-b");

        let dag = EventDag::from_events(&[
            root.clone(),
            a1.clone(),
            b1.clone(),
            ma.clone(),
            mb.clone(),
            a2.clone(),
            b2.clone(),
        ]);

        let mut lcas = find_all_lcas(&dag, &a2.event_hash, &b2.event_hash).unwrap();
        lcas.sort();
        // ma is only an ancestor of a2 (not b2), and mb is only an ancestor
        // of b2 (not a2), so neither is a *common* ancestor. The actual
        // common ancestors are a1 and b1, and neither is an ancestor of
        // the other, so both are LCAs.
        assert_eq!(lcas.len(), 2);
        let mut expected = vec![a1.event_hash.clone(), b1.event_hash.clone()];
        expected.sort();
        assert_eq!(lcas, expected);
    }

    #[test]
    fn all_lcas_same_tip() {
        let root = make_root(1_000, "agent-a");
        let dag = EventDag::from_events(&[root.clone()]);

        let lcas = find_all_lcas(&dag, &root.event_hash, &root.event_hash).unwrap();
        assert_eq!(lcas, vec![root.event_hash]);
    }

    #[test]
    fn all_lcas_disjoint() {
        let root_a = make_root(1_000, "agent-a");
        let root_b = make_root(1_100, "agent-b");
        let dag = EventDag::from_events(&[root_a.clone(), root_b.clone()]);

        let lcas = find_all_lcas(&dag, &root_a.event_hash, &root_b.event_hash).unwrap();
        assert!(lcas.is_empty());
    }

    #[test]
    fn all_lcas_event_not_found() {
        let dag = EventDag::new();
        let err = find_all_lcas(&dag, "blake3:nope", "blake3:also-nope").unwrap_err();
        assert!(matches!(err, LcaError::EventNotFound(_)));
    }
}
