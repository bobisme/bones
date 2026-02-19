//! Incremental and full-cycle detection helpers for dependency graphs.
//!
//! # Edge Direction
//!
//! The triage graph uses edge direction `blocker → blocked`.
//! Adding a new edge `from → to` would create a cycle if `to` is already
//! reachable from `from` through existing edges.

#![allow(clippy::module_name_repetitions)]

use std::collections::{HashMap, HashSet, VecDeque};

use petgraph::algo::tarjan_scc;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;

/// Check whether adding `from -> to` would introduce a dependency cycle.
///
/// Returns a concrete cycle path when a cycle would be created, formatted as:
/// `from -> to -> ... -> from`.
///
/// If the edge already exists, this returns `None` (no *new* cycle is created).
#[must_use]
pub fn would_create_cycle(
    graph: &DiGraph<String, ()>,
    from: NodeIndex,
    to: NodeIndex,
) -> Option<Vec<String>> {
    if from == to {
        let id = node_id(graph, from);
        return Some(vec![id.clone(), id]);
    }

    if graph.contains_edge(from, to) {
        return None;
    }

    // BFS from `to` looking for `from`.
    // If reachable, then adding `from -> to` closes a cycle.
    let mut queue: VecDeque<NodeIndex> = VecDeque::from([to]);
    let mut visited: HashSet<NodeIndex> = HashSet::from([to]);
    let mut parent: HashMap<NodeIndex, NodeIndex> = HashMap::new();

    while let Some(current) = queue.pop_front() {
        if current == from {
            return Some(reconstruct_cycle_path(graph, from, to, &parent));
        }

        for edge in graph.edges(current) {
            let next = edge.target();
            if visited.insert(next) {
                parent.insert(next, current);
                queue.push_back(next);
            }
        }
    }

    None
}

/// Find all cycles currently present in `graph`.
///
/// Each entry is a sorted list of item IDs in one strongly connected
/// component (SCC). Self-loops are reported as a one-element cycle.
#[must_use]
pub fn find_all_cycles(graph: &DiGraph<String, ()>) -> Vec<Vec<String>> {
    let mut cycles: Vec<Vec<String>> = tarjan_scc(graph)
        .into_iter()
        .filter(|component| {
            component.len() > 1 || component.first().is_some_and(|node| has_self_loop(graph, *node))
        })
        .map(|component| {
            let mut ids: Vec<String> = component.into_iter().map(|idx| node_id(graph, idx)).collect();
            ids.sort_unstable();
            ids
        })
        .collect();

    cycles.sort_unstable();
    cycles
}

#[must_use]
fn has_self_loop(graph: &DiGraph<String, ()>, node: NodeIndex) -> bool {
    graph.find_edge(node, node).is_some()
}

fn reconstruct_cycle_path(
    graph: &DiGraph<String, ()>,
    from: NodeIndex,
    to: NodeIndex,
    parent: &HashMap<NodeIndex, NodeIndex>,
) -> Vec<String> {
    // Parent links represent a path: to -> ... -> from.
    // Rebuild that path and then prepend `from` to represent the newly added
    // edge `from -> to` that closes the cycle.
    let mut to_to_from: Vec<NodeIndex> = vec![from];
    let mut cursor = from;

    while cursor != to {
        if let Some(next) = parent.get(&cursor) {
            cursor = *next;
            to_to_from.push(cursor);
        } else {
            break;
        }
    }

    to_to_from.reverse();

    let mut cycle: Vec<String> = Vec::with_capacity(to_to_from.len() + 1);
    cycle.push(node_id(graph, from));
    cycle.extend(to_to_from.into_iter().map(|idx| node_id(graph, idx)));
    cycle
}

fn node_id(graph: &DiGraph<String, ()>, idx: NodeIndex) -> String {
    graph
        .node_weight(idx)
        .cloned()
        .unwrap_or_else(|| format!("#{}", idx.index()))
}

// ---------------------------------------------------------------------------
// Cycle break suggestions
// ---------------------------------------------------------------------------

/// A detected dependency cycle with suggested edges to remove to break it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleReport {
    /// Sorted item IDs that form this cycle (members of the SCC).
    pub members: Vec<String>,
    /// Suggested `(blocker, blocked)` edges to remove to break the cycle.
    ///
    /// These are back-edges identified by DFS within the SCC.  Removing all
    /// suggested edges will make the SCC acyclic.  In most cases a single
    /// edge is sufficient.
    pub suggested_breaks: Vec<(String, String)>,
}

