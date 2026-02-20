//! Whittle Index computation for multi-agent scheduling.
//!
//! Models the scheduling problem as a restless multi-armed bandit. Each work
//! item is an "arm" with a value (composite score + downstream unblock value),
//! cost (opportunity cost of alternatives), and expected completion time
//! (derived from size estimate).
//!
//! The Whittle Index prioritises items by computing an index that balances
//! immediate value against completion time:
//!
//! ```text
//! Index_i = (V_i + Σ_j V_j · P(unblock_j | complete_i)) / E[T_i]
//! ```
//!
//! where `V_i` is the composite score, `T_i` is the expected time, and the
//! sum is over items directly blocked by `i`.
//!
//! # Indexability Gate
//!
//! Before using Whittle indices the caller should verify that the workload
//! satisfies indexability conditions via [`check_indexability`]. When
//! indexability fails (e.g., cycles in the dependency graph) the caller
//! should fall back to the constrained-optimisation scheduler (bn-afb.2).

use std::collections::HashMap;

use petgraph::Direction;
use petgraph::graph::NodeIndex;

use crate::graph::diagnostics::DiGraph;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Computed Whittle Index for a single item.
#[derive(Debug, Clone, PartialEq)]
pub struct WhittleIndex {
    /// The item ID this index was computed for.
    pub item_id: String,
    /// The Whittle index value. Higher values indicate higher scheduling
    /// priority.
    pub index: f64,
    /// Breakdown of the value components for observability.
    pub breakdown: WhittleBreakdown,
}

/// Detailed breakdown of a Whittle Index computation.
#[derive(Debug, Clone, PartialEq)]
pub struct WhittleBreakdown {
    /// Base composite score (V_i).
    pub base_value: f64,
    /// Additional value from unblocking downstream items.
    pub unblock_value: f64,
    /// Expected completion time in work units.
    pub expected_time: f64,
    /// Whether the item is blocked by an in-progress item (discount applied).
    pub is_dynamically_blocked: bool,
}

/// Result of an indexability check.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexabilityResult {
    /// Whether the workload satisfies Whittle indexability conditions.
    pub indexable: bool,
    /// Human-readable reasons why indexability fails (empty if indexable).
    pub violations: Vec<String>,
}

/// Configuration for the Whittle Index computation.
#[derive(Debug, Clone, PartialEq)]
pub struct WhittleConfig {
    /// Discount factor applied to items blocked by an in-progress dependency.
    /// Values in `(0, 1)`. Default: `0.3`.
    pub dynamic_block_discount: f64,
}

impl Default for WhittleConfig {
    fn default() -> Self {
        Self {
            dynamic_block_discount: 0.3,
        }
    }
}

// ---------------------------------------------------------------------------
// Size → expected time mapping
// ---------------------------------------------------------------------------

/// Map a size string to expected completion time in work units.
///
/// Follows the bead spec's exponential sizing: `xxs=0.5`, `xs=1`, `s=1`,
/// `m=2`, `l=4`, `xl=8`, `xxl=16`. Unknown sizes default to `2` (medium).
fn size_to_time(size: Option<&str>) -> f64 {
    match size {
        Some("xxs") => 0.5,
        Some("xs") => 1.0,
        Some("s") => 1.0,
        Some("m") => 2.0,
        Some("l") => 4.0,
        Some("xl") => 8.0,
        Some("xxl") => 16.0,
        _ => 2.0, // default to medium
    }
}

// ---------------------------------------------------------------------------
// Indexability check
// ---------------------------------------------------------------------------

