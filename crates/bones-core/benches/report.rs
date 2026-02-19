mod support;

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use support::{TIERS, bytes_per_event_by_type, generate_corpus_for_bench};

fn bench_reporting(c: &mut Criterion) {
    let mut group = c.benchmark_group("reporting");

    for tier in TIERS {
        let corpus = generate_corpus_for_bench(tier, 0x51EEDu64 + tier.item_count as u64);

        group.bench_with_input(
            BenchmarkId::new("bytes_per_event", tier.name),
            &corpus,
            |b, corpus| {
                b.iter(|| {
                    let stats = bytes_per_event_by_type(corpus);
                    black_box(stats)
                });
            },
        );

        let mut rows = bytes_per_event_by_type(&corpus)
            .into_iter()
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| a.0.cmp(&b.0));

        for (event_type, bytes_per_event) in rows {
            eprintln!(
                "SLO tier={} metric=bytes_per_event class={} value={:.2}",
                tier.name, event_type, bytes_per_event
            );
        }
    }

    group.finish();
}

criterion_group!(benches, bench_reporting);
criterion_main!(benches);
