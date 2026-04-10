//! Triage hot-path benches: pagerank + composite score pipeline.
//!
//! Constructs a synthetic `RawGraph` with N items and ~2N blocking edges
//! (sparse, realistic for dependency graphs), then measures:
//!
//! - `NormalizedGraph::from_raw`  — condensation + transitive reduction
//! - `pagerank`                    — iterative power method on condensed DAG
//! - `composite_score` (bulk)      — apply per-item scoring over N inputs
//!
//! Tiered at N = 1_000, 10_000. The 100 000 tier is gated behind
//! `BONES_BENCH_LARGE=1` because `NormalizedGraph::from_raw` includes an
//! O(n·m) transitive reduction that pushes the largest tier into minutes.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::missing_const_for_fn,
    clippy::if_same_then_else,
    clippy::items_after_statements,
    clippy::similar_names,
    clippy::doc_markdown
)]

use bones_core::model::item::Urgency;
use bones_triage::graph::build::RawGraph;
use bones_triage::graph::normalize::NormalizedGraph;
use bones_triage::metrics::pagerank::{PageRankConfig, pagerank};
use bones_triage::score::composite::{CompositeWeights, MetricInputs, composite_score};
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;

/// Deterministic LCG for reproducible synthetic graphs.
struct Prng(u64);

impl Prng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }
    fn next_idx(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() as usize) % n
    }
}

fn tier_sizes() -> Vec<usize> {
    if std::env::var("BONES_BENCH_LARGE").is_ok() {
        vec![1_000, 10_000, 100_000]
    } else {
        vec![1_000, 10_000]
    }
}

fn synthetic_raw_graph(n: usize, avg_out_degree: usize, seed: u64) -> RawGraph {
    let mut prng = Prng::new(seed);
    let mut graph: DiGraph<String, ()> = DiGraph::with_capacity(n, n * avg_out_degree);
    let mut node_map: HashMap<String, NodeIndex> = HashMap::with_capacity(n);

    for i in 0..n {
        let id = format!("bn-{i:06x}");
        let idx = graph.add_node(id.clone());
        node_map.insert(id, idx);
    }

    // Forward-pointing edges produce a DAG with occasional SCCs (~5% back-edges).
    let edge_count = n * avg_out_degree;
    for _ in 0..edge_count {
        let src = prng.next_idx(n);
        let dst = prng.next_idx(n);
        if src == dst {
            continue;
        }
        // 5% chance of allowing any direction (creates SCCs).
        let (a, b) = if prng.next_u64() % 100 < 5 {
            (src, dst)
        } else if src < dst {
            (src, dst)
        } else {
            (dst, src)
        };
        let ai = NodeIndex::new(a);
        let bi = NodeIndex::new(b);
        if !graph.contains_edge(ai, bi) {
            graph.add_edge(ai, bi, ());
        }
    }

    RawGraph {
        graph,
        node_map,
        content_hash: "bench".to_string(),
    }
}

fn bench_normalize(c: &mut Criterion) {
    let mut group = c.benchmark_group("triage.normalize");
    group.sample_size(10);
    for &n in &tier_sizes() {
        let raw = synthetic_raw_graph(n, 2, 42);
        group.bench_with_input(BenchmarkId::new("from_raw", n), &n, |b, _| {
            b.iter_with_large_drop(|| {
                // Clone raw because from_raw consumes it.
                let raw_clone = synthetic_raw_graph(n, 2, 42);
                NormalizedGraph::from_raw(raw_clone)
            });
        });
        black_box(&raw);
    }
    group.finish();
}

fn bench_pagerank(c: &mut Criterion) {
    let mut group = c.benchmark_group("triage.pagerank");
    group.sample_size(10);
    let cfg = PageRankConfig::default();
    for &n in &tier_sizes() {
        let raw = synthetic_raw_graph(n, 2, 42);
        let ng = NormalizedGraph::from_raw(raw);
        group.bench_with_input(BenchmarkId::new("full", n), &n, |b, _| {
            b.iter(|| black_box(pagerank(&ng, &cfg)));
        });
    }
    group.finish();
}

fn bench_composite(c: &mut Criterion) {
    let mut group = c.benchmark_group("triage.composite");
    let weights = CompositeWeights::<f64>::default();
    for &n in &tier_sizes() {
        let inputs: Vec<MetricInputs> = (0..n)
            .map(|i| MetricInputs {
                critical_path: (i as f64) / (n as f64),
                pagerank: ((i * 7) % n) as f64 / (n as f64),
                betweenness: ((i * 13) % n) as f64 / (n as f64),
                urgency: if i % 100 == 0 {
                    Urgency::Urgent
                } else {
                    Urgency::Default
                },
                decay_days: (i % 30) as f64,
            })
            .collect();
        group.bench_with_input(BenchmarkId::new("bulk_score", n), &inputs, |b, inputs| {
            b.iter(|| {
                let mut acc = 0.0_f64;
                for input in inputs {
                    acc += composite_score(input, &weights);
                }
                black_box(acc)
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_normalize, bench_pagerank, bench_composite);
criterion_main!(benches);
