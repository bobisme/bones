//! Eigenvector centrality via power iteration.
//!
//! # Overview
//!
//! Eigenvector centrality scores nodes based on the idea that connections to
//! high-scoring nodes contribute more to a node's score. It's the dominant
//! eigenvector of the adjacency matrix.
//!
//! # Algorithm
//!
//! Power iteration on the adjacency matrix:
//!
//! 1. Initialize scores uniformly.
//! 2. For each node `v`: `score(v) = sum of score(u) for all u → v`.
//! 3. Normalize the score vector to unit L2 norm.
//! 4. Repeat until convergence or max iterations.
//!
//! For disconnected or DAG graphs, some components may converge to zero.
//! This is expected — only nodes in the strongly connected component
//! containing the dominant eigenvector get non-zero scores in the limit.
//!
//! Since our condensed graph is a DAG (no cycles), pure power iteration
//! would converge to zero for all nodes. We handle this by:
//! - Using the **undirected** version of the condensed graph for eigenvector
//!   centrality (treating edges as undirected), which preserves meaningful
//!   scores for DAGs.
//!
//! # Output
//!
//! Returns a `HashMap<String, f64>` mapping item IDs to their eigenvector
//! centrality scores. Items in the same SCC share the same score.

use std::collections::HashMap;

use petgraph::{
    graph::NodeIndex,
    visit::{IntoNodeIdentifiers, NodeIndexable},
    Direction,
};
use tracing::instrument;

use crate::graph::normalize::NormalizedGraph;

/// Result of eigenvector centrality computation.
#[derive(Debug, Clone)]
pub struct EigenvectorResult {
    /// Eigenvector centrality scores: item ID → score.
    pub scores: HashMap<String, f64>,
    /// Number of iterations performed.
    pub iterations: usize,
    /// Whether the algorithm converged within `max_iter`.
    pub converged: bool,
}

