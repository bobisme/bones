//! Cycle detection for the blocking dependency graph.
//!
//! # Overview
//!
//! Blocking dependencies form a directed graph. Cycles make items permanently
//! stuck (each item waits on another in the loop). This module detects cycles
//! when a new blocking edge is added and returns a warning with the cycle path.
//!
//! # Design
//!
//! - **DFS-based**: Standard depth-first search from the target of the new
//!   edge, looking for a path back to the source. This detects the cycle that
//!   the new edge closes.
//! - **Warn, don't block**: Cycles are user errors, not system errors. The
//!   link is still added to the CRDT; the caller is responsible for surfacing
//!   the warning.
//! - **O(V+E)**: Each detection check visits each node and edge at most once.
//!
//! # Usage
//!
//! ```rust,ignore
//! use bones_core::graph::cycles::{detect_cycle_on_add, CycleWarning};
//! use bones_core::graph::blocking::BlockingGraph;
//!
//! let graph = BlockingGraph::from_states(&states);
//! if let Some(warning) = detect_cycle_on_add(&graph, "bn-task1", "bn-task2") {
//!     eprintln!("Warning: {warning}");
//! }
//! ```

#![allow(
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::missing_const_for_fn,
    clippy::collapsible_if,
    clippy::doc_markdown,
    clippy::bool_to_int_with_if,
    clippy::redundant_closure_for_method_calls,
)]

use std::collections::{HashMap, HashSet};
use std::fmt;

use super::blocking::BlockingGraph;

// ---------------------------------------------------------------------------
// CycleWarning
// ---------------------------------------------------------------------------

/// A warning emitted when a new blocking edge would close a cycle.
///
/// Contains the cycle path (ordered list of item IDs forming the loop)
/// and the edge that triggered detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleWarning {
    /// The ordered list of item IDs forming the cycle.
    ///
    /// The path starts at the source of the new edge, follows blocking
    /// dependencies, and ends at the source again. For example, if
    /// adding edge A→B creates cycle A→B→C→A, the path is `["A", "B", "C", "A"]`.
    pub cycle_path: Vec<String>,

    /// The source of the newly added edge (the item being blocked).
    pub edge_from: String,

    /// The target of the newly added edge (the blocker).
    pub edge_to: String,
}

impl CycleWarning {
    /// Number of distinct items in the cycle (path length minus the repeated
    /// start node).
    pub fn cycle_len(&self) -> usize {
        if self.cycle_path.len() <= 1 {
            return 0;
        }
        self.cycle_path.len() - 1
    }

    /// Returns `true` if this is a self-loop (item blocks itself).
    pub fn is_self_loop(&self) -> bool {
        self.edge_from == self.edge_to
    }

    /// Returns `true` if this is a mutual block (2-node cycle: A↔B).
    pub fn is_mutual_block(&self) -> bool {
        self.cycle_len() == 2
    }
}

