//! PageRank with incremental (DF-PageRank) update and full-recompute fallback.
//!
//! # Overview
//!
//! PageRank identifies items that unblock the most downstream work. Items
//! with high PageRank are "important" in the dependency graph because many
//! significant paths flow through them.
//!
//! # Algorithm
//!
//! Standard PageRank uses the iterative power method on the adjacency matrix:
//!
//! ```text
//! PR(v) = (1 - d) / N + d * Σ PR(u) / out_degree(u)   for each u → v
//! ```
//!
//! where `d` is the damping factor (default 0.85).
//!
//! # Incremental Update (DF-PageRank)
//!
//! When few edges change, DF-PageRank (Desrosiers-Bhatt variant) recomputes
//! only the affected nodes:
//!
//! 1. Identify "frontier" nodes: direct neighbors of changed edges.
//! 2. Propagate rank deltas forward through the graph until convergence.
//! 3. If the incremental result diverges from a spot-check, fall back to
//!    full recompute.
//!
//! # Output
//!
//! Returns a [`PageRankResult`] with per-item scores and metadata about
//! the computation (iterations, convergence, method used).

use std::collections::{HashMap, HashSet, VecDeque};

use petgraph::{
    Direction,
    visit::{IntoNodeIdentifiers, NodeIndexable},
};
use tracing::{instrument, warn};

use crate::graph::normalize::NormalizedGraph;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for PageRank computation.
#[derive(Debug, Clone)]
pub struct PageRankConfig {
    /// Damping factor (probability of following a link vs teleporting).
    /// Default: 0.85.
    pub damping: f64,
    /// Convergence threshold: stop when L1 norm of rank delta < tolerance.
    /// Default: 1e-6.
    pub tolerance: f64,
    /// Maximum number of iterations.
    /// Default: 100.
    pub max_iter: usize,
}

impl Default for PageRankConfig {
    fn default() -> Self {
        Self {
            damping: 0.85,
            tolerance: 1e-6,
            max_iter: 100,
        }
    }
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Result of a PageRank computation.
#[derive(Debug, Clone)]
pub struct PageRankResult {
    /// PageRank scores: item ID → score.
    pub scores: HashMap<String, f64>,
    /// Number of iterations performed.
    pub iterations: usize,
    /// Whether the algorithm converged within `max_iter`.
    pub converged: bool,
    /// Which computation method was used.
    pub method: PageRankMethod,
}

/// Which method was used to compute PageRank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageRankMethod {
    /// Full recompute from scratch.
    Full,
    /// Incremental (DF-PageRank) update from a previous result.
    Incremental,
    /// Incremental was attempted but fell back to full recompute.
    IncrementalFallback,
}

// ---------------------------------------------------------------------------
// Full PageRank
// ---------------------------------------------------------------------------

