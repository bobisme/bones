//! Divergent-branch replay for CRDT state reconstruction.
//!
//! When two branches diverge from an LCA, replay collects the events on each
//! branch since the LCA and returns them in a deterministic merged order
//! suitable for CRDT state reconstruction.
//!
//! # Algorithm
//!
//! 1. Find events on branch A: walk from `tip_a` back to LCA, collecting
//!    all events that are descendants of LCA and ancestors of `tip_a`.
//! 2. Find events on branch B: same for `tip_b`.
//! 3. Union the two sets (some events may appear on both branches if there
//!    were intermediate merges).
//! 4. Sort in deterministic order: `(wall_ts_us, agent, event_hash)`.
//!
//! # Performance
//!
//! O(D) where D is the number of divergent events (events since LCA),
//! not O(N) where N is the total DAG size.

use std::collections::HashSet;

use crate::event::Event;

use super::graph::EventDag;
use super::lca::{LcaError, find_lca};

/// Errors from divergent-branch replay.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    /// LCA computation failed.
    #[error(transparent)]
    Lca(#[from] LcaError),

    /// The two tips have no common ancestor (disjoint roots).
    #[error("tips have no common ancestor; cannot compute divergent replay")]
    NoDivergence,
}

/// The result of replaying divergent branches.
#[derive(Debug, Clone)]
pub struct DivergentReplay {
    /// The LCA event hash — the point where the branches diverged.
    pub lca: String,

    /// Events only on branch A (not on branch B, not the LCA).
    pub branch_a: Vec<Event>,

    /// Events only on branch B (not on branch A, not the LCA).
    pub branch_b: Vec<Event>,

    /// All divergent events from both branches, merged and sorted
    /// by `(wall_ts_us, agent, event_hash)` for deterministic replay.
    pub merged: Vec<Event>,
}

/// Collect and replay divergent events between two branch tips.
///
/// Given two tip events, finds their LCA and collects events that are
/// on each branch but not reachable from the other (plus events on
/// both branches, which appear once in the merged result).
///
/// # Arguments
///
/// * `dag`   — the event DAG
/// * `tip_a` — hash of the tip of branch A
/// * `tip_b` — hash of the tip of branch B
///
/// # Returns
///
/// A [`DivergentReplay`] containing the LCA, per-branch events, and
/// the merged (deduplicated, sorted) event sequence.
///
/// # Special cases
///
/// - If `tip_a == tip_b`, there is no divergence — returns empty branches
///   and empty merged list.
/// - If one tip is an ancestor of the other, only events on the longer
///   branch are returned (the shorter branch is empty).
///
/// # Errors
///
/// Returns [`ReplayError::Lca`] if either tip is not in the DAG, or
/// [`ReplayError::NoDivergence`] if the tips share no common ancestor.
pub fn replay_divergent(
    dag: &EventDag,
    tip_a: &str,
    tip_b: &str,
) -> Result<DivergentReplay, ReplayError> {
    let lca_hash = find_lca(dag, tip_a, tip_b)?.ok_or(ReplayError::NoDivergence)?;

    // Same tip → no divergence.
    if tip_a == tip_b {
        return Ok(DivergentReplay {
            lca: lca_hash,
            branch_a: vec![],
            branch_b: vec![],
            merged: vec![],
        });
    }

    // Collect events on each branch since LCA.
    let events_a = events_between(dag, &lca_hash, tip_a);
    let events_b = events_between(dag, &lca_hash, tip_b);

    // Build hash sets for branch membership.
    let hashes_a: HashSet<&str> = events_a.iter().map(|e| e.event_hash.as_str()).collect();
    let hashes_b: HashSet<&str> = events_b.iter().map(|e| e.event_hash.as_str()).collect();

    // Events only on A (not on B).
    let branch_a: Vec<Event> = events_a
        .iter()
        .filter(|e| !hashes_b.contains(e.event_hash.as_str()))
        .cloned()
        .collect();

    // Events only on B (not on A).
    let branch_b: Vec<Event> = events_b
        .iter()
        .filter(|e| !hashes_a.contains(e.event_hash.as_str()))
        .cloned()
        .collect();

    // Merged: union of both branches, deduplicated, sorted.
    let mut seen: HashSet<String> = HashSet::new();
    let mut merged: Vec<Event> = Vec::new();
    for event in events_a.iter().chain(events_b.iter()) {
        if seen.insert(event.event_hash.clone()) {
            merged.push(event.clone());
        }
    }

    // Sort by (wall_ts_us, agent, event_hash) for deterministic replay order.
    merged.sort_by(|a, b| {
        a.wall_ts_us
            .cmp(&b.wall_ts_us)
            .then_with(|| a.agent.cmp(&b.agent))
            .then_with(|| a.event_hash.cmp(&b.event_hash))
    });

    Ok(DivergentReplay {
        lca: lca_hash,
        branch_a,
        branch_b,
        merged,
    })
}

