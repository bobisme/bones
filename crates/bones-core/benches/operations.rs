mod support;

use bones_core::event::parse_line;
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use support::{
    SyntheticCorpus, TIERS, generate_corpus_for_bench, sample_latencies, summarize_latencies,
};

fn bench_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("operations.tiered");

    for tier in TIERS {
        let corpus = generate_corpus_for_bench(tier, 0xC0A215_u64 + tier.event_count as u64);
        group.throughput(Throughput::Elements(corpus.lines.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("create", tier.name),
            &corpus,
            |b, corpus| b.iter(|| black_box(create_stub(corpus))),
        );

        group.bench_with_input(BenchmarkId::new("next", tier.name), &corpus, |b, corpus| {
            b.iter(|| black_box(next_stub(corpus)))
        });

        group.bench_with_input(
            BenchmarkId::new("search", tier.name),
            &corpus,
            |b, corpus| b.iter(|| black_box(search_stub(corpus, "search"))),
        );

        group.bench_with_input(
            BenchmarkId::new("rebuild", tier.name),
            &corpus,
            |b, corpus| b.iter(|| black_box(rebuild_stub(corpus))),
        );

        emit_latency_report(tier.name, &corpus);
    }

    group.finish();
}

fn create_stub(corpus: &SyntheticCorpus) -> usize {
    let mut score = corpus.lines.len();
    if let Some(first) = corpus.lines.first() {
        score += first.len();
    }
    score
}

fn next_stub(corpus: &SyntheticCorpus) -> Option<&str> {
    corpus
        .lines
        .iter()
        .rev()
        .find(|line| line.contains("item.create") || line.contains("item.move"))
        .map(String::as_str)
}

fn search_stub<'a>(corpus: &'a SyntheticCorpus, needle: &str) -> Vec<&'a str> {
    corpus
        .lines
        .iter()
        .filter_map(|line| line.contains(needle).then_some(line.as_str()))
        .collect()
}

fn rebuild_stub(corpus: &SyntheticCorpus) -> usize {
    corpus
        .lines
        .iter()
        .filter(|line| parse_line(line).is_ok())
        .count()
}

fn emit_latency_report(tier_name: &str, corpus: &SyntheticCorpus) {
    let create = summarize_latencies(&sample_latencies(64, || {
        black_box(create_stub(corpus));
    }));
    let next = summarize_latencies(&sample_latencies(64, || {
        black_box(next_stub(corpus));
    }));
    let search = summarize_latencies(&sample_latencies(64, || {
        black_box(search_stub(corpus, "agent"));
    }));
    let rebuild = summarize_latencies(&sample_latencies(32, || {
        black_box(rebuild_stub(corpus));
    }));

    eprintln!(
        "SLO tier={tier_name} op=create p50={:?} p95={:?} p99={:?}",
        create.p50, create.p95, create.p99
    );
    eprintln!(
        "SLO tier={tier_name} op=next p50={:?} p95={:?} p99={:?}",
        next.p50, next.p95, next.p99
    );
    eprintln!(
        "SLO tier={tier_name} op=search p50={:?} p95={:?} p99={:?}",
        search.p50, search.p95, search.p99
    );
    eprintln!(
        "SLO tier={tier_name} op=rebuild p50={:?} p95={:?} p99={:?}",
        rebuild.p50, rebuild.p95, rebuild.p99
    );
}

criterion_group!(benches, bench_operations);
criterion_main!(benches);