/// Compute eigenvector centrality for all items in the graph.
///
/// Uses the **undirected** interpretation of the condensed DAG: an edge
/// between two SCC nodes contributes to both nodes' scores regardless of
/// direction. This gives meaningful results for DAGs (where the directed
/// version would converge to zero).
///
/// # Arguments
///
/// * `ng` — A [`NormalizedGraph`] containing the condensed DAG.
/// * `max_iter` — Maximum number of iterations.
/// * `tolerance` — Convergence threshold: stop when the L2 norm of the
///   change in scores is below this value.
///
/// # Returns
///
/// An [`EigenvectorResult`] with scores for each item ID.
#[must_use]
#[instrument(skip(ng))]
pub fn eigenvector_centrality(
    ng: &NormalizedGraph,
    max_iter: usize,
    tolerance: f64,
) -> EigenvectorResult {
    let g = &ng.condensed;
    let n = g.node_count();

    if n == 0 {
        return EigenvectorResult {
            scores: HashMap::new(),
            iterations: 0,
            converged: true,
        };
    }

    // Initialize scores uniformly.
    let init_val = 1.0 / (n as f64).sqrt();
    let mut scores: Vec<f64> = vec![init_val; n];

    let mut converged = false;
    let mut iterations = 0;

    // Build adjacency lists for the undirected interpretation.
    // For each node, neighbors = incoming ∪ outgoing.
    let neighbors: Vec<Vec<NodeIndex>> = g
        .node_identifiers()
        .map(|v| {
            let mut nbrs: Vec<NodeIndex> = Vec::new();
            for u in g.neighbors_directed(v, Direction::Incoming) {
                nbrs.push(u);
            }
            for w in g.neighbors_directed(v, Direction::Outgoing) {
                if !nbrs.contains(&w) {
                    nbrs.push(w);
                }
            }
            nbrs
        })
        .collect();

    for iter in 0..max_iter {
        iterations = iter + 1;

        let mut new_scores = vec![0.0; n];

        for v in g.node_identifiers() {
            let vi = g.to_index(v);
            for &u in &neighbors[vi] {
                let ui = g.to_index(u);
                new_scores[vi] += scores[ui];
            }
        }

        // Normalize to unit L2 norm.
        let norm: f64 = new_scores.iter().map(|x| x * x).sum::<f64>().sqrt();
        if norm > 0.0 {
            for x in new_scores.iter_mut() {
                *x /= norm;
            }
        }

        // Check convergence.
        let diff: f64 = scores
            .iter()
            .zip(new_scores.iter())
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f64>()
            .sqrt();

        scores = new_scores;

        if diff < tolerance {
            converged = true;
            break;
        }
    }

    // Map condensed node scores back to item IDs.
    let mut result = HashMap::new();

    for idx in g.node_identifiers() {
        let i = g.to_index(idx);
        let score = scores[i];

        if let Some(scc_node) = g.node_weight(idx) {
            for member in &scc_node.members {
                result.insert(member.clone(), score);
            }
        }
    }

    EigenvectorResult {
        scores: result,
        iterations,
        converged,
    }
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
        let result = eigenvector_centrality(&make_normalized_nodes(&[], &[]), 100, 1e-6);
        assert!(result.scores.is_empty());
        assert!(result.converged);
    }

    #[test]
    fn single_node_has_score() {
        let result = eigenvector_centrality(&make_normalized_nodes(&["A"], &[]), 100, 1e-6);
        // Single isolated node: no neighbors, so score goes to 0.
        assert!(result.scores.contains_key("A"));
    }

    #[test]
    fn simple_pair_equal_scores() {
        // A → B (undirected: A—B)
        // Both should converge to equal scores (symmetric when undirected).
        let result = eigenvector_centrality(&make_normalized(&[("A", "B")]), 100, 1e-6);

        let score_a = result.scores["A"];
        let score_b = result.scores["B"];

        assert!(
            (score_a - score_b).abs() < 1e-6,
            "Pair should have equal eigenvector centrality: A={score_a} B={score_b}"
        );
        assert!(result.converged, "Should converge for simple pair");
    }

    #[test]
    fn star_center_highest_eigenvector() {
        // A → B, A → C, A → D (undirected star centered on A)
        // A has 3 neighbors, B/C/D have 1 each.
        // A should have the highest eigenvector centrality.
        let ng = make_normalized(&[("A", "B"), ("A", "C"), ("A", "D")]);
        let result = eigenvector_centrality(&ng, 100, 1e-6);

        assert!(
            result.scores["A"] > result.scores["B"],
            "Center A should have higher score than leaf B: A={} B={}",
            result.scores["A"],
            result.scores["B"]
        );

        // B, C, D should have equal scores (symmetric leaves).
        assert!(
            (result.scores["B"] - result.scores["C"]).abs() < 1e-6,
            "Leaves should be equal"
        );
        assert!(
            (result.scores["C"] - result.scores["D"]).abs() < 1e-6,
            "Leaves should be equal"
        );
    }

    #[test]
    fn chain_middle_nodes_highest() {
        // A → B → C → D (undirected: A—B—C—D)
        // In a path graph, middle nodes have higher eigenvector centrality.
        // For path of length 4: B and C should have higher scores than A and D.
        let ng = make_normalized(&[("A", "B"), ("B", "C"), ("C", "D")]);
        let result = eigenvector_centrality(&ng, 100, 1e-6);

        assert!(
            result.scores["B"] > result.scores["A"],
            "B should have higher score than endpoint A: B={} A={}",
            result.scores["B"],
            result.scores["A"]
        );
        assert!(
            result.scores["C"] > result.scores["D"],
            "C should have higher score than endpoint D: C={} D={}",
            result.scores["C"],
            result.scores["D"]
        );
        // B and C should be equal (by symmetry of the path).
        assert!(
            (result.scores["B"] - result.scores["C"]).abs() < 1e-6,
            "Middle nodes should be symmetric: B={} C={}",
            result.scores["B"],
            result.scores["C"]
        );
    }

    #[test]
    fn eigenvector_converges_for_connected_graph() {
        let ng = make_normalized(&[("A", "B"), ("B", "C"), ("C", "D"), ("A", "D")]);
        let result = eigenvector_centrality(&ng, 1000, 1e-10);
        assert!(result.converged, "Should converge for connected graph");
    }

    #[test]
    fn disconnected_components_handled() {
        // Two disjoint edges: A → B and C → D.
        let ng = make_normalized(&[("A", "B"), ("C", "D")]);
        let result = eigenvector_centrality(&ng, 100, 1e-6);

        // All nodes should have some score.
        for id in ["A", "B", "C", "D"] {
            assert!(
                result.scores.contains_key(id),
                "{id} should have a score"
            );
        }

        // By symmetry: A≈C and B≈D (two identical disconnected components).
        assert!(
            (result.scores["A"] - result.scores["C"]).abs() < 1e-6,
            "Symmetric components: A={} C={}",
            result.scores["A"],
            result.scores["C"]
        );
    }

    #[test]
    fn cycle_members_share_eigenvector() {
        // A → B → A (cycle), A → C
        // After condensation: {A,B} → {C}
        let ng = make_normalized(&[("A", "B"), ("B", "A"), ("A", "C")]);
        let result = eigenvector_centrality(&ng, 100, 1e-6);

        // A and B are in the same SCC → same score.
        assert!(
            (result.scores["A"] - result.scores["B"]).abs() < 1e-10,
            "SCC members should have equal eigenvector scores"
        );
    }

    #[test]
    fn diamond_graph_eigenvector() {
        // A → B → D, A → C → D (diamond shape, undirected)
        // By symmetry B and C should have equal scores.
        // A and D have 2 undirected neighbors each, B and C have 2 each.
        // All nodes have degree 2 → scores should be approximately equal.
        let ng = make_normalized(&[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D")]);
        let result = eigenvector_centrality(&ng, 100, 1e-6);

        assert!(
            (result.scores["B"] - result.scores["C"]).abs() < 1e-6,
            "B and C symmetric: B={} C={}",
            result.scores["B"],
            result.scores["C"]
        );
        assert!(
            (result.scores["A"] - result.scores["D"]).abs() < 1e-6,
            "A and D symmetric: A={} D={}",
            result.scores["A"],
            result.scores["D"]
        );
    }

    #[test]
    fn scores_are_non_negative() {
        let ng = make_normalized(&[("A", "B"), ("B", "C"), ("A", "C")]);
        let result = eigenvector_centrality(&ng, 100, 1e-6);

        for (id, score) in &result.scores {
            assert!(
                *score >= 0.0,
                "Score for {id} should be non-negative: {score}"
            );
        }
    }
}
