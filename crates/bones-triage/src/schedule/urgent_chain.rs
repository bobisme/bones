//! Urgent-chain assignment mode for multi-agent scheduling.
//!
//! When an urgent dependency chain exists, this scheduler seeds the first K
//! slots with items from the critical chain (prerequisites of urgent items),
//! then fills remaining slots using the standard scheduler ordering.
//!
//! # Algorithm
//!
//! 1. Identify all items with `score == f64::INFINITY` (urgent) that are
//!    blocked (have unresolved dependencies).
//! 2. Walk backwards through the dependency graph collecting their
//!    unblocked prerequisites — these form the "urgent chain front".
//! 3. Rank the chain-front items by their Whittle index (or score).
//! 4. Assign the first K slots to chain-front items (K = number of
//!    chain-front items, capped at `agent_slots`).
//! 5. Fill remaining slots from the normal ranked ordering, skipping
//!    items already assigned.

use std::collections::{HashMap, HashSet, VecDeque};

use petgraph::Direction;

use crate::graph::diagnostics::DiGraph;

/// Result of urgent-chain analysis: ordered item IDs to assign.
#[derive(Debug, Clone)]
pub struct UrgentChainResult {
    /// Items from the urgent chain front, in priority order.
    pub chain_front: Vec<String>,
    /// True if any urgent chain was found.
    pub has_urgent_chain: bool,
}

