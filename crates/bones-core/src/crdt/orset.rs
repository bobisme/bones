//! OR-Set (Observed-Remove Set) with add-wins semantics.
//!
//! This module implements an OR-Set backed by unique tags (timestamps) for
//! each add operation. The set tracks both additions and removals, and uses
//! tag-based causality to resolve concurrent add/remove conflicts.
//!
//! # Add-Wins Semantics
//!
//! When an add and a remove for the same element occur concurrently (neither
//! causally depends on the other), the add wins. This is achieved by:
//!
//! - Each add creates a new unique tag (timestamp).
//! - A remove only tombstones the tags that were observed at the time of removal.
//! - A concurrent add introduces a tag the remove never saw, so it survives.
//!
//! # DAG Integration
//!
//! In bones, OR-Set operations (add/remove) are stored as events in the
//! Eg-Walker DAG. The OR-Set state can be materialized by replaying events
//! from the LCA of divergent branches. See [`materialize_from_replay`] for
//! the replay-based construction.
//!
//! # Semilattice Properties
//!
//! The merge operation satisfies the semilattice laws:
//! - **Commutative**: merge(A, B) = merge(B, A)
//! - **Associative**: merge(merge(A, B), C) = merge(A, merge(B, C))
//! - **Idempotent**: merge(A, A) = A

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use super::merge::Merge;
use super::{OrSet, Timestamp};

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// An operation on an OR-Set element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrSetOp<T> {
    /// Add an element with a unique tag (timestamp).
    Add(T, Timestamp),
    /// Remove an element, tombstoning all currently-observed tags for it.
    Remove(T, Vec<Timestamp>),
}

impl<T: Hash + Eq + Clone> OrSet<T> {
    /// Create a new empty OR-Set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            elements: HashSet::new(),
            tombstone: HashSet::new(),
        }
    }

    /// Add an element to the set with a unique tag.
    ///
    /// The tag (timestamp) must be unique across all operations to ensure
    /// correct causality tracking. In bones, this is guaranteed by the
    /// event hash or ITC stamp.
    pub fn add(&mut self, value: T, tag: Timestamp) {
        self.elements.insert((value, tag));
    }

    /// Remove an element from the set.
    ///
    /// This tombstones all currently-observed tags for the given element.
    /// Any tags added concurrently (not yet observed) will survive,
    /// implementing add-wins semantics.
    ///
    /// Returns the tags that were tombstoned (empty if element was not present).
    pub fn remove(&mut self, value: &T) -> Vec<Timestamp>
    where
        T: Eq + Hash,
    {
        // Find all active tags for this value.
        let active_tags: Vec<Timestamp> = self
            .elements
            .iter()
            .filter(|(v, _)| v == value)
            .map(|(_, ts)| ts.clone())
            .collect();

        // Tombstone each active tag.
        for tag in &active_tags {
            self.tombstone.insert((value.clone(), tag.clone()));
        }

        active_tags
    }

    /// Remove specific tags for an element.
    ///
    /// Tombstones only the provided tags.
    pub fn remove_specific(&mut self, value: &T, tags: &[Timestamp])
    where
        T: Eq + Hash,
    {
        for tag in tags {
            self.tombstone.insert((value.clone(), tag.clone()));
        }
    }

    /// Check if an element is present in the set.
    ///
    /// An element is present if it has at least one add-tag that is not
    /// covered by a corresponding tombstone.
    pub fn contains(&self, value: &T) -> bool {
        self.elements
            .iter()
            .any(|(v, ts)| v == value && !self.tombstone.contains(&(value.clone(), ts.clone())))
    }

    /// Return all currently-present values in the set.
    ///
    /// An element is present if it has at least one un-tombstoned add-tag.
    #[must_use]
    pub fn values(&self) -> HashSet<&T> {
        let mut result = HashSet::new();
        for (value, tag) in &self.elements {
            if !self.tombstone.contains(&(value.clone(), tag.clone())) {
                result.insert(value);
            }
        }
        result
    }

    /// Return the number of distinct present values.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values().len()
    }

    /// Return `true` if no values are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return all active (un-tombstoned) tags for a given value.
    pub fn active_tags(&self, value: &T) -> Vec<&Timestamp> {
        self.elements
            .iter()
            .filter(|(v, ts)| v == value && !self.tombstone.contains(&(value.clone(), ts.clone())))
            .map(|(_, ts)| ts)
            .collect()
    }

    /// Apply an operation to the OR-Set.
    pub fn apply(&mut self, op: OrSetOp<T>) {
        match op {
            OrSetOp::Add(value, tag) => {
                self.add(value, tag);
            }
            OrSetOp::Remove(value, observed_tags) => {
                // Only tombstone the specific tags that were observed.
                for tag in observed_tags {
                    self.tombstone.insert((value.clone(), tag));
                }
            }
        }
    }
}

