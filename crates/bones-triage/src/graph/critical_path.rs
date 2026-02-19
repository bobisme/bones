//! Critical path analysis for the dependency graph.
//!
//! # Overview
//!
//! The critical path is the *longest* dependency chain in the project.  Items
//! on the critical path have **zero slack** — any delay on them delays the
//! earliest possible completion of the entire project.
//!
//! # Definitions
//!
//! All durations are measured in "steps" where each item contributes 1 step.
//!
//! | Term              | Definition |
//! |-------------------|------------|
//! | `earliest_start`  | Earliest step at which an item can begin (all predecessors done). |
//! | `earliest_finish` | `earliest_start + 1`. |
//! | `latest_start`    | Latest step at which the item can begin without delaying the project. |
//! | `latest_finish`   | `latest_start + 1`. |
//! | `slack`           | `latest_start - earliest_start` — zero on the critical path. |
//!
//! # Algorithm
//!
//! 1. Work on the **condensed DAG** so cycles are handled (each SCC becomes
//!    one super-node; its members are all reported as critical together).
//! 2. **Forward pass** in topological order: compute `earliest_start` /
//!    `earliest_finish` for every condensed node.
//! 3. **Backward pass** in reverse topological order: compute `latest_finish`
//!    / `latest_start`.
//! 4. **Slack** = `latest_start − earliest_start`.  Nodes with slack = 0 are
//!    on the critical path.
//! 5. **Path reconstruction**: walk forward through zero-slack nodes choosing
//!    the zero-slack successor at each step.
//!
//! The result exposes both *per-item* timings (accounting for SCC membership)
//! and the reconstructed critical path as an ordered list of item IDs.

#![allow(clippy::module_name_repetitions)]

use std::collections::{HashMap, HashSet};

use petgraph::{algo::toposort, graph::NodeIndex, visit::EdgeRef, Direction};

use crate::graph::normalize::NormalizedGraph;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-item timing computed during critical path analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemTiming {
    /// Earliest step at which the item can start (0-based).
    pub earliest_start: usize,
    /// Earliest step at which the item finishes (`earliest_start + 1`).
    pub earliest_finish: usize,
    /// Latest step at which the item can start without delaying the project.
    pub latest_start: usize,
    /// Latest step at which the item finishes (`latest_start + 1`).
    pub latest_finish: usize,
    /// Total float (slack): `latest_start - earliest_start`.
    ///
    /// Zero for items on the critical path.
    pub slack: usize,
}

/// Result of critical path analysis on a dependency graph.
#[derive(Debug, Clone)]
pub struct CriticalPathResult {
    /// Item IDs on the critical path, in dependency order (sources first).
    ///
    /// Empty when the graph has no items.
    pub critical_path: Vec<String>,
    /// All item IDs with zero slack.
    ///
    /// May include items *not* on the reconstructed `critical_path` when
    /// there are multiple parallel critical paths of equal length.
    pub critical_items: HashSet<String>,
    /// Per-item timing information.
    pub item_timings: HashMap<String, ItemTiming>,
    /// Length of the critical path (number of items).
    pub total_length: usize,
}

impl CriticalPathResult {
    /// Return an empty result for a graph with no items.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            critical_path: Vec::new(),
            critical_items: HashSet::new(),
            item_timings: HashMap::new(),
            total_length: 0,
        }
    }

    /// Return `true` if the critical path is empty (no items in the graph).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.critical_path.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Core computation
// ---------------------------------------------------------------------------