/// Collect all events between `lca` (exclusive) and `tip` (inclusive)
/// by walking backward from `tip` and stopping at `lca`.
///
/// Uses BFS from `tip` upward, collecting events until we reach `lca`.
/// Returns events sorted by `(wall_ts_us, agent, event_hash)`.
fn events_between(dag: &EventDag, lca: &str, tip: &str) -> Vec<Event> {
    if lca == tip {
        return vec![];
    }

    // BFS backward from tip, stopping at lca.
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    let mut result: Vec<Event> = Vec::new();

    visited.insert(tip.to_string());
    queue.push_back(tip.to_string());

    while let Some(current) = queue.pop_front() {
        // Don't include the LCA itself in the result.
        if current == lca {
            continue;
        }

        if let Some(node) = dag.get(&current) {
            result.push(node.event.clone());

            for parent_hash in &node.parents {
                if visited.insert(parent_hash.clone()) {
                    queue.push_back(parent_hash.clone());
                }
            }
        }
    }

    // Sort for deterministic output.
    result.sort_by(|a, b| {
        a.wall_ts_us
            .cmp(&b.wall_ts_us)
            .then_with(|| a.agent.cmp(&b.agent))
            .then_with(|| a.event_hash.cmp(&b.event_hash))
    });

    result
}

