//! Blocking and relates dependency graph built from CRDT item states.
//!
//! # Overview
//!
//! This module materializes a scheduling dependency graph from [`WorkItemState`]
//! OR-Sets. Two orthogonal link types are supported:
//!
//! - **Blocking** (`blocked_by`): scheduling dependency. An item with non-empty
//!   `blocked_by` is considered blocked and will not appear in "ready" work.
//! - **Relates** (`related_to`): informational link. Has no effect on scheduling.
//!
//! # Data Model
//!
//! Both link types are stored in [`WorkItemState`] as OR-Sets of item IDs:
//!
//! - `blocked_by: OrSet<String>` — items that must complete before this one
//! - `related_to: OrSet<String>` — items informationally related to this one
//!
//! The OR-Set semantics ensure convergent behavior under concurrent
//! `item.link` / `item.unlink` events: concurrent add+remove resolves
//! as add-wins (the link survives).
//!
//! # Cross-Goal Dependencies
//!
//! An item's `blocked_by` set may reference items from any goal, not just
//! the parent goal. `BlockingGraph` treats all item IDs uniformly.
//!
//! # Causation
//!
//! The `causation` field on `item.link` events captures audit provenance
//! (which event caused this link). It is stored in the event stream and
//! queryable via the event DAG — it is NOT a graph edge in the blocking
//! graph.
//!
//! # Usage
//!
//! ```rust,ignore
//! use std::collections::HashMap;
//! use bones_core::graph::blocking::BlockingGraph;
//! use bones_core::crdt::item_state::WorkItemState;
//!
//! let states: HashMap<String, WorkItemState> = /* ... */;
//! let graph = BlockingGraph::from_states(&states);
//!
//! if graph.is_blocked("bn-task1") {
//!     let blockers = graph.get_blockers("bn-task1");
//!     println!("bn-task1 is blocked by: {blockers:?}");
//! }
//!
//! let ready = graph.ready_items();
//! println!("Ready items: {ready:?}");
//! ```

#![allow(
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
)]

use std::collections::{HashMap, HashSet};

use crate::crdt::item_state::WorkItemState;

// ---------------------------------------------------------------------------
// BlockingGraph
// ---------------------------------------------------------------------------

/// A blocking/relates dependency graph materialized from CRDT item states.
///
/// Constructed from a snapshot of [`WorkItemState`] values keyed by item ID.
/// The graph is immutable once built — call [`BlockingGraph::from_states`]
/// again if states change.
///
/// # Scheduling Semantics
///
/// An item is **blocked** if its `blocked_by` OR-Set is non-empty (at least
/// one blocking link is present). Blocked items are excluded from "ready" work.
///
/// An item is **ready** if it is not blocked (its `blocked_by` OR-Set is empty).
///
/// # Relates Semantics
///
/// Relates links are informational only. They do not affect the blocked/ready
/// computation.
#[derive(Debug, Clone, Default)]
pub struct BlockingGraph {
    /// item_id → set of item_ids that block it.
    blocked_by: HashMap<String, HashSet<String>>,
    /// item_id → set of related item_ids.
    related_to: HashMap<String, HashSet<String>>,
    /// All known item IDs (the full key set from the source states map).
    all_items: HashSet<String>,
}

impl BlockingGraph {
    /// Build a [`BlockingGraph`] from a map of item states.
    ///
    /// Iterates every state and extracts `blocked_by` and `related_to`
    /// OR-Set values. Deleted items are included — callers may filter them
    /// with [`WorkItemState::is_deleted`] before passing states to this function.
    ///
    /// # Complexity
    ///
    /// O(N * L) where N is the number of items and L is the average number
    /// of links per item.
    pub fn from_states(states: &HashMap<String, WorkItemState>) -> Self {
        let all_items: HashSet<String> = states.keys().cloned().collect();
        let mut blocked_by: HashMap<String, HashSet<String>> = HashMap::new();
        let mut related_to: HashMap<String, HashSet<String>> = HashMap::new();

        for (item_id, state) in states {
            let blockers: HashSet<String> =
                state.blocked_by_ids().into_iter().cloned().collect();
            if !blockers.is_empty() {
                blocked_by.insert(item_id.clone(), blockers);
            }

            let related: HashSet<String> =
                state.related_to_ids().into_iter().cloned().collect();
            if !related.is_empty() {
                related_to.insert(item_id.clone(), related);
            }
        }

        Self {
            blocked_by,
            related_to,
            all_items,
        }
    }