/// Compute the critical path for the dependency graph described by `ng`.
///
/// Uses the **condensed DAG** so the computation is correct even when the
/// raw graph contains dependency cycles.  Members of cycle SCCs are assigned
/// the timing of their super-node and all have zero slack (they are mutually
/// blocking and therefore always on the critical path of their SCC).
///
/// # Returns
///
/// A [`CriticalPathResult`] containing:
/// - The reconstructed critical path (one representative path).
/// - The set of all zero-slack items.
/// - Per-item timing data.
/// - The total project length in steps.
#[must_use]
pub fn compute_critical_path(ng: &NormalizedGraph) -> CriticalPathResult {
    let condensed = &ng.condensed;

    if condensed.node_count() == 0 {
        return CriticalPathResult::empty();
    }

    // --- Topological sort of the condensed DAG ---
    // The condensed graph is a DAG by construction; toposort will not fail.
    let topo: Vec<NodeIndex> = toposort(condensed, None).unwrap_or_else(|_| {
        // Defensive fallback (should never happen on a condensed DAG).
        condensed.node_indices().collect()
    });

    // --- Forward pass: earliest_start / earliest_finish ---
    let mut earliest_finish: HashMap<NodeIndex, usize> = HashMap::with_capacity(topo.len());

    for &v in &topo {
        let max_pred_finish = condensed
            .edges_directed(v, Direction::Incoming)
            .map(|e| earliest_finish.get(&e.source()).copied().unwrap_or(0))
            .max()
            .unwrap_or(0);
        earliest_finish.insert(v, max_pred_finish + 1);
    }

    // Project duration = max earliest_finish over all nodes.
    let project_finish = earliest_finish.values().copied().max().unwrap_or(1);

    // --- Backward pass: latest_finish / latest_start ---
    let mut latest_finish: HashMap<NodeIndex, usize> = HashMap::with_capacity(topo.len());

    for &v in topo.iter().rev() {
        let min_succ_start = condensed
            .edges_directed(v, Direction::Outgoing)
            .map(|e| {
                let lf = latest_finish.get(&e.target()).copied().unwrap_or(project_finish);
                lf - 1 // latest_start of successor = latest_finish[succ] - 1
            })
            .min()
            .unwrap_or(project_finish);
        // latest_finish[v] = min_succ_start (the successor's latest_start)
        latest_finish.insert(v, min_succ_start);
    }

    // --- Build per-item timings and critical item set ---
    let mut item_timings: HashMap<String, ItemTiming> = HashMap::new();
    let mut critical_items: HashSet<String> = HashSet::new();

    // Map NodeIndex → slack for path reconstruction.
    let mut node_slack: HashMap<NodeIndex, usize> = HashMap::with_capacity(topo.len());

    for &v in &topo {
        let ef = earliest_finish[&v];
        let es = ef.saturating_sub(1);
        let lf = latest_finish[&v];
        let ls = lf.saturating_sub(1);
        let slack = ls.saturating_sub(es);

        node_slack.insert(v, slack);

        let timing = ItemTiming {
            earliest_start: es,
            earliest_finish: ef,
            latest_start: ls,
            latest_finish: lf,
            slack,
        };

        // All members of this condensed node share the same timing.
        if let Some(scc_node) = condensed.node_weight(v) {
            for member in &scc_node.members {
                item_timings.insert(member.clone(), timing.clone());
                if slack == 0 {
                    critical_items.insert(member.clone());
                }
            }
        }
    }

    // --- Critical path reconstruction ---
    // Find the source node(s) with zero slack and earliest_start == 0,
    // then walk forward always choosing a zero-slack successor.
    let critical_path = reconstruct_critical_path(condensed, &topo, &earliest_finish, &node_slack);

    // Expand condensed path to item IDs (each SCC node → sorted members).
    let critical_path_items: Vec<String> = critical_path
        .iter()
        .flat_map(|&idx| {
            condensed
                .node_weight(idx)
                .map(|n| n.members.clone())
                .unwrap_or_default()
        })
        .collect();

    let total_length = critical_path_items.len();

    CriticalPathResult {
        critical_path: critical_path_items,
        critical_items,
        item_timings,
        total_length,
    }
}

// ---------------------------------------------------------------------------
// Path reconstruction helper
// ---------------------------------------------------------------------------

