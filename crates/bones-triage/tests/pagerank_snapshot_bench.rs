use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use bones_triage::graph::{NormalizedGraph, RawGraph};
use bones_triage::metrics::pagerank::{
    EdgeChange, EdgeChangeKind, PageRankConfig, PageRankMethod, pagerank, pagerank_incremental,
};
use petgraph::graph::DiGraph;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rusqlite::Connection;

const MAX_DIFF_EPS: f64 = 1e-4;

#[test]
#[ignore = "local benchmark; set BONES_PAGERANK_SNAPSHOT_DB"]
fn benchmark_pagerank_on_real_snapshot() {
    let db_path = std::env::var("BONES_PAGERANK_SNAPSHOT_DB")
        .expect("set BONES_PAGERANK_SNAPSHOT_DB to a .bones/bones.db path");

    let (nodes, edges) = load_snapshot(Path::new(&db_path));
    assert!(!nodes.is_empty(), "snapshot must contain nodes");

    let base = build_normalized(&nodes, &edges);
    let config = PageRankConfig::default();
    let previous = pagerank(&base, &config).scores;

    let mut rng = StdRng::seed_from_u64(0xB0E5_u64);
    let edge_set: HashSet<(String, String)> = edges.iter().cloned().collect();
    let add_changes = sample_add_mutations(&nodes, &edge_set, 30, &mut rng);
    let remove_changes = sample_remove_mutations(&edges, 30, &mut rng);

    let mut full_total = Duration::ZERO;
    let mut inc_total = Duration::ZERO;
    let mut method_incremental = 0usize;
    let mut method_fallback = 0usize;
    let mut method_full = 0usize;

    for change in add_changes.iter().chain(remove_changes.iter()) {
        let mutated_edges = apply_change(&edges, change);
        let mutated = build_normalized(&nodes, &mutated_edges);

        let t_full = Instant::now();
        let full_result = pagerank(&mutated, &config);
        full_total += t_full.elapsed();

        let t_inc = Instant::now();
        let inc_result =
            pagerank_incremental(&mutated, &previous, std::slice::from_ref(change), &config);
        inc_total += t_inc.elapsed();

        match inc_result.method {
            PageRankMethod::Incremental => method_incremental += 1,
            PageRankMethod::IncrementalFallback => method_fallback += 1,
            PageRankMethod::Full => method_full += 1,
        }

        let max_diff = max_abs_diff(&inc_result.scores, &full_result.scores);
        assert!(
            max_diff <= MAX_DIFF_EPS,
            "change {:?} -> max diff {:.3e} > {:.3e}",
            change,
            max_diff,
            MAX_DIFF_EPS
        );
    }

    let trials = add_changes.len() + remove_changes.len();
    let full_us = full_total.as_micros();
    let inc_us = inc_total.as_micros();
    let ratio = if inc_us > 0 {
        full_us as f64 / inc_us as f64
    } else {
        f64::INFINITY
    };

    println!(
        "Snapshot benchmark ({trials} mutations):\n  \
         Graph: nodes={}, edges={}\n  \
         Methods: incremental={method_incremental}, fallback={method_fallback}, full={method_full} \
         (fallback_rate={:.1}%)\n  \
         Full: {full_us}us total ({:.1}us/mutation)\n  \
         Inc:  {inc_us}us total ({:.1}us/mutation)\n  \
         Ratio (full/inc): {ratio:.2}x",
        nodes.len(),
        edges.len(),
        100.0 * method_fallback as f64 / trials as f64,
        full_us as f64 / trials as f64,
        inc_us as f64 / trials as f64,
    );

    assert!(full_us > 0);
}

fn load_snapshot(db_path: &Path) -> (Vec<String>, Vec<(String, String)>) {
    let conn = Connection::open(db_path)
        .unwrap_or_else(|e| panic!("open sqlite snapshot {}: {e}", db_path.display()));

    let nodes = {
        let mut stmt = conn
            .prepare("SELECT item_id FROM items WHERE is_deleted = 0 ORDER BY item_id")
            .expect("prepare node query");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query node rows");
        rows.collect::<Result<Vec<_>, _>>().expect("collect nodes")
    };

    let mut edges = {
        let mut stmt = conn
            .prepare(
                "SELECT depends_on_item_id, item_id
                 FROM item_dependencies
                 WHERE link_type IN ('blocks', 'blocked_by')",
            )
            .expect("prepare edge query");
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query edge rows");
        rows.collect::<Result<Vec<_>, _>>().expect("collect edges")
    };

    edges.sort_unstable();
    edges.dedup();
    (nodes, edges)
}

fn build_normalized(nodes: &[String], edges: &[(String, String)]) -> NormalizedGraph {
    let mut graph = DiGraph::<String, ()>::new();
    let mut node_map = HashMap::new();

    for id in nodes {
        let idx = graph.add_node(id.clone());
        node_map.insert(id.clone(), idx);
    }

    for (from, to) in edges {
        let from_idx = *node_map
            .entry(from.clone())
            .or_insert_with(|| graph.add_node(from.clone()));
        let to_idx = *node_map
            .entry(to.clone())
            .or_insert_with(|| graph.add_node(to.clone()));
        if !graph.contains_edge(from_idx, to_idx) {
            graph.add_edge(from_idx, to_idx, ());
        }
    }

    let raw = RawGraph {
        graph,
        node_map,
        content_hash: format!("snapshot:{}:{}", nodes.len(), edges.len()),
    };
    NormalizedGraph::from_raw(raw)
}

fn sample_add_mutations(
    nodes: &[String],
    edges: &HashSet<(String, String)>,
    n: usize,
    rng: &mut StdRng,
) -> Vec<EdgeChange> {
    let mut changes = Vec::new();
    let mut used = HashSet::new();

    while changes.len() < n {
        let from = nodes[rng.gen_range(0..nodes.len())].clone();
        let to = nodes[rng.gen_range(0..nodes.len())].clone();
        if from == to {
            continue;
        }
        if edges.contains(&(from.clone(), to.clone())) {
            continue;
        }
        if !used.insert((from.clone(), to.clone())) {
            continue;
        }
        changes.push(EdgeChange {
            from,
            to,
            kind: EdgeChangeKind::Added,
        });
    }

    changes
}

fn sample_remove_mutations(
    edges: &[(String, String)],
    n: usize,
    rng: &mut StdRng,
) -> Vec<EdgeChange> {
    let mut changes = Vec::new();
    let mut used = HashSet::new();

    while changes.len() < n {
        let idx = rng.gen_range(0..edges.len());
        let (from, to) = &edges[idx];
        if !used.insert((from.clone(), to.clone())) {
            continue;
        }
        changes.push(EdgeChange {
            from: from.clone(),
            to: to.clone(),
            kind: EdgeChangeKind::Removed,
        });
    }

    changes
}

fn apply_change(edges: &[(String, String)], change: &EdgeChange) -> Vec<(String, String)> {
    let mut out = edges.to_vec();
    match change.kind {
        EdgeChangeKind::Added => {
            if !out.contains(&(change.from.clone(), change.to.clone())) {
                out.push((change.from.clone(), change.to.clone()));
            }
        }
        EdgeChangeKind::Removed => {
            out.retain(|(from, to)| !(from == &change.from && to == &change.to));
        }
    }
    out
}

fn max_abs_diff(a: &HashMap<String, f64>, b: &HashMap<String, f64>) -> f64 {
    a.keys()
        .map(|k| {
            let av = a.get(k).copied().unwrap_or(0.0);
            let bv = b.get(k).copied().unwrap_or(0.0);
            (av - bv).abs()
        })
        .fold(0.0_f64, f64::max)
}
