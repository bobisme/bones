# Performance Baseline (bn-1582 perf pass, captured under bn-2ury)

This file locks in the baseline numbers before any fff.nvim-inspired
optimizations land. Every follow-up perf task (bn-1ylg / bn-17xg / bn-pu4y /
etc.) re-runs the same benches and has to justify its change against these
numbers.

## Profile correction — read this first

Before bn-2ury, this workspace shipped a `[profile.release]` of
`opt-level = "z"` (size-optimized) and had **no `[profile.bench]` override**.
Criterion's `bench` profile silently inherits from `release`, so every
historical criterion number in this repo measured a **size-optimized build**.

bn-2ury adds two profile entries in the root `Cargo.toml`:

```toml
[profile.bench]          # speed-optimized for measurement accuracy
opt-level = 3
lto = "thin"
codegen-units = 16
debug = "line-tables-only"

[profile.release-fast]   # speed-optimized installable; use for `bn` perf runs
inherits = "release"
opt-level = 3
lto = "thin"
codegen-units = 16
strip = true
```

The `release` profile is left at `opt-level = "z"` so end-user binary size
doesn't change without a separate decision. A follow-up bone should evaluate
flipping `release` itself — back-of-envelope, users probably want an 80 KB
larger binary in exchange for ~30–60 % faster dependency graph / replay /
rebuild operations.

## Environment

- Profile: `[profile.bench]` as shown above (`opt-level = 3, lto = "thin"`).
- Toolchain: stable, `rustc` from project `rust-toolchain.toml`.
- Host: Linux x86_64, multi-core.
- Reproduction:
  ```bash
  BONES_BENCH_MAX_EVENTS=50000 cargo bench -p bones-core   --bench large_repo
  cargo bench -p bones-core   --bench operations   # filter with `-- S` to skip M/L (they use the same 50k cap)
  cargo bench -p bones-triage --bench triage
  ```

## SLO targets (from `large_repo.rs`)

| operation                           | target p99 |
|-------------------------------------|------------|
| `bn list` open items (Tier M)       | < 200 ms   |
| incremental apply (10 new events)   | < 50 ms    |
| full projection rebuild (Tier M)    | < 8 s      |

Tier M = 10 000 items, 500 000 events (capped to `BONES_BENCH_MAX_EVENTS`,
default 50 000).

## Baseline numbers

### `large_repo` — SQLite projection path, Tier M (max_events=50 000)

| bench                              | low       | median    | high      | target | pass |
|------------------------------------|-----------|-----------|-----------|--------|------|
| `list_open_items/M`                | 783.83 µs | 789.83 µs | 794.97 µs | 200 ms | ✔ (253× headroom) |
| `incremental_apply_10_new_events/M`| 3.9515 ms | 4.1235 ms | 4.2516 ms | 50 ms  | ✔ (12× headroom) |
| `full_rebuild/M`                   | 4.7255 s  | 4.7559 s  | 4.7827 s  | 8 s    | ✔ (**only 1.7× headroom — real hot path**) |

SLO report (sampled latencies, p50/p95/p99):
```
op=list_open           p50=772.1µs   p95=835.3µs   p99=861.2µs
op=incremental_apply_10 p50=2.610 ms  p95=6.497 ms  p99=8.218 ms
op=full_rebuild        p50=4.674 s   p95=4.689 s   p99=4.689 s
```

### `operations` — in-memory replay path, Tier S

`operations` replays a `Vec<String>` corpus through `parse_line` → CRDT state
merge, bypassing SQLite. Tiers M and L are capped at the same 50 000 events
by `BONES_BENCH_MAX_EVENTS`, so we only measured S and left M/L deferred (they
would be near-duplicates of S).

| bench                       | low       | median    | high      | throughput |
|-----------------------------|-----------|-----------|-----------|------------|
| `create/S` (parse+filter)   | 213.34 ms | 214.48 ms | 215.68 ms | 233 K elem/s |
| `next/S` (full replay + max)| 682.37 ms | 684.22 ms | 686.07 ms | 73 K elem/s  |

Interpretation:
- `parse_line` hot path: ~4.3 µs per TSJSON event line (derived from
  `create/S` throughput).
- Full in-memory replay of 50 000 events: ~684 ms → ~14 µs per applied event.
- The SQLite-backed `full_rebuild` at 4.76 s is **~7× slower** than the pure
  in-memory replay for the same event count — most of that is SQLite writes,
  transaction overhead, and FTS indexing, not the CRDT merge itself.

### `triage` — new in bn-2ury, synthetic DiGraph + condensation + pagerank

Sizes: N = 1 000 and N = 10 000 items, avg out-degree 2, seeded. Gate
`BONES_BENCH_LARGE=1` to also run N = 100 000 (minutes per sample because the
current `NormalizedGraph::from_raw` runs an O(n · m) transitive reduction).

