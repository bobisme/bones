//! Betweenness centrality via Brandes' algorithm.
//!
//! # Overview
//!
//! Betweenness centrality measures how often a node lies on shortest paths
//! between other pairs of nodes. High-betweenness items are "bridges" or
//! "bottlenecks" — removing them would disconnect parts of the graph.
//!
//! # Algorithm
//!
//! We implement Brandes' algorithm (2001) for unweighted graphs:
//!
//! 1. For each source node `s`, run BFS to compute shortest-path counts
//!    and distances.
//! 2. Accumulate dependency scores in reverse BFS order (farthest nodes first).
//! 3. Sum the dependency scores across all source nodes.
//!
//! Complexity: O(V * E) for unweighted graphs.
//!
//! # Output
//!
//! Returns a `HashMap<String, f64>` mapping item IDs to their betweenness
//! centrality scores. Scores are **not** normalized by default — callers
//! can normalize by dividing by `(n-1)*(n-2)` for directed graphs where
//! `n` is the node count.
//!
//! Items within the same SCC receive the same score (the SCC node's score).

use std::collections::{HashMap, VecDeque};

use petgraph::{
    Direction,
    graph::NodeIndex,
    visit::{IntoNodeIdentifiers, NodeIndexable},
};
use tracing::instrument;

use crate::graph::normalize::NormalizedGraph;