    /// Return `true` if the item has at least one blocking dependency.
    ///
    /// An item is blocked if its `blocked_by` OR-Set is non-empty. To unblock,
    /// submit an `item.unlink` event which removes the link from the OR-Set.
    pub fn is_blocked(&self, item_id: &str) -> bool {
        self.blocked_by
            .get(item_id)
            .is_some_and(|blockers| !blockers.is_empty())
    }

    /// Return the set of item IDs that block the given item.
    ///
    /// Returns an empty set if the item is not blocked or not known.
    pub fn get_blockers(&self, item_id: &str) -> HashSet<&str> {
        self.blocked_by
            .get(item_id)
            .map(|blockers| blockers.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Return the set of item IDs related to the given item.
    ///
    /// Relates links are informational — they do not affect scheduling.
    /// Returns an empty set if the item has no relates links or is not known.
    pub fn get_related(&self, item_id: &str) -> HashSet<&str> {
        self.related_to
            .get(item_id)
            .map(|related| related.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Return all item IDs that have no active blocking dependencies.
    ///
    /// An item is "ready" if its `blocked_by` OR-Set is empty. Items with
    /// no known state entries are not included — only items present in the
    /// source states map appear here.
    ///
    /// For scheduling purposes, callers typically filter deleted and
    /// done/archived items before calling [`BlockingGraph::from_states`].
    pub fn ready_items(&self) -> HashSet<&str> {
        self.all_items
            .iter()
            .filter(|id| !self.is_blocked(id.as_str()))
            .map(String::as_str)
            .collect()
    }

    /// Return all item IDs that have at least one active blocking dependency.
    pub fn blocked_items(&self) -> HashSet<&str> {
        self.blocked_by
            .iter()
            .filter(|(_, blockers)| !blockers.is_empty())
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Return the total number of items in the graph.
    pub fn len(&self) -> usize {
        self.all_items.len()
    }

    /// Return `true` if the graph has no items.
    pub fn is_empty(&self) -> bool {
        self.all_items.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Standalone convenience functions
// ---------------------------------------------------------------------------

/// Check if an item is blocked given a map of states.
///
/// Returns `false` if the item is not present in `states`.
pub fn is_blocked(item_id: &str, states: &HashMap<String, WorkItemState>) -> bool {
    states
        .get(item_id)
        .is_some_and(|state| !state.blocked_by.is_empty())
}

/// Return the set of item IDs blocking the given item.
///
/// Returns an empty set if the item is not present or has no blockers.
pub fn get_blockers(item_id: &str, states: &HashMap<String, WorkItemState>) -> HashSet<String> {
    states
        .get(item_id)
        .map(|state| state.blocked_by_ids().into_iter().cloned().collect())
        .unwrap_or_default()
}

/// Return item IDs that have no active blocking dependencies.
///
/// Iterates all items in `states` and returns those whose `blocked_by`
/// OR-Set is empty. Order is unspecified.
pub fn ready_items(states: &HashMap<String, WorkItemState>) -> Vec<String> {
    states
        .keys()
        .filter(|id| !is_blocked(id.as_str(), states))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt::item_state::WorkItemState;
    use crate::event::data::{EventData, LinkData, UnlinkData};
    use crate::event::types::EventType;
    use crate::event::Event;
    use crate::clock::itc::Stamp;
    use crate::model::item_id::ItemId;
    use std::collections::BTreeMap;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_link_event(
        target: &str,
        link_type: &str,
        wall_ts: i64,
        agent: &str,
        hash: &str,
    ) -> Event {
        let mut stamp = Stamp::seed();
        stamp.event();
        Event {
            wall_ts_us: wall_ts,
            agent: agent.to_string(),
            itc: stamp.to_string(),
            parents: vec![],
            event_type: EventType::Link,
            item_id: ItemId::new_unchecked("bn-test"),
            data: EventData::Link(LinkData {
                target: target.to_string(),
                link_type: link_type.to_string(),
                extra: BTreeMap::new(),
            }),
            event_hash: hash.to_string(),
        }
    }

    fn make_unlink_event(
        target: &str,
        link_type: Option<&str>,
        wall_ts: i64,
        agent: &str,
        hash: &str,
    ) -> Event {
        let mut stamp = Stamp::seed();
        stamp.event();
        Event {
            wall_ts_us: wall_ts,
            agent: agent.to_string(),
            itc: stamp.to_string(),
            parents: vec![],
            event_type: EventType::Unlink,
            item_id: ItemId::new_unchecked("bn-test"),
            data: EventData::Unlink(UnlinkData {
                target: target.to_string(),
                link_type: link_type.map(|s| s.to_string()),
                extra: BTreeMap::new(),
            }),
            event_hash: hash.to_string(),
        }
    }

    /// Build a WorkItemState with the given blocking links applied.
    fn state_with_blockers(blocker_ids: &[&str]) -> WorkItemState {
        let mut state = WorkItemState::new();
        for (i, blocker) in blocker_ids.iter().enumerate() {
            let hash = format!("blake3:link{i}");
            let event = make_link_event(blocker, "blocks", 1000 + i as i64, "agent", &hash);
            state.apply_event(&event);
        }
        state
    }

    /// Build a WorkItemState with the given relates links applied.
    fn state_with_related(related_ids: &[&str]) -> WorkItemState {
        let mut state = WorkItemState::new();
        for (i, related) in related_ids.iter().enumerate() {
            let hash = format!("blake3:rel{i}");
            let event = make_link_event(related, "related_to", 1000 + i as i64, "agent", &hash);
            state.apply_event(&event);
        }
        state
    }

    // -----------------------------------------------------------------------
    // BlockingGraph construction
    // -----------------------------------------------------------------------

    #[test]
    fn empty_graph_from_empty_states() {
        let states: HashMap<String, WorkItemState> = HashMap::new();
        let graph = BlockingGraph::from_states(&states);

        assert!(graph.is_empty());
        assert_eq!(graph.len(), 0);
        assert!(graph.ready_items().is_empty());
        assert!(graph.blocked_items().is_empty());
    }

    #[test]
    fn unblocked_item_is_ready() {
        let mut states = HashMap::new();
        states.insert("bn-1".to_string(), WorkItemState::new());

        let graph = BlockingGraph::from_states(&states);

        assert!(!graph.is_blocked("bn-1"));
        assert!(graph.ready_items().contains("bn-1"));
        assert!(!graph.blocked_items().contains("bn-1"));
    }

    #[test]
    fn blocked_item_is_not_ready() {
        let mut states = HashMap::new();
        states.insert("bn-1".to_string(), state_with_blockers(&["bn-2"]));
        states.insert("bn-2".to_string(), WorkItemState::new());

        let graph = BlockingGraph::from_states(&states);

        assert!(graph.is_blocked("bn-1"));
        assert!(!graph.ready_items().contains("bn-1"));
        assert!(graph.blocked_items().contains("bn-1"));

        // The blocker (bn-2) itself is ready.
        assert!(!graph.is_blocked("bn-2"));
        assert!(graph.ready_items().contains("bn-2"));
    }

    #[test]
    fn multiple_blockers() {
        let mut states = HashMap::new();
        states.insert("bn-1".to_string(), state_with_blockers(&["bn-2", "bn-3"]));
        states.insert("bn-2".to_string(), WorkItemState::new());
        states.insert("bn-3".to_string(), WorkItemState::new());

        let graph = BlockingGraph::from_states(&states);

        assert!(graph.is_blocked("bn-1"));
        let blockers = graph.get_blockers("bn-1");
        assert_eq!(blockers.len(), 2);
        assert!(blockers.contains("bn-2"));
        assert!(blockers.contains("bn-3"));
    }

    #[test]
    fn related_links_do_not_block() {
        let mut states = HashMap::new();
        states.insert("bn-1".to_string(), state_with_related(&["bn-2"]));
        states.insert("bn-2".to_string(), WorkItemState::new());

        let graph = BlockingGraph::from_states(&states);

        // Relates links are informational — bn-1 is NOT blocked.
        assert!(!graph.is_blocked("bn-1"));
        assert!(graph.ready_items().contains("bn-1"));

        // But related link is recorded.
        let related = graph.get_related("bn-1");
        assert!(related.contains("bn-2"));
    }

    // -----------------------------------------------------------------------
    // Link/Unlink via events
    // -----------------------------------------------------------------------

    #[test]
    fn link_then_unlink_removes_blocker() {
        let mut state = WorkItemState::new();
        state.apply_event(&make_link_event(
            "bn-dep",
            "blocks",
            1000,
            "alice",
            "blake3:l1",
        ));
        assert!(!state.blocked_by.is_empty());

        state.apply_event(&make_unlink_event(
            "bn-dep",
            Some("blocks"),
            2000,
            "alice",
            "blake3:ul1",
        ));
        assert!(state.blocked_by.is_empty());

        let mut states = HashMap::new();
        states.insert("bn-task".to_string(), state);

        let graph = BlockingGraph::from_states(&states);
        assert!(!graph.is_blocked("bn-task"));
        assert!(graph.ready_items().contains("bn-task"));
    }

    #[test]
    fn unlink_without_link_is_noop() {
        let mut state = WorkItemState::new();
        state.apply_event(&make_unlink_event(
            "bn-nonexistent",
            Some("blocks"),
            1000,
            "alice",
            "blake3:ul1",
        ));

        let mut states = HashMap::new();
        states.insert("bn-task".to_string(), state);

        let graph = BlockingGraph::from_states(&states);
        assert!(!graph.is_blocked("bn-task"));
    }

    // -----------------------------------------------------------------------
    // OR-Set convergence for concurrent link/unlink
    // -----------------------------------------------------------------------

    #[test]
    fn concurrent_link_and_unlink_add_wins() {
        // Agent A adds a blocking link (tag1).
        let mut state_a = WorkItemState::new();
        state_a.apply_event(&make_link_event(
            "bn-dep",
            "blocks",
            1000,
            "alice",
            "blake3:l1",
        ));

        // Agent B also starts from empty state and removes (sees no tags).
        let state_b = WorkItemState::new();
        // B has no tags to tombstone — so the unlink is effectively empty.

        // Merge A and B — A's add survives because B never saw the tag.
        let mut merged = state_a.clone();
        use crate::crdt::merge::Merge;
        merged.merge(&state_b);

        let mut states = HashMap::new();
        states.insert("bn-task".to_string(), merged);

        let graph = BlockingGraph::from_states(&states);
        assert!(
            graph.is_blocked("bn-task"),
            "add-wins: concurrent add should survive empty remove"
        );
    }

    #[test]
    fn concurrent_add_wins_over_remove() {
        // Base state: bn-task blocks on bn-dep (tag1 present).
        let mut base = WorkItemState::new();
        base.apply_event(&make_link_event(
            "bn-dep",
            "blocks",
            1000,
            "alice",
            "blake3:l1",
        ));

        // Agent A removes the link (tombstones tag1).
        let mut state_a = base.clone();
        state_a.apply_event(&make_unlink_event(
            "bn-dep",
            Some("blocks"),
            2000,
            "alice",
            "blake3:ul1",
        ));
        assert!(state_a.blocked_by.is_empty());

        // Agent B concurrently re-adds the link (tag2 — new tag not seen by A).
        let mut state_b = base.clone();
        state_b.apply_event(&make_link_event(
            "bn-dep",
            "blocks",
            2000,
            "bob",
            "blake3:l2",
        ));

        // Merge: B's tag2 survives A's tombstone of tag1.
        let mut merged = state_a.clone();
        use crate::crdt::merge::Merge;
        merged.merge(&state_b);

        let mut states = HashMap::new();
        states.insert("bn-task".to_string(), merged);

        let graph = BlockingGraph::from_states(&states);
        assert!(
            graph.is_blocked("bn-task"),
            "add-wins: B's concurrent re-add should survive A's remove"
        );
    }

    // -----------------------------------------------------------------------
    // Cross-goal dependencies
    // -----------------------------------------------------------------------

    #[test]
    fn cross_goal_blocking_works() {
        // Item in goal 1 is blocked by item in goal 2.
        let mut states = HashMap::new();
        states.insert(
            "bn-goal1-task".to_string(),
            state_with_blockers(&["bn-goal2-task"]),
        );
        states.insert("bn-goal2-task".to_string(), WorkItemState::new());

        let graph = BlockingGraph::from_states(&states);

        assert!(graph.is_blocked("bn-goal1-task"));
        let blockers = graph.get_blockers("bn-goal1-task");
        assert!(blockers.contains("bn-goal2-task"));

        // Cross-goal blocker has no blockers itself.
        assert!(!graph.is_blocked("bn-goal2-task"));
    }

    #[test]
    fn cross_goal_blocker_not_in_states_still_blocks() {
        // The blocker may not be in the states map (e.g., different shard).
        let mut states = HashMap::new();
        states.insert(
            "bn-task".to_string(),
            state_with_blockers(&["bn-external"]),
        );
        // bn-external is NOT in states.

        let graph = BlockingGraph::from_states(&states);

        // bn-task is still blocked even though bn-external is not in the map.
        assert!(graph.is_blocked("bn-task"));
        assert!(!graph.ready_items().contains("bn-task"));
    }

    // -----------------------------------------------------------------------
    // ready_items correctness
    // -----------------------------------------------------------------------

    #[test]
    fn ready_items_excludes_blocked() {
        let mut states = HashMap::new();
        states.insert("bn-1".to_string(), WorkItemState::new());
        states.insert("bn-2".to_string(), state_with_blockers(&["bn-1"]));
        states.insert("bn-3".to_string(), WorkItemState::new());

        let graph = BlockingGraph::from_states(&states);
        let ready = graph.ready_items();

        assert!(ready.contains("bn-1"));
        assert!(!ready.contains("bn-2"));
        assert!(ready.contains("bn-3"));
        assert_eq!(ready.len(), 2);
    }

    #[test]
    fn chain_blocking_all_after_first_blocked() {
        // bn-1 ← bn-2 ← bn-3 (bn-3 blocked by bn-2, bn-2 blocked by bn-1)
        let mut states = HashMap::new();
        states.insert("bn-1".to_string(), WorkItemState::new());
        states.insert("bn-2".to_string(), state_with_blockers(&["bn-1"]));
        states.insert("bn-3".to_string(), state_with_blockers(&["bn-2"]));

        let graph = BlockingGraph::from_states(&states);
        let ready = graph.ready_items();

        assert!(ready.contains("bn-1"), "bn-1 has no blockers");
        assert!(!ready.contains("bn-2"), "bn-2 blocked by bn-1");
        assert!(!ready.contains("bn-3"), "bn-3 blocked by bn-2");
        assert_eq!(ready.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Standalone function tests
    // -----------------------------------------------------------------------

    #[test]
    fn standalone_is_blocked() {
        let mut states = HashMap::new();
        states.insert("bn-1".to_string(), state_with_blockers(&["bn-2"]));
        states.insert("bn-2".to_string(), WorkItemState::new());

        assert!(is_blocked("bn-1", &states));
        assert!(!is_blocked("bn-2", &states));
        assert!(!is_blocked("bn-unknown", &states));
    }

    #[test]
    fn standalone_get_blockers() {
        let mut states = HashMap::new();
        states.insert("bn-1".to_string(), state_with_blockers(&["bn-2", "bn-3"]));

        let blockers = get_blockers("bn-1", &states);
        assert_eq!(blockers.len(), 2);
        assert!(blockers.contains("bn-2"));
        assert!(blockers.contains("bn-3"));

        // Non-existent item returns empty.
        assert!(get_blockers("bn-unknown", &states).is_empty());
    }

    #[test]
    fn standalone_ready_items() {
        let mut states = HashMap::new();
        states.insert("bn-1".to_string(), WorkItemState::new());
        states.insert("bn-2".to_string(), state_with_blockers(&["bn-1"]));
        states.insert("bn-3".to_string(), WorkItemState::new());

        let ready = ready_items(&states);
        let ready_set: HashSet<_> = ready.iter().map(String::as_str).collect();

        assert!(ready_set.contains("bn-1"));
        assert!(!ready_set.contains("bn-2"));
        assert!(ready_set.contains("bn-3"));
    }

    // -----------------------------------------------------------------------
    // get_blockers for unknown items
    // -----------------------------------------------------------------------

    #[test]
    fn get_blockers_for_unknown_item_returns_empty() {
        let states: HashMap<String, WorkItemState> = HashMap::new();
        let graph = BlockingGraph::from_states(&states);

        assert!(graph.get_blockers("bn-unknown").is_empty());
        assert!(graph.get_related("bn-unknown").is_empty());
        assert!(!graph.is_blocked("bn-unknown"));
    }

    // -----------------------------------------------------------------------
    // Relates and blocking mixed
    // -----------------------------------------------------------------------

    #[test]
    fn item_with_both_blocking_and_relates() {
        let mut state = WorkItemState::new();
        state.apply_event(&make_link_event(
            "bn-blocker",
            "blocks",
            1000,
            "alice",
            "blake3:l1",
        ));
        state.apply_event(&make_link_event(
            "bn-related",
            "related_to",
            1001,
            "alice",
            "blake3:l2",
        ));

        let mut states = HashMap::new();
        states.insert("bn-task".to_string(), state);

        let graph = BlockingGraph::from_states(&states);

        assert!(graph.is_blocked("bn-task"));
        assert!(graph.get_blockers("bn-task").contains("bn-blocker"));
        assert!(graph.get_related("bn-task").contains("bn-related"));
        assert!(!graph.ready_items().contains("bn-task"));
    }

    // -----------------------------------------------------------------------
    // blocked_by type aliases
    // -----------------------------------------------------------------------

    #[test]
    fn blocked_by_type_alias_works() {
        // "blocked_by" is another valid link_type that should register as a blocker
        let mut state = WorkItemState::new();
        state.apply_event(&make_link_event(
            "bn-dep",
            "blocked_by",
            1000,
            "alice",
            "blake3:l1",
        ));

        let mut states = HashMap::new();
        states.insert("bn-task".to_string(), state);

        let graph = BlockingGraph::from_states(&states);
        assert!(graph.is_blocked("bn-task"));
    }

    #[test]
    fn related_type_alias_works() {
        // "related" is another valid link_type for relates links
        let mut state = WorkItemState::new();
        state.apply_event(&make_link_event(
            "bn-dep",
            "related",
            1000,
            "alice",
            "blake3:l1",
        ));

        let mut states = HashMap::new();
        states.insert("bn-task".to_string(), state);

        let graph = BlockingGraph::from_states(&states);
        assert!(!graph.is_blocked("bn-task"));
        assert!(graph.get_related("bn-task").contains("bn-dep"));
    }
}