/// Detect all cycles and, for each, suggest edges to remove to break them.
///
/// Internally uses Tarjan's SCC to identify cycle members, then runs a DFS
/// within each SCC sub-graph to collect back-edges.  Back-edges are the
/// canonical set to remove to make an SCC acyclic.
///
/// Self-loops are reported with the single edge `(id, id)` as the break.
#[must_use]
pub fn report_cycles_with_breaks(graph: &DiGraph<String, ()>) -> Vec<CycleReport> {
    let scc_list = tarjan_scc(graph);

    let mut reports: Vec<CycleReport> = scc_list
        .into_iter()
        .filter(|component| {
            component.len() > 1
                || component
                    .first()
                    .is_some_and(|node| has_self_loop(graph, *node))
        })
        .map(|component| {
            let mut members: Vec<String> =
                component.iter().map(|&idx| node_id(graph, idx)).collect();
            members.sort_unstable();

            // Self-loop: single node with an edge to itself.
            if component.len() == 1 {
                let id = members[0].clone();
                return CycleReport {
                    members,
                    suggested_breaks: vec![(id.clone(), id)],
                };
            }

            // Find back-edges within this SCC via DFS.
            let member_set: HashSet<NodeIndex> = component.iter().copied().collect();
            let suggested_breaks = find_back_edges_in_scc(graph, &component, &member_set);

            CycleReport {
                members,
                suggested_breaks,
            }
        })
        .collect();

    reports.sort_unstable_by(|a, b| a.members.cmp(&b.members));
    reports
}