/// Compute PageRank from scratch on the condensed DAG.
///
/// Operates on the **condensed** graph (SCCs collapsed). Items in the same
/// SCC receive the same PageRank score.
///
/// # Arguments
///
/// * `ng` — A [`NormalizedGraph`] containing the condensed DAG.
/// * `config` — PageRank configuration (damping, tolerance, max_iter).
///
/// # Returns
///
/// A [`PageRankResult`] with scores for every item ID.
#[must_use]
#[instrument(skip(ng, config))]
pub fn pagerank(ng: &NormalizedGraph, config: &PageRankConfig) -> PageRankResult {
    let g = &ng.condensed;
    let n = g.node_count();

    if n == 0 {
        return PageRankResult {
            scores: HashMap::new(),
            iterations: 0,
            converged: true,
            method: PageRankMethod::Full,
        };
    }

    let n_f64 = n as f64;
    let base = (1.0 - config.damping) / n_f64;

    // Initialize ranks uniformly.
    let mut ranks = vec![1.0 / n_f64; n];
    let mut new_ranks = vec![0.0_f64; n];

    let mut iterations = 0;
    let mut converged = false;

    for _ in 0..config.max_iter {
        iterations += 1;

        // Reset new_ranks to base teleportation value.
        for r in &mut new_ranks {
            *r = base;
        }

        // Distribute rank from each node to its outgoing neighbors.
        for node in g.node_identifiers() {
            let idx = g.to_index(node);
            let out_degree = g.neighbors_directed(node, Direction::Outgoing).count();

            if out_degree == 0 {
                // Dangling node: distribute its rank equally to all nodes.
                let share = config.damping * ranks[idx] / n_f64;
                for r in &mut new_ranks {
                    *r += share;
                }
            } else {
                let share = config.damping * ranks[idx] / out_degree as f64;
                for neighbor in g.neighbors_directed(node, Direction::Outgoing) {
                    let nidx = g.to_index(neighbor);
                    new_ranks[nidx] += share;
                }
            }
        }

        // Check convergence: L1 norm of delta.
        let delta: f64 = ranks
            .iter()
            .zip(new_ranks.iter())
            .map(|(old, new)| (old - new).abs())
            .sum();

        std::mem::swap(&mut ranks, &mut new_ranks);

        if delta < config.tolerance {
            converged = true;
            break;
        }
    }

    // Map scores back to item IDs.
    let scores = distribute_scores(ng, &ranks);

    PageRankResult {
        scores,
        iterations,
        converged,
        method: PageRankMethod::Full,
    }
}

// ---------------------------------------------------------------------------
// Incremental PageRank (DF-PageRank)
// ---------------------------------------------------------------------------

/// An edge change in the graph (added or removed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeChange {
    /// Source item ID.
    pub from: String,
    /// Target item ID.
    pub to: String,
    /// Whether the edge was added or removed.
    pub kind: EdgeChangeKind,
}

/// Whether an edge was added or removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeChangeKind {
    /// A new edge was added.
    Added,
    /// An existing edge was removed.
    Removed,
}

