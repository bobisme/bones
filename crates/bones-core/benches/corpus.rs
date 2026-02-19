mod support;

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use support::{TIERS, generate_corpus};

fn bench_corpus_generation(c: &mut Criterion) {
    let mut group = c.benchmark_group("corpus.generate");

    for tier in TIERS {
        group.throughput(Throughput::Elements(tier.event_count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(tier.name), &tier, |b, tier| {
            b.iter(|| {
                let corpus = generate_corpus(*tier, 0xB0E500_u64);
                black_box(corpus.lines.len())
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_corpus_generation);
criterion_main!(benches);
