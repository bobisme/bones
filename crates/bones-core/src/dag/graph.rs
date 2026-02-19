//! In-memory DAG structure with parent/descendant traversal.
//!
//! The [`EventDag`] indexes events by their content-addressed hash for efficient
//! parent lookup, descendant traversal, and topological iteration.
//!
//! # Construction
//!
//! The DAG is built incrementally via [`EventDag::insert`]. Events can be
//! inserted in any order; parent/child links are resolved lazily as events
//! arrive. This supports both full replay and incremental appending.
//!
//! # Deduplication
//!
//! Duplicate events (same hash) are silently skipped on insert. This is
//! inherent to content-addressed design and supports scenarios like
//! git merges that bring both sides of a branch.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::event::Event;

// ---------------------------------------------------------------------------
// DagNode
// ---------------------------------------------------------------------------

/// A node in the event DAG, storing the event and its bidirectional links.
#[derive(Debug, Clone)]
pub struct DagNode {
    /// The event stored at this node.
    pub event: Event,
    /// Hashes of this node's parent events (events that causally precede it).
    pub parents: Vec<String>,
    /// Hashes of this node's child events (events that causally follow it).
    pub children: Vec<String>,
}

// ---------------------------------------------------------------------------
// EventDag
// ---------------------------------------------------------------------------

/// An in-memory DAG of events, indexed by content-addressed hash.
///
/// Provides O(1) insertion, O(1) node lookup, and efficient traversal
/// in both causal directions (ancestors and descendants).
#[derive(Debug, Clone)]
pub struct EventDag {
    /// All nodes, keyed by event hash.
    nodes: HashMap<String, DagNode>,
}

