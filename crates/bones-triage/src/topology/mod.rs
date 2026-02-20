//! Advanced topological analysis for dependency graphs.
//!
//! # Overview
//!
//! This module implements advanced graph analysis techniques:
//! - **Spectral Sparsification**: Reduces graph size while preserving spectral properties (eigenvalues of Laplacian).
//! - **Path Homology**: Computes directed path homology groups (H0, H1) to detect independent components and structural cycles.
//!
//! These features are gated behind the `advanced` topology mode, as they are computationally expensive (O(N^3) or worse).

pub mod homology;
pub mod sparsify;

use petgraph::Direction;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TopologyMode {
    #[default]
    Basic,
    Advanced,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyResult {
    pub mode: TopologyMode,
    pub advanced_applied: bool,
    pub spectral_gap: Option<f64>,
    pub betti_0: Option<usize>, // Connected components
    pub betti_1: Option<usize>, // Cycles (structural holes)
    pub effective_resistance_stats: Option<Vec<f64>>,
    pub messages: Vec<String>,
}

/// Run topological analysis on the graph.
pub fn analyze<N, E>(
    graph: &petgraph::Graph<N, E>,
    mode: TopologyMode,
) -> anyhow::Result<TopologyResult>
where
    N: std::hash::Hash + Eq + Clone + std::fmt::Debug,
    E: std::fmt::Debug,
{
    if mode == TopologyMode::Basic {
        return Ok(basic_result(Vec::new()));
    }

    // Runtime precondition checks and safe fallback to basic diagnostics.
    if graph.node_count() > 2000 {
        return Ok(basic_result(vec![format!(
            "advanced topology preconditions not met: graph too large ({} nodes, limit: 2000); falling back to basic diagnostics",
            graph.node_count()
        )]));
    }

    let mut messages = Vec::new();
    let mut advanced_applied = false;

    let use_symmetrized_laplacian = !is_eulerian(graph);
    if use_symmetrized_laplacian {
        messages.push(
            "directed-Laplacian preconditions not met (graph is not Eulerian); using approved symmetrized approximation"
                .to_string(),
        );
    }

    let (betti_0, betti_1) = match homology::compute_betti_numbers(graph) {
        Ok((b0, b1)) => {
            advanced_applied = true;
            (Some(b0), Some(b1))
        }
        Err(err) => {
            messages.push(format!(
                "path homology preconditions not met: {err}; omitting homology output"
            ));
            (None, None)
        }
    };

    let (spectral_gap, resistances) = match sparsify::SpectralSparsifier::new(graph).and_then(
        |sparsifier| {
            let resistances = sparsifier.effective_resistances()?;
            let gap = sparsifier.spectral_gap()?;
            Ok((gap, resistances))
        },
    ) {
        Ok((gap, resistances)) => {
            advanced_applied = true;
            (Some(gap), Some(resistances))
        }
        Err(err) => {
            messages.push(format!(
                "spectral sparsification preconditions not met: {err}; omitting sparsification output"
            ));
            (None, None)
        }
    };

    Ok(TopologyResult {
        mode: if advanced_applied {
            TopologyMode::Advanced
        } else {
            TopologyMode::Basic
        },
        advanced_applied,
        spectral_gap,
        betti_0,
        betti_1,
        effective_resistance_stats: resistances,
        messages,
    })
}

fn basic_result(messages: Vec<String>) -> TopologyResult {
    TopologyResult {
        mode: TopologyMode::Basic,
        advanced_applied: false,
        spectral_gap: None,
        betti_0: None,
        betti_1: None,
        effective_resistance_stats: None,
        messages,
    }
}

fn is_eulerian<N, E>(graph: &petgraph::Graph<N, E>) -> bool {
    graph.node_indices().all(|node| {
        graph.neighbors_directed(node, Direction::Incoming).count()
            == graph.neighbors_directed(node, Direction::Outgoing).count()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use petgraph::Graph;

    #[test]
    fn test_basic_mode_skips_analysis() {
        let graph = Graph::<(), ()>::new();
        let res = analyze(&graph, TopologyMode::Basic).unwrap();
        assert!(!res.advanced_applied);
        assert!(res.betti_0.is_none());
        assert!(res.spectral_gap.is_none());
    }

    #[test]
    fn test_homology_filled_triangle() {
        // Triangle: 0->1->2 with shortcut 0->2
        let mut graph = Graph::<String, ()>::new();
        let n0 = graph.add_node("0".into());
        let n1 = graph.add_node("1".into());
        let n2 = graph.add_node("2".into());

        graph.add_edge(n0, n1, ());
        graph.add_edge(n1, n2, ());
        graph.add_edge(n0, n2, ()); // Filling edge

        let res = analyze(&graph, TopologyMode::Advanced).unwrap();
        assert_eq!(res.betti_0, Some(1));
        assert_eq!(res.betti_1, Some(0));
    }

    #[test]
    fn test_homology_empty_cycle() {
        // Cycle: 0->1->2->0
        let mut graph = Graph::<String, ()>::new();
        let n0 = graph.add_node("0".into());
        let n1 = graph.add_node("1".into());
        let n2 = graph.add_node("2".into());

        graph.add_edge(n0, n1, ());
        graph.add_edge(n1, n2, ());
        graph.add_edge(n2, n0, ());

        let res = analyze(&graph, TopologyMode::Advanced).unwrap();
        assert_eq!(res.betti_0, Some(1));
        assert_eq!(res.betti_1, Some(1));
    }

    #[test]
    fn test_sparsify_stats() {
        // Simple line graph: 0-1-2-3
        let mut graph = Graph::<String, ()>::new();
        let n0 = graph.add_node("0".into());
        let n1 = graph.add_node("1".into());
        let n2 = graph.add_node("2".into());
        let n3 = graph.add_node("3".into());

        graph.add_edge(n0, n1, ());
        graph.add_edge(n1, n2, ());
        graph.add_edge(n2, n3, ());

        let res = analyze(&graph, TopologyMode::Advanced).unwrap();
        assert!(res.spectral_gap.is_some());
        assert!(res.effective_resistance_stats.is_some());

        let gap = res.spectral_gap.unwrap();
        assert!(gap > 0.0);

        let resistances = res.effective_resistance_stats.unwrap();
        assert_eq!(resistances.len(), 3);
        for r in resistances {
            assert!((r - 1.0).abs() < 1e-6);
        }

        assert!(
            res.messages
                .iter()
                .any(|m| m.contains("directed-Laplacian preconditions not met"))
        );
    }

    #[test]
    fn test_advanced_falls_back_for_oversized_graph() {
        let mut graph = Graph::<usize, ()>::new();
        let mut nodes = Vec::new();
        for i in 0..2001 {
            nodes.push(graph.add_node(i));
        }
        for pair in nodes.windows(2) {
            graph.add_edge(pair[0], pair[1], ());
        }

        let res = analyze(&graph, TopologyMode::Advanced).unwrap();
        assert_eq!(res.mode, TopologyMode::Basic);
        assert!(!res.advanced_applied);
        assert!(
            res.messages
                .iter()
                .any(|m| m.contains("preconditions not met"))
        );
    }
}
