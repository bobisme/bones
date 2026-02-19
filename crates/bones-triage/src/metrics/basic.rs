//! Static graph metrics: degree centrality, topological sort, and density.
//!
//! # Overview
//!
//! These are Phase 1 metrics — synchronous, < 20ms on typical graphs.
//! They are computed on every triage invocation and provide baseline
//! statistics about the dependency structure.
//!
//! All metrics operate on the condensed DAG from [`NormalizedGraph`].
//! Items sharing an SCC receive identical scores.

use std::collections::HashMap;

use petgraph::{
    algo::toposort,
    graph::NodeIndex,
    visit::IntoNodeIdentifiers,
    Direction,
};

use crate::graph::normalize::NormalizedGraph;

// ---------------------------------------------------------------------------
// Degree Centrality
// ---------------------------------------------------------------------------

/// Per-item degree centrality scores.
#[derive(Debug, Clone, PartialEq)]
pub struct DegreeCentrality {
    /// In-degree per item ID (how many things block this item).
    pub in_degree: HashMap<String, usize>,
    /// Out-degree per item ID (how many things this item blocks).
    pub out_degree: HashMap<String, usize>,
    /// Total degree per item ID (in + out).
    pub total_degree: HashMap<String, usize>,
}

/// Compute degree centrality for all items in the condensed graph.
///
/// Items in the same SCC share the same degree (the SCC node's degree).
///
/// # Returns
///
/// A [`DegreeCentrality`] with in-degree, out-degree, and total-degree
/// maps indexed by original item ID.
#[must_use]
pub fn degree_centrality(ng: &NormalizedGraph) -> DegreeCentrality {
    let mut in_degree = HashMap::new();
    let mut out_degree = HashMap::new();
    let mut total_degree = HashMap::new();

    for idx in ng.condensed.node_identifiers() {
        let in_d = ng
            .condensed
            .neighbors_directed(idx, Direction::Incoming)
            .count();
        let out_d = ng
            .condensed
            .neighbors_directed(idx, Direction::Outgoing)
            .count();
        let total = in_d + out_d;

        // Distribute to all items in this SCC
        if let Some(scc) = ng.condensed.node_weight(idx) {
            for member in &scc.members {
                in_degree.insert(member.clone(), in_d);
                out_degree.insert(member.clone(), out_d);
                total_degree.insert(member.clone(), total);
            }
        }
    }

    DegreeCentrality {
        in_degree,
        out_degree,
        total_degree,
    }
}

// ---------------------------------------------------------------------------
// Topological Sort
// ---------------------------------------------------------------------------

/// Compute a topological ordering of items on the condensed DAG.
///
/// Items are returned in dependency order: if A blocks B, A appears before B.
/// Items within the same SCC are grouped together (their internal order
/// is lexicographic for determinism).
///
/// # Returns
///
/// A `Vec<String>` of item IDs in topological order, or `None` if the
/// condensed graph unexpectedly contains a cycle (should not happen).
#[must_use]
pub fn topological_order(ng: &NormalizedGraph) -> Option<Vec<String>> {
    let topo = toposort(&ng.condensed, None).ok()?;

    let mut result = Vec::with_capacity(ng.raw.node_count());
    for idx in topo {
        if let Some(scc) = ng.condensed.node_weight(idx) {
            // Members are already sorted (from NormalizedGraph::from_raw)
            result.extend(scc.members.iter().cloned());
        }
    }

    Some(result)
}

// ---------------------------------------------------------------------------
// Graph Density
// ---------------------------------------------------------------------------

/// Compute graph density of the condensed DAG.
///
/// Density = edges / (nodes * (nodes - 1)) for a directed graph.
/// Returns 0.0 for graphs with fewer than 2 nodes.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn condensed_density(ng: &NormalizedGraph) -> f64 {
    let n = ng.condensed.node_count();
    if n < 2 {
        return 0.0;
    }
    let max_edges = (n * (n - 1)) as f64;
    ng.condensed.edge_count() as f64 / max_edges
}

// ---------------------------------------------------------------------------
// Component Analysis
// ---------------------------------------------------------------------------

/// Information about weakly connected components in the condensed graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentInfo {
    /// Number of weakly connected components.
    pub count: usize,
    /// Sizes (node counts) of each component, sorted descending.
    pub sizes: Vec<usize>,
}