impl EventDag {
    /// Create an empty DAG.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
        }
    }

    /// Insert an event into the DAG.
    ///
    /// - Registers the event as a node with its declared parents.
    /// - Creates bidirectional parent→child links for any parents already in the DAG.
    /// - If a later-inserted parent references this event, the link is created then.
    /// - Duplicate events (same `event_hash`) are silently skipped.
    ///
    /// Runs in O(P) where P is the number of parents for this event.
    pub fn insert(&mut self, event: Event) {
        let hash = event.event_hash.clone();

        // Deduplicate: skip if already present.
        if self.nodes.contains_key(&hash) {
            return;
        }

        let parents = event.parents.clone();

        // Insert the new node.
        self.nodes.insert(
            hash.clone(),
            DagNode {
                event,
                parents: parents.clone(),
                children: Vec::new(),
            },
        );

        // Link parents → this child.
        for parent_hash in &parents {
            if let Some(parent_node) = self.nodes.get_mut(parent_hash) {
                parent_node.children.push(hash.clone());
            }
        }

        // Link this node as child of any already-inserted events that
        // we haven't linked yet. This handles out-of-order insertion:
        // if an event was inserted before its children arrived, we need
        // to update the child's children list when the child is finally
        // found as a parent.
        //
        // Actually, the above parent→child link already handles the
        // forward direction. For out-of-order, we also need to check
        // if any existing node lists `hash` as a parent but hasn't
        // been linked yet.
        //
        // Re-scan: for any node that has `hash` in its parents list,
        // add it as a child of the newly inserted node.
        // This is O(N) in the worst case but only needed for out-of-order.
        // We optimize by doing a single pass only for new inserts.
        let children_to_add: Vec<String> = self
            .nodes
            .iter()
            .filter(|(k, _)| *k != &hash)
            .filter(|(_, node)| node.parents.contains(&hash))
            .map(|(k, _)| k.clone())
            .collect();

        if let Some(node) = self.nodes.get_mut(&hash) {
            for child_hash in children_to_add {
                if !node.children.contains(&child_hash) {
                    node.children.push(child_hash);
                }
            }
        }
    }

    /// Build a DAG from a slice of events.
    ///
    /// Events are inserted in the order given. For replay from an event log,
    /// this is typically chronological order.
    #[must_use]
    pub fn from_events(events: &[Event]) -> Self {
        let mut dag = Self::with_capacity(events.len());
        for event in events {
            dag.insert(event.clone());
        }
        dag
    }

    /// Create an empty DAG with pre-allocated capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            nodes: HashMap::with_capacity(capacity),
        }
    }

    /// Number of events in the DAG.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns `true` if the DAG has no events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Look up a node by its event hash.
    #[must_use]
    pub fn get(&self, hash: &str) -> Option<&DagNode> {
        self.nodes.get(hash)
    }

    /// Return the event for a given hash.
    #[must_use]
    pub fn get_event(&self, hash: &str) -> Option<&Event> {
        self.nodes.get(hash).map(|n| &n.event)
    }

    /// Returns `true` if the DAG contains an event with the given hash.
    #[must_use]
    pub fn contains(&self, hash: &str) -> bool {
        self.nodes.contains_key(hash)
    }

    /// Return the hashes of all root events (events with no parents in the DAG).
    ///
    /// Multiple roots occur when agents create items concurrently without
    /// seeing each other's genesis events.
    #[must_use]
    pub fn roots(&self) -> Vec<&str> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.parents.is_empty())
            .map(|(hash, _)| hash.as_str())
            .collect()
    }

    /// Return the hashes of all tip events (events with no children).
    ///
    /// Tips are the "current heads" of the DAG — events that no other
    /// event has yet referenced as a parent.
    #[must_use]
    pub fn tips(&self) -> Vec<&str> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.children.is_empty())
            .map(|(hash, _)| hash.as_str())
            .collect()
    }

    /// Get all ancestor hashes of the given event (transitive parents).
    ///
    /// Performs a BFS walk up the parent chain. Returns an empty set
    /// for root events. Does NOT include the starting event itself.
    #[must_use]
    pub fn ancestors(&self, hash: &str) -> HashSet<String> {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();

        if let Some(node) = self.nodes.get(hash) {
            for parent in &node.parents {
                if visited.insert(parent.clone()) {
                    queue.push_back(parent.clone());
                }
            }
        }

        while let Some(current) = queue.pop_front() {
            if let Some(node) = self.nodes.get(&current) {
                for parent in &node.parents {
                    if visited.insert(parent.clone()) {
                        queue.push_back(parent.clone());
                    }
                }
            }
        }

        visited
    }

    /// Get all descendant hashes of the given event (transitive children).
    ///
    /// Performs a BFS walk down the children chain. Returns an empty set
    /// for tip events. Does NOT include the starting event itself.
    #[must_use]
    pub fn descendants(&self, hash: &str) -> HashSet<String> {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();

        if let Some(node) = self.nodes.get(hash) {
            for child in &node.children {
                if visited.insert(child.clone()) {
                    queue.push_back(child.clone());
                }
            }
        }

        while let Some(current) = queue.pop_front() {
            if let Some(node) = self.nodes.get(&current) {
                for child in &node.children {
                    if visited.insert(child.clone()) {
                        queue.push_back(child.clone());
                    }
                }
            }
        }

        visited
    }

    /// Iterate events in topological (causal) order via Kahn's algorithm.
    ///
    /// Events with no unresolved parents come first. If multiple events
    /// are ready simultaneously, they are returned in hash-sorted order
    /// for determinism.
    ///
    /// # Multiple Roots
    ///
    /// The algorithm handles multiple roots naturally — all root events
    /// start in the ready set.
    #[must_use]
    pub fn topological_order(&self) -> Vec<&Event> {
        // Count in-degree (number of parents in the DAG) per node.
        let mut in_degree: HashMap<&str, usize> = HashMap::with_capacity(self.nodes.len());
        for (hash, node) in &self.nodes {
            // Count only parents that are actually in the DAG.
            let parent_count = node
                .parents
                .iter()
                .filter(|p| self.nodes.contains_key(p.as_str()))
                .count();
            in_degree.insert(hash.as_str(), parent_count);
        }

        // Seed with all roots (in-degree 0), sorted for determinism.
        let mut ready: Vec<&str> = in_degree
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(&hash, _)| hash)
            .collect();
        ready.sort();

        let mut result = Vec::with_capacity(self.nodes.len());

        while !ready.is_empty() {
            // Pop the lexicographically smallest ready node for determinism.
            let current = ready.remove(0);

            if let Some(node) = self.nodes.get(current) {
                result.push(&node.event);

                // Decrement in-degree for all children.
                let mut new_ready = Vec::new();
                for child_hash in &node.children {
                    if let Some(deg) = in_degree.get_mut(child_hash.as_str()) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 {
                            new_ready.push(child_hash.as_str());
                        }
                    }
                }

                // Sort new ready nodes for determinism, then add to ready list.
                new_ready.sort();
                for h in new_ready {
                    // Insert in sorted position.
                    let pos = ready.binary_search(&h).unwrap_or_else(|p| p);
                    ready.insert(pos, h);
                }
            }
        }

        result
    }

    /// Check if event `a` is a causal ancestor of event `b`.
    ///
    /// Returns `true` if there is a directed path from `a` to `b` in the DAG
    /// (i.e., `a` happened before `b`).
    #[must_use]
    pub fn is_ancestor(&self, a: &str, b: &str) -> bool {
        if a == b {
            return false;
        }
        self.ancestors(b).contains(a)
    }

    /// Check if two events are concurrent (neither is an ancestor of the other).
    #[must_use]
    pub fn are_concurrent(&self, a: &str, b: &str) -> bool {
        if a == b {
            return false;
        }
        !self.is_ancestor(a, b) && !self.is_ancestor(b, a)
    }

    /// Return an iterator over all event hashes in the DAG.
    pub fn hashes(&self) -> impl Iterator<Item = &str> {
        self.nodes.keys().map(String::as_str)
    }

    /// Return an iterator over all nodes in the DAG.
    pub fn nodes(&self) -> impl Iterator<Item = (&str, &DagNode)> {
        self.nodes.iter().map(|(k, v)| (k.as_str(), v))
    }
}