impl<T: Hash + Eq + Clone> Default for OrSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Merge (semilattice join)
// ---------------------------------------------------------------------------

/// Merge implementation for OR-Set.
///
/// The merge is a union of both the element sets and the tombstone sets.
/// This satisfies semilattice laws because set union is commutative,
/// associative, and idempotent.
///
/// After merge, an element is present iff it has at least one add-tag
/// not covered by a tombstone entry.
impl<T: Eq + Hash + Clone> Merge for OrSet<T> {
    fn merge(&mut self, other: Self) {
        self.elements.extend(other.elements);
        self.tombstone.extend(other.tombstone);
    }
}

// ---------------------------------------------------------------------------
// DAG Replay Materialization
// ---------------------------------------------------------------------------

use crate::dag::graph::EventDag;
use crate::dag::replay::{DivergentReplay, ReplayError, replay_divergent};
use crate::event::Event;
use crate::event::data::{AssignAction, EventData};
use crate::event::types::EventType;

/// An OR-Set field descriptor for DAG-based materialization.
///
/// Identifies which event type and field name should be interpreted as
/// add/remove operations for an OR-Set.
#[derive(Debug, Clone)]
pub enum OrSetField {
    /// Labels: add/remove via item.update with field="labels" and
    /// JSON value encoding the operation.
    Labels,
    /// Assignees: add/remove via item.assign events.
    Assignees,
    /// Blocked-by links: add/remove via item.link/item.unlink events.
    BlockedBy,
    /// Related-to links: add/remove via item.link/item.unlink events.
    RelatedTo,
}

/// Extract OR-Set operations from a sequence of DAG events for a given field.
///
/// This is the bridge between the event DAG and the OR-Set CRDT. Each event
/// that affects the target field is translated into an [`OrSetOp`].
///
/// # Timestamp Construction
///
/// Each event's unique identity (`event_hash` + `wall_ts` + agent) is mapped to
/// a [`Timestamp`] for use as the OR-Set tag.
///
/// # DAG Visibility
///
/// If `dag` is provided, `Remove` operations will only target tags that are
/// causally visible (ancestors) to the removing event. This is crucial for
/// handling concurrent edits in `merged` replays.
///
/// If `base_state` is provided, the set is initialized with it. Tags present
/// in `base_state` are assumed to be visible to all events (ancestors).
pub fn ops_from_events(
    events: &[Event],
    field: &OrSetField,
    base_state: Option<&OrSet<String>>,
    dag: Option<&EventDag>,
) -> Vec<OrSetOp<String>> {
    let mut current_set = base_state.map_or_else(OrSet::new, Clone::clone);
    let mut tag_map: HashMap<Timestamp, String> = HashMap::new();
    let mut ops = Vec::new();

    for event in events {
        let tag = event_to_timestamp(event);
        if let Some(op) = dispatch_event(event, field, &tag, dag, &tag_map, &mut current_set) {
            if let OrSetOp::Add(_, ref t) = op {
                tag_map.insert(t.clone(), event.event_hash.clone());
            }
            ops.push(op);
        }
    }

    ops
}

/// Dispatch a single event to the appropriate field handler.
fn dispatch_event(
    event: &Event,
    field: &OrSetField,
    tag: &Timestamp,
    dag: Option<&EventDag>,
    tag_map: &HashMap<Timestamp, String>,
    current_set: &mut OrSet<String>,
) -> Option<OrSetOp<String>> {
    match field {
        OrSetField::Assignees => handle_assignee(event, tag, dag, tag_map, current_set),
        OrSetField::Labels => handle_label(event, tag, dag, tag_map, current_set),
        OrSetField::BlockedBy => handle_link(
            event,
            tag,
            dag,
            tag_map,
            current_set,
            &["blocks", "blocked_by"],
        ),
        OrSetField::RelatedTo => handle_link(
            event,
            tag,
            dag,
            tag_map,
            current_set,
            &["related_to", "related"],
        ),
    }
}

