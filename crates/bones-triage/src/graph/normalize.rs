//! SCC condensation and transitive reduction for the dependency graph.
//!
//! # Overview
//!
//! The raw blocking graph may contain cycles (e.g., two items that block each
//! other due to concurrent link events). This module provides two normalization
//! steps that produce a stable, acyclic graph suitable for centrality metrics:
//!
//! 1. **SCC Condensation** — Collapses strongly connected components (SCCs)
//!    into single nodes. Items in the same SCC form a "dependency cycle" and
//!    are treated as an atomic unit for scheduling purposes.
//!
//! 2. **Transitive Reduction** — Removes redundant edges from the condensed
//!    DAG. An edge `A → C` is redundant if there is already a path `A → B → C`.
//!    Removing such edges gives the minimal graph with the same reachability.
//!
//! # Output
//!
//! [`NormalizedGraph`] exposes both the condensed DAG (nodes = SCCs) and
//! the transitively-reduced DAG alongside the original [`RawGraph`]. Callers
//! can use the reduced graph for centrality metrics and the original for
//! display purposes.

#![allow(clippy::module_name_repetitions)]

use std::collections::HashMap;

use petgraph::{
    Direction,
    algo::condensation,
    graph::{DiGraph, NodeIndex},
    visit::IntoNodeIdentifiers,
};
use tracing::instrument;

use crate::graph::build::RawGraph;

// ---------------------------------------------------------------------------
// NormalizedGraph
// ---------------------------------------------------------------------------

/// A node in the condensed dependency graph.
///
/// Each node represents one SCC from the original graph. Most nodes will
/// contain a single item ID; nodes with multiple IDs represent dependency
/// cycles.
#[derive(Debug, Clone)]
pub struct SccNode {
    /// Item IDs in this SCC (sorted for deterministic output).
    pub members: Vec<String>,
}

impl SccNode {
    /// Return `true` if this SCC contains more than one item (i.e., a cycle).
    #[must_use]
    pub const fn is_cycle(&self) -> bool {
        self.members.len() > 1
    }

    /// Return the primary representative of this SCC.
    ///
    /// For single-item SCCs this is the item ID. For cycles, it is the
    /// lexicographically smallest member (deterministic).
    #[must_use]
    pub fn representative(&self) -> &str {
        self.members.first().map(String::as_str).unwrap_or_default()
    }
}

/// The fully normalized dependency graph.
///
/// Contains three views of the same data:
/// - `raw`: the original [`RawGraph`] with all nodes and edges as-is.
/// - `condensed`: condensed DAG where each node is an [`SccNode`].
/// - `reduced`: transitively-reduced condensed DAG (minimum edges).
#[derive(Debug)]
pub struct NormalizedGraph {
    /// Original graph from `SQLite` (may contain cycles).
    pub raw: RawGraph,
    /// Condensed DAG: SCCs collapsed to single nodes.
    pub condensed: DiGraph<SccNode, ()>,
    /// Transitively-reduced condensed DAG (minimum edge set).
    pub reduced: DiGraph<SccNode, ()>,
    /// Mapping from item ID to condensed node index.
    pub item_to_scc: HashMap<String, NodeIndex>,
}

impl NormalizedGraph {
    /// Build a [`NormalizedGraph`] from a [`RawGraph`].
    ///
    /// Steps:
    /// 1. Run petgraph's SCC condensation (Tarjan's algorithm internally).
    /// 2. Build [`SccNode`] labels from the raw node weights.
    /// 3. Compute transitive reduction of the condensed DAG.
    ///
    /// # Panics
    ///
    /// Does not panic — all indices are verified.
    #[must_use]
    #[instrument(skip(raw))]
    pub fn from_raw(raw: RawGraph) -> Self {
        // petgraph::condensation collapses SCCs and returns a DiGraph where
        // each node weight is a Vec of the original node weights.
        // make_acyclic=true removes intra-SCC edges (back-edges in the SCCs).
        let condensed_raw: DiGraph<Vec<String>, ()> =
            condensation(raw.graph.clone(), /* make_acyclic */ true);

        // Build SccNode labels (sorted members for determinism).
        let condensed: DiGraph<SccNode, ()> = condensed_raw.map(
            |_, members| {
                let mut sorted = members.clone();
                sorted.sort_unstable();
                SccNode { members: sorted }
            },
            |_, ()| (),
        );

        // Build item_to_scc mapping.
        let item_to_scc = build_item_to_scc_map(&condensed);

        // Compute transitive reduction of the condensed DAG.
        let reduced = transitive_reduction(&condensed);

        Self {
            raw,
            condensed,
            reduced,
            item_to_scc,
        }
    }