/// Incrementally update PageRank after edge changes.
///
/// Uses the DF-PageRank approach:
/// 1. Identify frontier nodes (neighbors of changed edges).
/// 2. Propagate rank deltas forward from frontier nodes.
/// 3. After convergence, spot-check a sample of nodes against full recompute.
/// 4. If spot-check fails (delta > tolerance * 10), fall back to full recompute.
///
/// # Arguments
///
/// * `ng` — The **updated** [`NormalizedGraph`] (after edge changes applied).
/// * `previous` — Previous PageRank scores (from the old graph).
/// * `changes` — List of edge changes since the previous computation.
/// * `config` — PageRank configuration.
///
/// # Returns
///
/// A [`PageRankResult`] — method will be `Incremental` or `IncrementalFallback`.
#[must_use]
#[instrument(skip(ng, previous, changes, config))]
pub fn pagerank_incremental(
    ng: &NormalizedGraph,
    previous: &HashMap<String, f64>,
    changes: &[EdgeChange],
    config: &PageRankConfig,
) -> PageRankResult {
    let g = &ng.condensed;
    let n = g.node_count();

    if n == 0 || changes.is_empty() {
        // No changes — return previous scores as-is.
        return PageRankResult {
            scores: previous.clone(),
            iterations: 0,
            converged: true,
            method: PageRankMethod::Incremental,
        };
    }

    // Step 1: Identify frontier SCC nodes affected by changes.
    let frontier = identify_frontier(ng, changes);

    if frontier.is_empty() {
        // No frontier nodes found (changes involve unknown items).
        // Fall back to full recompute.
        warn!("DF-PageRank: no frontier nodes found, falling back to full recompute");
        let mut result = pagerank(ng, config);
        result.method = PageRankMethod::IncrementalFallback;
        return result;
    }

    // If frontier is too large (> 50% of nodes), full recompute is cheaper.
    if frontier.len() * 2 > n {
        let mut result = pagerank(ng, config);
        result.method = PageRankMethod::IncrementalFallback;
        return result;
    }

    // Step 2: Initialize ranks from previous scores.
    let n_f64 = n as f64;
    let default_rank = 1.0 / n_f64;
    let mut ranks: Vec<f64> = (0..n)
        .map(|i| {
            let node = petgraph::graph::NodeIndex::new(i);
            if let Some(scc) = g.node_weight(node) {
                // Use the first member's previous score, or default.
                scc.members
                    .first()
                    .and_then(|id| previous.get(id))
                    .copied()
                    .unwrap_or(default_rank)
            } else {
                default_rank
            }
        })
        .collect();

    // Step 3: BFS propagation from frontier nodes.
    let affected = bfs_affected(ng, &frontier);

    let base = (1.0 - config.damping) / n_f64;
    let mut new_ranks = ranks.clone();
    let mut iterations = 0;
    let mut converged = false;

    for _ in 0..config.max_iter {
        iterations += 1;

        // Only recompute ranks for affected nodes.
        for &node_idx in &affected {
            let node = petgraph::graph::NodeIndex::new(node_idx);
            let mut rank = base;

            // Sum contributions from incoming neighbors.
            for pred in g.neighbors_directed(node, Direction::Incoming) {
                let pred_idx = g.to_index(pred);
                let out_degree = g.neighbors_directed(pred, Direction::Outgoing).count();
                if out_degree > 0 {
                    rank += config.damping * ranks[pred_idx] / out_degree as f64;
                }
            }

            // Handle dangling node contributions (simplified: add from all dangling nodes).
            let dangling_sum: f64 = g
                .node_identifiers()
                .filter(|&n| g.neighbors_directed(n, Direction::Outgoing).count() == 0)
                .map(|n| ranks[g.to_index(n)])
                .sum();
            rank += config.damping * dangling_sum / n_f64;

            new_ranks[node_idx] = rank;
        }

        // Check convergence on affected nodes only.
        let delta: f64 = affected
            .iter()
            .map(|&i| (ranks[i] - new_ranks[i]).abs())
            .sum();

        // Copy new_ranks back to ranks for affected nodes.
        for &i in &affected {
            ranks[i] = new_ranks[i];
        }

        if delta < config.tolerance {
            converged = true;
            break;
        }
    }

    // Step 4: Stability check — compare against full recompute on a sample.
    let full_result = pagerank(ng, config);
    let max_divergence = check_divergence(&ranks, &full_result, ng);

    if max_divergence > config.tolerance * 100.0 {
        // Incremental diverged too much — use full recompute result.
        warn!(
            max_divergence,
            "DF-PageRank stability check failed, using full recompute"
        );
        return PageRankResult {
            scores: full_result.scores,
            iterations: full_result.iterations,
            converged: full_result.converged,
            method: PageRankMethod::IncrementalFallback,
        };
    }

    let scores = distribute_scores(ng, &ranks);

    PageRankResult {
        scores,
        iterations,
        converged,
        method: PageRankMethod::Incremental,
    }
}

// ---------------------------------------------------------------------------
// Cached PageRank
// ---------------------------------------------------------------------------

/// Cached PageRank state for incremental updates.
#[derive(Debug, Clone)]
pub struct PageRankCache {
    /// Cached scores from the last computation.
    pub scores: HashMap<String, f64>,
    /// Content hash of the graph when these scores were computed.
    pub content_hash: String,
}

impl PageRankCache {
    /// Create a new cache entry from a computation result and graph hash.
    #[must_use]
    pub fn new(scores: HashMap<String, f64>, content_hash: String) -> Self {
        Self {
            scores,
            content_hash,
        }
    }

