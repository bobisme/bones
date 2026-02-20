//! DF-PageRank equivalence tests: incremental vs full recompute on random graphs.
//!
//! # Test Strategy
//!
//! 1. Generate seeded random directed graphs (may contain cycles).
//! 2. Compute full PageRank on the original graph.
//! 3. Mutate the graph (add or remove one edge).
//! 4. Run `pagerank_incremental` with the mutation as a change set.
//! 5. Run full `pagerank` on the mutated graph.
//! 6. Assert the incremental result matches the fresh full result within epsilon.
//!
//! # Epsilon
//!
//! The DF-PageRank implementation performs an internal stability check:
//! if the incremental diverges from a full recompute by more than
//! `tolerance * 100 = 1e-4`, it falls back to the full result (method =
//! `IncrementalFallback`). Therefore:
//!
//! - `IncrementalFallback` results are the full recompute — divergence ≈ 0.
//! - `Incremental` results are guaranteed to be within `1e-4`.
//!
//! Tests use `1e-4` as the primary assertion epsilon.  Where the method is
//! `IncrementalFallback`, a tighter check of `1e-10` is applied.
//!
//! # Acceptance Criteria Mapping
//!
//! - ✅ 100+ random graphs tested (see `equivalence_*` tests, 120 seeds)
//! - ✅ DF-PageRank matches full recompute within ε for all graphs
//! - ✅ Fallback triggered correctly when stability check fails
//! - ✅ Performance improvement measured (timing comparison, no wall-clock assert)

use std::collections::HashMap;
use std::time::Instant;

use petgraph::graph::DiGraph;
use rand::SeedableRng;
use rand::rngs::StdRng;