/// Compute weakly connected component information for the condensed DAG.
///
/// Uses a simple BFS/DFS approach treating edges as undirected.
#[must_use]
pub fn component_info(ng: &NormalizedGraph) -> ComponentInfo {
    let node_count = ng.condensed.node_count();
    if node_count == 0 {
        return ComponentInfo {
            count: 0,
            sizes: vec![],
        };
    }

    let mut visited = vec![false; node_count];
    let mut sizes = Vec::new();

    for start in ng.condensed.node_identifiers() {
        if visited[start.index()] {
            continue;
        }

        // BFS from start, treating edges as undirected
        let mut stack = vec![start];
        let mut component_size = 0usize;

        while let Some(node) = stack.pop() {
            if visited[node.index()] {
                continue;
            }
            visited[node.index()] = true;
            component_size += 1;

            // Follow outgoing edges
            for neighbor in ng.condensed.neighbors_directed(node, Direction::Outgoing) {
                if !visited[neighbor.index()] {
                    stack.push(neighbor);
                }
            }
            // Follow incoming edges (treat as undirected)
            for neighbor in ng.condensed.neighbors_directed(node, Direction::Incoming) {
                if !visited[neighbor.index()] {
                    stack.push(neighbor);
                }
            }
        }

        sizes.push(component_size);
    }

    sizes.sort_unstable_by(|a, b| b.cmp(a)); // descending

    ComponentInfo {
        count: sizes.len(),
        sizes,
    }
}

// ---------------------------------------------------------------------------
// Source / Sink identification
// ---------------------------------------------------------------------------

/// Return item IDs that are sources in the condensed DAG (no incoming edges).
///
/// These are items (or SCC groups) with no blockers — ready to start.
#[must_use]
pub fn source_items(ng: &NormalizedGraph) -> Vec<String> {
    let mut sources = Vec::new();
    for idx in ng.condensed.node_identifiers() {
        if ng
            .condensed
            .neighbors_directed(idx, Direction::Incoming)
            .next()
            .is_none()
        {
            if let Some(scc) = ng.condensed.node_weight(idx) {
                sources.extend(scc.members.iter().cloned());
            }
        }
    }
    sources.sort_unstable();
    sources
}