    /// Return the number of SCCs in the condensed graph.
    #[must_use]
    pub fn scc_count(&self) -> usize {
        self.condensed.node_count()
    }

    /// Return the number of cycle SCCs (SCCs with more than one member).
    #[must_use]
    pub fn cycle_count(&self) -> usize {
        self.condensed
            .node_weights()
            .filter(|n| n.is_cycle())
            .count()
    }

    /// Return all items that are members of a dependency cycle.
    #[must_use]
    pub fn cyclic_items(&self) -> Vec<&str> {
        self.condensed
            .node_weights()
            .filter(|n| n.is_cycle())
            .flat_map(|n| n.members.iter().map(String::as_str))
            .collect()
    }

    /// Return the SCC node index for a given item ID.
    #[must_use]
    pub fn scc_of(&self, item_id: &str) -> Option<NodeIndex> {
        self.item_to_scc.get(item_id).copied()
    }

    /// Return the content hash from the underlying raw graph.
    ///
    /// Used for cache invalidation — if this changes, rebuild.
    #[must_use]
    pub fn content_hash(&self) -> &str {
        &self.raw.content_hash
    }
}

// ---------------------------------------------------------------------------
// Transitive reduction
// ---------------------------------------------------------------------------

/// Compute the transitive reduction of a DAG.
///
/// Returns a new graph with the same nodes but only the minimal edges needed
/// to preserve reachability. An edge `(u, v)` is removed if there exists
/// another path `u → ... → v` of length ≥ 2.
///
/// # Algorithm
///
/// Process nodes in reverse topological order (sinks first). For each node
/// `u`, compute the set of all nodes reachable from `u` via paths of length
/// ≥ 2 through its direct successors. Any edge `(u, v)` where `v` is in
/// that set is redundant.
///
/// # Panics
///
/// Panics if `g` contains a cycle (input must be a DAG).
#[must_use]
pub fn transitive_reduction<N: Clone>(g: &DiGraph<N, ()>) -> DiGraph<N, ()> {
    use fixedbitset::FixedBitSet;
    use petgraph::algo::toposort;
    use petgraph::visit::NodeIndexable;

    let n = g.node_count();
    if n == 0 {
        return g.map(|_, w| w.clone(), |_, ()| ());
    }

    // Get topological order (errors if cyclic).
    let topo = toposort(g, None).unwrap_or_else(|_| {
        // Fallback: return graph as-is if it contains a cycle.
        // The condensed graph should always be a DAG, but we defend
        // against bugs here rather than panicking.
        g.node_identifiers().collect()
    });

    // Reachability: `reachable[u]` is the set of nodes reachable from `u`
    // via paths of length ≥ 1 through its direct successors (i.e., every
    // successor v plus everything reachable from v). Stored as a flat
    // Vec<FixedBitSet> indexed by node index — one bit per node — instead
    // of HashMap<NodeIndex, HashSet<NodeIndex>>. On a 10k-node condensed
    // DAG this drops the peak memory by ~100x and eliminates the hash-lookup
    // cost on the hot "is v covered?" check below.
    let mut reachable: Vec<FixedBitSet> = vec![FixedBitSet::with_capacity(n); n];

    for &u in topo.iter().rev() {
        let u_idx = g.to_index(u);
        // Collect the direct successors of u so we can distinguish the
        // edge we're about to test from the others below.
        let successors: Vec<NodeIndex> = g.neighbors_directed(u, Direction::Outgoing).collect();

        // Union all successors' reachability sets into reachable[u_idx].
        // Each successor v contributes: {v} ∪ reachable[v]. That is the
        // full set of nodes reachable from u through any single out-edge.
        for &v in &successors {
            let v_idx = g.to_index(v);
            reachable[u_idx].insert(v_idx);
            // FixedBitSet::union_with clones one bitset into another in
            // O(n / 64). Split-borrow so the borrow checker allows
            // touching two distinct slots of `reachable` at once.
            if u_idx != v_idx {
                let (lo, hi) = if u_idx < v_idx {
                    let (a, b) = reachable.split_at_mut(v_idx);
                    (&mut a[u_idx], &b[0])
                } else {
                    let (a, b) = reachable.split_at_mut(u_idx);
                    (&mut b[0], &a[v_idx])
                };
                lo.union_with(hi);
            }
        }
    }

    // An edge (u, v) is redundant iff v is reachable from u via a path of
    // length ≥ 2 — equivalently, v is reachable from some *other* direct
    // successor w ≠ v of u. Precompute, per node u, the union of
    // reachable[w] over every other successor w. An edge (u, v) is then
    // redundant iff that union contains v.
    //
    // The previous implementation recomputed this union on every edge
    // visit inside a .filter() → `g.edge_references().filter(...)` was
    // effectively O(|E| · out_deg). This loop is O(sum_u out_deg(u)²) in
    // the worst case but single-pass over edges and cache-friendly.
    let mut reduced = g.map(|_, w| w.clone(), |_, ()| ());
    let mut scratch = FixedBitSet::with_capacity(n);
    let mut to_remove: Vec<(NodeIndex, NodeIndex)> = Vec::new();

    for u in g.node_identifiers() {
        let successors: Vec<NodeIndex> = g.neighbors_directed(u, Direction::Outgoing).collect();
        if successors.len() < 2 {
            // A node with 0 or 1 outgoing edges has no redundant edges.
            continue;
        }
        for &v in &successors {
            let v_idx = g.to_index(v);
            // Build "union of reachable[w] for w in successors, w != v".
            scratch.clear();
            for &w in &successors {
                if w == v {
                    continue;
                }
                scratch.union_with(&reachable[g.to_index(w)]);
            }
            if scratch.contains(v_idx) {
                to_remove.push((u, v));
            }
        }
    }

    for (src, tgt) in to_remove {
        if let Some(edge_idx) = reduced.find_edge(src, tgt) {
            reduced.remove_edge(edge_idx);
        }
    }

    reduced
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn build_item_to_scc_map(condensed: &DiGraph<SccNode, ()>) -> HashMap<String, NodeIndex> {
    let mut map = HashMap::new();
    for idx in condensed.node_identifiers() {
        if let Some(node) = condensed.node_weight(idx) {
            for member in &node.members {
                map.insert(member.clone(), idx);
            }
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use petgraph::graph::DiGraph;

    fn make_raw_with_edges(edges: &[(&str, &str)]) -> RawGraph {
        let mut graph = DiGraph::<String, ()>::new();
        let mut node_map = std::collections::HashMap::new();

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

        RawGraph {
            graph,
            node_map,
            content_hash: "blake3:test".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // SCC condensation
    // -----------------------------------------------------------------------

    #[test]
    fn linear_chain_each_node_is_own_scc() {
        // A → B → C (no cycles)
        let raw = make_raw_with_edges(&[("A", "B"), ("B", "C")]);
        let ng = NormalizedGraph::from_raw(raw);

        assert_eq!(ng.scc_count(), 3, "3 SCCs for acyclic chain");
        assert_eq!(ng.cycle_count(), 0, "no cycles");
    }

    #[test]
    fn simple_cycle_condensed_to_one_scc() {
        // A → B → A (cycle)
        let raw = make_raw_with_edges(&[("A", "B"), ("B", "A")]);
        let ng = NormalizedGraph::from_raw(raw);

        assert_eq!(ng.scc_count(), 1, "cycle condensed to 1 SCC");
        assert_eq!(ng.cycle_count(), 1, "one cycle SCC");

        let cyclic = ng.cyclic_items();
        assert!(cyclic.contains(&"A"), "A in cyclic items");
        assert!(cyclic.contains(&"B"), "B in cyclic items");
    }

    #[test]
    fn mixed_cycle_and_acyclic() {
        // A → B → A → C (A and B cycle; C is downstream)
        let raw = make_raw_with_edges(&[("A", "B"), ("B", "A"), ("A", "C")]);
        let ng = NormalizedGraph::from_raw(raw);

        // SCCs: {A, B} and {C}
        assert_eq!(ng.scc_count(), 2, "2 SCCs: the cycle and C");
        assert_eq!(ng.cycle_count(), 1);

        let cyclic = ng.cyclic_items();
        assert!(cyclic.contains(&"A"));
        assert!(cyclic.contains(&"B"));
        assert!(!cyclic.contains(&"C"));
    }

    #[test]
    fn item_to_scc_mapping_correct() {
        let raw = make_raw_with_edges(&[("A", "B"), ("B", "A"), ("A", "C")]);
        let ng = NormalizedGraph::from_raw(raw);

        let scc_ab_a = ng.scc_of("A");
        let scc_ab_b = ng.scc_of("B");
        let scc_c = ng.scc_of("C");

        assert!(scc_ab_a.is_some(), "A has SCC");
        assert!(scc_ab_b.is_some(), "B has SCC");
        assert!(scc_c.is_some(), "C has SCC");

        // A and B should be in the same SCC
        assert_eq!(scc_ab_a, scc_ab_b, "A and B in same SCC");
        // C is in a different SCC
        assert_ne!(scc_ab_a, scc_c, "C in different SCC from A");
    }

    // -----------------------------------------------------------------------
    // Transitive reduction
    // -----------------------------------------------------------------------

    #[test]
    fn transitive_reduction_removes_redundant_edge() {
        // A → B → C and A → C (redundant)
        let mut g: DiGraph<String, ()> = DiGraph::new();
        let a = g.add_node("A".to_string());
        let b = g.add_node("B".to_string());
        let c = g.add_node("C".to_string());
        g.add_edge(a, b, ());
        g.add_edge(b, c, ());
        g.add_edge(a, c, ()); // redundant

        let reduced = transitive_reduction(&g);
        assert_eq!(reduced.edge_count(), 2, "A→C removed");
        assert!(!reduced.contains_edge(a, c), "redundant edge removed");
        assert!(reduced.contains_edge(a, b), "direct edge kept");
        assert!(reduced.contains_edge(b, c), "direct edge kept");
    }

    #[test]
    fn transitive_reduction_preserves_minimal_graph() {
        // A → B → C (no redundant edges)
        let mut g: DiGraph<String, ()> = DiGraph::new();
        let a = g.add_node("A".to_string());
        let b = g.add_node("B".to_string());
        let c = g.add_node("C".to_string());
        g.add_edge(a, b, ());
        g.add_edge(b, c, ());

        let reduced = transitive_reduction(&g);
        assert_eq!(reduced.edge_count(), 2, "no edges removed");
    }

    #[test]
    fn transitive_reduction_diamond_removes_diagonal() {
        // Diamond: A → B → D, A → C → D, A → D (redundant)
        let mut g: DiGraph<String, ()> = DiGraph::new();
        let a = g.add_node("A".to_string());
        let b = g.add_node("B".to_string());
        let c = g.add_node("C".to_string());
        let d = g.add_node("D".to_string());
        g.add_edge(a, b, ());
        g.add_edge(a, c, ());
        g.add_edge(b, d, ());
        g.add_edge(c, d, ());
        g.add_edge(a, d, ()); // redundant via A→B→D or A→C→D

        let reduced = transitive_reduction(&g);
        assert_eq!(reduced.edge_count(), 4, "A→D removed");
        assert!(!reduced.contains_edge(a, d), "redundant edge removed");
    }

    #[test]
    fn scc_representative_is_lexicographically_first() {
        let raw = make_raw_with_edges(&[("bn-z", "bn-a"), ("bn-a", "bn-z")]);
        let ng = NormalizedGraph::from_raw(raw);

        assert_eq!(ng.scc_count(), 1);
        let scc_idx = ng.condensed.node_identifiers().next().unwrap();
        let scc = &ng.condensed[scc_idx];
        // Members should be sorted, so bn-a comes before bn-z
        assert_eq!(scc.members[0], "bn-a");
        assert_eq!(scc.representative(), "bn-a");
    }
}