/// Identify the urgent chain front: unblocked prerequisites of blocked urgent
/// items.
///
/// # Arguments
///
/// * `graph` - Directed dependency graph where edge A -> B means "A blocks B".
/// * `scores` - Composite scores keyed by item ID.
/// * `unblocked_ids` - Set of item IDs that are currently unblocked (ready to work).
/// * `urgent_ids` - Set of item IDs that are urgent.
///
/// # Returns
///
/// An [`UrgentChainResult`] with the chain front items sorted by score
/// descending for deterministic assignment.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn find_urgent_chain_front(
    graph: &DiGraph,
    scores: &HashMap<String, f64>,
    unblocked_ids: &HashSet<&str>,
    urgent_ids: &HashSet<&str>,
) -> UrgentChainResult {
    // Step 1: Find blocked urgent items (urgent items NOT in unblocked set,
    // OR urgent items that are unblocked but have predecessors that are also
    // urgent — we want to find the prerequisites chain).
    // Actually, we want to find urgent items that are blocked, then walk
    // backwards to find their unblocked prerequisites.
    //
    // But we also want to capture the case where an urgent item IS unblocked
    // — it should be in the chain front directly.

    let idx_to_id: HashMap<petgraph::graph::NodeIndex, &str> = graph
        .node_indices()
        .filter_map(|idx| graph.node_weight(idx).map(|id| (idx, id.as_str())))
        .collect();

    let id_to_idx: HashMap<&str, petgraph::graph::NodeIndex> =
        idx_to_id.iter().map(|(&idx, &id)| (id, idx)).collect();

    // Find all urgent items (both blocked and unblocked).
    let blocked_urgent: Vec<&str> = urgent_ids
        .iter()
        .filter(|&&id| !unblocked_ids.contains(id))
        .copied()
        .collect();

    if blocked_urgent.is_empty() {
        // No blocked urgent items. If there are unblocked urgent items, they
        // will naturally rank first via their infinite score. No special
        // chain-front logic needed.
        return UrgentChainResult {
            chain_front: Vec::new(),
            has_urgent_chain: false,
        };
    }

    // Step 2: BFS backwards from blocked urgent items to find unblocked
    // prerequisites (the "chain front").
    let mut chain_front: HashSet<&str> = HashSet::new();
    let mut visited: HashSet<petgraph::graph::NodeIndex> = HashSet::new();

    for &urgent_id in &blocked_urgent {
        let Some(&start_idx) = id_to_idx.get(urgent_id) else {
            continue;
        };

        let mut queue: VecDeque<petgraph::graph::NodeIndex> = VecDeque::new();
        // Walk predecessors (incoming edges = items that block this one).
        for pred in graph.neighbors_directed(start_idx, Direction::Incoming) {
            queue.push_back(pred);
        }

        while let Some(node) = queue.pop_front() {
            if !visited.insert(node) {
                continue;
            }

            let Some(&item_id) = idx_to_id.get(&node) else {
                continue;
            };

            if unblocked_ids.contains(item_id) {
                // This is an unblocked prerequisite — part of the chain front.
                chain_front.insert(item_id);
                // Don't walk further back from an unblocked item — it's actionable.
            } else {
                // This item is also blocked; keep walking backwards.
                for pred in graph.neighbors_directed(node, Direction::Incoming) {
                    if !visited.contains(&pred) {
                        queue.push_back(pred);
                    }
                }
            }
        }
    }

    // Step 3: Sort chain front by score descending (ties by ID for determinism).
    let mut front_vec: Vec<String> = chain_front.into_iter().map(String::from).collect();
    front_vec.sort_by(|a, b| {
        let sa = scores.get(a.as_str()).copied().unwrap_or(0.0);
        let sb = scores.get(b.as_str()).copied().unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(b))
    });

    UrgentChainResult {
        has_urgent_chain: !front_vec.is_empty(),
        chain_front: front_vec,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use petgraph::graph::DiGraph as PetDiGraph;

    fn build_graph(nodes: &[&str], edges: &[(&str, &str)]) -> crate::graph::diagnostics::DiGraph {
        let mut graph = PetDiGraph::<String, ()>::new();
        let mut node_map: HashMap<&str, petgraph::graph::NodeIndex> = HashMap::new();

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

    fn id_set<'a>(ids: &[&'a str]) -> HashSet<&'a str> {
        ids.iter().copied().collect()
    }

    // -------------------------------------------------------------------
    // No urgent chain
    // -------------------------------------------------------------------

    #[test]
    fn no_urgent_items_returns_empty() {
        let graph = build_graph(&["a", "b", "c"], &[("a", "b"), ("b", "c")]);
        let s = scores(&[("a", 5.0), ("b", 3.0), ("c", 1.0)]);
        let unblocked = id_set(&["a"]);
        let urgent = id_set(&[]);

        let result = find_urgent_chain_front(&graph, &s, &unblocked, &urgent);

        assert!(!result.has_urgent_chain);
        assert!(result.chain_front.is_empty());
    }

    #[test]
    fn urgent_but_unblocked_returns_no_chain() {
        // Urgent item is unblocked, no special chain front needed.
        let graph = build_graph(&["a", "b"], &[]);
        let s = scores(&[("a", f64::INFINITY), ("b", 3.0)]);
        let unblocked = id_set(&["a", "b"]);
        let urgent = id_set(&["a"]);

        let result = find_urgent_chain_front(&graph, &s, &unblocked, &urgent);

        assert!(!result.has_urgent_chain);
        assert!(result.chain_front.is_empty());
    }

    // -------------------------------------------------------------------
    // Urgent chain present
    // -------------------------------------------------------------------

    #[test]
    fn simple_chain_finds_unblocked_prerequisite() {
        // a (unblocked) blocks b (blocked, urgent)
        let graph = build_graph(&["a", "b"], &[("a", "b")]);
        let s = scores(&[("a", 5.0), ("b", f64::INFINITY)]);
        let unblocked = id_set(&["a"]);
        let urgent = id_set(&["b"]);

        let result = find_urgent_chain_front(&graph, &s, &unblocked, &urgent);

        assert!(result.has_urgent_chain);
        assert_eq!(result.chain_front, vec!["a".to_string()]);
    }

    #[test]
    fn deep_chain_finds_root_prerequisite() {
        // a (unblocked) -> b (blocked) -> c (blocked, urgent)
        let graph = build_graph(&["a", "b", "c"], &[("a", "b"), ("b", "c")]);
        let s = scores(&[("a", 5.0), ("b", 3.0), ("c", f64::INFINITY)]);
        let unblocked = id_set(&["a"]);
        let urgent = id_set(&["c"]);

        let result = find_urgent_chain_front(&graph, &s, &unblocked, &urgent);

        assert!(result.has_urgent_chain);
        assert_eq!(result.chain_front, vec!["a".to_string()]);
    }

    #[test]
    fn multiple_prerequisites_sorted_by_score() {
        // Both a and b are unblocked prerequisites of urgent c.
        let graph = build_graph(&["a", "b", "c"], &[("a", "c"), ("b", "c")]);
        let s = scores(&[("a", 3.0), ("b", 7.0), ("c", f64::INFINITY)]);
        let unblocked = id_set(&["a", "b"]);
        let urgent = id_set(&["c"]);

        let result = find_urgent_chain_front(&graph, &s, &unblocked, &urgent);

        assert!(result.has_urgent_chain);
        assert_eq!(result.chain_front.len(), 2);
        // b has higher score, should be first.
        assert_eq!(result.chain_front[0], "b");
        assert_eq!(result.chain_front[1], "a");
    }

    #[test]
    fn chain_front_excludes_blocked_intermediaries() {
        // a (unblocked) -> b (blocked) -> c (blocked, urgent)
        // b is blocked, not in chain front. Only a is.
        let graph = build_graph(&["a", "b", "c"], &[("a", "b"), ("b", "c")]);
        let s = scores(&[("a", 5.0), ("b", 3.0), ("c", f64::INFINITY)]);
        let unblocked = id_set(&["a"]);
        let urgent = id_set(&["c"]);

        let result = find_urgent_chain_front(&graph, &s, &unblocked, &urgent);

        assert!(result.has_urgent_chain);
        assert_eq!(result.chain_front, vec!["a".to_string()]);
        assert!(
            !result.chain_front.contains(&"b".to_string()),
            "blocked intermediary should not be in chain front"
        );
    }

    #[test]
    fn deterministic_ordering_for_equal_scores() {
        let graph = build_graph(&["a", "b", "c"], &[("a", "c"), ("b", "c")]);
        let s = scores(&[("a", 5.0), ("b", 5.0), ("c", f64::INFINITY)]);
        let unblocked = id_set(&["a", "b"]);
        let urgent = id_set(&["c"]);

        let r1 = find_urgent_chain_front(&graph, &s, &unblocked, &urgent);
        let r2 = find_urgent_chain_front(&graph, &s, &unblocked, &urgent);

        assert_eq!(
            r1.chain_front, r2.chain_front,
            "ordering must be deterministic"
        );
        // With equal scores, sorted by ID ascending.
        assert_eq!(r1.chain_front, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn unrelated_items_not_in_chain_front() {
        // a (unblocked) -> b (blocked, urgent)
        // x (unblocked, unrelated)
        let graph = build_graph(&["a", "b", "x"], &[("a", "b")]);
        let s = scores(&[("a", 5.0), ("b", f64::INFINITY), ("x", 10.0)]);
        let unblocked = id_set(&["a", "x"]);
        let urgent = id_set(&["b"]);

        let result = find_urgent_chain_front(&graph, &s, &unblocked, &urgent);

        assert!(result.has_urgent_chain);
        assert_eq!(result.chain_front, vec!["a".to_string()]);
        assert!(
            !result.chain_front.contains(&"x".to_string()),
            "unrelated item should not be in chain front"
        );
    }
}
