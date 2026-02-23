//! PageRank with incremental (DF-PageRank-style) updates and guarded fallback.
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
//! # Incremental Update
//!
//! `pagerank_incremental` updates only a frontier/downstream closure and falls
//! back to full recomputation when safety checks indicate global coupling or
//! likely divergence.
//!
//! # Output
//!
//! Returns a [`PageRankResult`] with per-item scores and metadata about
//! the computation (iterations, convergence, method used).

use std::collections::{HashMap, HashSet, VecDeque};

use petgraph::{
    Direction,
    visit::{EdgeRef, IntoNodeIdentifiers, NodeIndexable},
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
// Incremental gating
// ---------------------------------------------------------------------------

// Empirical gate: incremental overhead tends to outweigh benefits on small graphs.
const INCREMENTAL_MIN_NODES: usize = 300;
const INCREMENTAL_MIN_EDGES: usize = 600;
// If too many edges changed at once, full recompute is usually cheaper/safer.
const INCREMENTAL_MAX_CHANGE_PCT: usize = 5;

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
/// Strategy:
/// 1. Build a frontier from changed edge endpoints.
/// 2. Propagate to all downstream affected nodes.
/// 3. Iterate only affected nodes while keeping unaffected inputs fixed.
/// 4. Run cheap global safety checks; if unsafe, fall back to full recompute.
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
/// A [`PageRankResult`] with method set to `Incremental` or
/// `IncrementalFallback`.
#[must_use]
#[instrument(skip(ng, _previous, _changes, config))]
pub fn pagerank_incremental(
    ng: &NormalizedGraph,
    _previous: &HashMap<String, f64>,
    _changes: &[EdgeChange],
    config: &PageRankConfig,
) -> PageRankResult {
    let g = &ng.condensed;
    let n = g.node_count();
    let edge_count = ng.raw.edge_count();

    if n == 0 {
        return PageRankResult {
            scores: HashMap::new(),
            iterations: 0,
            converged: true,
            method: PageRankMethod::Full,
        };
    }

    if _changes.is_empty() {
        if _previous.is_empty() {
            return incremental_fallback(
                ng,
                config,
                "DF-PageRank: empty changes without previous scores",
            );
        }
        return PageRankResult {
            scores: _previous.clone(),
            iterations: 0,
            converged: true,
            method: PageRankMethod::Incremental,
        };
    }

    // Automatic gate: small graphs usually run faster with full recompute.
    if n < INCREMENTAL_MIN_NODES && edge_count < INCREMENTAL_MIN_EDGES {
        return incremental_fallback(
            ng,
            config,
            "DF-PageRank: graph below size gate, using full recompute",
        );
    }

    // Automatic gate: large batch changes are effectively global updates.
    let edge_count_nonzero = edge_count.max(1);
    if _changes.len() * 100 > edge_count_nonzero * INCREMENTAL_MAX_CHANGE_PCT {
        return incremental_fallback(
            ng,
            config,
            "DF-PageRank: change volume exceeds gate, using full recompute",
        );
    }

    // Conservative guard: source dangling-status transitions induce global
    // effects. Instead of immediate fallback, promote to global affected set.
    let force_global_affected = has_possible_dangling_transition(ng, _changes);

    let frontier = identify_frontier(ng, _changes);
    if frontier.is_empty() {
        return incremental_fallback(
            ng,
            config,
            "DF-PageRank: no frontier nodes found, using full recompute",
        );
    }

    let mut affected = bfs_affected(ng, &frontier);
    if force_global_affected {
        affected = (0..n).collect();
    }
    if affected.is_empty() {
        return incremental_fallback(
            ng,
            config,
            "DF-PageRank: empty affected set, using full recompute",
        );
    }

    // If affected is very large, full recompute is usually cheaper and safer.
    if affected.len() * 5 > n * 4 {
        return incremental_fallback(
            ng,
            config,
            "DF-PageRank: affected set exceeds 80%, using full recompute",
        );
    }

    let n_f64 = n as f64;
    let default_rank = 1.0 / n_f64;

    let mut ranks: Vec<f64> = (0..n)
        .map(|i| {
            let node = petgraph::graph::NodeIndex::new(i);
            g.node_weight(node)
                .and_then(|scc| scc.members.first())
                .and_then(|id| _previous.get(id))
                .copied()
                .unwrap_or(default_rank)
        })
        .collect();
    normalize_ranks(&mut ranks);

    let initial_dangling_sum = dangling_sum(g, &ranks);
    let base = (1.0 - config.damping) / n_f64;
    let mut new_ranks = ranks.clone();
    let mut iterations = 0;
    let mut converged = false;

    for _ in 0..config.max_iter {
        iterations += 1;

        let dangling_mass = config.damping * dangling_sum(g, &ranks) / n_f64;

        for &node_idx in &affected {
            let node = petgraph::graph::NodeIndex::new(node_idx);
            let mut rank = base + dangling_mass;

            for pred in g.neighbors_directed(node, Direction::Incoming) {
                let pred_idx = g.to_index(pred);
                let out_degree = g.neighbors_directed(pred, Direction::Outgoing).count();
                if out_degree > 0 {
                    rank += config.damping * ranks[pred_idx] / out_degree as f64;
                }
            }

            new_ranks[node_idx] = rank;
        }

        let delta: f64 = affected
            .iter()
            .map(|&i| (ranks[i] - new_ranks[i]).abs())
            .sum();

        for &i in &affected {
            ranks[i] = new_ranks[i];
        }

        if delta < config.tolerance {
            converged = true;
            break;
        }
    }

    // Dangling-mass drift indicates global coupling that subset updates miss.
    let final_dangling_sum = dangling_sum(g, &ranks);
    if (final_dangling_sum - initial_dangling_sum).abs() > config.tolerance * 10.0 {
        return incremental_fallback(
            ng,
            config,
            "DF-PageRank: dangling mass drift exceeded threshold, using full recompute",
        );
    }

    // One-step global residual check (cheap, O(E + N), no iterative full solve).
    let residual = global_residual_max(ng, &ranks, config);
    if residual > config.tolerance * 10.0 {
        return incremental_fallback(
            ng,
            config,
            "DF-PageRank: residual check failed, using full recompute",
        );
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

fn incremental_fallback(
    ng: &NormalizedGraph,
    config: &PageRankConfig,
    message: &str,
) -> PageRankResult {
    warn!("{message}");
    let mut result = pagerank(ng, config);
    result.method = PageRankMethod::IncrementalFallback;
    result
}

/// Identify SCC frontier indices affected by edge changes.
fn identify_frontier(ng: &NormalizedGraph, changes: &[EdgeChange]) -> HashSet<usize> {
    let g = &ng.condensed;
    let mut frontier = HashSet::new();

    for change in changes {
        if let Some(&from_scc) = ng.item_to_scc.get(&change.from) {
            frontier.insert(g.to_index(from_scc));
        }
        // Needed for removals: `from -> to` may no longer exist in new graph,
        // but `to` still receives changed incoming mass.
        if let Some(&to_scc) = ng.item_to_scc.get(&change.to) {
            frontier.insert(g.to_index(to_scc));
        }
    }

    frontier
}

/// Collect downstream closure from frontier via outgoing BFS.
fn bfs_affected(ng: &NormalizedGraph, frontier: &HashSet<usize>) -> Vec<usize> {
    let g = &ng.condensed;
    let mut visited = frontier.clone();
    let mut queue: VecDeque<usize> = frontier.iter().copied().collect();
    let mut affected = Vec::with_capacity(frontier.len());

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

/// Estimate whether any changed source may have toggled dangling status.
fn has_possible_dangling_transition(ng: &NormalizedGraph, changes: &[EdgeChange]) -> bool {
    let pair_counts_new = scc_pair_edge_counts(ng);
    let mut pair_counts_old: HashMap<(usize, usize), isize> = pair_counts_new
        .iter()
        .map(|(pair, &count)| (*pair, count as isize))
        .collect();
    let mut touched_sources = HashSet::new();

    for change in changes {
        let Some(&from_scc) = ng.item_to_scc.get(&change.from) else {
            continue;
        };
        let Some(&to_scc) = ng.item_to_scc.get(&change.to) else {
            continue;
        };

        let from_idx = ng.condensed.to_index(from_scc);
        let to_idx = ng.condensed.to_index(to_scc);

        // Intra-SCC edges do not contribute to condensed out-degree.
        if from_idx == to_idx {
            continue;
        }

        touched_sources.insert(from_idx);
        let entry = pair_counts_old.entry((from_idx, to_idx)).or_insert(0);
        match change.kind {
            EdgeChangeKind::Added => *entry -= 1,
            EdgeChangeKind::Removed => *entry += 1,
        }
    }

    for source in touched_sources {
        let new_out = pair_counts_new
            .iter()
            .filter(|((from, _), count)| *from == source && **count > 0)
            .count();
        let old_out = pair_counts_old
            .iter()
            .filter(|((from, _), count)| *from == source && **count > 0)
            .count();

        if (new_out == 0) != (old_out == 0) {
            return true;
        }
    }

    false
}

/// Count item-level edges grouped by condensed SCC source/target pair.
fn scc_pair_edge_counts(ng: &NormalizedGraph) -> HashMap<(usize, usize), usize> {
    let mut counts = HashMap::new();

    for edge in ng.raw.graph.edge_references() {
        let Some(from_item) = ng.raw.graph.node_weight(edge.source()) else {
            continue;
        };
        let Some(to_item) = ng.raw.graph.node_weight(edge.target()) else {
            continue;
        };
        let Some(&from_scc) = ng.item_to_scc.get(from_item) else {
            continue;
        };
        let Some(&to_scc) = ng.item_to_scc.get(to_item) else {
            continue;
        };

        let from_idx = ng.condensed.to_index(from_scc);
        let to_idx = ng.condensed.to_index(to_scc);

        // Condensed out-degree ignores intra-SCC edges.
        if from_idx == to_idx {
            continue;
        }

        *counts.entry((from_idx, to_idx)).or_insert(0) += 1;
    }

    counts
}

fn dangling_sum(
    g: &petgraph::graph::DiGraph<crate::graph::normalize::SccNode, ()>,
    ranks: &[f64],
) -> f64 {
    g.node_identifiers()
        .filter(|&n| g.neighbors_directed(n, Direction::Outgoing).count() == 0)
        .map(|n| ranks[g.to_index(n)])
        .sum()
}

fn normalize_ranks(ranks: &mut [f64]) {
    let sum: f64 = ranks.iter().sum();
    if !sum.is_finite() || sum <= 0.0 {
        let n = ranks.len() as f64;
        if n > 0.0 {
            let uniform = 1.0 / n;
            for r in ranks {
                *r = uniform;
            }
        }
        return;
    }

    for r in ranks {
        *r /= sum;
    }
}

/// One global power-iteration step residual against current ranks.
fn global_residual_max(ng: &NormalizedGraph, ranks: &[f64], config: &PageRankConfig) -> f64 {
    let g = &ng.condensed;
    let n = g.node_count();
    if n == 0 {
        return 0.0;
    }

    let n_f64 = n as f64;
    let base = (1.0 - config.damping) / n_f64;
    let dangling_mass = config.damping * dangling_sum(g, ranks) / n_f64;
    let mut probe = vec![base + dangling_mass; n];

    for node in g.node_identifiers() {
        let idx = g.to_index(node);
        let out_degree = g.neighbors_directed(node, Direction::Outgoing).count();
        if out_degree == 0 {
            continue;
        }
        let share = config.damping * ranks[idx] / out_degree as f64;
        for neighbor in g.neighbors_directed(node, Direction::Outgoing) {
            let nidx = g.to_index(neighbor);
            probe[nidx] += share;
        }
    }

    probe
        .iter()
        .zip(ranks.iter())
        .map(|(new, old)| (new - old).abs())
        .fold(0.0_f64, f64::max)
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

    fn make_chain_normalized(n: usize, extras: &[(usize, usize)]) -> NormalizedGraph {
        let mut graph = DiGraph::<String, ()>::new();
        let mut node_map = HashMap::new();

        for i in 0..n {
            let id = format!("n{i}");
            let idx = graph.add_node(id.clone());
            node_map.insert(id, idx);
        }

        for i in 0..n.saturating_sub(1) {
            let a = node_map[&format!("n{i}")];
            let b = node_map[&format!("n{}", i + 1)];
            graph.add_edge(a, b, ());
        }

        for (from, to) in extras {
            let a = node_map[&format!("n{from}")];
            let b = node_map[&format!("n{to}")];
            if !graph.contains_edge(a, b) {
                graph.add_edge(a, b, ());
            }
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

    #[test]
    fn incremental_small_graph_gate_falls_back() {
        let ng_old = make_chain_normalized(20, &[]);
        let prev = pagerank(&ng_old, &default_config());

        let ng_new = make_chain_normalized(20, &[(0, 10)]);
        let changes = vec![EdgeChange {
            from: "n0".to_string(),
            to: "n10".to_string(),
            kind: EdgeChangeKind::Added,
        }];

        let result = pagerank_incremental(&ng_new, &prev.scores, &changes, &default_config());
        assert_eq!(result.method, PageRankMethod::IncrementalFallback);
    }

    #[test]
    fn incremental_change_volume_gate_falls_back() {
        let ng_old = make_chain_normalized(350, &[]);
        let prev = pagerank(&ng_old, &default_config());

        let extras: Vec<(usize, usize)> = (0..30).map(|i| (i, i + 2)).collect();
        let ng_new = make_chain_normalized(350, &extras);
        let changes: Vec<EdgeChange> = extras
            .iter()
            .map(|(from, to)| EdgeChange {
                from: format!("n{from}"),
                to: format!("n{to}"),
                kind: EdgeChangeKind::Added,
            })
            .collect();

        let result = pagerank_incremental(&ng_new, &prev.scores, &changes, &default_config());
        assert_eq!(result.method, PageRankMethod::IncrementalFallback);
    }

    #[test]
    fn dangling_transition_heuristic_ignores_parallel_member_addition() {
        // SCC {A, B} already has outgoing mass to C via A -> C.
        // Adding B -> C should not imply dangling-status transition.
        let ng_new = make_normalized(&[("A", "B"), ("B", "A"), ("A", "C"), ("B", "C")]);
        let changes = vec![EdgeChange {
            from: "B".to_string(),
            to: "C".to_string(),
            kind: EdgeChangeKind::Added,
        }];

        assert!(!has_possible_dangling_transition(&ng_new, &changes));
    }

    #[test]
    fn dangling_transition_heuristic_detects_first_outgoing_edge() {
        // SCC {A, B} had no outgoing edges before; adding one should be treated
        // as a possible dangling-status transition.
        let ng_new = make_normalized(&[("A", "B"), ("B", "A"), ("B", "C")]);
        let changes = vec![EdgeChange {
            from: "B".to_string(),
            to: "C".to_string(),
            kind: EdgeChangeKind::Added,
        }];

        assert!(has_possible_dangling_transition(&ng_new, &changes));
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