impl Default for EventDag {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::event::data::{CreateData, EventData, MoveData, UpdateData};
    use crate::event::types::EventType;
    use crate::event::writer::write_event;
    use crate::model::item::{Kind, State, Urgency};
    use crate::model::item_id::ItemId;

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

    fn make_update(ts: i64, parents: &[&str], field: &str) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: "agent-a".into(),
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

    // -------------------------------------------------------------------
    // Construction
    // -------------------------------------------------------------------

    #[test]
    fn empty_dag() {
        let dag = EventDag::new();
        assert_eq!(dag.len(), 0);
        assert!(dag.is_empty());
        assert!(dag.roots().is_empty());
        assert!(dag.tips().is_empty());
    }

    #[test]
    fn single_root() {
        let root = make_root(1_000, "agent-a");
        let dag = EventDag::from_events(&[root.clone()]);

        assert_eq!(dag.len(), 1);
        assert!(!dag.is_empty());
        assert!(dag.contains(&root.event_hash));
        assert_eq!(dag.roots(), vec![root.event_hash.as_str()]);
        assert_eq!(dag.tips(), vec![root.event_hash.as_str()]);
    }

    #[test]
    fn linear_chain() {
        let root = make_root(1_000, "agent-a");
        let child = make_child(2_000, &[&root.event_hash], "agent-a");
        let grandchild = make_child(3_000, &[&child.event_hash], "agent-a");

        let dag = EventDag::from_events(&[root.clone(), child.clone(), grandchild.clone()]);

        assert_eq!(dag.len(), 3);

        // Root detection
        let roots = dag.roots();
        assert_eq!(roots.len(), 1);
        assert!(roots.contains(&root.event_hash.as_str()));

        // Tip detection
        let tips = dag.tips();
        assert_eq!(tips.len(), 1);
        assert!(tips.contains(&grandchild.event_hash.as_str()));

        // Parent/child links
        let root_node = dag.get(&root.event_hash).unwrap();
        assert!(root_node.parents.is_empty());
        assert_eq!(root_node.children, vec![child.event_hash.clone()]);

        let child_node = dag.get(&child.event_hash).unwrap();
        assert_eq!(child_node.parents, vec![root.event_hash.clone()]);
        assert_eq!(child_node.children, vec![grandchild.event_hash.clone()]);

        let gc_node = dag.get(&grandchild.event_hash).unwrap();
        assert_eq!(gc_node.parents, vec![child.event_hash.clone()]);
        assert!(gc_node.children.is_empty());
    }