/// Walk from a zero-slack source to a zero-slack sink along zero-slack edges.
///
/// Returns the sequence of condensed node indices that form one critical path.
fn reconstruct_critical_path(
    condensed: &petgraph::graph::DiGraph<crate::graph::normalize::SccNode, ()>,
    topo: &[NodeIndex],
    earliest_finish: &HashMap<NodeIndex, usize>,
    node_slack: &HashMap<NodeIndex, usize>,
) -> Vec<NodeIndex> {
    // Find the zero-slack node with the greatest earliest_finish (the sink of
    // the critical path).
    let Some(&sink) = topo
        .iter()
        .filter(|&&v| node_slack.get(&v).copied().unwrap_or(1) == 0)
        .max_by_key(|&&v| earliest_finish.get(&v).copied().unwrap_or(0))
    else {
        return Vec::new();
    };

    // Walk backwards from the sink: at each step pick the zero-slack
    // predecessor with the largest earliest_finish (ties broken by ID sort
    // via SccNode::representative for determinism).
    let mut path: Vec<NodeIndex> = vec![sink];
    let mut current = sink;

    loop {
        let prev = condensed
            .edges_directed(current, Direction::Incoming)
            .filter(|e| node_slack.get(&e.source()).copied().unwrap_or(1) == 0)
            .max_by_key(|e| earliest_finish.get(&e.source()).copied().unwrap_or(0));

        match prev {
            Some(e) => {
                current = e.source();
                path.push(current);
            }
            None => break,
        }
    }

    path.reverse();
    path
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

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn make_normalized(edges: &[(&str, &str)]) -> NormalizedGraph {
        make_normalized_nodes(
            &edges
                .iter()
                .flat_map(|(a, b)| [*a, *b])
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>(),
            edges,
        )
    }

    fn make_normalized_nodes(nodes: &[&str], edges: &[(&str, &str)]) -> NormalizedGraph {
        let mut graph = DiGraph::<String, ()>::new();
        let mut node_map: HashMap<String, _> = HashMap::new();

        for &id in nodes {
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

    // -----------------------------------------------------------------------
    // Empty / trivial graphs
    // -----------------------------------------------------------------------

    #[test]
    fn empty_graph_returns_empty_result() {
        let ng = make_normalized_nodes(&[], &[]);
        let result = compute_critical_path(&ng);

        assert!(result.is_empty());
        assert!(result.critical_path.is_empty());
        assert!(result.critical_items.is_empty());
        assert_eq!(result.total_length, 0);
    }

    #[test]
    fn single_node_is_critical() {
        let ng = make_normalized_nodes(&["A"], &[]);
        let result = compute_critical_path(&ng);

        assert_eq!(result.total_length, 1);
        assert!(result.critical_items.contains("A"));
        assert_eq!(result.critical_path, vec!["A".to_string()]);

        let timing = &result.item_timings["A"];
        assert_eq!(timing.earliest_start, 0);
        assert_eq!(timing.earliest_finish, 1);
        assert_eq!(timing.slack, 0);
    }

    // -----------------------------------------------------------------------
    // Linear chain
    // -----------------------------------------------------------------------

    #[test]
    fn linear_chain_all_items_critical() {
        // A → B → C (all on critical path, no parallel branches)
        let ng = make_normalized(&[("A", "B"), ("B", "C")]);
        let result = compute_critical_path(&ng);

        assert_eq!(result.total_length, 3);
        assert!(result.critical_items.contains("A"));
        assert!(result.critical_items.contains("B"));
        assert!(result.critical_items.contains("C"));

        // All slack should be zero
        for item in ["A", "B", "C"] {
            assert_eq!(result.item_timings[item].slack, 0, "slack({item}) should be 0");
        }

        // Critical path should be in dependency order A→B→C
        assert_eq!(
            result.critical_path,
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
    }

    // -----------------------------------------------------------------------
    // Diamond topology
    // -----------------------------------------------------------------------

    #[test]
    fn diamond_top_and_bottom_are_critical() {
        // Diamond: A → B → D, A → C → D
        // A: es=0, B: es=1, C: es=1, D: es=2
        // B and C have equal ES; both have slack 0 in a pure diamond... wait
        // let me think: project length = 3 (A, B/C, D)
        // ES[A]=0 EF[A]=1
        // ES[B]=1 EF[B]=2
        // ES[C]=1 EF[C]=2
        // ES[D]=2 EF[D]=3
        // LS[D]=2 LF[D]=3
        // LS[B]: succ is D with LS=2, so LF[B]=2, LS[B]=1, slack=0
        // LS[C]: succ is D with LS=2, so LF[C]=2, LS[C]=1, slack=0
        // LS[A]: succs B,C with LS=1, so LF[A]=1, LS[A]=0, slack=0
        // All have zero slack in a diamond with uniform weights!
        let ng = make_normalized(&[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D")]);
        let result = compute_critical_path(&ng);

        assert_eq!(result.total_length, 3, "one critical path A→B→D or A→C→D");
        // A and D are always critical
        assert!(result.critical_items.contains("A"), "A is critical");
        assert!(result.critical_items.contains("D"), "D is critical");

        // Timings
        let ta = &result.item_timings["A"];
        assert_eq!(ta.earliest_start, 0);
        assert_eq!(ta.slack, 0);

        let td = &result.item_timings["D"];
        assert_eq!(td.earliest_start, 2);
        assert_eq!(td.slack, 0);
    }

    // -----------------------------------------------------------------------
    // Parallel branches (slack visible)
    // -----------------------------------------------------------------------

    #[test]
    fn parallel_branches_shorter_branch_has_slack() {
        // A → B → C → D (long branch, length 4)
        // A → E → D      (short branch via E, length 3)
        // E has slack 1 (can start 1 step later than earliest)
        let ng = make_normalized(&[
            ("A", "B"),
            ("B", "C"),
            ("C", "D"),
            ("A", "E"),
            ("E", "D"),
        ]);
        let result = compute_critical_path(&ng);

        // Project length should be 4 (A→B→C→D)
        assert_eq!(result.total_length, 4, "critical path A→B→C→D");

        // A, B, C, D have zero slack
        for item in ["A", "B", "C", "D"] {
            assert_eq!(
                result.item_timings[item].slack, 0,
                "{item} should have zero slack"
            );
        }

        // E has slack 1
        assert_eq!(
            result.item_timings["E"].slack, 1,
            "E on shorter branch should have slack 1"
        );

        // E should NOT be in critical_items
        assert!(!result.critical_items.contains("E"), "E not on critical path");
    }

    // -----------------------------------------------------------------------
    // Graph with cycle (condensed)
    // -----------------------------------------------------------------------

    #[test]
    fn cycle_members_reported_as_critical() {
        // A → B → A (cycle condensed to one super-node)
        // The super-node {A,B} is the whole graph, so it's trivially critical.
        let ng = make_normalized(&[("A", "B"), ("B", "A")]);
        let result = compute_critical_path(&ng);

        // Both members of the SCC should appear.
        assert!(result.critical_items.contains("A"));
        assert!(result.critical_items.contains("B"));
        assert_eq!(result.total_length, 2, "SCC expands to both members");
    }

    // -----------------------------------------------------------------------
    // Timing invariants
    // -----------------------------------------------------------------------

    #[test]
    fn timing_invariants_hold_for_chain() {
        // A → B → C
        let ng = make_normalized(&[("A", "B"), ("B", "C")]);
        let result = compute_critical_path(&ng);

        for (id, t) in &result.item_timings {
            assert_eq!(
                t.earliest_finish,
                t.earliest_start + 1,
                "{id}: earliest_finish = earliest_start + 1"
            );
            assert_eq!(
                t.latest_finish,
                t.latest_start + 1,
                "{id}: latest_finish = latest_start + 1"
            );
            assert_eq!(
                t.slack,
                t.latest_start.saturating_sub(t.earliest_start),
                "{id}: slack = latest_start - earliest_start"
            );
        }
    }

    #[test]
    fn timing_invariants_hold_for_parallel_branches() {
        let ng = make_normalized(&[
            ("A", "B"),
            ("B", "C"),
            ("C", "D"),
            ("A", "E"),
            ("E", "D"),
        ]);
        let result = compute_critical_path(&ng);

        for (id, t) in &result.item_timings {
            assert_eq!(
                t.earliest_finish,
                t.earliest_start + 1,
                "{id}: ef = es + 1"
            );
            assert_eq!(t.latest_finish, t.latest_start + 1, "{id}: lf = ls + 1");
            assert!(
                t.latest_start >= t.earliest_start,
                "{id}: ls >= es"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Critical path ordering
    // -----------------------------------------------------------------------

    #[test]
    fn critical_path_is_in_dependency_order() {
        // A → B → C → D
        let ng = make_normalized(&[("A", "B"), ("B", "C"), ("C", "D")]);
        let result = compute_critical_path(&ng);

        let path = &result.critical_path;
        assert_eq!(path.len(), 4);

        // Verify ordering via timings
        for window in path.windows(2) {
            let (a, b) = (&window[0], &window[1]);
            let ta = &result.item_timings[a];
            let tb = &result.item_timings[b];
            assert!(
                ta.earliest_start < tb.earliest_start,
                "{a} should come before {b}"
            );
        }
    }

    #[test]
    fn disjoint_graphs_longest_path_selected() {
        // Chain 1: A → B → C (length 3)
        // Chain 2: X → Y     (length 2)
        let ng = make_normalized(&[("A", "B"), ("B", "C"), ("X", "Y")]);
        let result = compute_critical_path(&ng);

        // The critical path should be the longer chain (3 items)
        assert_eq!(result.total_length, 3, "longest chain selected");
        assert!(result.critical_items.contains("A"));
        assert!(result.critical_items.contains("C"));
    }
}
