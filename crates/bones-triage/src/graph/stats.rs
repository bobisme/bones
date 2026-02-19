//! Basic graph statistics for the dependency graph.
//!
//! # Statistics Provided
//!
//! - **node_count**: Total number of items (nodes) in the original graph.
//! - **edge_count**: Total number of blocking edges in the original graph.
//! - **density**: Ratio of actual edges to maximum possible edges for a
//!   directed graph: `density = edge_count / (node_count * (node_count - 1))`.
//!   A fully connected directed graph has density 1.0. An empty or
//!   single-node graph has density 0.0.
//! - **scc_count**: Number of strongly connected components (= number of nodes
//!   in the condensed graph). In an acyclic graph this equals `node_count`.
//! - **cycle_count**: Number of SCCs with more than one member (dependency
//!   cycles that need resolution).
//! - **weakly_connected_component_count**: Number of weakly connected
//!   components. A value greater than 1 means the dependency graph is split
//!   into disjoint subgraphs with no edges between them.
//! - **isolated_node_count**: Nodes with no edges at all (neither in-edges
//!   nor out-edges). These items have no dependencies and no dependents.
//! - **max_in_degree**: Highest in-degree (most blocked-by dependencies)
//!   in the original graph.
//! - **max_out_degree**: Highest out-degree (most items blocked) in the
//!   original graph.

use petgraph::{
    algo::connected_components,
    Direction,
    visit::IntoNodeIdentifiers,
};

use crate::graph::normalize::NormalizedGraph;

// ---------------------------------------------------------------------------
// GraphStats
// ---------------------------------------------------------------------------

/// Summary statistics for a dependency graph.
///
/// Computed from a [`NormalizedGraph`] by [`GraphStats::from_normalized`].
/// All counts refer to the original (non-condensed) graph unless otherwise
/// noted.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphStats {
    /// Number of items (nodes) in the graph.
    pub node_count: usize,
    /// Number of blocking edges in the original graph.
    pub edge_count: usize,
    /// Graph density: `edge_count / (node_count * (node_count - 1))`.
    /// Ranges from 0.0 (no edges) to 1.0 (all possible edges present).
    /// Zero for graphs with 0 or 1 node.
    pub density: f64,
    /// Number of strongly connected components.
    ///
    /// Equals `node_count` in a fully acyclic graph.
    pub scc_count: usize,
    /// Number of SCCs with more than one member (dependency cycles).
    pub cycle_count: usize,
    /// Number of weakly connected components (disjoint subgraphs).
    pub weakly_connected_component_count: usize,
    /// Number of nodes with no in-edges and no out-edges.
    pub isolated_node_count: usize,
    /// Maximum in-degree (most incoming blocking edges on one node).
    pub max_in_degree: usize,
    /// Maximum out-degree (most outgoing blocking edges from one node).
    pub max_out_degree: usize,
    /// Number of nodes in the condensed (reduced) graph.
    ///
    /// Equals `scc_count` — provided for convenience.
    pub reduced_node_count: usize,
    /// Number of edges in the transitively-reduced condensed graph.
    pub reduced_edge_count: usize,
}

impl GraphStats {
    /// Compute statistics from a [`NormalizedGraph`].
    #[must_use]
    pub fn from_normalized(ng: &NormalizedGraph) -> Self {
        let node_count = ng.raw.node_count();
        let edge_count = ng.raw.edge_count();

        let density = compute_density(node_count, edge_count);

        let scc_count = ng.scc_count();
        let cycle_count = ng.cycle_count();

        // Weakly connected components: treat the directed graph as undirected
        // and count connected components.
        let wcc = connected_components(&ng.raw.graph);

        // Isolated nodes: degree 0 (no in or out edges).
        let isolated_node_count = ng
            .raw
            .graph
            .node_identifiers()
            .filter(|&idx| {
                ng.raw
                    .graph
                    .neighbors_directed(idx, Direction::Incoming)
                    .next()
                    .is_none()
                    && ng.raw
                        .graph
                        .neighbors_directed(idx, Direction::Outgoing)
                        .next()
                        .is_none()
            })
            .count();

        // Max in/out degree over all nodes in the original graph.
        let max_in_degree = ng
            .raw
            .graph
            .node_identifiers()
            .map(|idx| {
                ng.raw
                    .graph
                    .neighbors_directed(idx, Direction::Incoming)
                    .count()
            })
            .max()
            .unwrap_or(0);

        let max_out_degree = ng
            .raw
            .graph
            .node_identifiers()
            .map(|idx| {
                ng.raw
                    .graph
                    .neighbors_directed(idx, Direction::Outgoing)
                    .count()
            })
            .max()
            .unwrap_or(0);

        let reduced_node_count = ng.reduced.node_count();
        let reduced_edge_count = ng.reduced.edge_count();

        Self {
            node_count,
            edge_count,
            density,
            scc_count,
            cycle_count,
            weakly_connected_component_count: wcc,
            isolated_node_count,
            max_in_degree,
            max_out_degree,
            reduced_node_count,
            reduced_edge_count,
        }
    }

    /// Return `true` if the graph has no blocking edges.
    #[must_use]
    pub fn is_flat(&self) -> bool {
        self.edge_count == 0
    }

    /// Return `true` if the graph contains at least one dependency cycle.
    #[must_use]
    pub fn has_cycles(&self) -> bool {
        self.cycle_count > 0
    }

    /// Return the reduction ratio: how many edges were removed by transitive
    /// reduction vs the raw edge count.
    ///
    /// Returns 0.0 if there are no edges in the original graph.
    #[must_use]
    pub fn reduction_ratio(&self) -> f64 {
        compute_ratio(self.edge_count, self.reduced_edge_count)
    }
}