/// Check whether the current workload satisfies Whittle indexability conditions.
///
/// Indexability requires:
/// 1. **Acyclic dependencies** — the dependency graph must be a DAG (no cycles).
///    Items in a cycle cannot be independently scheduled.
/// 2. **Decomposable rewards** — each item's reward must depend only on its own
///    state, not on the combined state of other items. This is assumed true when
///    the score map contains independent per-item scores.
///
/// Returns an [`IndexabilityResult`] with `indexable = true` if all conditions
/// hold, or a list of violations explaining why indexability fails.
#[must_use]
pub fn check_indexability(graph: &DiGraph) -> IndexabilityResult {
    let mut violations = Vec::new();

    // Check 1: Detect cycles using Tarjan's SCC algorithm.
    let sccs = petgraph::algo::tarjan_scc(graph);
    let mut cycle_count = 0;
    for scc in &sccs {
        if scc.len() > 1 {
            cycle_count += 1;
            let members: Vec<String> = scc
                .iter()
                .filter_map(|idx| graph.node_weight(*idx).cloned())
                .collect();
            violations.push(format!(
                "Dependency cycle detected among {} items: [{}]",
                members.len(),
                members.join(", "),
            ));
        } else if scc.len() == 1 {
            // Check for self-loop.
            let idx = scc[0];
            if graph.contains_edge(idx, idx) {
                let id = graph
                    .node_weight(idx)
                    .cloned()
                    .unwrap_or_else(|| "?".to_string());
                violations.push(format!("Self-loop on item {id}"));
                cycle_count += 1;
            }
        }
    }

    if cycle_count > 0 {
        violations.insert(
            0,
            format!(
                "Workload has {cycle_count} dependency cycle(s); \
                 Whittle Index requires DAG structure. Use fallback scheduler."
            ),
        );
    }

    IndexabilityResult {
        indexable: violations.is_empty(),
        violations,
    }
}

// ---------------------------------------------------------------------------
// Whittle Index computation
// ---------------------------------------------------------------------------

/// Compute Whittle indices for all items in the dependency graph.
///
/// # Arguments
///
/// * `graph` — Directed dependency graph where edge `A → B` means "A blocks B".
/// * `scores` — Composite scores for each item (keyed by item ID). Items not
///   in the map are assigned a score of `0.0`.
/// * `sizes` — T-shirt sizes for each item (keyed by item ID). Items not in
///   the map default to `"m"` (medium).
/// * `in_progress` — Set of item IDs currently being worked on. Items blocked
///   by an in-progress item receive a discount because they may become
///   unblocked soon (but are not actionable yet).
/// * `config` — Tuning parameters for the computation.
///
/// # Returns
///
/// A vector of [`WhittleIndex`] sorted in descending order by index value
/// (highest priority first).
///
/// # Panics
///
/// Does not panic. Gracefully handles missing data with defaults.
#[must_use]
pub fn compute_whittle_indices(
    graph: &DiGraph,
    scores: &HashMap<String, f64>,
    sizes: &HashMap<String, String>,
    in_progress: &[String],
    config: &WhittleConfig,
) -> Vec<WhittleIndex> {
    let in_progress_set: std::collections::HashSet<&str> =
        in_progress.iter().map(String::as_str).collect();

    // Build a node-index-to-item-id map.
    let idx_to_id: HashMap<NodeIndex, &str> = graph
        .node_indices()
        .filter_map(|idx| graph.node_weight(idx).map(|id| (idx, id.as_str())))
        .collect();
    let id_to_idx: HashMap<&str, NodeIndex> = idx_to_id
        .iter()
        .map(|(&idx, &id)| (id, idx))
        .collect();

    let mut indices: Vec<WhittleIndex> = Vec::with_capacity(graph.node_count());

    for idx in graph.node_indices() {
        let Some(item_id) = graph.node_weight(idx) else {
            continue;
        };

        // Base value: composite score for this item (default 0.0).
        let base_value = scores.get(item_id.as_str()).copied().unwrap_or(0.0);

        // Unblock value: sum of scores of items directly blocked by this one,
        // weighted by probability of unblocking. For simplicity, P(unblock) = 1
        // if this item is the ONLY remaining blocker, otherwise 1/k where k is
        // the number of unsatisfied blockers.
        let unblock_value = compute_unblock_value(
            graph,
            idx,
            scores,
            &idx_to_id,
            &id_to_idx,
            &in_progress_set,
        );

        // Expected completion time from size.
        let size_str = sizes.get(item_id.as_str()).map(String::as_str);
        let expected_time = size_to_time(size_str);

        // Dynamic block discount: if any of this item's dependencies
        // (predecessors in the graph) are currently in-progress, apply a
        // discount because this item cannot be started yet.
        let is_dynamically_blocked = graph
            .neighbors_directed(idx, Direction::Incoming)
            .any(|pred_idx| {
                idx_to_id
                    .get(&pred_idx)
                    .is_some_and(|pred_id| in_progress_set.contains(pred_id))
            });

        let total_value = base_value + unblock_value;
        let raw_index = total_value / expected_time;
        let index = if is_dynamically_blocked {
            raw_index * config.dynamic_block_discount
        } else {
            raw_index
        };

        indices.push(WhittleIndex {
            item_id: item_id.clone(),
            index,
            breakdown: WhittleBreakdown {
                base_value,
                unblock_value,
                expected_time,
                is_dynamically_blocked,
            },
        });
    }

    // Sort descending by index value; ties broken by item_id for determinism.
    indices.sort_by(|a, b| {
        b.index
            .partial_cmp(&a.index)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.item_id.cmp(&b.item_id))
    });

    indices
}

