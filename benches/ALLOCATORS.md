# Allocator comparison (bn-17xg)

`bones-cli` exposes three opt-in global allocators; the default is the
system allocator. This document captures how `mimalloc` compares against
the system allocator on the `tui_memory` bench, which replays the exact
allocation pattern the TUI hot loop uses. `jemalloc` already existed as
an opt-in feature before bn-17xg; the plumbing added here allows it to
be A/B'd the same way.

| feature flag   | allocator                        | default |
|----------------|----------------------------------|---------|
| *(none)*       | system (`glibc` malloc on Linux) | ✔       |
| `jemalloc`     | `tikv-jemallocator`              |         |
| `mimalloc`     | `mimalloc` (**added by bn-17xg**)|         |

## Reproducing

```bash
# System allocator (default)
cargo bench -p bones-cli --bench tui_memory --no-default-features

# mimalloc
cargo bench -p bones-cli --bench tui_memory --no-default-features --features mimalloc

# jemalloc
cargo bench -p bones-cli --bench tui_memory --no-default-features --features jemalloc
```

`--no-default-features` drops the OTEL stack (~100 crates). The bench
doesn't touch OTEL or semantic search, so this is a fair comparison and
keeps iteration time down by ~60 %. The bench prints `Allocator: <name>`
as its first line so runs are labelled in the output.

## Results

`tui_memory --items 750 --comments 15`, run under `[profile.bench]`
(`opt-level = 3`, `lto = "thin"`). Same machine, sequential runs, no
other allocator-sensitive work in flight.

| metric                         | system   | mimalloc | delta               |
|--------------------------------|----------|----------|---------------------|
| baseline RSS                   |  4.8 MB  |  5.3 MB  | +0.5 MB             |
| after reload loop (500 iters)  |  5.7 MB  |  6.5 MB  | +0.8 MB             |
| after detail loop (500 iters)  |  5.7 MB  |  6.5 MB  | +0.8 MB             |
| after render loop (5 000 iters)|  6.0 MB  |  6.7 MB  | +0.7 MB             |
| after full cycle (500 iters)   |  6.0 MB  |  8.3 MB  | **+2.3 MB**         |
| **RSS total growth**           |  1.2 MB  |  3.1 MB  | **+1.9 MB (+158 %)**|
| per-tick reload                | 10.80 ms | 10.64 ms | **−1.5 %**          |
| per-tick detail load           | 244.0 µs | 231.6 µs | **−5.1 %**          |
| per-tick md render             |  29.3 µs |  26.5 µs | **−9.6 %**          |
| per-tick full cycle            | 10.97 ms | 10.75 ms | **−2.0 %**          |

## Interpretation

1. **mimalloc gives a small but consistent latency win** (~2 % on the
   full tick, up to ~10 % on the markdown-render inner loop). The win is
   real but not transformative — the TUI tick is SQLite-dominated.
2. **mimalloc uses materially more RSS** in this workload. It pre-reserves
   arenas for speed, which shows up as a higher baseline and higher
   steady-state footprint. Growth over the full tick cycle is ~2.5× the
   system allocator.

## The bn-17xg original hypothesis was wrong

bn-17xg was filed expecting mimalloc to help the open 1.6 GB TUI RSS
investigation (see `memory/project_tui_memory_investigation.md`). The
measurement contradicts that hypothesis cleanly: **mimalloc makes RSS
worse, not better**, because the system allocator is already near-optimal
for this specific workload (very little fragmentation; arenas stay
compact).

That means **the 1.6 GB RSS is almost certainly not an allocator problem**
at all. The most likely culprits, in priority order, are:

1. **ORT semantic-search model state** — `bones-search` with the
   `semantic-ort` feature loads large ONNX models and keeps them resident
   for the lifetime of the TUI session. The 1.6 GB figure is in the same
   order of magnitude as all-MiniLM-L6-v2 + tokenizer state held across
   ticks. This should be the next thing we profile with the `dhat-heap`
   feature.
2. **ratatui buffer retention** — the TUI allocates new `Vec<Line<'_>>`
   buffers per frame and may retain them across the scroll-back viewport.
   Not an allocator issue either, but worth confirming.
3. **SQLite page cache** — configurable via `PRAGMA cache_size`; default
   is a few MB, not hundreds. Unlikely to be the root cause but easy to
   rule out.

## Recommendations

1. **Do not make mimalloc the default.** Ship it as an opt-in feature
   for users who explicitly want the ~2 % latency win and can pay for it
   in RSS.
2. **Do not wire the fff.nvim-style `mi_collect(true)` post-churn hint
   right now.** There's nothing to collect — the system allocator already
   keeps RSS flat in this workload, and mimalloc's RSS growth isn't from
   churn but from preallocated arenas. Calling `mi_collect` wouldn't
   shrink those.
3. **Open a new bone to investigate the 1.6 GB RSS properly.** Use
   `cargo bench --bench tui_memory --no-default-features --features
   dhat-heap` (already exists) and then load the resulting
   `dhat-heap.json` to see what actually holds memory. The existing
   `tui_memory` workload is too small to reproduce the problem; we need
   a longer-running scenario that exercises semantic search.
4. **Keep the plumbing.** The allocator-selection `#[global_allocator]`
   gates in both `main.rs` and `benches/tui_memory.rs` + the `mimalloc`
   feature flag are a small permanent addition that makes future A/B
   testing trivial. Changing from mimalloc to jemalloc to system is now
   a single `--features` flip.

## Status

- bn-17xg: **merge the plumbing + doc + explicit rejection** of mimalloc
  as default.
- bn-1582 (parent goal): revise the "follow-up bones" table in
  `benches/BASELINE.md` so that bn-17xg's conclusion is visible, and add
  a new bone for the ORT semantic-search memory investigation.