/// Convenience: replay divergent events affecting a specific item.
///
/// Same as [`replay_divergent`], but filters the merged events to only
/// include those targeting the given `item_id`. Useful when merging
/// state for a single work item.
pub fn replay_divergent_for_item(
    dag: &EventDag,
    tip_a: &str,
    tip_b: &str,
    item_id: &str,
) -> Result<DivergentReplay, ReplayError> {
    let mut replay = replay_divergent(dag, tip_a, tip_b)?;

    replay.branch_a.retain(|e| e.item_id.as_str() == item_id);
    replay.branch_b.retain(|e| e.item_id.as_str() == item_id);
    replay.merged.retain(|e| e.item_id.as_str() == item_id);

    Ok(replay)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::graph::EventDag;
    use crate::event::Event;
    use crate::event::data::{CreateData, EventData, MoveData, UpdateData};
    use crate::event::types::EventType;
    use crate::event::writer::write_event;
    use crate::model::item::{Kind, State, Urgency};
    use crate::model::item_id::ItemId;
    use std::collections::BTreeMap;

    // -------------------------------------------------------------------
    // Helpers
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

    fn make_event_for_item(ts: i64, parents: &[&str], agent: &str, item: &str) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: agent.into(),
            itc: format!("itc:AQ.{ts}"),
            parents: parents.iter().map(|s| (*s).to_string()).collect(),
            event_type: EventType::Update,
            item_id: ItemId::new_unchecked(item),
            data: EventData::Update(UpdateData {
                field: "title".into(),
                value: serde_json::json!("updated"),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        write_event(&mut event).unwrap();
        event
    }

    // ===================================================================
    // replay_divergent tests
    // ===================================================================

    #[test]
    fn replay_same_tip_returns_empty() {
        let root = make_root(1_000, "agent-a");
        let dag = EventDag::from_events(&[root.clone()]);

        let replay = replay_divergent(&dag, &root.event_hash, &root.event_hash).unwrap();
        assert_eq!(replay.lca, root.event_hash);
        assert!(replay.branch_a.is_empty());
        assert!(replay.branch_b.is_empty());
        assert!(replay.merged.is_empty());
    }

    #[test]
    fn replay_one_ancestor_of_other() {
        // root → child
        // LCA(root, child) = root
        // Only child is on the longer branch
        let root = make_root(1_000, "agent-a");
        let child = make_child(2_000, &[&root.event_hash], "agent-a");
        let dag = EventDag::from_events(&[root.clone(), child.clone()]);

        let replay = replay_divergent(&dag, &root.event_hash, &child.event_hash).unwrap();
        assert_eq!(replay.lca, root.event_hash);
        assert!(replay.branch_a.is_empty()); // root is the LCA, no events past it on branch A
        assert_eq!(replay.branch_b.len(), 1);
        assert_eq!(replay.branch_b[0].event_hash, child.event_hash);
        assert_eq!(replay.merged.len(), 1);
    }

    #[test]
    fn replay_simple_fork() {
        //      root
        //     /    \
        //   left   right
        let root = make_root(1_000, "agent-a");
        let left = make_update(2_000, &[&root.event_hash], "title", "agent-a");
        let right = make_update(2_100, &[&root.event_hash], "priority", "agent-b");
        let dag = EventDag::from_events(&[root.clone(), left.clone(), right.clone()]);

        let replay = replay_divergent(&dag, &left.event_hash, &right.event_hash).unwrap();
        assert_eq!(replay.lca, root.event_hash);
        assert_eq!(replay.branch_a.len(), 1);
        assert_eq!(replay.branch_a[0].event_hash, left.event_hash);
        assert_eq!(replay.branch_b.len(), 1);
        assert_eq!(replay.branch_b[0].event_hash, right.event_hash);
        assert_eq!(replay.merged.len(), 2);
    }

    #[test]
    fn replay_deep_branches() {
        //  root → a → b → left1 → left2
        //                \→ right1 → right2
        let root = make_root(1_000, "agent-a");
        let a = make_child(2_000, &[&root.event_hash], "agent-a");
        let b = make_child(3_000, &[&a.event_hash], "agent-a");
        let left1 = make_update(4_000, &[&b.event_hash], "title", "agent-a");
        let left2 = make_update(5_000, &[&left1.event_hash], "desc", "agent-a");
        let right1 = make_update(4_100, &[&b.event_hash], "priority", "agent-b");
        let right2 = make_update(5_100, &[&right1.event_hash], "size", "agent-b");

        let dag = EventDag::from_events(&[
            root.clone(),
            a.clone(),
            b.clone(),
            left1.clone(),
            left2.clone(),
            right1.clone(),
            right2.clone(),
        ]);

        let replay = replay_divergent(&dag, &left2.event_hash, &right2.event_hash).unwrap();
        assert_eq!(replay.lca, b.event_hash);
        assert_eq!(replay.branch_a.len(), 2); // left1, left2
        assert_eq!(replay.branch_b.len(), 2); // right1, right2
        assert_eq!(replay.merged.len(), 4); // all 4 divergent events
    }

    #[test]
    fn replay_merged_events_sorted_deterministically() {
        let root = make_root(1_000, "agent-a");
        let left = make_update(3_000, &[&root.event_hash], "title", "agent-b"); // later ts, agent-b
        let right = make_update(2_000, &[&root.event_hash], "priority", "agent-a"); // earlier ts, agent-a
        let dag = EventDag::from_events(&[root.clone(), left.clone(), right.clone()]);

        let replay = replay_divergent(&dag, &left.event_hash, &right.event_hash).unwrap();
        assert_eq!(replay.merged.len(), 2);
        // Should be sorted by ts: right (2_000) before left (3_000)
        assert_eq!(replay.merged[0].wall_ts_us, 2_000);
        assert_eq!(replay.merged[1].wall_ts_us, 3_000);
    }

    #[test]
    fn replay_symmetric() {
        let root = make_root(1_000, "agent-a");
        let left = make_update(2_000, &[&root.event_hash], "title", "agent-a");
        let right = make_update(2_100, &[&root.event_hash], "priority", "agent-b");
        let dag = EventDag::from_events(&[root.clone(), left.clone(), right.clone()]);

        let replay_ab = replay_divergent(&dag, &left.event_hash, &right.event_hash).unwrap();
        let replay_ba = replay_divergent(&dag, &right.event_hash, &left.event_hash).unwrap();

        // Merged should be identical (deterministic sort).
        let hashes_ab: Vec<&str> = replay_ab
            .merged
            .iter()
            .map(|e| e.event_hash.as_str())
            .collect();
        let hashes_ba: Vec<&str> = replay_ba
            .merged
            .iter()
            .map(|e| e.event_hash.as_str())
            .collect();
        assert_eq!(hashes_ab, hashes_ba, "merged replay must be symmetric");
    }

    #[test]
    fn replay_disjoint_roots_returns_error() {
        let root_a = make_root(1_000, "agent-a");
        let root_b = make_root(1_100, "agent-b");
        let dag = EventDag::from_events(&[root_a.clone(), root_b.clone()]);

        let err = replay_divergent(&dag, &root_a.event_hash, &root_b.event_hash).unwrap_err();
        assert!(matches!(err, ReplayError::NoDivergence));
    }

    #[test]
    fn replay_event_not_found() {
        let dag = EventDag::new();
        let err = replay_divergent(&dag, "blake3:nope", "blake3:also-nope").unwrap_err();
        assert!(matches!(err, ReplayError::Lca(LcaError::EventNotFound(_))));
    }

    // ===================================================================
    // replay_divergent_for_item tests
    // ===================================================================

    #[test]
    fn replay_for_item_filters_correctly() {
        // Two items diverge from the same root
        let root = make_root(1_000, "agent-a"); // item: bn-test
        let update_test = make_event_for_item(2_000, &[&root.event_hash], "agent-a", "bn-test");
        let update_other = make_event_for_item(2_100, &[&root.event_hash], "agent-b", "bn-other");

        let dag = EventDag::from_events(&[root.clone(), update_test.clone(), update_other.clone()]);

        let replay = replay_divergent_for_item(
            &dag,
            &update_test.event_hash,
            &update_other.event_hash,
            "bn-test",
        )
        .unwrap();

        // Only bn-test events should be in the result
        assert!(
            replay
                .merged
                .iter()
                .all(|e| e.item_id.as_str() == "bn-test")
        );
    }

    // ===================================================================
    // Complex topology tests
    // ===================================================================

    #[test]
    fn replay_after_previous_merge() {
        //     root
        //    /    \
        //  a1      b1
        //    \    /
        //    merge
        //    /    \
        //  a2      b2
        let root = make_root(1_000, "agent-a");
        let a1 = make_update(2_000, &[&root.event_hash], "title", "agent-a");
        let b1 = make_update(2_100, &[&root.event_hash], "priority", "agent-b");
        let merge = make_child(3_000, &[&a1.event_hash, &b1.event_hash], "agent-a");
        let a2 = make_update(4_000, &[&merge.event_hash], "desc", "agent-a");
        let b2 = make_update(4_100, &[&merge.event_hash], "size", "agent-b");

        let dag = EventDag::from_events(&[
            root.clone(),
            a1.clone(),
            b1.clone(),
            merge.clone(),
            a2.clone(),
            b2.clone(),
        ]);

        let replay = replay_divergent(&dag, &a2.event_hash, &b2.event_hash).unwrap();
        // LCA should be the merge point, not the root.
        assert_eq!(replay.lca, merge.event_hash);
        assert_eq!(replay.branch_a.len(), 1); // a2
        assert_eq!(replay.branch_b.len(), 1); // b2
        assert_eq!(replay.merged.len(), 2);
    }

    #[test]
    fn replay_handles_multiple_items() {
        // Fork with events affecting different items
        let root = make_root(1_000, "agent-a");
        let left = make_event_for_item(2_000, &[&root.event_hash], "agent-a", "bn-item1");
        let right = make_event_for_item(2_100, &[&root.event_hash], "agent-b", "bn-item2");

        let dag = EventDag::from_events(&[root.clone(), left.clone(), right.clone()]);

        let replay = replay_divergent(&dag, &left.event_hash, &right.event_hash).unwrap();
        assert_eq!(replay.merged.len(), 2);

        // Filter for item1
        let replay_item1 =
            replay_divergent_for_item(&dag, &left.event_hash, &right.event_hash, "bn-item1")
                .unwrap();
        assert_eq!(replay_item1.merged.len(), 1);
        assert_eq!(replay_item1.merged[0].item_id.as_str(), "bn-item1");

        // Filter for item2
        let replay_item2 =
            replay_divergent_for_item(&dag, &left.event_hash, &right.event_hash, "bn-item2")
                .unwrap();
        assert_eq!(replay_item2.merged.len(), 1);
        assert_eq!(replay_item2.merged[0].item_id.as_str(), "bn-item2");
    }

    #[test]
    fn replay_performance_proportional_to_divergence() {
        // Build a long chain, then fork near the end.
        // The replay should only collect events from the fork point, not the whole chain.
        let mut events = vec![make_root(1_000, "agent-a")];
        for i in 1..50 {
            let parent_hash = events[i - 1].event_hash.clone();
            events.push(make_child(
                1_000 + i as i64 * 100,
                &[&parent_hash],
                "agent-a",
            ));
        }

        // Fork at event 49 (index 49)
        let fork_hash = events[49].event_hash.clone();
        let left = make_update(6_000, &[&fork_hash], "title", "agent-a");
        let right = make_update(6_100, &[&fork_hash], "priority", "agent-b");
        events.push(left.clone());
        events.push(right.clone());

        let dag = EventDag::from_events(&events);

        let replay = replay_divergent(&dag, &left.event_hash, &right.event_hash).unwrap();
        // LCA should be the fork point
        assert_eq!(replay.lca, fork_hash);
        // Only 2 divergent events, not 50
        assert_eq!(replay.merged.len(), 2);
    }
}