// ---------------------------------------------------------------------------
// Internal helpers (cast precision suppressed at function scope)
// ---------------------------------------------------------------------------

#[allow(clippy::cast_precision_loss)]
fn compute_density(node_count: usize, edge_count: usize) -> f64 {
    if node_count < 2 {
        return 0.0_f64;
    }
    let max_edges = (node_count * (node_count - 1)) as f64;
    edge_count as f64 / max_edges
}

#[allow(clippy::cast_precision_loss)]
fn compute_ratio(raw: usize, reduced: usize) -> f64 {
    if raw == 0 {
        return 0.0_f64;
    }
    let removed = (raw as f64 - reduced as f64).max(0.0_f64);
    removed / raw as f64
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

    #[test]
    fn empty_graph_stats() {
        let ng = make_normalized_nodes(&[], &[]);
        let stats = GraphStats::from_normalized(&ng);

        assert_eq!(stats.node_count, 0);
        assert_eq!(stats.edge_count, 0);
        assert!((stats.density - 0.0).abs() < f64::EPSILON);
        assert_eq!(stats.scc_count, 0);
        assert_eq!(stats.cycle_count, 0);
        assert_eq!(stats.weakly_connected_component_count, 0);
        assert_eq!(stats.isolated_node_count, 0);
        assert_eq!(stats.max_in_degree, 0);
        assert_eq!(stats.max_out_degree, 0);
        assert!(stats.is_flat());
        assert!(!stats.has_cycles());
    }

    #[test]
    fn single_node_no_edges() {
        let ng = make_normalized_nodes(&["bn-001"], &[]);
        let stats = GraphStats::from_normalized(&ng);

        assert_eq!(stats.node_count, 1);
        assert_eq!(stats.edge_count, 0);
        assert!((stats.density - 0.0).abs() < f64::EPSILON);
        assert_eq!(stats.isolated_node_count, 1);
        assert_eq!(stats.weakly_connected_component_count, 1);
    }

    #[test]
    fn linear_chain_stats() {
        // A → B → C
        let ng = make_normalized(&[("A", "B"), ("B", "C")]);
        let stats = GraphStats::from_normalized(&ng);

        assert_eq!(stats.node_count, 3);
        assert_eq!(stats.edge_count, 2);
        assert_eq!(stats.scc_count, 3);
        assert_eq!(stats.cycle_count, 0);
        assert!(!stats.has_cycles());
        assert!(!stats.is_flat());
        assert_eq!(stats.max_in_degree, 1);
        assert_eq!(stats.max_out_degree, 1);
    }

    #[test]
    fn cycle_detection_in_stats() {
        // A → B → A (cycle)
        let ng = make_normalized(&[("A", "B"), ("B", "A")]);
        let stats = GraphStats::from_normalized(&ng);

        assert_eq!(stats.node_count, 2);
        assert_eq!(stats.edge_count, 2);
        assert_eq!(stats.scc_count, 1, "one condensed SCC");
        assert_eq!(stats.cycle_count, 1);
        assert!(stats.has_cycles());
    }

    #[test]
    fn density_two_node_one_edge() {
        // A → B: density = 1 / (2 * 1) = 0.5
        let ng = make_normalized(&[("A", "B")]);
        let stats = GraphStats::from_normalized(&ng);

        assert!((stats.density - 0.5).abs() < 1e-10, "density = 0.5");
    }

    #[test]
    fn density_complete_directed_graph() {
        // A → B, B → A: density = 2 / (2 * 1) = 1.0
        let ng = make_normalized(&[("A", "B"), ("B", "A")]);
        let stats = GraphStats::from_normalized(&ng);

        assert!((stats.density - 1.0).abs() < 1e-10, "density = 1.0");
    }

    #[test]
    fn disjoint_components() {
        // Two disconnected chains: A→B and C→D
        let ng = make_normalized(&[("A", "B"), ("C", "D")]);
        let stats = GraphStats::from_normalized(&ng);

        assert_eq!(stats.weakly_connected_component_count, 2);
        assert_eq!(stats.isolated_node_count, 0);
    }

    #[test]
    fn isolated_nodes_counted() {
        // Three nodes, no edges
        let ng = make_normalized_nodes(&["A", "B", "C"], &[]);
        let stats = GraphStats::from_normalized(&ng);

        assert_eq!(stats.isolated_node_count, 3);
        assert_eq!(stats.weakly_connected_component_count, 3);
    }

    #[test]
    fn max_degree_correct() {
        // Hub: A→C, B→C, D→C, C→E
        let ng = make_normalized(&[("A", "C"), ("B", "C"), ("D", "C"), ("C", "E")]);
        let stats = GraphStats::from_normalized(&ng);

        assert_eq!(stats.max_in_degree, 3, "C has 3 in-edges");
        assert_eq!(stats.max_out_degree, 1, "each node has at most 1 out-edge");
    }

    #[test]
    fn transitive_reduction_stats() {
        // A→B→C and A→C (redundant)
        let ng = make_normalized(&[("A", "B"), ("B", "C"), ("A", "C")]);
        let stats = GraphStats::from_normalized(&ng);

        assert_eq!(stats.edge_count, 3, "original has 3 edges");
        assert_eq!(stats.reduced_edge_count, 2, "A→C removed in reduction");
    }

    #[test]
    fn is_flat_no_edges() {
        let ng = make_normalized_nodes(&["A", "B"], &[]);
        let stats = GraphStats::from_normalized(&ng);
        assert!(stats.is_flat());
    }

    #[test]
    fn is_flat_with_edges() {
        let ng = make_normalized(&[("A", "B")]);
        let stats = GraphStats::from_normalized(&ng);
        assert!(!stats.is_flat());
    }
}