    #[test]
    fn fork_topology() {
        //      root
        //     /    \
        //   left   right
        let root = make_root(1_000, "agent-a");
        let left = make_child(2_000, &[&root.event_hash], "agent-a");
        let right = make_child(2_100, &[&root.event_hash], "agent-b");

        let dag = EventDag::from_events(&[root.clone(), left.clone(), right.clone()]);

        assert_eq!(dag.len(), 3);
        assert_eq!(dag.roots().len(), 1);

        let tips = dag.tips();
        assert_eq!(tips.len(), 2);
        assert!(tips.contains(&left.event_hash.as_str()));
        assert!(tips.contains(&right.event_hash.as_str()));

        // Root has two children
        let root_node = dag.get(&root.event_hash).unwrap();
        assert_eq!(root_node.children.len(), 2);
    }

    #[test]
    fn merge_topology() {
        //    root
        //   /    \
        // left   right
        //   \    /
        //   merge
        let root = make_root(1_000, "agent-a");
        let left = make_child(2_000, &[&root.event_hash], "agent-a");
        let right = make_child(2_100, &[&root.event_hash], "agent-b");
        let merge = make_child(3_000, &[&left.event_hash, &right.event_hash], "agent-a");

        let dag =
            EventDag::from_events(&[root.clone(), left.clone(), right.clone(), merge.clone()]);

        assert_eq!(dag.len(), 4);
        assert_eq!(dag.tips().len(), 1);
        assert!(dag.tips().contains(&merge.event_hash.as_str()));

        let merge_node = dag.get(&merge.event_hash).unwrap();
        assert_eq!(merge_node.parents.len(), 2);
    }

    #[test]
    fn multiple_roots() {
        // Two independent genesis events (concurrent agents).
        let root_a = make_root(1_000, "agent-a");
        let root_b = make_root(1_100, "agent-b");

        let dag = EventDag::from_events(&[root_a.clone(), root_b.clone()]);

        assert_eq!(dag.len(), 2);
        let roots = dag.roots();
        assert_eq!(roots.len(), 2);
        assert!(roots.contains(&root_a.event_hash.as_str()));
        assert!(roots.contains(&root_b.event_hash.as_str()));
    }

    // -------------------------------------------------------------------
    // Deduplication
    // -------------------------------------------------------------------

    #[test]
    fn duplicate_insert_is_noop() {
        let root = make_root(1_000, "agent-a");
        let mut dag = EventDag::new();

        dag.insert(root.clone());
        dag.insert(root.clone()); // duplicate

        assert_eq!(dag.len(), 1);
    }

    #[test]
    fn duplicate_in_from_events() {
        let root = make_root(1_000, "agent-a");
        let dag = EventDag::from_events(&[root.clone(), root.clone()]);

        assert_eq!(dag.len(), 1);
    }

    // -------------------------------------------------------------------
    // Out-of-order insertion
    // -------------------------------------------------------------------

    #[test]
    fn out_of_order_insertion() {
        let root = make_root(1_000, "agent-a");
        let child = make_child(2_000, &[&root.event_hash], "agent-a");

        // Insert child BEFORE parent.
        let dag = EventDag::from_events(&[child.clone(), root.clone()]);

        assert_eq!(dag.len(), 2);

        // Parent→child links should still be correct.
        let root_node = dag.get(&root.event_hash).unwrap();
        assert!(root_node.children.contains(&child.event_hash));

        let child_node = dag.get(&child.event_hash).unwrap();
        assert!(child_node.parents.contains(&root.event_hash));
    }

    // -------------------------------------------------------------------
    // Traversal
    // -------------------------------------------------------------------

    #[test]
    fn ancestors_of_root_is_empty() {
        let root = make_root(1_000, "agent-a");
        let dag = EventDag::from_events(&[root.clone()]);

        assert!(dag.ancestors(&root.event_hash).is_empty());
    }

