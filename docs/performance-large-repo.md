# Large-repo performance benchmark (bn-50e)

This document records a real end-to-end benchmark run for large repositories.

## Scenario

- Items: **10,000**
- Events per item: **10**
- Total events: **100,000**
- Command:

```sh
env BONES_BENCH_EVENTS_PER_ITEM=10 cargo bench -p bones-core --bench large_repo -- --noplot
```

## Measured SLO summary

From `crates/bones-core/benches/large_repo.rs` SLO preflight output:

- `list_open` p99: **2.681 ms** (target: 200 ms) ✅
- `incremental_apply_10` p99: **14.651 ms** (target: 50 ms) ✅
- `full_rebuild` p50: **9.42 s** (target in benchmark header: 5 s) ⚠️

## Notes

- `bn list` latency remains comfortably below the 200 ms target at 10k/100k scale.
- Incremental projection replay for 10 appended events remains below the 50 ms target.
- Full rebuild latency is the current bottleneck at this event volume.