/// Run DFS within the nodes of an SCC and collect back-edges.
///
/// A back-edge `(u, v)` is an edge to an ancestor in the DFS tree — removing
/// it eliminates the cycle it closes.
///
/// Uses iterative DFS with an explicit ancestor set for correctness on large
/// cycles without stack overflow risk.
fn find_back_edges_in_scc(
    graph: &DiGraph<String, ()>,
    component: &[NodeIndex],
    member_set: &HashSet<NodeIndex>,
) -> Vec<(String, String)> {
    // Start DFS from the lexicographically smallest node for determinism.
    let start = component
        .iter()
        .min_by_key(|&&idx| node_id(graph, idx))
        .copied()
        .expect("component non-empty");

    let mut visited: HashSet<NodeIndex> = HashSet::new();
    let mut ancestor_stack: Vec<NodeIndex> = Vec::new(); // current DFS path
    let mut ancestor_set: HashSet<NodeIndex> = HashSet::new(); // for O(1) ancestor lookup
    let mut back_edges: Vec<(String, String)> = Vec::new();

    // Each stack entry: (node, index into its neighbor list).
    let mut call_stack: Vec<(NodeIndex, Vec<NodeIndex>, usize)> = Vec::new();

    // Bootstrap: push the start node.
    if !visited.contains(&start) {
        visited.insert(start);
        ancestor_stack.push(start);
        ancestor_set.insert(start);
        let neighbors: Vec<NodeIndex> = graph
            .neighbors_directed(start, petgraph::Direction::Outgoing)
            .filter(|n| member_set.contains(n))
            .collect();
        call_stack.push((start, neighbors, 0));
    }

    // Run DFS over all SCC nodes (handles disconnected sub-components).
    let mut all_starts: Vec<NodeIndex> = component.to_vec();
    all_starts.sort_unstable_by_key(|&idx| node_id(graph, idx));
    let mut extra_starts = all_starts.into_iter().peekable();

    loop {
        if let Some(frame) = call_stack.last_mut() {
            let current = frame.0;
            let neighbors = &frame.1;
            let idx = &mut frame.2;

            if *idx < neighbors.len() {
                let neighbor = neighbors[*idx];
                *idx += 1;

                if ancestor_set.contains(&neighbor) {
                    // Back-edge: current → neighbor (neighbor is an ancestor).
                    back_edges.push((node_id(graph, current), node_id(graph, neighbor)));
                } else if !visited.contains(&neighbor) {
                    visited.insert(neighbor);
                    ancestor_stack.push(neighbor);
                    ancestor_set.insert(neighbor);
                    let next_neighbors: Vec<NodeIndex> = graph
                        .neighbors_directed(neighbor, petgraph::Direction::Outgoing)
                        .filter(|n| member_set.contains(n))
                        .collect();
                    call_stack.push((neighbor, next_neighbors, 0));
                }
            } else {
                // Pop this frame; current node is on top of ancestor_stack.
                call_stack.pop();
                ancestor_stack.pop(); // current is always the top when we finish it
                ancestor_set.remove(&current);
            }
        } else {
            // call_stack empty — try next unvisited SCC node (for disconnected SCCs).
            let Some(next) = extra_starts.next() else { break };
            if !visited.contains(&next) {
                visited.insert(next);
                ancestor_stack.push(next);
                ancestor_set.insert(next);
                let neighbors: Vec<NodeIndex> = graph
                    .neighbors_directed(next, petgraph::Direction::Outgoing)
                    .filter(|n| member_set.contains(n))
                    .collect();
                call_stack.push((next, neighbors, 0));
            }
        }
    }

    back_edges.sort_unstable();
    back_edges
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn graph_with_nodes_and_edges(
        nodes: &[&str],
        edges: &[(&str, &str)],
    ) -> (DiGraph<String, ()>, HashMap<String, NodeIndex>) {
        let mut graph = DiGraph::<String, ()>::new();
        let mut map: HashMap<String, NodeIndex> = HashMap::new();

        for &node in nodes {
            let idx = graph.add_node(node.to_string());
            map.insert(node.to_string(), idx);
        }

        for &(from, to) in edges {
            let from_idx = *map
                .entry(from.to_string())
                .or_insert_with(|| graph.add_node(from.to_string()));
            let to_idx = *map
                .entry(to.to_string())
                .or_insert_with(|| graph.add_node(to.to_string()));
            graph.add_edge(from_idx, to_idx, ());
        }

        (graph, map)
    }

    #[test]
    fn would_create_cycle_detects_self_loop() {
        let (graph, nodes) = graph_with_nodes_and_edges(&["A"], &[]);
        let a = nodes["A"];

        let cycle = would_create_cycle(&graph, a, a);

        assert_eq!(cycle, Some(vec!["A".to_string(), "A".to_string()]));
    }

    #[test]
    fn would_create_cycle_detects_three_node_loop() {
        // Existing: A -> B -> C
        // New edge: C -> A (creates C -> A -> B -> C)
        let (graph, nodes) = graph_with_nodes_and_edges(
            &["A", "B", "C"],
            &[("A", "B"), ("B", "C")],
        );

        let cycle = would_create_cycle(&graph, nodes["C"], nodes["A"])
            .unwrap_or_else(|| panic!("expected cycle"));

        assert_eq!(cycle, vec!["C", "A", "B", "C"]);
    }

    #[test]
    fn would_create_cycle_returns_none_for_safe_edge() {
        // Existing: A -> B -> C
        // New edge: A -> C (no cycle)
        let (graph, nodes) = graph_with_nodes_and_edges(
            &["A", "B", "C"],
            &[("A", "B"), ("B", "C")],
        );

        assert!(would_create_cycle(&graph, nodes["A"], nodes["C"]).is_none());
    }

    #[test]
    fn would_create_cycle_returns_none_for_duplicate_edge() {
        let (graph, nodes) = graph_with_nodes_and_edges(&["A", "B"], &[("A", "B")]);

        assert!(would_create_cycle(&graph, nodes["A"], nodes["B"]).is_none());
    }

    #[test]
    fn find_all_cycles_reports_sccs_and_self_loops() {
        // SCC1: A <-> B
        // SCC2: C -> D -> E -> C
        // SCC3: F -> F
        let (graph, _) = graph_with_nodes_and_edges(
            &["A", "B", "C", "D", "E", "F", "G"],
            &[
                ("A", "B"),
                ("B", "A"),
                ("C", "D"),
                ("D", "E"),
                ("E", "C"),
                ("F", "F"),
            ],
        );

        let cycles = find_all_cycles(&graph);

        assert_eq!(
            cycles,
            vec![
                vec!["A".to_string(), "B".to_string()],
                vec!["C".to_string(), "D".to_string(), "E".to_string()],
                vec!["F".to_string()]
            ]
        );
    }

    #[test]
    fn find_all_cycles_empty_graph() {
        let (graph, _) = graph_with_nodes_and_edges(&[], &[]);
        assert!(find_all_cycles(&graph).is_empty());
    }

    // -----------------------------------------------------------------------
    // report_cycles_with_breaks tests
    // -----------------------------------------------------------------------

    #[test]
    fn report_cycles_empty_graph() {
        let (graph, _) = graph_with_nodes_and_edges(&[], &[]);
        let reports = report_cycles_with_breaks(&graph);
        assert!(reports.is_empty(), "no cycles in empty graph");
    }

    #[test]
    fn report_cycles_acyclic_graph() {
        let (graph, _) = graph_with_nodes_and_edges(&["A", "B", "C"], &[("A", "B"), ("B", "C")]);
        let reports = report_cycles_with_breaks(&graph);
        assert!(reports.is_empty(), "no cycles in acyclic graph");
    }

    #[test]
    fn report_cycles_self_loop() {
        let (graph, _) = graph_with_nodes_and_edges(&["A"], &[("A", "A")]);
        let reports = report_cycles_with_breaks(&graph);

        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r.members, vec!["A".to_string()]);
        assert_eq!(
            r.suggested_breaks,
            vec![("A".to_string(), "A".to_string())]
        );
    }

    #[test]
    fn report_cycles_two_node_cycle() {
        // A ⇄ B: back-edge is B→A (DFS enters A first, then B, B→A is back-edge).
        let (graph, _) =
            graph_with_nodes_and_edges(&["A", "B"], &[("A", "B"), ("B", "A")]);
        let reports = report_cycles_with_breaks(&graph);

        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r.members, vec!["A".to_string(), "B".to_string()]);

        // Exactly one back-edge should be suggested.
        assert_eq!(r.suggested_breaks.len(), 1, "one break needed for 2-cycle");

        // The suggested break should be a valid edge in the graph.
        let (from, to) = &r.suggested_breaks[0];
        // It must be either A→B or B→A.
        let is_valid = (from == "B" && to == "A") || (from == "A" && to == "B");
        assert!(is_valid, "break must be an existing edge, got {from}→{to}");
    }

    #[test]
    fn report_cycles_three_node_cycle_one_break() {
        // A → B → C → A: one back-edge breaks the cycle.
        let (graph, _) = graph_with_nodes_and_edges(
            &["A", "B", "C"],
            &[("A", "B"), ("B", "C"), ("C", "A")],
        );
        let reports = report_cycles_with_breaks(&graph);

        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(
            r.members,
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
        // One back-edge (C→A) should be detected.
        assert_eq!(r.suggested_breaks.len(), 1);
        assert_eq!(
            r.suggested_breaks[0],
            ("C".to_string(), "A".to_string()),
            "back-edge in A→B→C→A is C→A"
        );
    }

    #[test]
    fn report_cycles_multiple_independent_cycles() {
        // Cycle 1: A ⇄ B
        // Cycle 2: C → D → E → C
        // Cycle 3: F → F (self-loop)
        let (graph, _) = graph_with_nodes_and_edges(
            &["A", "B", "C", "D", "E", "F", "G"],
            &[
                ("A", "B"),
                ("B", "A"),
                ("C", "D"),
                ("D", "E"),
                ("E", "C"),
                ("F", "F"),
            ],
        );
        let reports = report_cycles_with_breaks(&graph);

        assert_eq!(reports.len(), 3, "three separate cycles");

        // Check members (reports sorted by members).
        assert_eq!(reports[0].members, vec!["A", "B"]);
        assert_eq!(reports[1].members, vec!["C", "D", "E"]);
        assert_eq!(reports[2].members, vec!["F"]);

        // Each cycle should have at least one suggested break.
        assert!(!reports[0].suggested_breaks.is_empty());
        assert!(!reports[1].suggested_breaks.is_empty());
        assert!(!reports[2].suggested_breaks.is_empty());

        // Self-loop break should be F→F.
        assert_eq!(
            reports[2].suggested_breaks,
            vec![("F".to_string(), "F".to_string())]
        );
    }

    #[test]
    fn report_cycles_break_is_valid_existing_edge() {
        // For each suggested break, verify it is an actual edge in the graph.
        let (graph, _) = graph_with_nodes_and_edges(
            &["A", "B", "C"],
            &[("A", "B"), ("B", "C"), ("C", "A")],
        );
        let reports = report_cycles_with_breaks(&graph);

        for report in &reports {
            for (from, to) in &report.suggested_breaks {
                let from_idx = graph
                    .node_indices()
                    .find(|&i| graph.node_weight(i).map(String::as_str) == Some(from.as_str()))
                    .expect("from node exists");
                let to_idx = graph
                    .node_indices()
                    .find(|&i| graph.node_weight(i).map(String::as_str) == Some(to.as_str()))
                    .expect("to node exists");
                assert!(
                    graph.contains_edge(from_idx, to_idx),
                    "suggested break {from}→{to} must be an existing edge"
                );
            }
        }
    }
}
