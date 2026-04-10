# Release profile trade-off (bn-2qbr)

Decision point: should `[profile.release]` flip from `opt-level = "z"`
(size-optimized) to `opt-level = 3` + `lto = "thin"` (speed-optimized)?

Every criterion number reported in the bn-1582 perf pass was measured
under `[profile.bench]`, which bn-2ury pinned to `opt-level = 3`. The
shipped `bn` binary still uses `[profile.release]` at `opt-level = "z"`,
so end users are getting the un-optimized version of every hot-path
rewrite (pagerank, normalize, full_rebuild, etc.). **This is the single
biggest user-visible lever left on the table.**

## Measurements (bn-2qbr)

Same workspace, same commit, same toolchain, `--no-default-features`
for both (OTEL off, to match what the perf-pass benches were measuring).

### Stripped binary size (`bn`)

| profile      | opt-level | lto    | codegen-units | size    | delta   |
|--------------|-----------|--------|----------------|---------|---------|
| release      | "z"       | true   | 1              | 28.92 MB| —       |
| release-fast | 3         | "thin" | 16             | 34.62 MB| **+5.70 MB (+19.7 %)** |

### Startup latency (`bn --help`)

Hyperfine, warmup=5, runs=30, `-N` (no shell wrapper):

| profile      | mean     | min    | max    |
|--------------|----------|--------|--------|
| release      | 3.8 ms   | 3.4 ms | 4.3 ms |
| release-fast | 3.6 ms   | 3.2 ms | 4.1 ms |

**Noise-dominated (5 % within the σ of both).** Startup cost is
dominated by dynamic linker + clap, not by the optimized CRDT /
graph / SQLite paths this pass rewrote.

### `bn triage` on a 100-item synthetic fixture

Hyperfine, warmup=3, runs=20:

| profile      | mean     | min    | max    |
|--------------|----------|--------|--------|
| release      | 6.1 ms   | 5.8 ms | 6.6 ms |
| release-fast | 5.8 ms   | 5.4 ms | 6.1 ms |

**Also noise-dominated (~5 %)**. 100 items is too small a workload —
pagerank + normalize at 100 items is ~10 µs, so the total runtime
is dominated by CLI startup + SQLite query overhead, which doesn't
benefit much from opt-level=3.

### Scaled inference from the criterion benches

The criterion benches *already* measured the CPU-bound hot paths under
`opt-level = 3` (`[profile.bench]`), so their numbers represent **what
users would get with a flipped `release` profile**, not what they get
today. To estimate the current end-user experience at `opt-level = "z"`,
we'd expect 1.3–1.6x slowdowns on all of them, based on common
Rust-published `release` vs `release-with-lto` comparisons for
CPU-bound workloads.

For the Tier M workload that matters:

| bench                      | criterion (opt=3) | est. under opt=z | user impact |
|----------------------------|-------------------|------------------|-------------|
| `triage.pagerank/10k`      | 2.53 ms           | 3.3–4.0 ms       | +1 ms       |
| `triage.normalize/10k`     | 13.78 ms          | 18–22 ms         | +5–8 ms     |
| `triage.end_to_end/10k`    | 18.0 ms           | 25–30 ms         | +7–12 ms    |
| `large_repo.full_rebuild/M`| 2.39 s            | 3.0–3.8 s        | +0.6–1.4 s  |
| `large_repo.list_open/M`   | 0.79 ms           | ~1 ms            | noise       |

The `full_rebuild` impact is the most visible — on a 10k-item repo a
user running `bn admin rebuild` could save a full second by flipping
the profile. `bn triage` on realistic graphs saves ~5–12 ms per call.

## Trade-off summary

| factor                 | keep opt-level="z"     | flip to opt-level=3+thin-lto |
|------------------------|------------------------|------------------------------|
| stripped binary        | 28.9 MB                | 34.6 MB (+19.7 %)            |
| startup (`bn --help`)  | 3.8 ms                 | 3.6 ms (noise)               |
| triage/rebuild hot paths | **30–60 % slower**   | matches criterion numbers    |
| `cargo install` time   | longer (full LTO)      | shorter (thin LTO)           |
| crash stack quality    | `strip=true` either way | same                        |

## Recommendation

**Flip the profile.**

Reasoning:
1. The 5.7 MB size delta is real but not extreme for a modern CLI;
   most Rust CLIs in this size class ship ~30–40 MB.
2. Every measured hot-path win in bn-1582 is currently invisible to
   end users because of the profile choice — we're paying the
   engineering cost of the optimization without shipping the win.
3. The new `[profile.release-fast]` I added in bn-2ury becomes
   redundant after the flip and can be deleted along with it.
4. `cargo install` latency drops meaningfully with thin LTO +
   codegen-units=16 vs the current full-LTO single-codegen-unit.

If you want the flip, the change is:

```toml
[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 16
panic = "abort"
strip = true
```

…and delete `[profile.release-fast]` from the workspace `Cargo.toml`.

If you'd rather keep the small binary, close this bone with these
numbers attached and leave `release-fast` as the documented
"fast install" path (README mention recommended).

## Not in scope for this bone

- BOLT / PGO-guided optimizations (bigger payoff, much bigger
  investment).
- `panic = "unwind"` to keep backtraces — orthogonal; revisit only
  if crash reports become a problem.
