//! Planning and health diagnostics over dependency graphs.
//!
//! Exposes convenience helpers used by CLI diagnostics commands:
//! - [`topological_layers`] for parallel execution planning
//! - [`health_metrics`] for dashboard summaries
//! - [`find_sccs`] for cycle reporting

use std::collections::{BTreeSet, HashMap};

use petgraph::{
    Direction,
    algo::condensation,
    graph::{DiGraph as PetDiGraph, NodeIndex},
    visit::{EdgeRef, IntoNodeIdentifiers},
};

use crate::graph::{
    build::RawGraph, compute_critical_path, cycles::find_all_cycles, normalize::NormalizedGraph,
    stats::GraphStats,
};

/// Directed dependency graph type used by diagnostics helpers.
///
/// Node weights are item IDs. Edge direction is `blocker -> blocked`.
pub type DiGraph = PetDiGraph<String, ()>;

/// Project-level health metrics derived from a dependency graph.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthMetrics {
    /// Graph density in `[0.0, 1.0]`.
    pub density: f64,
    /// Number of strongly connected components.
    pub scc_count: usize,
    /// Length of the longest dependency chain.
    pub critical_path_length: usize,
    /// Number of items currently blocking at least one other item.
    pub blocker_count: usize,
}

/// Compute topological layers for parallel execution planning.
///
/// Each returned layer contains items that can be worked in parallel given the
/// edge direction `blocker -> blocked`.
///
/// When `scope` is provided, only nodes equal to `scope` or prefixed by
/// `"{scope}."` are included. This supports subtree-style IDs.
#[must_use]
pub fn topological_layers(graph: &DiGraph, scope: Option<&str>) -> Vec<Vec<String>> {
    let scoped_ids = scoped_node_ids(graph, scope);
    if scoped_ids.is_empty() {
        return Vec::new();
    }

    let scoped_graph = build_scoped_graph(graph, &scoped_ids);

    // Condense SCCs first so we can still produce deterministic layers when
    // the raw graph contains cycles.
    let condensed: PetDiGraph<Vec<String>, ()> = condensation(scoped_graph, true);

    let mut indegree: HashMap<NodeIndex, usize> = condensed
        .node_identifiers()
        .map(|idx| {
            (
                idx,
                condensed
                    .neighbors_directed(idx, Direction::Incoming)
                    .count(),
            )
        })
        .collect();

    let mut ready: Vec<NodeIndex> = indegree
        .iter()
        .filter_map(|(idx, deg)| (*deg == 0).then_some(*idx))
        .collect();
    ready.sort_by(|a, b| representative(&condensed, *a).cmp(representative(&condensed, *b)));

    let mut layers: Vec<Vec<String>> = Vec::new();

    while !ready.is_empty() {
        let current = std::mem::take(&mut ready);
        let mut next_ready: Vec<NodeIndex> = Vec::new();
        let mut layer: Vec<String> = Vec::new();

        for idx in current {
            let mut members = condensed.node_weight(idx).cloned().unwrap_or_default();
            members.sort_unstable();
            layer.extend(members);

            for edge in condensed.edges_directed(idx, Direction::Outgoing) {
                let target = edge.target();
                if let Some(entry) = indegree.get_mut(&target) {
                    if *entry > 0 {
                        *entry -= 1;
                        if *entry == 0 {
                            next_ready.push(target);
                        }
                    }
                }
            }

            indegree.remove(&idx);
        }

        layer.sort_unstable();
        if !layer.is_empty() {
            layers.push(layer);
        }

        next_ready
            .sort_by(|a, b| representative(&condensed, *a).cmp(representative(&condensed, *b)));
        next_ready.dedup();
        ready = next_ready;
    }

    // Defensive fallback: condensation should be acyclic, but if any nodes
    // remain due to unexpected graph corruption, emit them deterministically.
    if !indegree.is_empty() {
        let mut leftovers: Vec<NodeIndex> = indegree.keys().copied().collect();
        leftovers
            .sort_by(|a, b| representative(&condensed, *a).cmp(representative(&condensed, *b)));

        for idx in leftovers {
            let mut layer = condensed.node_weight(idx).cloned().unwrap_or_default();
            layer.sort_unstable();
            if !layer.is_empty() {
                layers.push(layer);
            }
        }
    }

    layers
}

/// Compute project health metrics from a dependency graph.
#[must_use]
pub fn health_metrics(graph: &DiGraph) -> HealthMetrics {
    let node_map: HashMap<String, NodeIndex> = graph
        .node_identifiers()
        .filter_map(|idx| graph.node_weight(idx).map(|id| (id.clone(), idx)))
        .collect();

    let raw = RawGraph {
        graph: graph.clone(),
        node_map,
        content_hash: graph_content_hash(graph),
    };

    let normalized = NormalizedGraph::from_raw(raw);
    let stats = GraphStats::from_normalized(&normalized);
    let cp = compute_critical_path(&normalized);

    let blocker_count = graph
        .node_identifiers()
        .filter(|&idx| {
            graph
                .neighbors_directed(idx, Direction::Outgoing)
                .next()
                .is_some()
        })
        .count();

    HealthMetrics {
        density: stats.density,
        scc_count: stats.scc_count,
        critical_path_length: cp.total_length,
        blocker_count,
    }
}

/// Find dependency cycles (strongly connected components).
///
/// Returns only SCCs that represent actual cycles:
/// - components with 2+ members
/// - self-loop singleton components
#[must_use]
pub fn find_sccs(graph: &DiGraph) -> Vec<Vec<String>> {
    find_all_cycles(graph)
}