/// Compute betweenness centrality for all items in the graph.
///
/// Operates on the **condensed** DAG (SCCs collapsed). Items in the same
/// SCC receive the same betweenness score.
///
/// # Arguments
///
/// * `ng` — A [`NormalizedGraph`] containing the condensed DAG.
///
/// # Returns
///
/// A `HashMap<String, f64>` mapping each item ID to its betweenness score.
/// Disconnected nodes and nodes with no shortest paths through them receive
/// a score of 0.0.
#[must_use]
#[instrument(skip(ng))]
pub fn betweenness_centrality(ng: &NormalizedGraph) -> HashMap<String, f64> {
    let g = &ng.condensed;
    let n = g.node_count();

    if n == 0 {
        return HashMap::new();
    }

    // Node-indexed betweenness accumulator.
    let mut cb: Vec<f64> = vec![0.0; n];

    // For each source node s, run Brandes' BFS-based algorithm.
    for s in g.node_identifiers() {
        let si = g.to_index(s);

        // Stack: nodes in order of discovery (farthest popped first).
        let mut stack: Vec<NodeIndex> = Vec::with_capacity(n);

        // Predecessor lists: predecessors[w] = list of nodes that immediately
        // precede w on shortest paths from s.
        let mut predecessors: Vec<Vec<NodeIndex>> = vec![Vec::new(); n];

        // sigma[t]: number of shortest paths from s to t.
        let mut sigma: Vec<f64> = vec![0.0; n];
        sigma[si] = 1.0;

        // dist[t]: distance from s to t (-1 = unvisited).
        let mut dist: Vec<i64> = vec![-1; n];
        dist[si] = 0;

        // BFS queue.
        let mut queue: VecDeque<NodeIndex> = VecDeque::new();
        queue.push_back(s);

        while let Some(v) = queue.pop_front() {
            let vi = g.to_index(v);
            stack.push(v);

            for w in g.neighbors_directed(v, Direction::Outgoing) {
                let wi = g.to_index(w);

                // First visit to w?
                if dist[wi] < 0 {
                    dist[wi] = dist[vi] + 1;
                    queue.push_back(w);
                }

                // Shortest path to w via v?
                if dist[wi] == dist[vi] + 1 {
                    sigma[wi] += sigma[vi];
                    predecessors[wi].push(v);
                }
            }
        }

        // Accumulate dependencies in reverse BFS order.
        let mut delta: Vec<f64> = vec![0.0; n];

        while let Some(w) = stack.pop() {
            let wi = g.to_index(w);

            for &v in &predecessors[wi] {
                let vi = g.to_index(v);
                if sigma[wi] > 0.0 {
                    delta[vi] += (sigma[vi] / sigma[wi]) * (1.0 + delta[wi]);
                }
            }

            if wi != si {
                cb[wi] += delta[wi];
            }
        }
    }

    // Map condensed node scores back to item IDs.
    let mut result = HashMap::new();

    for idx in g.node_identifiers() {
        let i = g.to_index(idx);
        let score = cb[i];

        if let Some(scc_node) = g.node_weight(idx) {
            for member in &scc_node.members {
                result.insert(member.clone(), score);
            }
        }
    }

    result
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
    fn empty_graph_returns_empty() {
        let ng = make_normalized_nodes(&[], &[]);
        let bc = betweenness_centrality(&ng);
        assert!(bc.is_empty());
    }

    #[test]
    fn single_node_zero_betweenness() {
        let ng = make_normalized_nodes(&["A"], &[]);
        let bc = betweenness_centrality(&ng);
        assert_eq!(bc.get("A"), Some(&0.0));
    }

    #[test]
    fn linear_chain_middle_node_has_betweenness() {
        // A → B → C
        // B is on the shortest path from A to C.
        // Betweenness of A = 0, B = 1.0, C = 0
        let ng = make_normalized(&[("A", "B"), ("B", "C")]);
        let bc = betweenness_centrality(&ng);

        assert!(
            (bc["A"] - 0.0).abs() < 1e-10,
            "A has no betweenness (source/leaf)"
        );
        assert!(
            (bc["B"] - 1.0).abs() < 1e-10,
            "B has betweenness 1.0 (on path A→C)"
        );
        assert!(
            (bc["C"] - 0.0).abs() < 1e-10,
            "C has no betweenness (source/leaf)"
        );
    }

    #[test]
    fn star_topology_center_has_zero_betweenness() {
        // A → C, B → C, D → C (star with C as sink)
        // No shortest paths pass *through* C (C is always an endpoint).
        // No shortest paths pass through any leaf either.
        let ng = make_normalized(&[("A", "C"), ("B", "C"), ("D", "C")]);
        let bc = betweenness_centrality(&ng);

        for id in ["A", "B", "C", "D"] {
            assert!(
                (bc[id] - 0.0).abs() < 1e-10,
                "{id} betweenness should be 0 in a star"
            );
        }
    }

    #[test]
    fn diamond_graph_betweenness() {
        // A → B → D, A → C → D
        // B and C each lie on one of the two shortest paths from A to D.
        // Betweenness of B = 0.5 (on 1/2 of shortest A→D paths × 1 pair).
        // Wait — in a directed graph: paths from A to D: A→B→D and A→C→D.
        // There are 2 shortest paths of length 2.
        // B lies on 1 of 2 shortest paths A→D = 1/2.
        // Similarly C lies on 1 of 2.
        // No other pairs have B or C as intermediaries.
        //
        // So betweenness(B) = 0.5, betweenness(C) = 0.5, A = 0, D = 0.
        let ng = make_normalized(&[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D")]);
        let bc = betweenness_centrality(&ng);

        assert!((bc["A"] - 0.0).abs() < 1e-10, "A source betweenness = 0");
        assert!(
            (bc["B"] - 0.5).abs() < 1e-10,
            "B on half of A→D shortest paths: got {}",
            bc["B"]
        );
        assert!(
            (bc["C"] - 0.5).abs() < 1e-10,
            "C on half of A→D shortest paths: got {}",
            bc["C"]
        );
        assert!((bc["D"] - 0.0).abs() < 1e-10, "D sink betweenness = 0");
    }

    #[test]
    fn chain_of_four_betweenness() {
        // A → B → C → D
        // B is on paths: A→C (1), A→D (1)  → betweenness = 2.0
        // C is on paths: A→D (1), B→D (1)  → betweenness = 2.0
        let ng = make_normalized(&[("A", "B"), ("B", "C"), ("C", "D")]);
        let bc = betweenness_centrality(&ng);

        assert!((bc["A"] - 0.0).abs() < 1e-10, "A betweenness = 0");
        assert!(
            (bc["B"] - 2.0).abs() < 1e-10,
            "B betweenness = 2.0, got {}",
            bc["B"]
        );
        assert!(
            (bc["C"] - 2.0).abs() < 1e-10,
            "C betweenness = 2.0, got {}",
            bc["C"]
        );
        assert!((bc["D"] - 0.0).abs() < 1e-10, "D betweenness = 0");
    }

    #[test]
    fn disconnected_components_no_cross_betweenness() {
        // A → B and C → D (disconnected)
        // No shortest paths cross components.
        let ng = make_normalized(&[("A", "B"), ("C", "D")]);
        let bc = betweenness_centrality(&ng);

        for id in ["A", "B", "C", "D"] {
            assert!(
                (bc[id] - 0.0).abs() < 1e-10,
                "{id} betweenness = 0 in disconnected pairs"
            );
        }
    }

    #[test]
    fn cycle_members_share_betweenness() {
        // A → B → A (cycle), A → C
        // After condensation: {A,B} → {C}
        // Simple 2-node DAG: no intermediate nodes → all betweenness = 0
        let ng = make_normalized(&[("A", "B"), ("B", "A"), ("A", "C")]);
        let bc = betweenness_centrality(&ng);

        // A and B are in the same SCC → same score
        assert!(
            (bc["A"] - bc["B"]).abs() < 1e-10,
            "SCC members should have equal betweenness"
        );
    }
}
