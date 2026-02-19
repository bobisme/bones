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
}