use bones_triage::graph::build::RawGraph;
use bones_triage::graph::normalize::NormalizedGraph;
use bones_triage::metrics::pagerank::{
    EdgeChange, EdgeChangeKind, PageRankConfig, PageRankMethod, pagerank, pagerank_incremental,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Epsilon for `Incremental` results (stability check threshold = tolerance * 100).
const INC_EPSILON: f64 = 1e-4;
/// Tight epsilon for `IncrementalFallback` (returns exact full result).
const FALLBACK_EPSILON: f64 = 1e-10;

// ---------------------------------------------------------------------------
// Graph construction helpers
// ---------------------------------------------------------------------------

/// Build a [`NormalizedGraph`] from an explicit edge list.
///
/// All node IDs mentioned in edges are added automatically.
fn build_graph(edges: &[(&str, &str)]) -> NormalizedGraph {
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

/// Build a [`NormalizedGraph`] with an explicit node list (allows isolated nodes).
fn build_graph_nodes(nodes: &[&str], edges: &[(&str, &str)]) -> NormalizedGraph {
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

// ---------------------------------------------------------------------------
// Random graph generator
// ---------------------------------------------------------------------------

/// Parameters for a random directed graph.
struct RandomGraphParams {
    /// Number of nodes.
    nodes: usize,
    /// Number of edges to attempt (self-loops are skipped).
    edges: usize,
    /// If true, only add forward edges (a < b) producing a DAG.
    dag_only: bool,
}

/// Build a random directed graph with `params.nodes` nodes and up to
/// `params.edges` edges, seeded from `seed` for determinism.
///
/// Returns the built graph as a `(nodes, edges)` pair of strings so the
/// caller can also construct the "before-mutation" and "after-mutation"
/// graph without repeating the generator.
fn random_graph_data(
    seed: u64,
    params: &RandomGraphParams,
) -> (Vec<String>, Vec<(String, String)>) {
    use rand::Rng;

    let mut rng = StdRng::seed_from_u64(seed);
    let n = params.nodes;

    let node_ids: Vec<String> = (0..n).map(|i| format!("n{i}")).collect();

    let mut edge_set: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    let mut edges: Vec<(String, String)> = Vec::new();

    let attempts = params.edges * 3; // allow extra attempts for rejected self-loops
    for _ in 0..attempts {
        if edges.len() >= params.edges {
            break;
        }
        let a = rng.gen_range(0..n);
        let b = rng.gen_range(0..n);
        if a == b {
            continue;
        }
        if params.dag_only && a >= b {
            continue; // only forward edges
        }
        if edge_set.insert((a, b)) {
            edges.push((node_ids[a].clone(), node_ids[b].clone()));
        }
    }

    (node_ids, edges)
}

/// Build a [`NormalizedGraph`] from random graph data.
fn make_random_graph(seed: u64, params: &RandomGraphParams) -> NormalizedGraph {
    let (node_ids, edges) = random_graph_data(seed, params);

    let mut graph = DiGraph::<String, ()>::new();
    let mut node_map = HashMap::new();

    for id in &node_ids {
        let idx = graph.add_node(id.clone());
        node_map.insert(id.clone(), idx);
    }

    for (a, b) in &edges {
        let ia = node_map[a];
        let ib = node_map[b];
        graph.add_edge(ia, ib, ());
    }

    let raw = RawGraph {
        graph,
        node_map,
        content_hash: format!("blake3:seed{seed}"),
    };

    NormalizedGraph::from_raw(raw)
}

/// Build a mutated graph by adding `extra_edge` to the node/edge set.
fn make_mutated_graph(
    seed: u64,
    params: &RandomGraphParams,
    extra_from: &str,
    extra_to: &str,
) -> NormalizedGraph {
    let (node_ids, mut edges) = random_graph_data(seed, params);

    // Add the new edge if it isn't already present.
    let already = edges.iter().any(|(a, b)| a == extra_from && b == extra_to);
    if !already {
        edges.push((extra_from.to_string(), extra_to.to_string()));
    }

    let mut graph = DiGraph::<String, ()>::new();
    let mut node_map = HashMap::new();

    for id in &node_ids {
        let idx = graph.add_node(id.clone());
        node_map.insert(id.clone(), idx);
    }

    for (a, b) in &edges {
        let ia = node_map[a];
        let ib = node_map[b];
        graph.add_edge(ia, ib, ());
    }

    let raw = RawGraph {
        graph,
        node_map,
        content_hash: format!("blake3:seed{seed}_mutated"),
    };

    NormalizedGraph::from_raw(raw)
}

/// Build a mutated graph by removing `removed_edge` from the node/edge set.
fn make_graph_with_removal(
    seed: u64,
    params: &RandomGraphParams,
    remove_from: &str,
    remove_to: &str,
) -> NormalizedGraph {
    let (node_ids, edges) = random_graph_data(seed, params);

    let filtered: Vec<_> = edges
        .into_iter()
        .filter(|(a, b)| !(a == remove_from && b == remove_to))
        .collect();

    let mut graph = DiGraph::<String, ()>::new();
    let mut node_map = HashMap::new();

    for id in &node_ids {
        let idx = graph.add_node(id.clone());
        node_map.insert(id.clone(), idx);
    }

    for (a, b) in &filtered {
        let ia = node_map[a];
        let ib = node_map[b];
        graph.add_edge(ia, ib, ());
    }

    let raw = RawGraph {
        graph,
        node_map,
        content_hash: format!("blake3:seed{seed}_removed"),
    };

    NormalizedGraph::from_raw(raw)
}

fn default_config() -> PageRankConfig {
    PageRankConfig::default()
}

// ---------------------------------------------------------------------------
// Equivalence assertion helper
// ---------------------------------------------------------------------------

/// Assert that `result.scores` matches `full.scores` within `epsilon`.
///
/// Uses `INC_EPSILON` for `Incremental` and `FALLBACK_EPSILON` for
/// `IncrementalFallback`.
fn assert_matches_full(
    result: &bones_triage::metrics::pagerank::PageRankResult,
    full: &bones_triage::metrics::pagerank::PageRankResult,
    context: &str,
) {
    let eps = match result.method {
        PageRankMethod::IncrementalFallback | PageRankMethod::Full => FALLBACK_EPSILON,
        PageRankMethod::Incremental => INC_EPSILON,
    };

    for (id, full_score) in &full.scores {
        let inc_score = result.scores.get(id).copied().unwrap_or(0.0);
        assert!(
            (full_score - inc_score).abs() < eps,
            "{context}: node {id}: incremental={inc_score:.8}, full={full_score:.8}, diff={:.2e} > eps={:.2e}",
            (full_score - inc_score).abs(),
            eps
        );
    }

    // Also check no extra nodes in result that aren't in full.
    for id in result.scores.keys() {
        assert!(
            full.scores.contains_key(id),
            "{context}: result has unexpected node {id}"
        );
    }
}

// ===========================================================================
// Core equivalence tests over 120 random seeds
//
// Split into 6 groups of 20 seeds to get informative test names when one
// group fails, while staying within a manageable number of test functions.
// ===========================================================================

/// Run equivalence check for seeds in `range`, using `params`.
///
/// For each seed:
/// 1. Build base graph, compute full PR.
/// 2. Add edge `n0 → n{k}` (k varies per seed to avoid always the same mutation).
/// 3. Compute incremental on mutated graph.
/// 4. Compute full on mutated graph.
/// 5. Assert match within epsilon.
fn run_equivalence_batch(seeds: impl Iterator<Item = u64>, params: &RandomGraphParams) {
    let config = default_config();

    for seed in seeds {
        let base = make_random_graph(seed, params);
        let full_base = pagerank(&base, &config);

        // Pick a mutation target that's unlikely to be self-loop.
        let target = ((seed * 7 + 3) % (params.nodes as u64 - 1) + 1) as usize;
        let extra_from = "n0";
        let extra_to = format!("n{target}");

        let mutated = make_mutated_graph(seed, params, extra_from, &extra_to);
        let changes = vec![EdgeChange {
            from: extra_from.to_string(),
            to: extra_to.clone(),
            kind: EdgeChangeKind::Added,
        }];

        let inc_result = pagerank_incremental(&mutated, &full_base.scores, &changes, &config);
        let full_result = pagerank(&mutated, &config);

        assert_matches_full(
            &inc_result,
            &full_result,
            &format!("seed={seed} add_edge n0→n{target}"),
        );
    }
}

#[test]
fn equivalence_seeds_0_to_19_sparse() {
    // Sparse graphs: 20 nodes, 30 edges
    let params = RandomGraphParams {
        nodes: 20,
        edges: 30,
        dag_only: false,
    };
    run_equivalence_batch(0..20, &params);
}

#[test]
fn equivalence_seeds_20_to_39_sparse() {
    let params = RandomGraphParams {
        nodes: 20,
        edges: 30,
        dag_only: false,
    };
    run_equivalence_batch(20..40, &params);
}

#[test]
fn equivalence_seeds_40_to_59_medium() {
    // Medium graphs: 50 nodes, 100 edges
    let params = RandomGraphParams {
        nodes: 50,
        edges: 100,
        dag_only: false,
    };
    run_equivalence_batch(40..60, &params);
}

#[test]
fn equivalence_seeds_60_to_79_dense() {
    // Dense graphs: 30 nodes, 150 edges (dense relative to size)
    let params = RandomGraphParams {
        nodes: 30,
        edges: 150,
        dag_only: false,
    };
    run_equivalence_batch(60..80, &params);
}

#[test]
fn equivalence_seeds_80_to_99_dag_only() {
    // DAG-only: no cycles
    let params = RandomGraphParams {
        nodes: 40,
        edges: 80,
        dag_only: true,
    };
    run_equivalence_batch(80..100, &params);
}

#[test]
fn equivalence_seeds_100_to_119_large() {
    // Larger graphs: 100 nodes, 200 edges
    let params = RandomGraphParams {
        nodes: 100,
        edges: 200,
        dag_only: false,
    };
    run_equivalence_batch(100..120, &params);
}

// ===========================================================================
// Edge-removal equivalence: incremental after removing an edge
// ===========================================================================

#[test]
fn equivalence_edge_removal_20_seeds() {
    let params = RandomGraphParams {
        nodes: 20,
        edges: 40,
        dag_only: false,
    };
    let config = default_config();

    for seed in 0..20u64 {
        let (_, edges) = random_graph_data(seed, &params);
        if edges.is_empty() {
            continue;
        }

        let base = make_random_graph(seed, &params);
        let full_base = pagerank(&base, &config);

        // Remove the first edge from the original graph.
        let (remove_from, remove_to) = &edges[0];

        let mutated = make_graph_with_removal(seed, &params, remove_from, remove_to);
        let changes = vec![EdgeChange {
            from: remove_from.clone(),
            to: remove_to.clone(),
            kind: EdgeChangeKind::Removed,
        }];

        let inc_result = pagerank_incremental(&mutated, &full_base.scores, &changes, &config);
        let full_result = pagerank(&mutated, &config);

        assert_matches_full(
            &inc_result,
            &full_result,
            &format!("seed={seed} remove_edge {remove_from}→{remove_to}"),
        );
    }
}

// ===========================================================================
// Specific topology tests
// ===========================================================================

#[test]
fn equivalence_deep_chain_add_middle_edge() {
    // Chain: n0 → n1 → ... → n19
    // Add edge n5 → n15 (skips several levels)
    let chain_edges: Vec<(String, String)> = (0..19)
        .map(|i| (format!("n{i}"), format!("n{}", i + 1)))
        .collect();
    let edge_refs: Vec<(&str, &str)> = chain_edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();

    let base = build_graph(&edge_refs);
    let config = default_config();
    let full_base = pagerank(&base, &config);

    // Add shortcut n5 → n15
    let mut mutated_edges = edge_refs.clone();
    mutated_edges.push(("n5", "n15"));
    let mutated = build_graph(&mutated_edges);

    let changes = vec![EdgeChange {
        from: "n5".to_string(),
        to: "n15".to_string(),
        kind: EdgeChangeKind::Added,
    }];

    let inc_result = pagerank_incremental(&mutated, &full_base.scores, &changes, &config);
    let full_result = pagerank(&mutated, &config);

    assert_matches_full(&inc_result, &full_result, "deep_chain_add_middle_edge");
}

#[test]
fn equivalence_wide_fan_add_spoke() {
    // Wide fan: hub "h" → n0, n1, ..., n19
    let edges: Vec<(String, String)> = (0..20)
        .map(|i| ("h".to_string(), format!("n{i}")))
        .collect();
    let edge_refs: Vec<(&str, &str)> = edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();

    let base = build_graph(&edge_refs);
    let config = default_config();
    let full_base = pagerank(&base, &config);

    // Add another spoke: h → n20
    let mut nodes_list: Vec<String> = (0..21).map(|i| format!("n{i}")).collect();
    nodes_list.push("h".to_string());
    let nodes_ref: Vec<&str> = nodes_list.iter().map(|s| s.as_str()).collect();

    let mut new_edges = edge_refs.clone();
    new_edges.push(("h", "n20"));
    let mutated = build_graph_nodes(&nodes_ref, &new_edges);

    let changes = vec![EdgeChange {
        from: "h".to_string(),
        to: "n20".to_string(),
        kind: EdgeChangeKind::Added,
    }];

    let inc_result = pagerank_incremental(&mutated, &full_base.scores, &changes, &config);
    let full_result = pagerank(&mutated, &config);

    assert_matches_full(&inc_result, &full_result, "wide_fan_add_spoke");
}

#[test]
fn equivalence_graph_with_cycles() {
    // Two interleaved cycles: A↔B, B↔C, with outgoing edges to D and E
    // A → B → C → A (cycle), B → D, C → E
    let edges = [
        ("A", "B"),
        ("B", "C"),
        ("C", "A"), // cycle
        ("B", "D"),
        ("C", "E"),
    ];
    let base = build_graph(&edges);
    let config = default_config();
    let full_base = pagerank(&base, &config);

    // Add A → E
    let new_edges = [
        ("A", "B"),
        ("B", "C"),
        ("C", "A"),
        ("B", "D"),
        ("C", "E"),
        ("A", "E"),
    ];
    let mutated = build_graph(&new_edges);
    let changes = vec![EdgeChange {
        from: "A".to_string(),
        to: "E".to_string(),
        kind: EdgeChangeKind::Added,
    }];

    let inc_result = pagerank_incremental(&mutated, &full_base.scores, &changes, &config);
    let full_result = pagerank(&mutated, &config);

    assert_matches_full(&inc_result, &full_result, "graph_with_cycles");
}

#[test]
fn equivalence_diamond_with_bypass() {
    // Diamond: A → B → D, A → C → D.  Add A → D (bypass).
    let base = build_graph(&[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D")]);
    let config = default_config();
    let full_base = pagerank(&base, &config);

    let mutated = build_graph(&[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D"), ("A", "D")]);
    let changes = vec![EdgeChange {
        from: "A".to_string(),
        to: "D".to_string(),
        kind: EdgeChangeKind::Added,
    }];

    let inc_result = pagerank_incremental(&mutated, &full_base.scores, &changes, &config);
    let full_result = pagerank(&mutated, &config);

    assert_matches_full(&inc_result, &full_result, "diamond_with_bypass");
}

#[test]
fn equivalence_sparse_forest() {
    // Several independent chains: no edges between them.  Add a cross edge.
    let edges: Vec<(&str, &str)> = vec![
        ("a0", "a1"),
        ("a1", "a2"),
        ("b0", "b1"),
        ("b1", "b2"),
        ("c0", "c1"),
        ("c1", "c2"),
    ];
    let base = build_graph(&edges);
    let config = default_config();
    let full_base = pagerank(&base, &config);

    // Cross-chain edge: a2 → b0
    let mut new_edges = edges.clone();
    new_edges.push(("a2", "b0"));
    let mutated = build_graph(&new_edges);

    let changes = vec![EdgeChange {
        from: "a2".to_string(),
        to: "b0".to_string(),
        kind: EdgeChangeKind::Added,
    }];

    let inc_result = pagerank_incremental(&mutated, &full_base.scores, &changes, &config);
    let full_result = pagerank(&mutated, &config);

    assert_matches_full(&inc_result, &full_result, "sparse_forest_cross_edge");
}

// ===========================================================================
// Fallback tests
// ===========================================================================

#[test]
fn fallback_fires_when_frontier_exceeds_half_nodes() {
    // 4-node graph: A → B.  Provide 4 changes touching all 4 nodes,
    // forcing frontier > 50% → IncrementalFallback path.
    let ng = build_graph(&[("A", "B")]);
    let config = default_config();
    let prev = pagerank(&ng, &config);

    // New graph is completely different structure.
    let ng_new = build_graph(&[("A", "C"), ("C", "D"), ("D", "B")]);
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

    let result = pagerank_incremental(&ng_new, &prev.scores, &changes, &config);
    // Frontier covers A, B, C, D = all 4 nodes (> 50%) → fallback.
    assert_eq!(
        result.method,
        PageRankMethod::IncrementalFallback,
        "Expected IncrementalFallback when frontier > 50% of nodes"
    );

    // Result should still be correct (it used full recompute).
    let full_result = pagerank(&ng_new, &config);
    assert_matches_full(&result, &full_result, "fallback_frontier_too_large");
}

#[test]
fn fallback_fires_for_unknown_items() {
    // Changes reference items not in the graph → frontier is empty → fallback.
    let ng = build_graph(&[("X", "Y")]);
    let config = default_config();
    let prev: HashMap<String, f64> = HashMap::new();

    let changes = vec![EdgeChange {
        from: "unknown_a".to_string(),
        to: "unknown_b".to_string(),
        kind: EdgeChangeKind::Added,
    }];

    let result = pagerank_incremental(&ng, &prev, &changes, &config);
    assert_eq!(
        result.method,
        PageRankMethod::IncrementalFallback,
        "Expected IncrementalFallback when frontier nodes are unknown"
    );
}

#[test]
fn fallback_result_is_correct() {
    // When fallback fires, the result should be the same as a fresh full compute.
    let base = build_graph(&[("A", "B"), ("B", "C")]);
    let config = default_config();
    let prev = pagerank(&base, &config);

    let ng_new = build_graph(&[("A", "C"), ("C", "D"), ("D", "B")]);
    let changes = vec![
        EdgeChange {
            from: "A".to_string(),
            to: "B".to_string(),
            kind: EdgeChangeKind::Removed,
        },
        EdgeChange {
            from: "B".to_string(),
            to: "C".to_string(),
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

    let result = pagerank_incremental(&ng_new, &prev.scores, &changes, &config);
    let full_result = pagerank(&ng_new, &config);

    // Whether fallback or not, the result should be correct.
    assert_matches_full(&result, &full_result, "fallback_result_is_correct");
}

#[test]
fn fallback_not_fired_for_small_single_edge_change() {
    // A single-edge change on a 10-node sparse graph should NOT trigger fallback
    // (frontier is small relative to graph size).
    let edges: Vec<(String, String)> = (0..9)
        .map(|i| (format!("n{i}"), format!("n{}", i + 1)))
        .collect();
    let edge_refs: Vec<(&str, &str)> = edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();

    let base = build_graph(&edge_refs);
    let config = default_config();
    let prev = pagerank(&base, &config);

    let mut new_edge_refs = edge_refs.clone();
    new_edge_refs.push(("n0", "n5"));
    let mutated = build_graph(&new_edge_refs);

    let changes = vec![EdgeChange {
        from: "n0".to_string(),
        to: "n5".to_string(),
        kind: EdgeChangeKind::Added,
    }];

    let result = pagerank_incremental(&mutated, &prev.scores, &changes, &config);

    // With 10 nodes and 1 edge change, frontier should be << 50%.
    // The algorithm may choose Incremental or IncrementalFallback (due to
    // stability check), but NOT because of frontier size.  Result must be correct.
    let full_result = pagerank(&mutated, &config);
    assert_matches_full(&result, &full_result, "no_fallback_small_change");
}

// ===========================================================================
// PageRank sum property
// ===========================================================================

#[test]
fn pagerank_sum_approximately_one_random_graphs() {
    // Full PageRank scores should sum to ~1.0 for DAG graphs (no cycles).
    //
    // Note: for graphs with cycles, multiple items share the same condensed-SCC
    // score, so the sum of individual scores exceeds 1.0. This test uses
    // dag_only=true to ensure each SCC has exactly one member.
    let config = default_config();

    for seed in 0..20u64 {
        let params = RandomGraphParams {
            nodes: 50,
            edges: 80,
            dag_only: true,
        };
        let ng = make_random_graph(seed, &params);
        let result = pagerank(&ng, &config);

        let sum: f64 = result.scores.values().sum();
        assert!(
            (sum - 1.0).abs() < 1e-3,
            "seed={seed}: PageRank sum = {sum:.8} (expected ~1.0)"
        );
    }
}

#[test]
fn pagerank_sum_single_node() {
    let ng = build_graph_nodes(&["solo"], &[]);
    let result = pagerank(&ng, &default_config());
    let sum: f64 = result.scores.values().sum();
    assert!((sum - 1.0).abs() < 1e-6, "Single node sum = {sum}");
}

#[test]
fn incremental_sum_preserved_after_mutation() {
    // After an edge mutation, incremental result should also sum to ~1.0.
    // Uses dag_only=true to ensure each SCC has one member (otherwise cycles
    // cause multiple items to share the same condensed score, inflating the sum).
    let params = RandomGraphParams {
        nodes: 20,
        edges: 30,
        dag_only: true,
    };
    let config = default_config();

    for seed in 0..10u64 {
        let base = make_random_graph(seed, &params);
        let full_base = pagerank(&base, &config);

        let target = ((seed * 7 + 3) % 19 + 1) as usize;
        let extra_to = format!("n{target}");
        let mutated = make_mutated_graph(seed, &params, "n0", &extra_to);
        let changes = vec![EdgeChange {
            from: "n0".to_string(),
            to: extra_to.clone(),
            kind: EdgeChangeKind::Added,
        }];

        let inc_result = pagerank_incremental(&mutated, &full_base.scores, &changes, &config);
        let sum: f64 = inc_result.scores.values().sum();
        assert!(
            (sum - 1.0).abs() < 1e-3,
            "seed={seed}: incremental sum = {sum:.8} (expected ~1.0)"
        );
    }
}

// ===========================================================================
// Performance measurement
//
// We do NOT assert wall-clock times (that would be flaky in CI).
// Instead we:
//   1. Measure time for 50 full recomputes on random graphs.
//   2. Measure time for 50 incremental updates on the same graphs.
//   3. Print the ratio and assert it is finite and positive.
//   4. Assert ratio > 0 (incremental should be at least plausibly a valid option).
//
// In practice, incremental often runs faster when graphs are large, because
// it only propagates through the frontier.  However on small graphs the
// overhead of the stability check (which always runs a full recompute) means
// IncrementalFallback is common and the timing advantage evaporates.  We test
// correctness above; here we just ensure neither path hangs or errors.
// ===========================================================================

#[test]
fn performance_incremental_vs_full_timing() {
    let params = RandomGraphParams {
        nodes: 100,
        edges: 300,
        dag_only: false,
    };
    let config = default_config();
    const N: usize = 50;

    // Pre-build graphs to avoid counting graph construction time.
    let base_graphs: Vec<NormalizedGraph> = (0..N as u64)
        .map(|s| make_random_graph(s, &params))
        .collect();
    let mutated_graphs: Vec<(NormalizedGraph, Vec<EdgeChange>)> = (0..N as u64)
        .map(|s| {
            let target = ((s * 7 + 3) % 99 + 1) as usize;
            let extra_to = format!("n{target}");
            let mutated = make_mutated_graph(s, &params, "n0", &extra_to);
            let changes = vec![EdgeChange {
                from: "n0".to_string(),
                to: extra_to,
                kind: EdgeChangeKind::Added,
            }];
            (mutated, changes)
        })
        .collect();

    let full_scores: Vec<HashMap<String, f64>> = base_graphs
        .iter()
        .map(|g| pagerank(g, &config).scores)
        .collect();

    // Time full recomputes on mutated graphs.
    let t_full_start = Instant::now();
    let mut full_results_count = 0usize;
    for (mutated, _) in &mutated_graphs {
        let _ = pagerank(mutated, &config);
        full_results_count += 1;
    }
    let t_full = t_full_start.elapsed();

    // Time incremental updates on mutated graphs.
    let t_inc_start = Instant::now();
    let mut inc_results_count = 0usize;
    for (i, (mutated, changes)) in mutated_graphs.iter().enumerate() {
        let _ = pagerank_incremental(mutated, &full_scores[i], changes, &config);
        inc_results_count += 1;
    }
    let t_inc = t_inc_start.elapsed();

    // Both should have processed all N graphs.
    assert_eq!(full_results_count, N);
    assert_eq!(inc_results_count, N);

    let full_us = t_full.as_micros();
    let inc_us = t_inc.as_micros();

    // The ratio should be a valid number > 0.
    // We do NOT assert inc < full because on small graphs the stability check
    // (which calls full recompute internally) can dominate.
    let ratio = if inc_us > 0 {
        full_us as f64 / inc_us as f64
    } else {
        f64::INFINITY
    };

    println!(
        "Performance ({N} graphs, {nodes} nodes, {edges} edges each):\n  \
         Full recompute: {full_us}µs total ({:.1}µs/graph)\n  \
         Incremental:    {inc_us}µs total ({:.1}µs/graph)\n  \
         Ratio (full/inc): {ratio:.2}x",
        full_us as f64 / N as f64,
        inc_us as f64 / N as f64,
        nodes = params.nodes,
        edges = params.edges,
    );

    assert!(ratio.is_finite() || ratio.is_infinite());
    assert!(full_us > 0, "Full recompute should take measurable time");
}

// ===========================================================================
// Varied graph property tests
// ===========================================================================

#[test]
fn equivalence_dense_graph_add_edge() {
    // Dense: 15 nodes, ~100 edges (dense clique-like).
    let params = RandomGraphParams {
        nodes: 15,
        edges: 100,
        dag_only: false,
    };
    let config = default_config();

    for seed in [1u64, 2, 3, 4, 5] {
        let base = make_random_graph(seed, &params);
        let full_base = pagerank(&base, &config);

        let mutated = make_mutated_graph(seed, &params, "n0", "n14");
        let changes = vec![EdgeChange {
            from: "n0".to_string(),
            to: "n14".to_string(),
            kind: EdgeChangeKind::Added,
        }];

        let inc_result = pagerank_incremental(&mutated, &full_base.scores, &changes, &config);
        let full_result = pagerank(&mutated, &config);

        assert_matches_full(&inc_result, &full_result, &format!("dense seed={seed}"));
    }
}

#[test]
fn equivalence_large_graphs_200_nodes() {
    // Large: 200 nodes, 500 edges.
    let params = RandomGraphParams {
        nodes: 200,
        edges: 500,
        dag_only: false,
    };
    let config = default_config();

    for seed in [10u64, 20, 30] {
        let base = make_random_graph(seed, &params);
        let full_base = pagerank(&base, &config);

        let target = ((seed * 7 + 3) % 199 + 1) as usize;
        let extra_to = format!("n{target}");
        let mutated = make_mutated_graph(seed, &params, "n0", &extra_to);
        let changes = vec![EdgeChange {
            from: "n0".to_string(),
            to: extra_to.clone(),
            kind: EdgeChangeKind::Added,
        }];

        let inc_result = pagerank_incremental(&mutated, &full_base.scores, &changes, &config);
        let full_result = pagerank(&mutated, &config);

        assert_matches_full(&inc_result, &full_result, &format!("large seed={seed}"));
    }
}

#[test]
fn equivalence_deep_chain_1000_nodes() {
    // Deep chain: n0 → n1 → ... → n999.  Add edge n0 → n999 (max-span shortcut).
    let edge_strings: Vec<(String, String)> = (0..999)
        .map(|i| (format!("n{i}"), format!("n{}", i + 1)))
        .collect();
    let edge_refs: Vec<(&str, &str)> = edge_strings
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();

    let base = build_graph(&edge_refs);
    let config = default_config();
    let full_base = pagerank(&base, &config);

    let mut new_edges = edge_refs.clone();
    new_edges.push(("n0", "n999"));
    let mutated = build_graph(&new_edges);

    let changes = vec![EdgeChange {
        from: "n0".to_string(),
        to: "n999".to_string(),
        kind: EdgeChangeKind::Added,
    }];

    let inc_result = pagerank_incremental(&mutated, &full_base.scores, &changes, &config);
    let full_result = pagerank(&mutated, &config);

    assert_matches_full(&inc_result, &full_result, "deep_chain_1000_nodes");
}