| bench                        | n      | low       | median    | high      |
|------------------------------|--------|-----------|-----------|-----------|
| `normalize/from_raw`         | 1 000  | 1.8854 ms | 1.8949 ms | 1.9041 ms |
| `normalize/from_raw`         | 10 000 | 39.554 ms | 39.863 ms | 40.008 ms |
| `pagerank/full`              | 1 000  | 703.73 µs | 708.00 µs | 711.72 µs |
| `pagerank/full`              | 10 000 | 64.188 ms | 64.665 ms | 65.064 ms |
| `composite/bulk_score`       | 1 000  | 11.902 µs | 11.926 µs | 11.951 µs |
| `composite/bulk_score`       | 10 000 | 118.74 µs | 119.01 µs | 119.27 µs |

Interpretation:
- `pagerank/full` is **91× slower from 1 k → 10 k** items (expected scaling is
  ~10× on a linear-in-edges power method). The gap is the `HashMap<String,
  f64>` result map plus repeated `neighbors_directed(...).count()` work in
  the inner loop.
- `normalize/from_raw` scales ~21× over the same step, almost entirely from
  `transitive_reduction` + the full `raw.graph.clone()` done before
  `condensation`.
- `composite_score` is **not a hot path**; ~12 ns per item. bn-3mof (frecency)
  can add work here cheaply.

## Code-review hot spots (pre-flamegraph, from reading the code while writing the harness)

1. **`[profile.release] opt-level = "z"`** — noted above. Speed vs size
   trade-off that should be revisited as a follow-up.
2. **`pagerank` inner loop** (`crates/bones-triage/src/metrics/pagerank.rs`
   around lines 149–173):
   - `g.neighbors_directed(node, Outgoing).count()` called once per node per
     iteration. That's O(out-degree) work on every iter where a single
     precomputed `out_degree: Vec<u32>` would make it O(1).
   - Dangling-node branch (`if out_degree == 0`) walks every rank and adds a
     per-node share. Should accumulate a single `dangling_rank_sum` scalar
     and add it once to every rank after the main pass (or bake it into the
     teleport term).
   - Results live in `HashMap<String, f64>` — many small allocs for IDs that
     already exist in the input graph. A `Vec<(NodeIndex, f64)>` or a reused
     `ahash::HashMap` would be cheaper.
3. **`NormalizedGraph::from_raw`** (`crates/bones-triage/src/graph/
   normalize.rs:101`): clones the full raw graph before `condensation`, then
   sorts each SCC's members on a separate clone. Allocations scale with
   `|V| + |E|`.
4. **No rayon / no parallelism anywhere in bones** — `grep -r rayon crates`
   returns nothing. `pagerank`, `normalize`, `full_rebuild`, search indexing
   are all single-threaded. Direct relevance to bn-30ub and bn-pu4y.
5. **TUI list sort** (`crates/bones-cli/src/tui/list/state.rs:247`): uses
   stable `sort_by`, but every comparator already tiebreaks on `item_id`, so
   the comparator is total. Safe to switch to `sort_unstable_by` today and
   to `select_nth_unstable_by` for paginated updates under bn-1ylg.

## TUI RSS benches

`crates/bones-cli/benches/tui_memory.rs`, `tui_render_memory.rs`, and
`tui_ort_memory.rs` already track RSS via `/proc/self/statm`. They are the
right harness for bn-17xg (mimalloc) and for the open
`tui_memory_investigation` tracked in the parent project's memory. Not
re-measured in this pass — bn-17xg will own a before/after on those.

## Flamegraph capture

`scripts/flamegraph.sh <scenario> [args...]` wraps `samply record` against
`target/release-fast/bn`. Requires `cargo install --locked samply` (user
install, no sudo).

```bash
scripts/flamegraph.sh triage                 # bn triage
scripts/flamegraph.sh search performance     # bn search "performance"
scripts/flamegraph.sh list                   # bn list
scripts/flamegraph.sh rebuild --incremental  # bn admin rebuild --incremental
```

Outputs land in `target/flamegraphs/<stamp>-<scenario>.json.gz`; view with
`samply load <file>`.

## What the follow-up bones should target first

Ranked by SLO pressure and measurement signal (high = do first):

| bone    | target signal                                                   | priority |
|---------|-----------------------------------------------------------------|----------|
| bn-pu4y | `pagerank/full/10k` 64.7 ms → goal < 10 ms                      | high     |
| bn-17xg | TUI RSS decay after churn (`tui_memory` benches)                | high     |
| bn-1ylg | TUI list sort at p99 during heavy filters                       | high     |
| bn-30ub | parallelize `full_rebuild` & `pagerank`                         | high     |
| bn-3f00 | `full_rebuild` 4.76 s → incremental overlay                     | medium   |
| bn-1vyj | only after flamegraph proves alloc/hash pressure                | medium   |
| bn-125x | only if `tui_memory` shows Issue-string alloc is visible        | low      |
| bn-3mof | feature, gate on user confirmation                              | low      |
| bn-1q3k | likely superseded by bn-1ylg                                    | low      |

bn-pu4y (bigram prefilter) was originally framed for `bn search`, but the
real win from the same fff.nvim technique ports to triage: the same
posting-list-AND-then-score pattern lets pagerank pre-filter candidates per
query keyword and short-circuit. Rewrite that bone's scope after flamegraph.
