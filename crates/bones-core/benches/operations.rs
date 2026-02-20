mod support;

use bones_core::crdt::item_state::WorkItemState;
use bones_core::crdt::state::Phase;
use bones_core::event::types::EventType;
use bones_core::event::{ParsedLine, parse_line};
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::collections::HashMap;
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
            |b, corpus| b.iter(|| black_box(create_operation(corpus))),
        );

        group.bench_with_input(BenchmarkId::new("next", tier.name), &corpus, |b, corpus| {
            b.iter(|| black_box(next_operation(corpus)))
        });

        group.bench_with_input(
            BenchmarkId::new("search", tier.name),
            &corpus,
            |b, corpus| b.iter(|| black_box(search_operation(corpus, "search"))),
        );

        group.bench_with_input(
            BenchmarkId::new("rebuild", tier.name),
            &corpus,
            |b, corpus| b.iter(|| black_box(rebuild_operation(corpus))),
        );

        emit_latency_report(tier.name, &corpus);
    }

    group.finish();
}

fn create_operation(corpus: &SyntheticCorpus) -> usize {
    corpus
        .lines
        .iter()
        .filter_map(|line| match parse_line(line).ok()? {
            ParsedLine::Event(event) => Some(event),
            _ => None,
        })
        .filter(|event| event.event_type == EventType::Create)
        .count()
}

fn next_operation(corpus: &SyntheticCorpus) -> Option<String> {
    replay_item_states(corpus)
        .into_iter()
        .filter(|(_, state)| !state.deleted.value)
        .filter(|(_, state)| matches!(state.state.phase, Phase::Open | Phase::Doing))
        .max_by(|(left_id, left), (right_id, right)| {
            left.updated_at
                .cmp(&right.updated_at)
                .then_with(|| left_id.cmp(right_id))
        })
        .map(|(item_id, _)| item_id)
}

fn search_operation(corpus: &SyntheticCorpus, needle: &str) -> Vec<String> {
    let needle = needle.to_ascii_lowercase();
    let mut matches = replay_item_states(corpus)
        .into_iter()
        .filter(|(_, state)| !state.deleted.value)
        .filter(|(item_id, state)| {
            item_id.to_ascii_lowercase().contains(&needle)
                || state.title.value.to_ascii_lowercase().contains(&needle)
                || state
                    .description
                    .value
                    .to_ascii_lowercase()
                    .contains(&needle)
                || state
                    .labels
                    .values()
                    .iter()
                    .any(|label| label.to_ascii_lowercase().contains(&needle))
        })
        .map(|(item_id, _)| item_id)
        .collect::<Vec<_>>();
    matches.sort_unstable();
    matches
}

fn rebuild_operation(corpus: &SyntheticCorpus) -> usize {
    replay_item_states(corpus).len()
}

fn replay_item_states(corpus: &SyntheticCorpus) -> HashMap<String, WorkItemState> {
    let mut states: HashMap<String, WorkItemState> = HashMap::new();
    for line in &corpus.lines {
        let Ok(parsed) = parse_line(line) else {
            continue;
        };

        let ParsedLine::Event(event) = parsed else {
            continue;
        };

        let item_id = event.item_id.to_string();
        states.entry(item_id).or_default().apply_event(&event);
    }

    states
}

fn emit_latency_report(tier_name: &str, corpus: &SyntheticCorpus) {
    let create = summarize_latencies(&sample_latencies(64, || {
        black_box(create_operation(corpus));
    }));
    let next = summarize_latencies(&sample_latencies(64, || {
        black_box(next_operation(corpus));
    }));
    let search = summarize_latencies(&sample_latencies(64, || {
        black_box(search_operation(corpus, "agent"));
    }));
    let rebuild = summarize_latencies(&sample_latencies(32, || {
        black_box(rebuild_operation(corpus));
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