    #[test]
    fn ancestors_of_child() {
        let root = make_root(1_000, "agent-a");
        let child = make_child(2_000, &[&root.event_hash], "agent-a");
        let grandchild = make_child(3_000, &[&child.event_hash], "agent-a");

        let dag = EventDag::from_events(&[root.clone(), child.clone(), grandchild.clone()]);

        let ancestors = dag.ancestors(&grandchild.event_hash);
        assert_eq!(ancestors.len(), 2);
        assert!(ancestors.contains(&root.event_hash));
        assert!(ancestors.contains(&child.event_hash));
    }

    #[test]
    fn ancestors_of_merge_event() {
        let root = make_root(1_000, "agent-a");
        let left = make_child(2_000, &[&root.event_hash], "agent-a");
        let right = make_child(2_100, &[&root.event_hash], "agent-b");
        let merge = make_child(3_000, &[&left.event_hash, &right.event_hash], "agent-a");

        let dag =
            EventDag::from_events(&[root.clone(), left.clone(), right.clone(), merge.clone()]);

        let ancestors = dag.ancestors(&merge.event_hash);
        assert_eq!(ancestors.len(), 3);
        assert!(ancestors.contains(&root.event_hash));
        assert!(ancestors.contains(&left.event_hash));
        assert!(ancestors.contains(&right.event_hash));
    }

    #[test]
    fn descendants_of_root() {
        let root = make_root(1_000, "agent-a");
        let child = make_child(2_000, &[&root.event_hash], "agent-a");
        let grandchild = make_child(3_000, &[&child.event_hash], "agent-a");

        let dag = EventDag::from_events(&[root.clone(), child.clone(), grandchild.clone()]);

        let descendants = dag.descendants(&root.event_hash);
        assert_eq!(descendants.len(), 2);
        assert!(descendants.contains(&child.event_hash));
        assert!(descendants.contains(&grandchild.event_hash));
    }

    #[test]
    fn descendants_of_tip_is_empty() {
        let root = make_root(1_000, "agent-a");
        let child = make_child(2_000, &[&root.event_hash], "agent-a");

        let dag = EventDag::from_events(&[root.clone(), child.clone()]);

        assert!(dag.descendants(&child.event_hash).is_empty());
    }

    #[test]
    fn descendants_of_fork_root() {
        let root = make_root(1_000, "agent-a");
        let left = make_child(2_000, &[&root.event_hash], "agent-a");
        let right = make_child(2_100, &[&root.event_hash], "agent-b");

        let dag = EventDag::from_events(&[root.clone(), left.clone(), right.clone()]);

        let descendants = dag.descendants(&root.event_hash);
        assert_eq!(descendants.len(), 2);
        assert!(descendants.contains(&left.event_hash));
        assert!(descendants.contains(&right.event_hash));
    }

    // -------------------------------------------------------------------
    // Topological order
    // -------------------------------------------------------------------

    #[test]
    fn topological_order_empty() {
        let dag = EventDag::new();
        assert!(dag.topological_order().is_empty());
    }

    #[test]
    fn topological_order_single() {
        let root = make_root(1_000, "agent-a");
        let dag = EventDag::from_events(&[root.clone()]);

        let order = dag.topological_order();
        assert_eq!(order.len(), 1);
        assert_eq!(order[0].event_hash, root.event_hash);
    }

    #[test]
    fn topological_order_linear_chain() {
        let root = make_root(1_000, "agent-a");
        let child = make_child(2_000, &[&root.event_hash], "agent-a");
        let grandchild = make_child(3_000, &[&child.event_hash], "agent-a");

        let dag = EventDag::from_events(&[root.clone(), child.clone(), grandchild.clone()]);

        let order = dag.topological_order();
        assert_eq!(order.len(), 3);
        assert_eq!(order[0].event_hash, root.event_hash);
        assert_eq!(order[1].event_hash, child.event_hash);
        assert_eq!(order[2].event_hash, grandchild.event_hash);
    }