/// Compute the downstream unblock value for a given item.
///
/// For each item `j` directly blocked by `item_idx`, compute:
///   `V_j * P(unblock_j | complete item_idx)`
///
/// where `P(unblock) = 1 / num_unsatisfied_blockers(j)`.
fn compute_unblock_value(
    graph: &DiGraph,
    item_idx: NodeIndex,
    scores: &HashMap<String, f64>,
    idx_to_id: &HashMap<NodeIndex, &str>,
    _id_to_idx: &HashMap<&str, NodeIndex>,
    in_progress: &std::collections::HashSet<&str>,
) -> f64 {
    let mut total = 0.0;

    // Items directly blocked by this one = outgoing neighbors.
    for blocked_idx in graph.neighbors_directed(item_idx, Direction::Outgoing) {
        let Some(&blocked_id) = idx_to_id.get(&blocked_idx) else {
            continue;
        };

        let blocked_value = scores.get(blocked_id).copied().unwrap_or(0.0);
        if blocked_value <= 0.0 {
            continue;
        }

        // Count how many unsatisfied blockers the blocked item has.
        // A blocker is "unsatisfied" if it is NOT in-progress AND is still
        // present as an incoming edge.
        let unsatisfied_blockers = graph
            .neighbors_directed(blocked_idx, Direction::Incoming)
            .filter(|&blocker_idx| {
                // The blocker is unsatisfied if it hasn't been completed.
                // We approximate "not completed" as "not in-progress" — items
                // that are completed would have been removed from the graph.
                // Items in-progress are on their way to completion, so we
                // count them as "partially satisfied".
                let Some(&blocker_id) = idx_to_id.get(&blocker_idx) else {
                    return true; // unknown → assume unsatisfied
                };
                // If blocker is in_progress, it's being worked on and counts
                // as partially satisfied. If it's the current item we're
                // evaluating, it would become satisfied upon completion.
                blocker_idx != item_idx && !in_progress.contains(blocker_id)
            })
            .count();

        // P(unblock) = 1 if this item is the last remaining blocker,
        // otherwise 1/(unsatisfied + 1) since completing this item removes one.
        let p_unblock = if unsatisfied_blockers == 0 {
            1.0 // This item is the sole remaining blocker
        } else {
            1.0 / (unsatisfied_blockers as f64 + 1.0)
        };

        total += blocked_value * p_unblock;
    }

    total
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use petgraph::graph::DiGraph as PetDiGraph;

    /// Helper: build a graph from node/edge lists.
    fn build_graph(nodes: &[&str], edges: &[(&str, &str)]) -> DiGraph {
        let mut graph = PetDiGraph::<String, ()>::new();
        let mut node_map: HashMap<&str, NodeIndex> = HashMap::new();

        for &id in nodes {
            let idx = graph.add_node(id.to_string());
            node_map.insert(id, idx);
        }

        for &(from, to) in edges {
            let from_idx = node_map[from];
            let to_idx = node_map[to];
            graph.add_edge(from_idx, to_idx, ());
        }

        graph
    }

    fn scores(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    fn sizes(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Indexability checks
    // -----------------------------------------------------------------------

    #[test]
    fn indexability_passes_for_dag() {
        let graph = build_graph(
            &["bn-a", "bn-b", "bn-c"],
            &[("bn-a", "bn-b"), ("bn-b", "bn-c")],
        );

        let result = check_indexability(&graph);
        assert!(result.indexable, "DAG should be indexable");
        assert!(result.violations.is_empty());
    }

    #[test]
    fn indexability_passes_for_empty_graph() {
        let graph = build_graph(&[], &[]);
        let result = check_indexability(&graph);
        assert!(result.indexable);
    }

    #[test]
    fn indexability_passes_for_isolated_nodes() {
        let graph = build_graph(&["bn-a", "bn-b", "bn-c"], &[]);
        let result = check_indexability(&graph);
        assert!(result.indexable);
    }

    #[test]
    fn indexability_fails_for_cycle() {
        let graph = build_graph(
            &["bn-a", "bn-b", "bn-c"],
            &[("bn-a", "bn-b"), ("bn-b", "bn-a")],
        );

        let result = check_indexability(&graph);
        assert!(!result.indexable, "cycle should fail indexability");
        assert!(result.violations.len() >= 2); // summary + details
        assert!(result.violations[0].contains("cycle"));
    }

    #[test]
    fn indexability_fails_for_self_loop() {
        let graph = build_graph(&["bn-a"], &[("bn-a", "bn-a")]);

        let result = check_indexability(&graph);
        assert!(!result.indexable, "self-loop should fail indexability");
        assert!(result.violations.iter().any(|v| v.contains("Self-loop")));
    }

    // -----------------------------------------------------------------------
    // Whittle Index computation
    // -----------------------------------------------------------------------

    #[test]
    fn single_item_gets_score_divided_by_time() {
        let graph = build_graph(&["bn-a"], &[]);
        let s = scores(&[("bn-a", 10.0)]);
        let sz = sizes(&[("bn-a", "s")]); // time = 1.0

        let indices =
            compute_whittle_indices(&graph, &s, &sz, &[], &WhittleConfig::default());

        assert_eq!(indices.len(), 1);
        assert_eq!(indices[0].item_id, "bn-a");
        assert!((indices[0].index - 10.0).abs() < f64::EPSILON);
        assert!((indices[0].breakdown.base_value - 10.0).abs() < f64::EPSILON);
        assert!((indices[0].breakdown.unblock_value - 0.0).abs() < f64::EPSILON);
        assert!((indices[0].breakdown.expected_time - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn higher_score_ranks_higher() {
        let graph = build_graph(&["bn-a", "bn-b"], &[]);
        let s = scores(&[("bn-a", 5.0), ("bn-b", 10.0)]);
        let sz = sizes(&[("bn-a", "m"), ("bn-b", "m")]); // same size

        let indices =
            compute_whittle_indices(&graph, &s, &sz, &[], &WhittleConfig::default());

        assert_eq!(indices[0].item_id, "bn-b");
        assert_eq!(indices[1].item_id, "bn-a");
    }

    #[test]
    fn smaller_size_ranks_higher_for_same_score() {
        let graph = build_graph(&["bn-a", "bn-b"], &[]);
        let s = scores(&[("bn-a", 10.0), ("bn-b", 10.0)]);
        let sz = sizes(&[("bn-a", "l"), ("bn-b", "s")]); // bn-a: 4, bn-b: 1

        let indices =
            compute_whittle_indices(&graph, &s, &sz, &[], &WhittleConfig::default());

        // bn-b: 10/1 = 10, bn-a: 10/4 = 2.5
        assert_eq!(indices[0].item_id, "bn-b");
        assert!((indices[0].index - 10.0).abs() < f64::EPSILON);
        assert!((indices[1].index - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn blocker_gets_unblock_bonus() {
        // bn-a blocks bn-b. Completing bn-a unblocks bn-b, so bn-a should
        // get a bonus from bn-b's value.
        let graph = build_graph(&["bn-a", "bn-b"], &[("bn-a", "bn-b")]);
        let s = scores(&[("bn-a", 5.0), ("bn-b", 8.0)]);
        let sz = sizes(&[("bn-a", "s"), ("bn-b", "s")]); // both time=1

        let indices =
            compute_whittle_indices(&graph, &s, &sz, &[], &WhittleConfig::default());

        // bn-a: (5 + 8*1.0) / 1 = 13.0 (sole blocker, P=1)
        // bn-b: (8 + 0) / 1 = 8.0
        let a = indices.iter().find(|i| i.item_id == "bn-a").unwrap();
        let b = indices.iter().find(|i| i.item_id == "bn-b").unwrap();

        assert!((a.index - 13.0).abs() < f64::EPSILON, "bn-a: {}", a.index);
        assert!((b.index - 8.0).abs() < f64::EPSILON, "bn-b: {}", b.index);
        assert!(a.index > b.index, "blocker should rank higher");
        assert!((a.breakdown.unblock_value - 8.0).abs() < f64::EPSILON);
    }

    #[test]
    fn multiple_blockers_share_unblock_probability() {
        // Both bn-a and bn-b block bn-c. Each gets partial unblock credit.
        let graph = build_graph(
            &["bn-a", "bn-b", "bn-c"],
            &[("bn-a", "bn-c"), ("bn-b", "bn-c")],
        );
        let s = scores(&[("bn-a", 5.0), ("bn-b", 5.0), ("bn-c", 10.0)]);
        let sz = sizes(&[("bn-a", "s"), ("bn-b", "s"), ("bn-c", "s")]);

        let indices =
            compute_whittle_indices(&graph, &s, &sz, &[], &WhittleConfig::default());

        let a = indices.iter().find(|i| i.item_id == "bn-a").unwrap();
        let b = indices.iter().find(|i| i.item_id == "bn-b").unwrap();

        // Each is one of 2 blockers. The other blocker is unsatisfied (count=1).
        // P(unblock) = 1/(1+1) = 0.5
        // bn-a: (5 + 10*0.5) / 1 = 10.0
        assert!(
            (a.breakdown.unblock_value - 5.0).abs() < f64::EPSILON,
            "a unblock: {}",
            a.breakdown.unblock_value,
        );
        assert!(
            (b.breakdown.unblock_value - 5.0).abs() < f64::EPSILON,
            "b unblock: {}",
            b.breakdown.unblock_value,
        );
    }

    #[test]
    fn dynamic_block_discount_applied() {
        // bn-a blocks bn-b. bn-a is in-progress. bn-b gets a discount.
        let graph = build_graph(&["bn-a", "bn-b"], &[("bn-a", "bn-b")]);
        let s = scores(&[("bn-a", 5.0), ("bn-b", 10.0)]);
        let sz = sizes(&[("bn-a", "s"), ("bn-b", "s")]);
        let in_progress = vec!["bn-a".to_string()];

        let config = WhittleConfig::default();
        let indices = compute_whittle_indices(&graph, &s, &sz, &in_progress, &config);

        let b = indices.iter().find(|i| i.item_id == "bn-b").unwrap();
        assert!(b.breakdown.is_dynamically_blocked);
        // bn-b: (10 + 0) / 1 * 0.3 = 3.0
        assert!(
            (b.index - 3.0).abs() < f64::EPSILON,
            "discounted: {}",
            b.index,
        );
    }

    #[test]
    fn in_progress_blocker_boosts_unblock_probability() {
        // bn-a and bn-b block bn-c. bn-a is in-progress.
        // When computing bn-b's unblock value for bn-c: bn-a is in-progress
        // so it counts as "partially satisfied", meaning bn-b has higher
        // P(unblock) for bn-c.
        let graph = build_graph(
            &["bn-a", "bn-b", "bn-c"],
            &[("bn-a", "bn-c"), ("bn-b", "bn-c")],
        );
        let s = scores(&[("bn-a", 5.0), ("bn-b", 5.0), ("bn-c", 10.0)]);
        let sz = sizes(&[("bn-a", "s"), ("bn-b", "s"), ("bn-c", "s")]);

        // Without in-progress: each gets P=0.5, unblock=5
        let indices_base =
            compute_whittle_indices(&graph, &s, &sz, &[], &WhittleConfig::default());
        let b_base = indices_base.iter().find(|i| i.item_id == "bn-b").unwrap();

        // With bn-a in-progress: bn-a is partially satisfied for bn-c, so
        // bn-b's P(unblock) should be higher.
        let in_progress = vec!["bn-a".to_string()];
        let indices_ip =
            compute_whittle_indices(&graph, &s, &sz, &in_progress, &WhittleConfig::default());
        let b_ip = indices_ip.iter().find(|i| i.item_id == "bn-b").unwrap();

        assert!(
            b_ip.breakdown.unblock_value > b_base.breakdown.unblock_value,
            "in-progress sibling should increase unblock probability: {} vs {}",
            b_ip.breakdown.unblock_value,
            b_base.breakdown.unblock_value,
        );
    }

    #[test]
    fn missing_score_defaults_to_zero() {
        let graph = build_graph(&["bn-a"], &[]);
        let s = scores(&[]); // no scores
        let sz = sizes(&[]);

        let indices =
            compute_whittle_indices(&graph, &s, &sz, &[], &WhittleConfig::default());

        assert_eq!(indices.len(), 1);
        assert!((indices[0].index - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn missing_size_defaults_to_medium() {
        let graph = build_graph(&["bn-a"], &[]);
        let s = scores(&[("bn-a", 10.0)]);
        let sz = sizes(&[]); // no sizes

        let indices =
            compute_whittle_indices(&graph, &s, &sz, &[], &WhittleConfig::default());

        // Default size = medium = 2 time units
        // Index = 10 / 2 = 5
        assert!((indices[0].index - 5.0).abs() < f64::EPSILON);
        assert!((indices[0].breakdown.expected_time - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_graph_returns_empty() {
        let graph = build_graph(&[], &[]);
        let indices = compute_whittle_indices(
            &graph,
            &HashMap::new(),
            &HashMap::new(),
            &[],
            &WhittleConfig::default(),
        );
        assert!(indices.is_empty());
    }

    #[test]
    fn ordering_is_deterministic() {
        let graph = build_graph(&["bn-a", "bn-b", "bn-c"], &[]);
        let s = scores(&[("bn-a", 5.0), ("bn-b", 5.0), ("bn-c", 5.0)]);
        let sz = sizes(&[("bn-a", "m"), ("bn-b", "m"), ("bn-c", "m")]);

        let indices1 =
            compute_whittle_indices(&graph, &s, &sz, &[], &WhittleConfig::default());
        let indices2 =
            compute_whittle_indices(&graph, &s, &sz, &[], &WhittleConfig::default());

        let ids1: Vec<&str> = indices1.iter().map(|i| i.item_id.as_str()).collect();
        let ids2: Vec<&str> = indices2.iter().map(|i| i.item_id.as_str()).collect();
        assert_eq!(ids1, ids2, "ordering should be deterministic");
    }

    #[test]
    fn chain_dependency_rewards_root_blocker() {
        // bn-a → bn-b → bn-c (chain). bn-a should get the most value because
        // it blocks bn-b which blocks bn-c.
        let graph = build_graph(
            &["bn-a", "bn-b", "bn-c"],
            &[("bn-a", "bn-b"), ("bn-b", "bn-c")],
        );
        let s = scores(&[("bn-a", 3.0), ("bn-b", 3.0), ("bn-c", 3.0)]);
        let sz = sizes(&[("bn-a", "s"), ("bn-b", "s"), ("bn-c", "s")]);

        let indices =
            compute_whittle_indices(&graph, &s, &sz, &[], &WhittleConfig::default());

        let a = indices.iter().find(|i| i.item_id == "bn-a").unwrap();
        let b = indices.iter().find(|i| i.item_id == "bn-b").unwrap();
        let c = indices.iter().find(|i| i.item_id == "bn-c").unwrap();

        // bn-a: (3 + 3*1.0) / 1 = 6.0 (blocks bn-b with sole blocker P=1)
        // bn-b: (3 + 3*1.0) / 1 = 6.0 (blocks bn-c with sole blocker P=1)
        // bn-c: (3 + 0) / 1 = 3.0
        assert!((a.index - 6.0).abs() < f64::EPSILON, "a: {}", a.index);
        assert!((b.index - 6.0).abs() < f64::EPSILON, "b: {}", b.index);
        assert!((c.index - 3.0).abs() < f64::EPSILON, "c: {}", c.index);
        assert!(a.index >= c.index);
    }

    #[test]
    fn size_to_time_covers_all_sizes() {
        assert!((size_to_time(Some("xxs")) - 0.5).abs() < f64::EPSILON);
        assert!((size_to_time(Some("xs")) - 1.0).abs() < f64::EPSILON);
        assert!((size_to_time(Some("s")) - 1.0).abs() < f64::EPSILON);
        assert!((size_to_time(Some("m")) - 2.0).abs() < f64::EPSILON);
        assert!((size_to_time(Some("l")) - 4.0).abs() < f64::EPSILON);
        assert!((size_to_time(Some("xl")) - 8.0).abs() < f64::EPSILON);
        assert!((size_to_time(Some("xxl")) - 16.0).abs() < f64::EPSILON);
        assert!((size_to_time(None) - 2.0).abs() < f64::EPSILON);
        assert!((size_to_time(Some("unknown")) - 2.0).abs() < f64::EPSILON);
    }
}