/// Filter candidate tags for DAG visibility relative to the given event.
fn visible_tags(
    candidates: Vec<&Timestamp>,
    event: &Event,
    dag: Option<&EventDag>,
    tag_map: &HashMap<Timestamp, String>,
) -> Vec<Timestamp> {
    candidates
        .into_iter()
        .filter(|t| {
            dag.is_none_or(|dag_ref| {
                tag_map
                    .get(t)
                    .is_none_or(|tag_hash| dag_ref.is_ancestor(tag_hash, &event.event_hash))
            })
        })
        .cloned()
        .collect()
}

/// Record an add: inserts the tag into the set and returns the op.
fn record_add(value: String, tag: &Timestamp, current_set: &mut OrSet<String>) -> OrSetOp<String> {
    let op = OrSetOp::Add(value.clone(), tag.clone());
    current_set.add(value, tag.clone());
    op
}

/// Record a remove: tombstones visible tags and returns the op.
fn record_remove(
    value: String,
    event: &Event,
    dag: Option<&EventDag>,
    tag_map: &HashMap<Timestamp, String>,
    current_set: &mut OrSet<String>,
) -> OrSetOp<String> {
    let candidate_tags = current_set.active_tags(&value);
    let observed_tags = visible_tags(candidate_tags, event, dag, tag_map);
    current_set.remove_specific(&value, &observed_tags);
    OrSetOp::Remove(value, observed_tags)
}

fn handle_assignee(
    event: &Event,
    tag: &Timestamp,
    dag: Option<&EventDag>,
    tag_map: &HashMap<Timestamp, String>,
    current_set: &mut OrSet<String>,
) -> Option<OrSetOp<String>> {
    if let EventData::Assign(data) = &event.data {
        return match data.action {
            AssignAction::Assign => Some(record_add(data.agent.clone(), tag, current_set)),
            AssignAction::Unassign => Some(record_remove(
                data.agent.clone(),
                event,
                dag,
                tag_map,
                current_set,
            )),
        };
    }
    None
}

fn handle_label(
    event: &Event,
    tag: &Timestamp,
    dag: Option<&EventDag>,
    tag_map: &HashMap<Timestamp, String>,
    current_set: &mut OrSet<String>,
) -> Option<OrSetOp<String>> {
    if event.event_type != EventType::Update {
        return None;
    }
    let EventData::Update(data) = &event.data else {
        return None;
    };
    if data.field != "labels" {
        return None;
    }
    let obj = data.value.as_object()?;
    let action_str = obj.get("action")?.as_str().unwrap_or("");
    let label_str = obj.get("label")?.as_str().unwrap_or("").to_string();

    match action_str {
        "add" => Some(record_add(label_str, tag, current_set)),
        "remove" => Some(record_remove(label_str, event, dag, tag_map, current_set)),
        _ => None,
    }
}

fn handle_link(
    event: &Event,
    tag: &Timestamp,
    dag: Option<&EventDag>,
    tag_map: &HashMap<Timestamp, String>,
    current_set: &mut OrSet<String>,
    link_types: &[&str],
) -> Option<OrSetOp<String>> {
    match event.event_type {
        EventType::Link => {
            if let EventData::Link(data) = &event.data
                && link_types.contains(&data.link_type.as_str())
            {
                return Some(record_add(data.target.clone(), tag, current_set));
            }
        }
        EventType::Unlink => {
            if let EventData::Unlink(data) = &event.data {
                let matches = data
                    .link_type
                    .as_ref()
                    .is_none_or(|lt| link_types.contains(&lt.as_str()));
                if matches {
                    return Some(record_remove(
                        data.target.clone(),
                        event,
                        dag,
                        tag_map,
                        current_set,
                    ));
                }
            }
        }
        _ => {}
    }
    None
}