impl fmt::Display for CycleWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_self_loop() {
            write!(
                f,
                "cycle detected: self-loop on '{}' (item blocks itself)",
                self.edge_from
            )
        } else if self.is_mutual_block() {
            write!(
                f,
                "cycle detected: mutual block between '{}' and '{}'",
                self.edge_from, self.edge_to
            )
        } else {
            let path_display = self.cycle_path.join(" → ");
            write!(
                f,
                "cycle detected ({} items): {}",
                self.cycle_len(),
                path_display
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Core detection
// ---------------------------------------------------------------------------

/// Detect whether adding a blocking edge `from → to` (meaning `from` is
/// blocked by `to`) would create a cycle in the blocking graph.
///
/// This checks if there is already a path from `to` back to `from` in the
/// existing graph. If so, adding `from → to` closes a cycle.
///
/// # Arguments
///
/// - `graph`: The current blocking graph (before the new edge is added).
/// - `from`: The item that would be blocked (source of the new edge).
/// - `to`: The blocker item (target of the new edge).
///
/// # Returns
///
/// `Some(CycleWarning)` if a cycle would be created, `None` otherwise.
///
/// # Complexity
///
/// O(V+E) where V is the number of items and E is the number of blocking
/// edges. Each node and edge is visited at most once during the DFS.
pub fn detect_cycle_on_add(graph: &BlockingGraph, from: &str, to: &str) -> Option<CycleWarning> {
    // Self-loop: from blocks itself.
    if from == to {
        return Some(CycleWarning {
            cycle_path: vec![from.to_string(), from.to_string()],
            edge_from: from.to_string(),
            edge_to: to.to_string(),
        });
    }

    // DFS from `to`, looking for a path back to `from`.
    // We follow the blocking direction: for each node, look at what it is
    // blocked_by (i.e., its outgoing edges in the "blocked_by" graph).
    //
    // The blocking graph stores: item → set of items that block it.
    // So if A is blocked by B, the edge is A→B in blocked_by.
    // We want to find: is there a path from `to` → ... → `from`?
    // Following blocked_by edges from `to`.

    let mut visited: HashSet<String> = HashSet::new();
    let mut parent_map: HashMap<String, String> = HashMap::new();

    if dfs_find_path(graph, to, from, &mut visited, &mut parent_map) {
        // Reconstruct path: from → to → ... → from
        let mut path = vec![from.to_string()];
        reconstruct_path(&parent_map, to, from, &mut path);

        Some(CycleWarning {
            cycle_path: path,
            edge_from: from.to_string(),
            edge_to: to.to_string(),
        })
    } else {
        None
    }
}

/// Detect all cycles in the blocking graph using Tarjan-style DFS.
///
/// Returns a list of all cycles found. Each cycle is represented as a
/// `CycleWarning` where `edge_from` and `edge_to` indicate one edge in the
/// cycle (the back edge that closes it).
///
/// # Complexity
///
/// O(V+E) — standard DFS traversal.
pub fn find_all_cycles(graph: &BlockingGraph) -> Vec<CycleWarning> {
    let mut warnings = Vec::new();
    let mut color: HashMap<String, Color> = HashMap::new();
    let mut parent_map: HashMap<String, String> = HashMap::new();

    // Initialize all nodes as White (unvisited).
    for item in graph.all_item_ids() {
        color.insert(item.to_string(), Color::White);
    }

    for item in graph.all_item_ids() {
        if color.get(item) == Some(&Color::White) {
            dfs_all_cycles(
                graph,
                item,
                &mut color,
                &mut parent_map,
                &mut warnings,
            );
        }
    }

    warnings
}

/// Check whether the blocking graph has any cycles at all.
///
/// More efficient than `find_all_cycles` when you only need a boolean answer
/// — it short-circuits on the first cycle found.
///
/// # Complexity
///
/// O(V+E) in the worst case (no cycles). O(1) best case (immediate self-loop).
pub fn has_cycles(graph: &BlockingGraph) -> bool {
    let mut color: HashMap<String, Color> = HashMap::new();

    for item in graph.all_item_ids() {
        color.insert(item.to_string(), Color::White);
    }

    for item in graph.all_item_ids() {
        if color.get(item) == Some(&Color::White) {
            if dfs_has_cycle(graph, item, &mut color) {
                return true;
            }
        }
    }

    false
}

// ---------------------------------------------------------------------------
// DFS internals
// ---------------------------------------------------------------------------

/// DFS colors for cycle detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Color {
    /// Not yet visited.
    White,
    /// Currently on the DFS stack (in progress).
    Gray,
    /// Fully processed (all descendants visited).
    Black,
}

/// DFS to find a path from `current` to `target` following blocked_by edges.
///
/// Records the traversal in `parent_map` so the path can be reconstructed.
/// Returns `true` if `target` is reachable from `current`.
fn dfs_find_path(
    graph: &BlockingGraph,
    current: &str,
    target: &str,
    visited: &mut HashSet<String>,
    parent_map: &mut HashMap<String, String>,
) -> bool {
    if current == target {
        return true;
    }

    if !visited.insert(current.to_string()) {
        return false;
    }

    for neighbor in graph.get_blockers(current) {
        if !visited.contains(neighbor) {
            parent_map.insert(neighbor.to_string(), current.to_string());
            if dfs_find_path(graph, neighbor, target, visited, parent_map) {
                return true;
            }
        }
    }

    false
}

/// Reconstruct the path from `start` to `end` using the parent map.
///
/// Appends nodes to `path` in order from `start` toward `end`.
fn reconstruct_path(
    parent_map: &HashMap<String, String>,
    start: &str,
    end: &str,
    path: &mut Vec<String>,
) {
    // Build the path from start to end by following parent_map backwards
    // from end to start, then reversing.
    let mut chain = Vec::new();
    let mut current = end.to_string();

    // Walk from end back to start through parent_map.
    while current != start {
        chain.push(current.clone());
        match parent_map.get(&current) {
            Some(parent) => current = parent.clone(),
            None => break,
        }
    }

    // chain is [end, ..., (node after start)] in reverse order.
    // We want [start, ..., end] appended to path.
    // start is already in path (or will be handled by caller).
    // We push start first, then the reversed chain.
    chain.push(start.to_string());
    chain.reverse();

    // Skip the first element if it's already the last in path.
    let skip = if path.last().map(|s| s.as_str()) == Some(start) {
        1
    } else {
        0
    };

    for node in chain.into_iter().skip(skip) {
        path.push(node);
    }
}

/// DFS to find all cycles, recording back edges.
fn dfs_all_cycles(
    graph: &BlockingGraph,
    node: &str,
    color: &mut HashMap<String, Color>,
    parent_map: &mut HashMap<String, String>,
    warnings: &mut Vec<CycleWarning>,
) {
    color.insert(node.to_string(), Color::Gray);

    for neighbor in graph.get_blockers(node) {
        match color.get(neighbor) {
            Some(Color::White) => {
                parent_map.insert(neighbor.to_string(), node.to_string());
                dfs_all_cycles(graph, neighbor, color, parent_map, warnings);
            }
            Some(Color::Gray) => {
                // Back edge: node → neighbor, and neighbor is on the stack.
                // This means there's a cycle from neighbor → ... → node → neighbor.
                let mut cycle_path = vec![neighbor.to_string()];
                let mut cur = node.to_string();
                while cur != neighbor {
                    cycle_path.push(cur.clone());
                    match parent_map.get(&cur) {
                        Some(p) => cur = p.clone(),
                        None => break,
                    }
                }
                cycle_path.push(neighbor.to_string());

                warnings.push(CycleWarning {
                    cycle_path,
                    edge_from: node.to_string(),
                    edge_to: neighbor.to_string(),
                });
            }
            _ => {} // Black — already fully processed, no cycle through this edge.
        }
    }

    color.insert(node.to_string(), Color::Black);
}

/// DFS that returns `true` as soon as any cycle (back edge) is found.
fn dfs_has_cycle(
    graph: &BlockingGraph,
    node: &str,
    color: &mut HashMap<String, Color>,
) -> bool {
    color.insert(node.to_string(), Color::Gray);

    for neighbor in graph.get_blockers(node) {
        match color.get(neighbor) {
            Some(Color::White) => {
                if dfs_has_cycle(graph, neighbor, color) {
                    return true;
                }
            }
            Some(Color::Gray) => {
                return true; // Back edge found — cycle exists.
            }
            _ => {} // Black — no cycle through this edge.
        }
    }

    color.insert(node.to_string(), Color::Black);
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt::item_state::WorkItemState;
    use crate::event::data::{EventData, LinkData};
    use crate::event::types::EventType;
    use crate::event::Event;
    use crate::clock::itc::Stamp;
    use crate::model::item_id::ItemId;
    use std::collections::{BTreeMap, HashMap};

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

    /// Build a blocking graph from a list of (item_id, blocked_by_ids) pairs.
    fn build_graph(edges: &[(&str, &[&str])]) -> BlockingGraph {
        let mut states: HashMap<String, WorkItemState> = HashMap::new();
        // First pass: ensure all item IDs exist in the map.
        for (item_id, blockers) in edges {
            states
                .entry(item_id.to_string())
                .or_insert_with(WorkItemState::new);
            for blocker in *blockers {
                states
                    .entry(blocker.to_string())
                    .or_insert_with(WorkItemState::new);
            }
        }
        // Second pass: apply blocking links.
        for (item_id, blockers) in edges {
            let state = state_with_blockers(blockers);
            states.insert(item_id.to_string(), state);
        }
        BlockingGraph::from_states(&states)
    }

    // -----------------------------------------------------------------------
    // CycleWarning display and properties
    // -----------------------------------------------------------------------

    #[test]
    fn cycle_warning_self_loop_display() {
        let w = CycleWarning {
            cycle_path: vec!["A".to_string(), "A".to_string()],
            edge_from: "A".to_string(),
            edge_to: "A".to_string(),
        };
        assert!(w.is_self_loop());
        assert!(!w.is_mutual_block());
        assert_eq!(w.cycle_len(), 1);
        let display = w.to_string();
        assert!(display.contains("self-loop"), "display: {display}");
        assert!(display.contains("A"), "display: {display}");
    }

    #[test]
    fn cycle_warning_mutual_block_display() {
        let w = CycleWarning {
            cycle_path: vec![
                "A".to_string(),
                "B".to_string(),
                "A".to_string(),
            ],
            edge_from: "A".to_string(),
            edge_to: "B".to_string(),
        };
        assert!(!w.is_self_loop());
        assert!(w.is_mutual_block());
        assert_eq!(w.cycle_len(), 2);
        let display = w.to_string();
        assert!(display.contains("mutual block"), "display: {display}");
    }

    #[test]
    fn cycle_warning_large_cycle_display() {
        let w = CycleWarning {
            cycle_path: vec![
                "A".to_string(),
                "B".to_string(),
                "C".to_string(),
                "D".to_string(),
                "A".to_string(),
            ],
            edge_from: "A".to_string(),
            edge_to: "B".to_string(),
        };
        assert!(!w.is_self_loop());
        assert!(!w.is_mutual_block());
        assert_eq!(w.cycle_len(), 4);
        let display = w.to_string();
        assert!(display.contains("4 items"), "display: {display}");
        assert!(display.contains("A → B → C → D → A"), "display: {display}");
    }

    // -----------------------------------------------------------------------
    // detect_cycle_on_add: self-loop
    // -----------------------------------------------------------------------

    #[test]
    fn self_loop_detected() {
        // Empty graph — adding A→A (A blocked by A) is a self-loop.
        let graph = build_graph(&[]);
        let warning = detect_cycle_on_add(&graph, "A", "A");
        assert!(warning.is_some());
        let w = warning.unwrap();
        assert!(w.is_self_loop());
        assert_eq!(w.edge_from, "A");
        assert_eq!(w.edge_to, "A");
    }

    // -----------------------------------------------------------------------
    // detect_cycle_on_add: 2-node mutual block
    // -----------------------------------------------------------------------

    #[test]
    fn mutual_block_detected() {
        // A is blocked by B. Adding B→A (B blocked by A) creates A↔B cycle.
        let graph = build_graph(&[("A", &["B"])]);
        let warning = detect_cycle_on_add(&graph, "B", "A");
        assert!(warning.is_some());
        let w = warning.unwrap();
        assert!(w.is_mutual_block());
        assert_eq!(w.cycle_len(), 2);
        // Path should be B → A → B
        assert_eq!(w.cycle_path.first().unwrap(), "B");
        assert_eq!(w.cycle_path.last().unwrap(), "B");
    }

    // -----------------------------------------------------------------------
    // detect_cycle_on_add: 3-node cycle
    // -----------------------------------------------------------------------

    #[test]
    fn three_node_cycle_detected() {
        // A blocked by B, B blocked by C. Adding C→A (C blocked by A) closes cycle.
        let graph = build_graph(&[("A", &["B"]), ("B", &["C"])]);
        let warning = detect_cycle_on_add(&graph, "C", "A");
        assert!(warning.is_some());
        let w = warning.unwrap();
        assert_eq!(w.cycle_len(), 3);
        assert_eq!(w.edge_from, "C");
        assert_eq!(w.edge_to, "A");
        // Path: C → A → B → C
        assert_eq!(w.cycle_path.first().unwrap(), "C");
        assert_eq!(w.cycle_path.last().unwrap(), "C");
    }

    // -----------------------------------------------------------------------
    // detect_cycle_on_add: no cycle
    // -----------------------------------------------------------------------

    #[test]
    fn no_cycle_in_dag() {
        // A → B → C (linear chain). Adding D→A doesn't create a cycle.
        let graph = build_graph(&[("A", &["B"]), ("B", &["C"])]);
        let warning = detect_cycle_on_add(&graph, "D", "A");
        assert!(warning.is_none());
    }

    #[test]
    fn no_cycle_parallel_chains() {
        // A → B, C → D. Adding A→C doesn't create a cycle.
        let graph = build_graph(&[("A", &["B"]), ("C", &["D"])]);
        let warning = detect_cycle_on_add(&graph, "A", "C");
        assert!(warning.is_none());
    }

    #[test]
    fn no_cycle_diamond_dag() {
        // Diamond: A → B, A → C, B → D, C → D. Adding E→A is safe.
        let graph = build_graph(&[
            ("A", &["B", "C"]),
            ("B", &["D"]),
            ("C", &["D"]),
        ]);
        let warning = detect_cycle_on_add(&graph, "E", "A");
        assert!(warning.is_none());
    }

    // -----------------------------------------------------------------------
    // detect_cycle_on_add: large cycle (10+ items)
    // -----------------------------------------------------------------------

    #[test]
    fn large_cycle_detected() {
        // Chain: item0 → item1 → item2 → ... → item9.
        // Adding item9 → item0 closes a 10-node cycle.
        let mut edges: Vec<(&str, Vec<&str>)> = Vec::new();
        let names: Vec<String> = (0..10).map(|i| format!("item{i}")).collect();

        for i in 0..9 {
            edges.push((&names[i], vec![&names[i + 1]]));
        }

        // Convert to the format build_graph expects
        let edge_refs: Vec<(&str, &[&str])> = edges
            .iter()
            .map(|(from, to)| (*from, to.as_slice()))
            .collect();

        let graph = build_graph(&edge_refs);
        let warning = detect_cycle_on_add(&graph, "item9", "item0");
        assert!(warning.is_some());
        let w = warning.unwrap();
        assert_eq!(w.cycle_len(), 10);
        assert_eq!(w.cycle_path.first().unwrap(), "item9");
        assert_eq!(w.cycle_path.last().unwrap(), "item9");
    }

    #[test]
    fn very_large_cycle_detected() {
        // 50-item chain closed into a cycle.
        let names: Vec<String> = (0..50).map(|i| format!("n{i}")).collect();
        let mut edges: Vec<(&str, Vec<&str>)> = Vec::new();

        for i in 0..49 {
            edges.push((&names[i], vec![&names[i + 1]]));
        }

        let edge_refs: Vec<(&str, &[&str])> = edges
            .iter()
            .map(|(from, to)| (*from, to.as_slice()))
            .collect();

        let graph = build_graph(&edge_refs);
        let warning = detect_cycle_on_add(&graph, &names[49], &names[0]);
        assert!(warning.is_some());
        let w = warning.unwrap();
        assert_eq!(w.cycle_len(), 50);
    }

    // -----------------------------------------------------------------------
    // detect_cycle_on_add: edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn empty_graph_no_cycle() {
        let graph = build_graph(&[]);
        let warning = detect_cycle_on_add(&graph, "A", "B");
        assert!(warning.is_none());
    }

    #[test]
    fn adding_duplicate_edge_to_existing_blocker_no_new_cycle() {
        // A → B exists. Adding A → B again doesn't create a cycle.
        let graph = build_graph(&[("A", &["B"])]);
        let warning = detect_cycle_on_add(&graph, "A", "B");
        assert!(warning.is_none());
    }

    #[test]
    fn cycle_in_subgraph_detected() {
        // Disconnected: X → Y → Z and A → B.
        // Adding B → A creates a 2-node cycle in the A-B subgraph.
        let graph = build_graph(&[("X", &["Y"]), ("Y", &["Z"]), ("A", &["B"])]);
        let warning = detect_cycle_on_add(&graph, "B", "A");
        assert!(warning.is_some());
        let w = warning.unwrap();
        assert!(w.is_mutual_block());
    }

    // -----------------------------------------------------------------------
    // find_all_cycles
    // -----------------------------------------------------------------------

    #[test]
    fn find_all_cycles_empty_graph() {
        let graph = build_graph(&[]);
        let cycles = find_all_cycles(&graph);
        assert!(cycles.is_empty());
    }

    #[test]
    fn find_all_cycles_dag_has_none() {
        let graph = build_graph(&[("A", &["B"]), ("B", &["C"])]);
        let cycles = find_all_cycles(&graph);
        assert!(cycles.is_empty());
    }

    #[test]
    fn find_all_cycles_self_loop() {
        let graph = build_graph(&[("A", &["A"])]);
        let cycles = find_all_cycles(&graph);
        assert!(!cycles.is_empty());
        // Should find the self-loop.
        assert!(cycles.iter().any(|w| w.edge_from == "A" && w.edge_to == "A"));
    }

    #[test]
    fn find_all_cycles_mutual_block() {
        let graph = build_graph(&[("A", &["B"]), ("B", &["A"])]);
        let cycles = find_all_cycles(&graph);
        assert!(!cycles.is_empty());
    }

    #[test]
    fn find_all_cycles_multiple_disjoint() {
        // Two independent cycles: A↔B and C↔D.
        let graph = build_graph(&[
            ("A", &["B"]),
            ("B", &["A"]),
            ("C", &["D"]),
            ("D", &["C"]),
        ]);
        let cycles = find_all_cycles(&graph);
        // Should find at least one cycle in each pair.
        assert!(cycles.len() >= 2);
    }

    // -----------------------------------------------------------------------
    // has_cycles
    // -----------------------------------------------------------------------

    #[test]
    fn has_cycles_false_for_dag() {
        let graph = build_graph(&[("A", &["B"]), ("B", &["C"]), ("A", &["C"])]);
        assert!(!has_cycles(&graph));
    }

    #[test]
    fn has_cycles_true_for_self_loop() {
        let graph = build_graph(&[("A", &["A"])]);
        assert!(has_cycles(&graph));
    }

    #[test]
    fn has_cycles_true_for_mutual_block() {
        let graph = build_graph(&[("A", &["B"]), ("B", &["A"])]);
        assert!(has_cycles(&graph));
    }

    #[test]
    fn has_cycles_true_for_large_cycle() {
        let names: Vec<String> = (0..15).map(|i| format!("item{i}")).collect();
        let mut edges: Vec<(&str, Vec<&str>)> = Vec::new();

        for i in 0..14 {
            edges.push((&names[i], vec![&names[i + 1]]));
        }
        // Close the cycle.
        edges.push((&names[14], vec![&names[0]]));

        let edge_refs: Vec<(&str, &[&str])> = edges
            .iter()
            .map(|(from, to)| (*from, to.as_slice()))
            .collect();

        let graph = build_graph(&edge_refs);
        assert!(has_cycles(&graph));
    }

    #[test]
    fn has_cycles_false_for_empty_graph() {
        let graph = build_graph(&[]);
        assert!(!has_cycles(&graph));
    }

    // -----------------------------------------------------------------------
    // Performance: O(V+E) correctness check
    // -----------------------------------------------------------------------

    #[test]
    fn performance_large_dag_no_cycle() {
        // Build a large DAG (1000 nodes in a chain). No cycle.
        // This verifies O(V+E) — should complete quickly.
        let names: Vec<String> = (0..1000).map(|i| format!("n{i}")).collect();
        let mut edges: Vec<(&str, Vec<&str>)> = Vec::new();

        for i in 0..999 {
            edges.push((&names[i], vec![&names[i + 1]]));
        }

        let edge_refs: Vec<(&str, &[&str])> = edges
            .iter()
            .map(|(from, to)| (*from, to.as_slice()))
            .collect();

        let graph = build_graph(&edge_refs);

        // detect_cycle_on_add: adding a new leaf doesn't create a cycle.
        let warning = detect_cycle_on_add(&graph, "new_item", &names[0]);
        assert!(warning.is_none());

        // has_cycles should be false.
        assert!(!has_cycles(&graph));
    }

    #[test]
    fn performance_large_dag_with_cycle_at_end() {
        // 1000-node chain with cycle at the end.
        let names: Vec<String> = (0..1000).map(|i| format!("n{i}")).collect();
        let mut edges: Vec<(&str, Vec<&str>)> = Vec::new();

        for i in 0..999 {
            edges.push((&names[i], vec![&names[i + 1]]));
        }

        let edge_refs: Vec<(&str, &[&str])> = edges
            .iter()
            .map(|(from, to)| (*from, to.as_slice()))
            .collect();

        let graph = build_graph(&edge_refs);

        // Adding n999 → n0 closes a 1000-node cycle.
        let warning = detect_cycle_on_add(&graph, &names[999], &names[0]);
        assert!(warning.is_some());
        assert_eq!(warning.unwrap().cycle_len(), 1000);
    }

    // -----------------------------------------------------------------------
    // Integration with BlockingGraph
    // -----------------------------------------------------------------------

    #[test]
    fn integration_with_crdt_state() {
        // Build states from CRDT events, construct graph, detect cycle.
        let mut states: HashMap<String, WorkItemState> = HashMap::new();

        // A is blocked by B.
        let mut state_a = WorkItemState::new();
        state_a.apply_event(&make_link_event("B", "blocks", 1000, "alice", "blake3:l1"));
        states.insert("A".to_string(), state_a);

        // B is blocked by C.
        let mut state_b = WorkItemState::new();
        state_b.apply_event(&make_link_event("C", "blocks", 1001, "alice", "blake3:l2"));
        states.insert("B".to_string(), state_b);

        // C exists, no blockers.
        states.insert("C".to_string(), WorkItemState::new());

        let graph = BlockingGraph::from_states(&states);

        // Adding C blocked by A would create A → B → C → A.
        let warning = detect_cycle_on_add(&graph, "C", "A");
        assert!(warning.is_some());
        let w = warning.unwrap();
        assert_eq!(w.cycle_len(), 3);
    }
}