fn scoped_node_ids(graph: &DiGraph, scope: Option<&str>) -> BTreeSet<String> {
    match scope {
        None => graph.node_weights().cloned().collect(),
        Some(scope_id) => {
            let child_prefix = format!("{scope_id}.");
            graph
                .node_weights()
                .filter(|id| id.as_str() == scope_id || id.starts_with(&child_prefix))
                .cloned()
                .collect()
        }
    }
}

fn build_scoped_graph(graph: &DiGraph, scoped_ids: &BTreeSet<String>) -> DiGraph {
    let mut scoped = DiGraph::new();

    let raw_nodes: HashMap<&str, NodeIndex> = graph
        .node_identifiers()
        .filter_map(|idx| graph.node_weight(idx).map(|id| (id.as_str(), idx)))
        .collect();

    let mut scoped_nodes: HashMap<&str, NodeIndex> = HashMap::with_capacity(scoped_ids.len());

    for item_id in scoped_ids {
        let idx = scoped.add_node(item_id.clone());
        scoped_nodes.insert(item_id.as_str(), idx);
    }

    for from_id in scoped_ids {
        let Some(&from_raw_idx) = raw_nodes.get(from_id.as_str()) else {
            continue;
        };
        let Some(&from_scoped_idx) = scoped_nodes.get(from_id.as_str()) else {
            continue;
        };

        for to_raw_idx in graph.neighbors_directed(from_raw_idx, Direction::Outgoing) {
            let Some(to_id) = graph.node_weight(to_raw_idx).map(String::as_str) else {
                continue;
            };
            let Some(&to_scoped_idx) = scoped_nodes.get(to_id) else {
                continue;
            };

            if !scoped.contains_edge(from_scoped_idx, to_scoped_idx) {
                scoped.add_edge(from_scoped_idx, to_scoped_idx, ());
            }
        }
    }

    scoped
}

fn representative(graph: &PetDiGraph<Vec<String>, ()>, idx: NodeIndex) -> &str {
    graph
        .node_weight(idx)
        .and_then(|members| members.iter().min())
        .map(String::as_str)
        .unwrap_or("")
}

fn graph_content_hash(graph: &DiGraph) -> String {
    let mut edges: Vec<(String, String)> = graph
        .edge_references()
        .filter_map(|edge| {
            let source = graph.node_weight(edge.source())?;
            let target = graph.node_weight(edge.target())?;
            Some((source.clone(), target.clone()))
        })
        .collect();
    edges.sort_unstable();

    let mut hasher = blake3::Hasher::new();
    for (source, target) in edges {
        hasher.update(source.as_bytes());
        hasher.update(b"\x00");
        hasher.update(target.as_bytes());
        hasher.update(b"\x00");
    }

    format!("blake3:{}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_graph(nodes: &[&str], edges: &[(&str, &str)]) -> DiGraph {
        let mut graph = DiGraph::new();
        let mut node_map: HashMap<&str, NodeIndex> = HashMap::new();

        for id in nodes {
            let idx = graph.add_node((*id).to_string());
            node_map.insert(*id, idx);
        }

        for (from, to) in edges {
            let from_idx = node_map[from];
            let to_idx = node_map[to];
            graph.add_edge(from_idx, to_idx, ());
        }

        graph
    }

    #[test]
    fn topological_layers_linear_chain() {
        let graph = build_graph(
            &["bn-a", "bn-b", "bn-c"],
            &[("bn-a", "bn-b"), ("bn-b", "bn-c")],
        );

        let layers = topological_layers(&graph, None);

        assert_eq!(
            layers,
            vec![
                vec!["bn-a".to_string()],
                vec!["bn-b".to_string()],
                vec!["bn-c".to_string()],
            ]
        );
    }

    #[test]
    fn topological_layers_parallel_work() {
        let graph = build_graph(
            &["bn-a", "bn-b", "bn-c", "bn-d"],
            &[("bn-a", "bn-c"), ("bn-b", "bn-c"), ("bn-c", "bn-d")],
        );

        let layers = topological_layers(&graph, None);

        assert_eq!(
            layers,
            vec![
                vec!["bn-a".to_string(), "bn-b".to_string()],
                vec!["bn-c".to_string()],
                vec!["bn-d".to_string()],
            ]
        );
    }

    #[test]
    fn topological_layers_condenses_cycles() {
        let graph = build_graph(
            &["bn-a", "bn-b", "bn-c"],
            &[("bn-a", "bn-b"), ("bn-b", "bn-a"), ("bn-b", "bn-c")],
        );

        let layers = topological_layers(&graph, None);

        assert_eq!(
            layers,
            vec![
                vec!["bn-a".to_string(), "bn-b".to_string()],
                vec!["bn-c".to_string()],
            ]
        );
    }

    #[test]
    fn find_sccs_detects_cycles_and_self_loops() {
        let graph = build_graph(
            &["bn-a", "bn-b", "bn-c", "bn-d"],
            &[("bn-a", "bn-b"), ("bn-b", "bn-a"), ("bn-c", "bn-c")],
        );

        let sccs = find_sccs(&graph);

        assert_eq!(
            sccs,
            vec![
                vec!["bn-a".to_string(), "bn-b".to_string()],
                vec!["bn-c".to_string()],
            ]
        );
    }

    #[test]
    fn health_metrics_reports_expected_values() {
        let graph = build_graph(
            &["bn-a", "bn-b", "bn-c"],
            &[("bn-a", "bn-b"), ("bn-b", "bn-c"), ("bn-a", "bn-c")],
        );

        let metrics = health_metrics(&graph);

        assert!((metrics.density - 0.5).abs() < f64::EPSILON);
        assert_eq!(metrics.scc_count, 3);
        assert_eq!(metrics.critical_path_length, 3);
        assert_eq!(metrics.blocker_count, 2);
    }
}