/// Materialize an OR-Set from a sequence of events.
///
/// Replays the given events in order, applying add/remove operations
/// to build the final OR-Set state.
#[must_use]
pub fn materialize_from_events(events: &[Event], field: &OrSetField) -> OrSet<String> {
    let ops = ops_from_events(events, field, None, None);
    let mut set = OrSet::new();
    for op in ops {
        set.apply(op);
    }
    set
}

/// Materialize an OR-Set by replaying divergent branches from the DAG.
///
/// Given two branch tips, finds their LCA, collects divergent events,
/// and applies them in deterministic order to produce the merged OR-Set.
///
/// The `base_state` is the OR-Set state at the LCA. The divergent events
/// from both branches are replayed on top of it.
///
/// # Add-Wins Resolution
///
/// If branch A adds element X and branch B removes element X concurrently:
/// - Branch B's remove tombstones only the tags B observed (from the LCA state).
/// - Branch A's add creates a new tag that B never saw.
/// - After merge, the new tag survives → element X is present (add wins).
///
/// # Errors
///
/// Returns [`ReplayError`] if the divergent replay fails (e.g., missing
/// tips or malformed DAG).
pub fn materialize_from_replay(
    dag: &EventDag,
    tip_a: &str,
    tip_b: &str,
    base_state: &OrSet<String>,
    field: &OrSetField,
) -> Result<OrSet<String>, ReplayError> {
    let replay = replay_divergent(dag, tip_a, tip_b)?;
    Ok(apply_replay(base_state, &replay, field, Some(dag)))
}

/// Apply a divergent replay to a base OR-Set state.
///
/// The merged events from the replay are applied in deterministic order
/// on top of the base state.
#[must_use]
pub fn apply_replay(
    base_state: &OrSet<String>,
    replay: &DivergentReplay,
    field: &OrSetField,
    dag: Option<&EventDag>,
) -> OrSet<String> {
    // Start from the base state at the LCA.
    let mut result = base_state.clone();

    // Apply all divergent events in merged (deterministic) order.
    // Pass base_state so ops_from_events knows about pre-existing tags.
    // Pass dag so ops_from_events can resolve visibility.
    let ops = ops_from_events(&replay.merged, field, Some(base_state), dag);
    for op in ops {
        result.apply(op);
    }

    result
}

/// Convert an event to a Timestamp for use as an OR-Set tag.
///
/// Uses the event's `wall_ts`, agent (hashed), and `event_hash` (hashed) to
/// produce a unique, deterministic timestamp.
fn event_to_timestamp(event: &Event) -> Timestamp {
    use chrono::TimeZone;

    let epoch_secs = event.wall_ts_us / 1_000_000;
    let subsec_nanos = u32::try_from((event.wall_ts_us % 1_000_000) * 1_000).unwrap_or(0);
    let wall = chrono::Utc.timestamp_opt(epoch_secs, subsec_nanos).unwrap();

    // Hash the agent string to a u64 for the actor field.
    let actor = hash_str_to_u64(&event.agent);

    // Hash the event_hash to a u64 for the event_hash field.
    let event_hash_u64 = hash_str_to_u64(&event.event_hash);

    // Use wall_ts_us as a simple ITC substitute.
    let itc = event.wall_ts_us.cast_unsigned();

    Timestamp {
        wall,
        actor,
        event_hash: event_hash_u64,
        itc,
    }
}