/// Return item IDs that are sinks in the condensed DAG (no outgoing edges).
///
/// These are items (or SCC groups) that nothing else depends on.
#[must_use]
pub fn sink_items(ng: &NormalizedGraph) -> Vec<String> {
    let mut sinks = Vec::new();
    for idx in ng.condensed.node_identifiers() {
        if ng
            .condensed
            .neighbors_directed(idx, Direction::Outgoing)
            .next()
            .is_none()
        {
            if let Some(scc) = ng.condensed.node_weight(idx) {
                sinks.extend(scc.members.iter().cloned());
            }
        }
    }
    sinks.sort_unstable();
    sinks
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build::RawGraph;
    use crate::graph::normalize::NormalizedGraph;
    use petgraph::graph::DiGraph;
    use std::collections::HashMap;

    fn make_normalized(edges: &[(&str, &str)]) -> NormalizedGraph {
        let mut graph = DiGraph::<String, ()>::new();
        let mut node_map = HashMap::new();

        let all_ids: std::collections::BTreeSet<&str> =
            edges.iter().flat_map(|(a, b)| [*a, *b]).collect();

        for id in all_ids {
            let idx = graph.add_node(id.to_string());
            node_map.insert(id.to_string(), idx);
        }

        for (a, b) in edges {
            let ia = node_map[*a];
            let ib = node_map[*b];
            graph.add_edge(ia, ib, ());
        }

        let raw = RawGraph {
            graph,
            node_map,
            content_hash: "blake3:test".to_string(),
        };

        NormalizedGraph::from_raw(raw)
    }

    fn make_normalized_nodes(nodes: &[&str], edges: &[(&str, &str)]) -> NormalizedGraph {
        let mut graph = DiGraph::<String, ()>::new();
        let mut node_map = HashMap::new();

        for id in nodes {
            let idx = graph.add_node((*id).to_string());
            node_map.insert((*id).to_string(), idx);
        }

        for (a, b) in edges {
            let ia = node_map[*a];
            let ib = node_map[*b];
            graph.add_edge(ia, ib, ());
        }

        let raw = RawGraph {
            graph,
            node_map,
            content_hash: "blake3:test".to_string(),
        };

        NormalizedGraph::from_raw(raw)
    }

    // -----------------------------------------------------------------------
    // Degree centrality
    // -----------------------------------------------------------------------

    #[test]
    fn degree_centrality_empty_graph() {
        let ng = make_normalized_nodes(&[], &[]);
        let dc = degree_centrality(&ng);
        assert!(dc.in_degree.is_empty());
        assert!(dc.out_degree.is_empty());
        assert!(dc.total_degree.is_empty());
    }

    #[test]
    fn degree_centrality_single_node() {
        let ng = make_normalized_nodes(&["A"], &[]);
        let dc = degree_centrality(&ng);
        assert_eq!(dc.in_degree["A"], 0);
        assert_eq!(dc.out_degree["A"], 0);
        assert_eq!(dc.total_degree["A"], 0);
    }

    #[test]
    fn degree_centrality_linear_chain() {
        // A → B → C
        let ng = make_normalized(&[("A", "B"), ("B", "C")]);
        let dc = degree_centrality(&ng);

        assert_eq!(dc.in_degree["A"], 0);
        assert_eq!(dc.out_degree["A"], 1);
        assert_eq!(dc.total_degree["A"], 1);

        assert_eq!(dc.in_degree["B"], 1);
        assert_eq!(dc.out_degree["B"], 1);
        assert_eq!(dc.total_degree["B"], 2);

        assert_eq!(dc.in_degree["C"], 1);
        assert_eq!(dc.out_degree["C"], 0);
        assert_eq!(dc.total_degree["C"], 1);
    }

    #[test]
    fn degree_centrality_star_topology() {
        // Hub: A→B, A→C, A→D
        let ng = make_normalized(&[("A", "B"), ("A", "C"), ("A", "D")]);
        let dc = degree_centrality(&ng);

        assert_eq!(dc.out_degree["A"], 3);
        assert_eq!(dc.in_degree["A"], 0);

        for leaf in ["B", "C", "D"] {
            assert_eq!(dc.in_degree[leaf], 1);
            assert_eq!(dc.out_degree[leaf], 0);
        }
    }

    #[test]
    fn degree_centrality_cycle_members_share_score() {
        // A → B → A (cycle) → C
        let ng = make_normalized(&[("A", "B"), ("B", "A"), ("A", "C")]);
        let dc = degree_centrality(&ng);

        // A and B are in the same SCC, which has out-degree 1 (to C's SCC)
        assert_eq!(dc.out_degree["A"], dc.out_degree["B"]);
        assert_eq!(dc.in_degree["A"], dc.in_degree["B"]);
    }

    // -----------------------------------------------------------------------
    // Topological sort
    // -----------------------------------------------------------------------

    #[test]
    fn topological_order_empty() {
        let ng = make_normalized_nodes(&[], &[]);
        let order = topological_order(&ng);
        assert_eq!(order, Some(vec![]));
    }

    #[test]
    fn topological_order_single_node() {
        let ng = make_normalized_nodes(&["A"], &[]);
        let order = topological_order(&ng);
        assert_eq!(order, Some(vec!["A".to_string()]));
    }

    #[test]
    fn topological_order_chain() {
        // A → B → C: A must come before B, B before C
        let ng = make_normalized(&[("A", "B"), ("B", "C")]);
        let order = topological_order(&ng).expect("should succeed");

        let pos_a = order.iter().position(|x| x == "A").expect("A present");
        let pos_b = order.iter().position(|x| x == "B").expect("B present");
        let pos_c = order.iter().position(|x| x == "C").expect("C present");

        assert!(pos_a < pos_b, "A before B");
        assert!(pos_b < pos_c, "B before C");
    }

    #[test]
    fn topological_order_diamond() {
        // A → B → D, A → C → D
        let ng = make_normalized(&[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D")]);
        let order = topological_order(&ng).expect("should succeed");

        let pos_a = order.iter().position(|x| x == "A").expect("A present");
        let pos_b = order.iter().position(|x| x == "B").expect("B present");
        let pos_c = order.iter().position(|x| x == "C").expect("C present");
        let pos_d = order.iter().position(|x| x == "D").expect("D present");

        assert!(pos_a < pos_b, "A before B");
        assert!(pos_a < pos_c, "A before C");
        assert!(pos_b < pos_d, "B before D");
        assert!(pos_c < pos_d, "C before D");
    }

    #[test]
    fn topological_order_cycle_grouped() {
        // A → B → A → C: SCC{A,B} should appear before C
        let ng = make_normalized(&[("A", "B"), ("B", "A"), ("A", "C")]);
        let order = topological_order(&ng).expect("should succeed");

        let pos_a = order.iter().position(|x| x == "A").expect("A present");
        let pos_b = order.iter().position(|x| x == "B").expect("B present");
        let pos_c = order.iter().position(|x| x == "C").expect("C present");

        // A and B should both come before C
        assert!(pos_a < pos_c, "A before C");
        assert!(pos_b < pos_c, "B before C");
    }

    #[test]
    fn topological_order_all_items_present() {
        let ng = make_normalized(&[("A", "B"), ("C", "D")]);
        let order = topological_order(&ng).expect("should succeed");
        assert_eq!(order.len(), 4);
        assert!(order.contains(&"A".to_string()));
        assert!(order.contains(&"B".to_string()));
        assert!(order.contains(&"C".to_string()));
        assert!(order.contains(&"D".to_string()));
    }

    // -----------------------------------------------------------------------
    // Condensed density
    // -----------------------------------------------------------------------

    #[test]
    fn condensed_density_empty() {
        let ng = make_normalized_nodes(&[], &[]);
        assert!((condensed_density(&ng) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn condensed_density_single_node() {
        let ng = make_normalized_nodes(&["A"], &[]);
        assert!((condensed_density(&ng) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn condensed_density_two_nodes_one_edge() {
        // A → B: density = 1 / (2*1) = 0.5
        let ng = make_normalized(&[("A", "B")]);
        assert!((condensed_density(&ng) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn condensed_density_cycle_collapses() {
        // A → B → A: condensed to 1 node, density = 0.0
        let ng = make_normalized(&[("A", "B"), ("B", "A")]);
        assert!((condensed_density(&ng) - 0.0).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Component info
    // -----------------------------------------------------------------------

    #[test]
    fn component_info_empty() {
        let ng = make_normalized_nodes(&[], &[]);
        let ci = component_info(&ng);
        assert_eq!(ci.count, 0);
        assert!(ci.sizes.is_empty());
    }

    #[test]
    fn component_info_single_component() {
        let ng = make_normalized(&[("A", "B"), ("B", "C")]);
        let ci = component_info(&ng);
        assert_eq!(ci.count, 1);
        assert_eq!(ci.sizes, vec![3]);
    }

    #[test]
    fn component_info_disjoint() {
        let ng = make_normalized(&[("A", "B"), ("C", "D")]);
        let ci = component_info(&ng);
        assert_eq!(ci.count, 2);
        assert_eq!(ci.sizes, vec![2, 2]); // sorted descending
    }

    #[test]
    fn component_info_mixed_sizes() {
        // Chain A→B→C and isolated D
        let ng = make_normalized_nodes(&["A", "B", "C", "D"], &[("A", "B"), ("B", "C")]);
        let ci = component_info(&ng);
        assert_eq!(ci.count, 2);
        assert_eq!(ci.sizes, vec![3, 1]); // larger first
    }

    // -----------------------------------------------------------------------
    // Source / Sink items
    // -----------------------------------------------------------------------

    #[test]
    fn source_items_chain() {
        // A → B → C: A is the only source
        let ng = make_normalized(&[("A", "B"), ("B", "C")]);
        assert_eq!(source_items(&ng), vec!["A".to_string()]);
    }

    #[test]
    fn sink_items_chain() {
        // A → B → C: C is the only sink
        let ng = make_normalized(&[("A", "B"), ("B", "C")]);
        assert_eq!(sink_items(&ng), vec!["C".to_string()]);
    }

    #[test]
    fn source_items_diamond() {
        // A → B → D, A → C → D
        let ng = make_normalized(&[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D")]);
        assert_eq!(source_items(&ng), vec!["A".to_string()]);
    }

    #[test]
    fn sink_items_diamond() {
        let ng = make_normalized(&[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D")]);
        assert_eq!(sink_items(&ng), vec!["D".to_string()]);
    }

    #[test]
    fn source_sink_isolated_nodes() {
        let ng = make_normalized_nodes(&["A", "B", "C"], &[]);
        let sources = source_items(&ng);
        let sinks = sink_items(&ng);
        // Isolated nodes are both sources and sinks
        assert_eq!(sources.len(), 3);
        assert_eq!(sinks.len(), 3);
    }

    #[test]
    fn source_sink_disjoint_chains() {
        // A → B, C → D: sources are A and C, sinks are B and D
        let ng = make_normalized(&[("A", "B"), ("C", "D")]);
        assert_eq!(source_items(&ng), vec!["A".to_string(), "C".to_string()]);
        assert_eq!(sink_items(&ng), vec!["B".to_string(), "D".to_string()]);
    }
}