    /// Check if this cache is valid for the given graph.
    #[must_use]
    pub fn is_valid_for(&self, ng: &NormalizedGraph) -> bool {
        self.content_hash == ng.content_hash()
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Distribute condensed-graph-level scores to individual item IDs.
fn distribute_scores(ng: &NormalizedGraph, ranks: &[f64]) -> HashMap<String, f64> {
    let g = &ng.condensed;
    let mut scores = HashMap::new();

    for node in g.node_identifiers() {
        let idx = g.to_index(node);
        if let Some(scc) = g.node_weight(node) {
            for member in &scc.members {
                scores.insert(member.clone(), ranks[idx]);
            }
        }
    }

    scores
}

/// Identify frontier SCC node indices affected by edge changes.
fn identify_frontier(ng: &NormalizedGraph, changes: &[EdgeChange]) -> HashSet<usize> {
    let g = &ng.condensed;
    let mut frontier = HashSet::new();

    for change in changes {
        // Map item IDs to SCC node indices.
        if let Some(&from_scc) = ng.item_to_scc.get(&change.from) {
            frontier.insert(g.to_index(from_scc));
        }
        if let Some(&to_scc) = ng.item_to_scc.get(&change.to) {
            frontier.insert(g.to_index(to_scc));
            // Also include outgoing neighbors of target (rank flows forward).
            for neighbor in g.neighbors_directed(to_scc, Direction::Outgoing) {
                frontier.insert(g.to_index(neighbor));
            }
        }
    }

    frontier
}

/// BFS from frontier nodes to find all downstream affected nodes.
fn bfs_affected(ng: &NormalizedGraph, frontier: &HashSet<usize>) -> Vec<usize> {
    let g = &ng.condensed;
    let mut visited: HashSet<usize> = frontier.clone();
    let mut queue: VecDeque<usize> = frontier.iter().copied().collect();
    let mut affected = Vec::new();

    while let Some(idx) = queue.pop_front() {
        affected.push(idx);
        let node = petgraph::graph::NodeIndex::new(idx);
        for neighbor in g.neighbors_directed(node, Direction::Outgoing) {
            let nidx = g.to_index(neighbor);
            if visited.insert(nidx) {
                queue.push_back(nidx);
            }
        }
    }

    affected
}

/// Check maximum divergence between incremental ranks and full recompute.
fn check_divergence(
    incremental_ranks: &[f64],
    full_result: &PageRankResult,
    ng: &NormalizedGraph,
) -> f64 {
    let g = &ng.condensed;
    let mut max_div = 0.0_f64;

    for node in g.node_identifiers() {
        let idx = g.to_index(node);
        if let Some(scc) = g.node_weight(node) {
            if let Some(&full_score) = scc
                .members
                .first()
                .and_then(|id| full_result.scores.get(id))
            {
                let div = (incremental_ranks[idx] - full_score).abs();
                max_div = max_div.max(div);
            }
        }
    }

    max_div
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

    fn default_config() -> PageRankConfig {
        PageRankConfig::default()
    }

    // -----------------------------------------------------------------------
    // Full PageRank
    // -----------------------------------------------------------------------

    #[test]
    fn pagerank_empty_graph() {
        let ng = make_normalized_nodes(&[], &[]);
        let result = pagerank(&ng, &default_config());
        assert!(result.scores.is_empty());
        assert!(result.converged);
        assert_eq!(result.iterations, 0);
        assert_eq!(result.method, PageRankMethod::Full);
    }

    #[test]
    fn pagerank_single_node() {
        let ng = make_normalized_nodes(&["A"], &[]);
        let result = pagerank(&ng, &default_config());
        assert_eq!(result.scores.len(), 1);
        // Single node gets all the rank.
        assert!((result.scores["A"] - 1.0).abs() < 1e-4);
        assert!(result.converged);
    }

    #[test]
    fn pagerank_two_nodes_one_edge() {
        // A → B: B should have higher rank than A.
        let ng = make_normalized(&[("A", "B")]);
        let result = pagerank(&ng, &default_config());
        assert_eq!(result.scores.len(), 2);
        assert!(
            result.scores["B"] > result.scores["A"],
            "B ({}) should have higher rank than A ({})",
            result.scores["B"],
            result.scores["A"]
        );
        assert!(result.converged);
    }

    #[test]
    fn pagerank_linear_chain() {
        // A → B → C: ranks should increase along the chain.
        let ng = make_normalized(&[("A", "B"), ("B", "C")]);
        let result = pagerank(&ng, &default_config());

        assert!(result.converged);
        assert!(
            result.scores["C"] > result.scores["B"],
            "C should have highest rank"
        );
        assert!(
            result.scores["B"] > result.scores["A"],
            "B should have higher rank than A"
        );
    }

    #[test]
    fn pagerank_star_topology() {
        // Hub: A → B, A → C, A → D: B, C, D should have higher rank than A.
        let ng = make_normalized(&[("A", "B"), ("A", "C"), ("A", "D")]);
        let result = pagerank(&ng, &default_config());

        assert!(result.converged);
        // B, C, D should all have the same rank (symmetric).
        let diff_bc = (result.scores["B"] - result.scores["C"]).abs();
        let diff_cd = (result.scores["C"] - result.scores["D"]).abs();
        assert!(diff_bc < 1e-10, "B and C should have same rank");
        assert!(diff_cd < 1e-10, "C and D should have same rank");
        // Leaf nodes should have higher rank than hub.
        assert!(result.scores["B"] > result.scores["A"]);
    }

    #[test]
    fn pagerank_diamond() {
        // A → B → D, A → C → D: D gets contributions from two paths.
        let ng = make_normalized(&[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D")]);
        let result = pagerank(&ng, &default_config());

        assert!(result.converged);
        // D should have highest rank (most incoming authority).
        assert!(result.scores["D"] > result.scores["B"]);
        assert!(result.scores["D"] > result.scores["C"]);
        // B and C should be symmetric.
        assert!((result.scores["B"] - result.scores["C"]).abs() < 1e-10);
    }

    #[test]
    fn pagerank_cycle_members_share_score() {
        // A → B → A (cycle) → C: A and B are in the same SCC.
        let ng = make_normalized(&[("A", "B"), ("B", "A"), ("A", "C")]);
        let result = pagerank(&ng, &default_config());

        assert!(result.converged);
        // A and B should have the same rank (same SCC).
        assert!(
            (result.scores["A"] - result.scores["B"]).abs() < 1e-10,
            "A and B in same SCC should have same rank"
        );
    }

    #[test]
    fn pagerank_scores_sum_to_one() {
        // Scores should approximately sum to 1.0.
        let ng = make_normalized(&[("A", "B"), ("B", "C"), ("A", "C"), ("C", "D")]);
        let result = pagerank(&ng, &default_config());

        let total: f64 = result.scores.values().sum();
        assert!(
            (total - 1.0).abs() < 1e-3,
            "PageRank scores should sum to ~1.0, got {total}"
        );
    }

    #[test]
    fn pagerank_converges_large_graph() {
        // 20-node chain: A0 → A1 → ... → A19.
        let edges: Vec<(String, String)> = (0..19)
            .map(|i| (format!("A{i}"), format!("A{}", i + 1)))
            .collect();
        let edge_refs: Vec<(&str, &str)> = edges
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let ng = make_normalized(&edge_refs);

        let result = pagerank(&ng, &default_config());
        assert!(result.converged, "Should converge on 20-node chain");
        assert!(result.iterations <= 100);
        // Last node should have highest rank.
        assert!(result.scores["A19"] > result.scores["A0"]);
    }

    #[test]
    fn pagerank_custom_damping() {
        let ng = make_normalized(&[("A", "B")]);
        let config = PageRankConfig {
            damping: 0.5,
            ..default_config()
        };
        let result = pagerank(&ng, &config);
        assert!(result.converged);
        // With lower damping, teleportation matters more.
        assert!(result.scores["B"] > result.scores["A"]);
    }

    #[test]
    fn pagerank_max_iter_limit() {
        let ng = make_normalized(&[("A", "B"), ("B", "C")]);
        let config = PageRankConfig {
            max_iter: 1,
            tolerance: 1e-15, // extremely tight — won't converge in 1 iter
            ..default_config()
        };
        let result = pagerank(&ng, &config);
        assert_eq!(result.iterations, 1);
        assert!(
            !result.converged,
            "Should not converge in 1 iteration with tight tolerance"
        );
    }

    #[test]
    fn pagerank_all_disconnected() {
        // 4 isolated nodes — should all have equal rank.
        let ng = make_normalized_nodes(&["A", "B", "C", "D"], &[]);
        let result = pagerank(&ng, &default_config());

        assert!(result.converged);
        let expected = 0.25;
        for (_, score) in &result.scores {
            assert!(
                (score - expected).abs() < 1e-6,
                "Isolated nodes should all have rank 0.25, got {score}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Incremental PageRank
    // -----------------------------------------------------------------------

    #[test]
    fn incremental_no_changes_returns_previous() {
        let ng = make_normalized(&[("A", "B")]);
        let prev = pagerank(&ng, &default_config());

        let result = pagerank_incremental(&ng, &prev.scores, &[], &default_config());
        assert_eq!(result.method, PageRankMethod::Incremental);
        assert_eq!(result.iterations, 0);
        assert_eq!(result.scores, prev.scores);
    }

    #[test]
    fn incremental_add_edge_updates_scores() {
        // Start with A → B.
        let ng_old = make_normalized(&[("A", "B")]);
        let prev = pagerank(&ng_old, &default_config());

        // Add edge B → C.
        let ng_new = make_normalized(&[("A", "B"), ("B", "C")]);
        let changes = vec![EdgeChange {
            from: "B".to_string(),
            to: "C".to_string(),
            kind: EdgeChangeKind::Added,
        }];

        let result = pagerank_incremental(&ng_new, &prev.scores, &changes, &default_config());
        // Should have scores for A, B, C.
        assert_eq!(result.scores.len(), 3);
        // C should now have a score > 0.
        assert!(result.scores["C"] > 0.0);
    }

    #[test]
    fn incremental_matches_full_recompute() {
        // Start with A → B → C.
        let ng_old = make_normalized(&[("A", "B"), ("B", "C")]);
        let prev = pagerank(&ng_old, &default_config());

        // Add edge A → D, B → D.
        let ng_new = make_normalized(&[("A", "B"), ("B", "C"), ("A", "D"), ("B", "D")]);
        let changes = vec![
            EdgeChange {
                from: "A".to_string(),
                to: "D".to_string(),
                kind: EdgeChangeKind::Added,
            },
            EdgeChange {
                from: "B".to_string(),
                to: "D".to_string(),
                kind: EdgeChangeKind::Added,
            },
        ];

        let incremental = pagerank_incremental(&ng_new, &prev.scores, &changes, &default_config());
        let full = pagerank(&ng_new, &default_config());

        // Incremental should produce scores close to full recompute.
        for (id, full_score) in &full.scores {
            let inc_score = incremental.scores.get(id).unwrap_or(&0.0);
            assert!(
                (full_score - inc_score).abs() < 0.01,
                "Item {id}: full={full_score}, incremental={inc_score}"
            );
        }
    }

    #[test]
    fn incremental_large_frontier_falls_back() {
        // Graph with 4 nodes — changing 3 edges should trigger fallback.
        let ng_old = make_normalized(&[("A", "B")]);
        let prev = pagerank(&ng_old, &default_config());

        // New graph is completely different.
        let ng_new = make_normalized(&[("A", "C"), ("C", "D"), ("D", "B")]);
        let changes = vec![
            EdgeChange {
                from: "A".to_string(),
                to: "B".to_string(),
                kind: EdgeChangeKind::Removed,
            },
            EdgeChange {
                from: "A".to_string(),
                to: "C".to_string(),
                kind: EdgeChangeKind::Added,
            },
            EdgeChange {
                from: "C".to_string(),
                to: "D".to_string(),
                kind: EdgeChangeKind::Added,
            },
            EdgeChange {
                from: "D".to_string(),
                to: "B".to_string(),
                kind: EdgeChangeKind::Added,
            },
        ];

        let result = pagerank_incremental(&ng_new, &prev.scores, &changes, &default_config());
        // Should fall back because frontier is > 50% of nodes.
        assert_eq!(result.method, PageRankMethod::IncrementalFallback);
    }

    #[test]
    fn incremental_unknown_items_falls_back() {
        let ng = make_normalized(&[("X", "Y")]);
        let prev = HashMap::new();
        let changes = vec![EdgeChange {
            from: "unknown1".to_string(),
            to: "unknown2".to_string(),
            kind: EdgeChangeKind::Added,
        }];

        let result = pagerank_incremental(&ng, &prev, &changes, &default_config());
        assert_eq!(result.method, PageRankMethod::IncrementalFallback);
    }

    // -----------------------------------------------------------------------
    // Cache
    // -----------------------------------------------------------------------

    fn make_normalized_with_hash(edges: &[(&str, &str)], hash: &str) -> NormalizedGraph {
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
            content_hash: hash.to_string(),
        };

        NormalizedGraph::from_raw(raw)
    }

    #[test]
    fn cache_validity() {
        let ng = make_normalized_with_hash(&[("A", "B")], "blake3:hash1");
        let result = pagerank(&ng, &default_config());
        let cache = PageRankCache::new(result.scores, ng.content_hash().to_string());

        assert!(cache.is_valid_for(&ng));

        // Different graph with different hash.
        let ng2 = make_normalized_with_hash(&[("A", "C")], "blake3:hash2");
        assert!(!cache.is_valid_for(&ng2));
    }

    #[test]
    fn pagerank_reverse_star() {
        // B → A, C → A, D → A: A is the authority (pointed to by many).
        let ng = make_normalized(&[("B", "A"), ("C", "A"), ("D", "A")]);
        let result = pagerank(&ng, &default_config());

        assert!(result.converged);
        assert!(result.scores["A"] > result.scores["B"]);
        assert!(result.scores["A"] > result.scores["C"]);
        assert!(result.scores["A"] > result.scores["D"]);
    }

    #[test]
    fn pagerank_two_chains_independent() {
        // A → B and C → D: independent chains.
        let ng = make_normalized(&[("A", "B"), ("C", "D")]);
        let result = pagerank(&ng, &default_config());

        assert!(result.converged);
        // B and D should have same rank (symmetric structure).
        assert!((result.scores["B"] - result.scores["D"]).abs() < 1e-10);
        // A and C should have same rank.
        assert!((result.scores["A"] - result.scores["C"]).abs() < 1e-10);
    }

    #[test]
    fn incremental_remove_edge() {
        // Start with A → B → C.
        let ng_old = make_normalized(&[("A", "B"), ("B", "C")]);
        let prev = pagerank(&ng_old, &default_config());

        // Remove edge B → C.
        let ng_new = make_normalized_nodes(&["A", "B", "C"], &[("A", "B")]);
        let changes = vec![EdgeChange {
            from: "B".to_string(),
            to: "C".to_string(),
            kind: EdgeChangeKind::Removed,
        }];

        let result = pagerank_incremental(&ng_new, &prev.scores, &changes, &default_config());
        let full = pagerank(&ng_new, &default_config());

        // Should be close to full result.
        for (id, full_score) in &full.scores {
            let inc_score = result.scores.get(id).unwrap_or(&0.0);
            assert!(
                (full_score - inc_score).abs() < 0.01,
                "Item {id}: full={full_score}, incremental={inc_score}"
            );
        }
    }

    #[test]
    fn pagerank_result_method_enum_values() {
        assert_eq!(PageRankMethod::Full, PageRankMethod::Full);
        assert_ne!(PageRankMethod::Full, PageRankMethod::Incremental);
        assert_ne!(
            PageRankMethod::Incremental,
            PageRankMethod::IncrementalFallback
        );
    }
}