/// Simple string hash to u64 for deterministic tag generation.
fn hash_str_to_u64(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::data::{AssignAction, AssignData, EventData};
    use crate::event::{Event, EventType};
    use chrono::{TimeZone, Utc};
    use std::collections::BTreeMap;

    fn make_tag(wall_secs: i64, actor: u64, event_hash: u64) -> Timestamp {
        Timestamp {
            wall: Utc.timestamp_opt(wall_secs, 0).unwrap(),
            actor,
            event_hash,
            itc: wall_secs as u64,
        }
    }

    // ===================================================================
    // Basic operations
    // ===================================================================

    #[test]
    fn new_orset_is_empty() {
        let set: OrSet<String> = OrSet::new();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        assert!(set.values().is_empty());
    }

    #[test]
    fn add_single_element() {
        let mut set: OrSet<String> = OrSet::new();
        let tag = make_tag(1, 1, 100);
        set.add("alice".into(), tag);

        assert!(set.contains(&"alice".into()));
        assert!(!set.contains(&"bob".into()));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn add_multiple_elements() {
        let mut set: OrSet<String> = OrSet::new();
        set.add("alice".into(), make_tag(1, 1, 100));
        set.add("bob".into(), make_tag(2, 1, 101));
        set.add("charlie".into(), make_tag(3, 1, 102));

        assert_eq!(set.len(), 3);
        assert!(set.contains(&"alice".into()));
        assert!(set.contains(&"bob".into()));
        assert!(set.contains(&"charlie".into()));
    }

    #[test]
    fn add_same_element_twice_with_different_tags() {
        let mut set: OrSet<String> = OrSet::new();
        set.add("alice".into(), make_tag(1, 1, 100));
        set.add("alice".into(), make_tag(2, 2, 101));

        // Still one distinct value, but two tags.
        assert_eq!(set.len(), 1);
        assert!(set.contains(&"alice".into()));
        assert_eq!(set.active_tags(&"alice".into()).len(), 2);
    }

    #[test]
    fn remove_element() {
        let mut set: OrSet<String> = OrSet::new();
        set.add("alice".into(), make_tag(1, 1, 100));
        assert!(set.contains(&"alice".into()));

        let removed = set.remove(&"alice".into());
        assert_eq!(removed.len(), 1);
        assert!(!set.contains(&"alice".into()));
        assert!(set.is_empty());
    }

    #[test]
    fn remove_nonexistent_element() {
        let mut set: OrSet<String> = OrSet::new();
        let removed = set.remove(&"alice".into());
        assert!(removed.is_empty());
        assert!(set.is_empty());
    }

    #[test]
    fn add_remove_add_cycle() {
        let mut set: OrSet<String> = OrSet::new();

        // First add
        set.add("alice".into(), make_tag(1, 1, 100));
        assert!(set.contains(&"alice".into()));

        // Remove
        set.remove(&"alice".into());
        assert!(!set.contains(&"alice".into()));

        // Re-add with new tag
        set.add("alice".into(), make_tag(3, 1, 102));
        assert!(set.contains(&"alice".into()));
        assert_eq!(set.active_tags(&"alice".into()).len(), 1);
    }

    #[test]
    fn multiple_add_remove_cycles() {
        let mut set: OrSet<String> = OrSet::new();

        for i in 0..5 {
            set.add("x".into(), make_tag(i * 2, 1, (i * 2) as u64));
            assert!(set.contains(&"x".into()));
            set.remove(&"x".into());
            assert!(!set.contains(&"x".into()));
        }

        // Final add
        set.add("x".into(), make_tag(100, 1, 999));
        assert!(set.contains(&"x".into()));
        assert_eq!(set.len(), 1);
    }

    // ===================================================================
    // Concurrent add+remove (add-wins)
    // ===================================================================

    #[test]
    fn concurrent_add_remove_add_wins() {
        // Simulate: base has element "x". Agent A removes "x", Agent B
        // concurrently adds "x" (with a new tag). After merge, "x" should
        // be present (add-wins).

        // Base state: "x" with tag1
        let tag1 = make_tag(1, 1, 100);
        let mut base: OrSet<String> = OrSet::new();
        base.add("x".into(), tag1.clone());

        // Agent A: removes "x" (observes tag1)
        let mut agent_a = base.clone();
        agent_a.remove(&"x".into());
        assert!(!agent_a.contains(&"x".into()));

        // Agent B: adds "x" with new tag (concurrent, doesn't see remove)
        let mut agent_b = base.clone();
        let tag2 = make_tag(2, 2, 200);
        agent_b.add("x".into(), tag2.clone());

        // Merge A into B
        let mut merged_ab = agent_a.clone();
        merged_ab.merge(agent_b.clone());
        assert!(
            merged_ab.contains(&"x".into()),
            "add-wins: concurrent add should survive remove"
        );

        // Merge B into A (commutativity check)
        let mut merged_ba = agent_b.clone();
        merged_ba.merge(agent_a.clone());
        assert!(
            merged_ba.contains(&"x".into()),
            "add-wins: merge must be commutative"
        );
    }

    #[test]
    fn concurrent_adds_both_present() {
        // Two agents concurrently add "x" with different tags.
        let mut agent_a: OrSet<String> = OrSet::new();
        agent_a.add("x".into(), make_tag(1, 1, 100));

        let mut agent_b: OrSet<String> = OrSet::new();
        agent_b.add("x".into(), make_tag(1, 2, 200));

        let mut merged = agent_a.clone();
        merged.merge(agent_b);

        assert!(merged.contains(&"x".into()));
        // Should have 2 tags for "x"
        assert_eq!(merged.active_tags(&"x".into()).len(), 2);
    }

    #[test]
    fn causal_remove_after_add_element_absent() {
        // Agent A adds "x", then causally removes it. Result: absent.
        let mut set: OrSet<String> = OrSet::new();
        set.add("x".into(), make_tag(1, 1, 100));
        set.remove(&"x".into());

        assert!(!set.contains(&"x".into()));
    }

    // ===================================================================
    // Merge semilattice properties
    // ===================================================================

    #[test]
    fn merge_commutative() {
        let mut a: OrSet<u32> = OrSet::new();
        a.add(1, make_tag(1, 1, 100));
        a.add(2, make_tag(2, 1, 101));

        let mut b: OrSet<u32> = OrSet::new();
        b.add(2, make_tag(3, 2, 200));
        b.add(3, make_tag(4, 2, 201));

        let mut ab = a.clone();
        ab.merge(b.clone());

        let mut ba = b.clone();
        ba.merge(a.clone());

        assert_eq!(ab, ba);
    }

    #[test]
    fn merge_associative() {
        let mut a: OrSet<u32> = OrSet::new();
        a.add(1, make_tag(1, 1, 100));

        let mut b: OrSet<u32> = OrSet::new();
        b.add(2, make_tag(2, 2, 200));

        let mut c: OrSet<u32> = OrSet::new();
        c.add(3, make_tag(3, 3, 300));

        // (a ⊔ b) ⊔ c
        let mut ab_c = a.clone();
        ab_c.merge(b.clone());
        ab_c.merge(c.clone());

        // a ⊔ (b ⊔ c)
        let mut bc = b.clone();
        bc.merge(c.clone());
        let mut a_bc = a.clone();
        a_bc.merge(bc);

        assert_eq!(ab_c, a_bc);
    }

    #[test]
    fn merge_idempotent() {
        let mut a: OrSet<u32> = OrSet::new();
        a.add(1, make_tag(1, 1, 100));
        a.add(2, make_tag(2, 1, 101));
        a.remove(&1);

        let before = a.clone();
        a.merge(before.clone());
        assert_eq!(a, before);
    }

    #[test]
    fn merge_empty_sets() {
        let a: OrSet<String> = OrSet::new();
        let b: OrSet<String> = OrSet::new();

        let mut merged = a.clone();
        merged.merge(b);

        assert!(merged.is_empty());
    }

    #[test]
    fn merge_with_empty_is_identity() {
        let mut a: OrSet<u32> = OrSet::new();
        a.add(1, make_tag(1, 1, 100));
        a.add(2, make_tag(2, 1, 101));

        let before = a.clone();
        a.merge(OrSet::new());
        assert_eq!(a, before);
    }

    // ===================================================================
    // Complex scenarios
    // ===================================================================

    #[test]
    fn three_way_concurrent_add_remove() {
        // Base: {x(tag1)}
        let tag1 = make_tag(1, 0, 1);
        let mut base: OrSet<String> = OrSet::new();
        base.add("x".into(), tag1.clone());

        // Agent A: removes x
        let mut a = base.clone();
        a.remove(&"x".into());

        // Agent B: adds x with new tag
        let mut b = base.clone();
        b.add("x".into(), make_tag(2, 2, 200));

        // Agent C: removes x
        let mut c = base.clone();
        c.remove(&"x".into());

        // Merge all three
        let mut result = a.clone();
        result.merge(b.clone());
        result.merge(c.clone());

        // Agent B's add should survive both removes
        assert!(
            result.contains(&"x".into()),
            "B's concurrent add should win over A and C's removes"
        );
    }

    #[test]
    fn remove_then_concurrent_re_adds() {
        // Base: {x(tag1)}
        let tag1 = make_tag(1, 0, 1);
        let mut base: OrSet<String> = OrSet::new();
        base.add("x".into(), tag1.clone());

        // Agent A: removes x, then adds x with new tag
        let mut a = base.clone();
        a.remove(&"x".into());
        a.add("x".into(), make_tag(3, 1, 300));

        // Agent B: also removes x, then adds x with different new tag
        let mut b = base.clone();
        b.remove(&"x".into());
        b.add("x".into(), make_tag(4, 2, 400));

        let mut merged = a.clone();
        merged.merge(b.clone());

        // Both new adds should survive
        assert!(merged.contains(&"x".into()));
        assert_eq!(merged.active_tags(&"x".into()).len(), 2);
    }

    #[test]
    fn mixed_elements_concurrent_ops() {
        // Base: {a(t1), b(t2)}
        let mut base: OrSet<String> = OrSet::new();
        base.add("a".into(), make_tag(1, 0, 1));
        base.add("b".into(), make_tag(2, 0, 2));

        // Agent 1: remove a, add c
        let mut s1 = base.clone();
        s1.remove(&"a".into());
        s1.add("c".into(), make_tag(3, 1, 100));

        // Agent 2: remove b, add d
        let mut s2 = base.clone();
        s2.remove(&"b".into());
        s2.add("d".into(), make_tag(4, 2, 200));

        let mut merged = s1.clone();
        merged.merge(s2);

        // a removed by agent 1 only (agent 2 didn't remove it,
        // but agent 2 kept original tag which IS tombstoned by agent 1)
        // Wait - agent 2 starts from base which has a(t1). Agent 2
        // doesn't touch a, so a(t1) stays in agent 2's elements.
        // Agent 1 tombstones a(t1). After merge, a(t1) is tombstoned.
        // So a is NOT present.
        assert!(!merged.contains(&"a".into()));

        // b removed by agent 2 only. Agent 1 keeps b(t2). Agent 2
        // tombstones b(t2). After merge, b(t2) is tombstoned.
        assert!(!merged.contains(&"b".into()));

        // c added by agent 1, d added by agent 2
        assert!(merged.contains(&"c".into()));
        assert!(merged.contains(&"d".into()));
    }

    // ===================================================================
    // Apply operations
    // ===================================================================

    #[test]
    fn apply_add_op() {
        let mut set: OrSet<String> = OrSet::new();
        let tag = make_tag(1, 1, 100);
        set.apply(OrSetOp::Add("alice".into(), tag));

        assert!(set.contains(&"alice".into()));
    }

    #[test]
    fn apply_remove_op_with_observed_tags() {
        let mut set: OrSet<String> = OrSet::new();
        let tag = make_tag(1, 1, 100);
        set.add("alice".into(), tag.clone());

        // Remove by specifying observed tags
        set.apply(OrSetOp::Remove("alice".into(), vec![tag]));
        assert!(!set.contains(&"alice".into()));
    }

    #[test]
    fn apply_remove_with_unobserved_tag_survives() {
        let mut set: OrSet<String> = OrSet::new();
        let tag1 = make_tag(1, 1, 100);
        let tag2 = make_tag(2, 2, 200);
        set.add("alice".into(), tag1.clone());
        set.add("alice".into(), tag2.clone());

        // Remove only observing tag1
        set.apply(OrSetOp::Remove("alice".into(), vec![tag1]));

        // tag2 survives
        assert!(set.contains(&"alice".into()));
        assert_eq!(set.active_tags(&"alice".into()).len(), 1);
    }

    // ===================================================================
    // Values method
    // ===================================================================

    #[test]
    fn values_returns_distinct_present_elements() {
        let mut set: OrSet<String> = OrSet::new();
        set.add("a".into(), make_tag(1, 1, 100));
        set.add("b".into(), make_tag(2, 1, 101));
        set.add("c".into(), make_tag(3, 1, 102));
        set.remove(&"b".into());

        let vals = set.values();
        assert_eq!(vals.len(), 2);
        assert!(vals.contains(&"a".to_string()));
        assert!(vals.contains(&"c".to_string()));
        assert!(!vals.contains(&"b".to_string()));
    }

    // ===================================================================
    // Reproduction Tests for Linearization Bugs
    // ===================================================================

    #[test]
    fn remove_base_state_item_succeeds() {
        // Scenario: Item added in base state, then removed in a divergent event.
        // With base_state passed to ops_from_events, this should now work.

        let base_tag = make_tag(1, 1, 100);
        let mut base_state: OrSet<String> = OrSet::new();
        base_state.add("alice".into(), base_tag.clone());

        // Event: Unassign alice (Unlink/Remove)
        let event = Event {
            wall_ts_us: 2000,
            agent: "agent".into(),
            itc: "itc".into(),
            parents: vec![],
            event_type: EventType::Assign,
            item_id: crate::model::item_id::ItemId::new_unchecked("bn-test"),
            data: EventData::Assign(AssignData {
                agent: "alice".into(),
                action: AssignAction::Unassign,
                extra: BTreeMap::new(),
            }),
            event_hash: "hash".into(),
        };

        // Pass base_state so it knows what to remove.
        let ops = ops_from_events(&[event], &OrSetField::Assignees, Some(&base_state), None);

        // Apply ops to base state
        let mut result = base_state.clone();
        for op in ops {
            result.apply(op);
        }

        // Alice should be removed
        assert!(
            !result.contains(&"alice".into()),
            "Alice should be removed when base_state is provided"
        );
    }

    #[test]
    fn concurrent_remove_respects_dag_visibility() {
        // Scenario: Concurrent Add(A) and Remove(A).
        // Remove(A) does NOT see Add(A).
        // Result: A should remain present (Add wins).

        let root_hash = "root";
        let add_hash = "add_hash";
        let remove_hash = "remove_hash";

        // DAG: root -> add, root -> remove
        let mut dag = EventDag::new();
        // We mock the DAG structure without full events for the DAG check
        // insert() requires full Event, so we construct minimal events.

        let make_evt = |hash: &str, parents: Vec<&str>| Event {
            wall_ts_us: 0,
            agent: "a".into(),
            itc: "i".into(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            event_type: EventType::Create, // dummy
            item_id: crate::model::item_id::ItemId::new_unchecked("bn"),
            data: EventData::Create(crate::event::data::CreateData {
                title: "".into(),
                kind: crate::model::item::Kind::Task,
                size: None,
                urgency: crate::model::item::Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: hash.into(),
        };

        dag.insert(make_evt(root_hash, vec![]));
        dag.insert(make_evt(add_hash, vec![root_hash]));
        dag.insert(make_evt(remove_hash, vec![root_hash]));

        // Events for ops_from_events
        // 1. Add "alice"
        let add_event = Event {
            wall_ts_us: 1000,
            agent: "alice".into(),
            itc: "1".into(),
            parents: vec![root_hash.into()],
            event_type: EventType::Assign,
            item_id: crate::model::item_id::ItemId::new_unchecked("bn"),
            data: EventData::Assign(AssignData {
                agent: "alice".into(),
                action: AssignAction::Assign,
                extra: BTreeMap::new(),
            }),
            event_hash: add_hash.into(),
        };

        // 2. Remove "alice" (concurrent)
        let remove_event = Event {
            wall_ts_us: 2000,
            agent: "bob".into(),
            itc: "2".into(),
            parents: vec![root_hash.into()],
            event_type: EventType::Assign,
            item_id: crate::model::item_id::ItemId::new_unchecked("bn"),
            data: EventData::Assign(AssignData {
                agent: "alice".into(),
                action: AssignAction::Unassign,
                extra: BTreeMap::new(),
            }),
            event_hash: remove_hash.into(),
        };

        // Replay order: Add then Remove (linearized)
        let events = vec![add_event, remove_event];

        let ops = ops_from_events(
            &events,
            &OrSetField::Assignees,
            None, // empty base
            Some(&dag),
        );

        let mut set = OrSet::new();
        for op in ops {
            set.apply(op);
        }

        // Alice should be present because Remove didn't see Add
        assert!(
            set.contains(&"alice".into()),
            "Concurrent Add should survive Remove"
        );
    }
}
