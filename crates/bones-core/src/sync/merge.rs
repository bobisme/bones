//! Union-merge logic for bones event shard files.
//!
//! This module implements the core merge operation used by both the git merge
//! driver (`bn merge-driver`) and any other sync path that needs to combine
//! two sets of TSJSON events into a single canonical, deduplicated, ordered
//! sequence.
//!
//! # Merge Semantics
//!
//! Bones `.events` files are **append-only CRDT logs**. A merge is always
//! safe: take the union of both sides, deduplicate by content hash, and sort
//! deterministically. No event is ever lost; no event appears twice.
//!
//! ## Sort Order
//!
//! Events are ordered by `(wall_ts_us, agent, event_hash)` — all ascending.
//! This order is:
//!
//! - **Deterministic**: given the same set of events, the output is always
//!   identical regardless of which replica produced it.
//! - **Causal-ish**: wall-clock timestamps order events that happened at
//!   different times; the agent + hash tiebreakers make concurrent events
//!   stable.
//!
//! Note that wall-clock timestamps can drift across agents. For strict causal
//! ordering, callers should use the ITC stamps; the sort here is only for
//! canonical file ordering.

use std::collections::HashSet;

use crate::event::Event;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The result of merging two event sets.
///
/// Contains the merged, deduplicated, deterministically sorted events.
#[derive(Debug, Clone)]
pub struct MergeResult {
    /// Merged events, sorted by `(wall_ts_us, agent, event_hash)`.
    pub events: Vec<Event>,
    /// Number of unique events present on `remote` but not in `local`.
    pub new_local: usize,
    /// Number of unique events present on `local` but not in `remote`.
    pub new_remote: usize,
    /// Number of duplicate input events skipped during deduplication.
    pub duplicates_skipped: usize,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Merge two sets of events using union-merge (CRDT join) semantics.
///
/// Takes all events from `local` and `remote`, deduplicates them by
/// `event_hash`, and returns a deterministically sorted [`MergeResult`].
///
/// # Arguments
///
/// * `local`  — events from the local replica (e.g. the "ours" side).
/// * `remote` — events from the remote replica (e.g. the "theirs" side).
///
/// # Returns
///
/// A [`MergeResult`] whose `events` are sorted by `(wall_ts_us, agent,
/// event_hash)`. All events from both sides are included; no event from
/// either side is dropped.
///
/// # Examples
///
/// ```
/// use bones_core::sync::merge::merge_event_sets;
/// use bones_core::event::Event;
///
/// let merged = merge_event_sets(&[], &[]);
/// assert!(merged.events.is_empty());
/// ```
#[must_use]
pub fn merge_event_sets(local: &[Event], remote: &[Event]) -> MergeResult {
    let local_hashes: HashSet<&str> = local.iter().map(|event| event.event_hash.as_str()).collect();
    let remote_hashes: HashSet<&str> = remote
        .iter()
        .map(|event| event.event_hash.as_str())
        .collect();

    let new_local = remote_hashes.difference(&local_hashes).count();
    let new_remote = local_hashes.difference(&remote_hashes).count();

    let mut seen: HashSet<String> = HashSet::with_capacity(local.len() + remote.len());
    let mut events: Vec<Event> = Vec::with_capacity(local.len() + remote.len());

    for event in local.iter().chain(remote.iter()) {
        if seen.insert(event.event_hash.clone()) {
            events.push(event.clone());
        }
    }

    let duplicates_skipped = local.len() + remote.len() - events.len();

    // Sort by (wall_ts_us, agent, event_hash) — deterministic, stable across replicas.
    events.sort_by(|a, b| {
        a.wall_ts_us
            .cmp(&b.wall_ts_us)
            .then_with(|| a.agent.cmp(&b.agent))
            .then_with(|| a.event_hash.cmp(&b.event_hash))
    });

    MergeResult {
        events,
        new_local,
        new_remote,
        duplicates_skipped,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{
        Event, EventData, EventType,
        data::{CommentData, CreateData, MoveData},
    };
    use crate::model::item::*;
    use crate::model::item_id::ItemId;
    use std::collections::BTreeMap;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn make_event(wall_ts_us: i64, agent: &str, hash_suffix: &str) -> Event {
        Event {
            wall_ts_us,
            agent: agent.to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Comment,
            item_id: ItemId::new_unchecked("bn-a7x"),
            data: EventData::Comment(CommentData {
                body: format!("Event {hash_suffix}"),
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:{hash_suffix}"),
        }
    }

    fn make_create_event(wall_ts_us: i64, agent: &str, hash_suffix: &str) -> Event {
        Event {
            wall_ts_us,
            agent: agent.to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-a7x"),
            data: EventData::Create(CreateData {
                title: "Test item".to_string(),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:{hash_suffix}"),
        }
    }

    fn make_move_event(wall_ts_us: i64, agent: &str, hash_suffix: &str) -> Event {
        Event {
            wall_ts_us,
            agent: agent.to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Move,
            item_id: ItemId::new_unchecked("bn-a7x"),
            data: EventData::Move(MoveData {
                state: State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:{hash_suffix}"),
        }
    }

    // -----------------------------------------------------------------------
    // Basic cases
    // -----------------------------------------------------------------------

    #[test]
    fn merge_both_empty() {
        let result = merge_event_sets(&[], &[]);
        assert!(result.events.is_empty());
        assert_eq!(result.new_local, 0);
        assert_eq!(result.new_remote, 0);
        assert_eq!(result.duplicates_skipped, 0);
    }

    #[test]
    fn merge_local_only() {
        let local = vec![
            make_event(1000, "alice", "aaa"),
            make_event(2000, "alice", "bbb"),
        ];
        let result = merge_event_sets(&local, &[]);
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].event_hash, "blake3:aaa");
        assert_eq!(result.events[1].event_hash, "blake3:bbb");
    }

    #[test]
    fn merge_remote_only() {
        let remote = vec![
            make_event(1000, "bob", "ccc"),
            make_event(2000, "bob", "ddd"),
        ];
        let result = merge_event_sets(&[], &remote);
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].event_hash, "blake3:ccc");
        assert_eq!(result.events[1].event_hash, "blake3:ddd");
    }

    #[test]
    fn merge_disjoint_sets() {
        let local = vec![make_event(1000, "alice", "aaa")];
        let remote = vec![make_event(2000, "bob", "bbb")];
        let result = merge_event_sets(&local, &remote);
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.new_local, 1);
        assert_eq!(result.new_remote, 1);
        assert_eq!(result.duplicates_skipped, 0);
    }

    // -----------------------------------------------------------------------
    // Deduplication
    // -----------------------------------------------------------------------

    #[test]
    fn dedup_identical_events_in_both_sides() {
        let e = make_event(1000, "alice", "aaa");
        let local = vec![e.clone()];
        let remote = vec![e];
        let result = merge_event_sets(&local, &remote);
        assert_eq!(result.events.len(), 1, "duplicate should be removed");
        assert_eq!(result.new_local, 0);
        assert_eq!(result.new_remote, 0);
        assert_eq!(result.duplicates_skipped, 1);
    }

    #[test]
    fn dedup_multiple_shared_events() {
        let e1 = make_event(1000, "alice", "aaa");
        let e2 = make_event(2000, "alice", "bbb");
        let e3 = make_event(3000, "bob", "ccc"); // only in remote
        let local = vec![e1.clone(), e2.clone()];
        let remote = vec![e1, e2, e3];
        let result = merge_event_sets(&local, &remote);
        assert_eq!(
            result.events.len(),
            3,
            "shared events deduped, remote-only kept"
        );
    }

    #[test]
    fn dedup_same_hash_different_position() {
        // If two events happen to have the same hash (content-identical),
        // only one should appear regardless of which side they came from.
        let e = make_event(5000, "agent", "zzz");
        let local = vec![make_event(1000, "alice", "aaa"), e.clone()];
        let remote = vec![e, make_event(9000, "bob", "bbb")];
        let result = merge_event_sets(&local, &remote);
        assert_eq!(result.events.len(), 3);
        // Should contain aaa, zzz, bbb — zzz only once
        let hashes: Vec<&str> = result
            .events
            .iter()
            .map(|e| e.event_hash.as_str())
            .collect();
        let zzz_count = hashes.iter().filter(|&&h| h == "blake3:zzz").count();
        assert_eq!(zzz_count, 1, "zzz event should appear exactly once");
    }

    // -----------------------------------------------------------------------
    // Sort order
    // -----------------------------------------------------------------------

    #[test]
    fn sorted_by_wall_ts_ascending() {
        let local = vec![
            make_event(3000, "alice", "ccc"),
            make_event(1000, "alice", "aaa"),
        ];
        let remote = vec![make_event(2000, "alice", "bbb")];
        let result = merge_event_sets(&local, &remote);
        assert_eq!(result.events.len(), 3);
        assert_eq!(result.events[0].wall_ts_us, 1000);
        assert_eq!(result.events[1].wall_ts_us, 2000);
        assert_eq!(result.events[2].wall_ts_us, 3000);
    }

    #[test]
    fn same_timestamp_sorted_by_agent() {
        let local = vec![make_event(1000, "charlie", "ccc")];
        let remote = vec![
            make_event(1000, "alice", "aaa"),
            make_event(1000, "bob", "bbb"),
        ];
        let result = merge_event_sets(&local, &remote);
        assert_eq!(result.events.len(), 3);
        assert_eq!(result.events[0].agent, "alice");
        assert_eq!(result.events[1].agent, "bob");
        assert_eq!(result.events[2].agent, "charlie");
    }

    #[test]
    fn same_timestamp_same_agent_sorted_by_hash() {
        // Two events from same agent at same time — sorted by hash
        let e1 = make_event(1000, "alice", "bbb");
        let e2 = make_event(1000, "alice", "aaa");
        let result = merge_event_sets(&[e1], &[e2]);
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].event_hash, "blake3:aaa");
        assert_eq!(result.events[1].event_hash, "blake3:bbb");
    }

    #[test]
    fn deterministic_output_regardless_of_input_order() {
        let e1 = make_event(1000, "alice", "aaa");
        let e2 = make_event(2000, "bob", "bbb");
        let e3 = make_event(3000, "carol", "ccc");

        // Call with different orderings
        let r1 = merge_event_sets(&[e1.clone(), e2.clone()], &[e3.clone()]);
        let r2 = merge_event_sets(&[e3.clone(), e1.clone()], &[e2.clone()]);
        let r3 = merge_event_sets(&[e2.clone(), e3.clone()], &[e1.clone()]);

        let hashes1: Vec<&str> = r1.events.iter().map(|e| e.event_hash.as_str()).collect();
        let hashes2: Vec<&str> = r2.events.iter().map(|e| e.event_hash.as_str()).collect();
        let hashes3: Vec<&str> = r3.events.iter().map(|e| e.event_hash.as_str()).collect();

        assert_eq!(hashes1, hashes2, "output order must be deterministic");
        assert_eq!(hashes2, hashes3, "output order must be deterministic");
    }

    // -----------------------------------------------------------------------
    // Realistic scenarios
    // -----------------------------------------------------------------------

    #[test]
    fn divergent_branches_with_shared_base() {
        // Both branches share a common ancestor (create event),
        // then each appends different events.
        let create = make_create_event(1000, "alice", "base");

        // Local branch: create → move
        let local_move = make_move_event(2000, "alice", "local-move");
        let local = vec![create.clone(), local_move.clone()];

        // Remote branch: create → comment
        let remote_comment = make_event(2500, "bob", "remote-comment");
        let remote = vec![create, remote_comment.clone()];

        let result = merge_event_sets(&local, &remote);
        assert_eq!(result.events.len(), 3, "base + local move + remote comment");

        // base comes first (lowest timestamp)
        assert_eq!(result.events[0].event_hash, "blake3:base");
        // Then local-move (ts 2000), then remote-comment (ts 2500)
        assert_eq!(result.events[1].event_hash, "blake3:local-move");
        assert_eq!(result.events[2].event_hash, "blake3:remote-comment");
    }

    #[test]
    fn concurrent_events_at_same_timestamp() {
        // Simulates two agents writing events at the same wall-clock second
        let local = vec![make_event(1_000_000, "alice", "alice-event")];
        let remote = vec![make_event(1_000_000, "bob", "bob-event")];

        let result = merge_event_sets(&local, &remote);
        assert_eq!(result.events.len(), 2);
        // alice < bob alphabetically
        assert_eq!(result.events[0].agent, "alice");
        assert_eq!(result.events[1].agent, "bob");
    }

    #[test]
    fn large_symmetric_merge_is_deterministic() {
        // Create events for both sides and verify merge is consistent
        let side_a: Vec<Event> = (0..50)
            .map(|i| make_event(i * 1000, "agent-a", &format!("{i:06}a")))
            .collect();
        let side_b: Vec<Event> = (0..50)
            .map(|i| make_event(i * 1000 + 500, "agent-b", &format!("{i:06}b")))
            .collect();

        let r1 = merge_event_sets(&side_a, &side_b);
        let r2 = merge_event_sets(&side_b, &side_a);

        let h1: Vec<&str> = r1.events.iter().map(|e| e.event_hash.as_str()).collect();
        let h2: Vec<&str> = r2.events.iter().map(|e| e.event_hash.as_str()).collect();

        assert_eq!(r1.events.len(), 100, "50 + 50 events, no duplicates");
        assert_eq!(
            h1, h2,
            "merge must be commutative (same output regardless of which is local/remote)"
        );
    }

    #[test]
    fn idempotent_merge() {
        // Merging a result with itself or with one of its inputs should be stable
        let local = vec![
            make_event(1000, "alice", "aaa"),
            make_event(2000, "alice", "bbb"),
        ];
        let remote = vec![make_event(1500, "bob", "ccc")];

        let r1 = merge_event_sets(&local, &remote);
        // Merge the result with local (simulate re-merge after partial sync)
        let r2 = merge_event_sets(&r1.events, &local);

        // r2 should equal r1 — no new events introduced
        let h1: Vec<&str> = r1.events.iter().map(|e| e.event_hash.as_str()).collect();
        let h2: Vec<&str> = r2.events.iter().map(|e| e.event_hash.as_str()).collect();
        assert_eq!(h1, h2, "merge should be idempotent");
    }
}