    #[test]
    fn topological_order_respects_causality() {
        // In any valid topological order, every parent must appear before
        // all of its children.
        let root = make_root(1_000, "agent-a");
        let left = make_child(2_000, &[&root.event_hash], "agent-a");
        let right = make_child(2_100, &[&root.event_hash], "agent-b");
        let merge = make_child(3_000, &[&left.event_hash, &right.event_hash], "agent-a");

        let dag =
            EventDag::from_events(&[root.clone(), left.clone(), right.clone(), merge.clone()]);

        let order = dag.topological_order();
        assert_eq!(order.len(), 4);

        // Build position map.
        let pos: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(i, e)| (e.event_hash.as_str(), i))
            .collect();

        // Root before everything.
        assert!(pos[root.event_hash.as_str()] < pos[left.event_hash.as_str()]);
        assert!(pos[root.event_hash.as_str()] < pos[right.event_hash.as_str()]);

        // Both branches before merge.
        assert!(pos[left.event_hash.as_str()] < pos[merge.event_hash.as_str()]);
        assert!(pos[right.event_hash.as_str()] < pos[merge.event_hash.as_str()]);
    }

    #[test]
    fn topological_order_is_deterministic() {
        let root = make_root(1_000, "agent-a");
        let left = make_child(2_000, &[&root.event_hash], "agent-a");
        let right = make_child(2_100, &[&root.event_hash], "agent-b");

        let dag = EventDag::from_events(&[root.clone(), left.clone(), right.clone()]);

        let order1: Vec<_> = dag.topological_order().iter().map(|e| e.event_hash.clone()).collect();
        let order2: Vec<_> = dag.topological_order().iter().map(|e| e.event_hash.clone()).collect();

        assert_eq!(order1, order2, "topological order must be deterministic");
    }

    #[test]
    fn topological_order_multiple_roots() {
        let root_a = make_root(1_000, "agent-a");
        let root_b = make_root(1_100, "agent-b");
        let child = make_child(2_000, &[&root_a.event_hash, &root_b.event_hash], "agent-a");

        let dag = EventDag::from_events(&[root_a.clone(), root_b.clone(), child.clone()]);

        let order = dag.topological_order();
        assert_eq!(order.len(), 3);

        // Both roots before child.
        let pos: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(i, e)| (e.event_hash.as_str(), i))
            .collect();

        assert!(pos[root_a.event_hash.as_str()] < pos[child.event_hash.as_str()]);
        assert!(pos[root_b.event_hash.as_str()] < pos[child.event_hash.as_str()]);
    }

    // -------------------------------------------------------------------
    // Ancestry queries
    // -------------------------------------------------------------------

    #[test]
    fn is_ancestor_linear() {
        let root = make_root(1_000, "agent-a");
        let child = make_child(2_000, &[&root.event_hash], "agent-a");
        let grandchild = make_child(3_000, &[&child.event_hash], "agent-a");

        let dag = EventDag::from_events(&[root.clone(), child.clone(), grandchild.clone()]);

        assert!(dag.is_ancestor(&root.event_hash, &grandchild.event_hash));
        assert!(dag.is_ancestor(&root.event_hash, &child.event_hash));
        assert!(dag.is_ancestor(&child.event_hash, &grandchild.event_hash));

        // Not ancestor in reverse.
        assert!(!dag.is_ancestor(&grandchild.event_hash, &root.event_hash));
        assert!(!dag.is_ancestor(&child.event_hash, &root.event_hash));

        // Not ancestor of self.
        assert!(!dag.is_ancestor(&root.event_hash, &root.event_hash));
    }

    #[test]
    fn are_concurrent() {
        let root = make_root(1_000, "agent-a");
        let left = make_child(2_000, &[&root.event_hash], "agent-a");
        let right = make_child(2_100, &[&root.event_hash], "agent-b");

        let dag = EventDag::from_events(&[root.clone(), left.clone(), right.clone()]);

        assert!(dag.are_concurrent(&left.event_hash, &right.event_hash));
        assert!(!dag.are_concurrent(&root.event_hash, &left.event_hash));
        assert!(!dag.are_concurrent(&left.event_hash, &left.event_hash));
    }

    // -------------------------------------------------------------------
    // Node / event accessors
    // -------------------------------------------------------------------

    #[test]
    fn get_event_returns_correct_event() {
        let root = make_root(1_000, "agent-a");
        let dag = EventDag::from_events(&[root.clone()]);

        let event = dag.get_event(&root.event_hash).unwrap();
        assert_eq!(event.agent, "agent-a");
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let dag = EventDag::new();
        assert!(dag.get("blake3:nonexistent").is_none());
        assert!(dag.get_event("blake3:nonexistent").is_none());
    }

    #[test]
    fn contains_works() {
        let root = make_root(1_000, "agent-a");
        let dag = EventDag::from_events(&[root.clone()]);

        assert!(dag.contains(&root.event_hash));
        assert!(!dag.contains("blake3:other"));
    }

    #[test]
    fn hashes_iterator() {
        let root = make_root(1_000, "agent-a");
        let child = make_child(2_000, &[&root.event_hash], "agent-a");

        let dag = EventDag::from_events(&[root.clone(), child.clone()]);

        let hashes: HashSet<&str> = dag.hashes().collect();
        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains(root.event_hash.as_str()));
        assert!(hashes.contains(child.event_hash.as_str()));
    }

    // -------------------------------------------------------------------
    // Ancestors/descendants for nonexistent hash
    // -------------------------------------------------------------------

    #[test]
    fn ancestors_of_nonexistent_is_empty() {
        let dag = EventDag::new();
        assert!(dag.ancestors("blake3:none").is_empty());
    }

    #[test]
    fn descendants_of_nonexistent_is_empty() {
        let dag = EventDag::new();
        assert!(dag.descendants("blake3:none").is_empty());
    }

    // -------------------------------------------------------------------
    // Incremental insert
    // -------------------------------------------------------------------

    #[test]
    fn incremental_insert() {
        let root = make_root(1_000, "agent-a");
        let child = make_child(2_000, &[&root.event_hash], "agent-a");

        let mut dag = EventDag::new();

        dag.insert(root.clone());
        assert_eq!(dag.len(), 1);
        assert_eq!(dag.tips().len(), 1);

        dag.insert(child.clone());
        assert_eq!(dag.len(), 2);
        assert_eq!(dag.tips().len(), 1);
        assert!(dag.tips().contains(&child.event_hash.as_str()));

        // Root is no longer a tip.
        assert!(!dag.tips().contains(&root.event_hash.as_str()));
    }

    // -------------------------------------------------------------------
    // Large DAG stress test
    // -------------------------------------------------------------------

    #[test]
    fn large_linear_chain() {
        let mut events = Vec::with_capacity(100);
        let root = make_root(1_000, "agent-a");
        events.push(root);

        for i in 1..100 {
            let parent_hash = events[i - 1].event_hash.clone();
            let child = make_child(1_000 + i as i64, &[&parent_hash], "agent-a");
            events.push(child);
        }

        let dag = EventDag::from_events(&events);
        assert_eq!(dag.len(), 100);
        assert_eq!(dag.roots().len(), 1);
        assert_eq!(dag.tips().len(), 1);

        let topo = dag.topological_order();
        assert_eq!(topo.len(), 100);

        // Verify ordering.
        for i in 0..99 {
            assert_eq!(topo[i].event_hash, events[i].event_hash);
        }
    }

    #[test]
    fn diamond_dag_with_updates() {
        //   root
        //  /    \
        // u1    u2
        //  \    /
        //  merge
        let root = make_root(1_000, "agent-a");
        let u1 = make_update(2_000, &[&root.event_hash], "title");
        let u2 = make_update(2_100, &[&root.event_hash], "priority");
        let merge = make_child(3_000, &[&u1.event_hash, &u2.event_hash], "agent-a");

        let dag = EventDag::from_events(&[root.clone(), u1.clone(), u2.clone(), merge.clone()]);

        assert_eq!(dag.len(), 4);
        assert_eq!(dag.roots().len(), 1);
        assert_eq!(dag.tips().len(), 1);

        // Ancestors of merge includes all 3 predecessors.
        let anc = dag.ancestors(&merge.event_hash);
        assert_eq!(anc.len(), 3);

        // Descendants of root includes all 3 successors.
        let desc = dag.descendants(&root.event_hash);
        assert_eq!(desc.len(), 3);
    }
}
