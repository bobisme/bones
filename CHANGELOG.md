# Changelog

## v0.24.4 - 2026-04-30

### Fixed

- **Rebuild projection before mutations** — `bn do`, `bn done`, `bn reopen`,
  `bn update`, and `bn archive` now refresh the projection before applying
  state changes, preventing mutations from being applied against a stale
  in-memory view.
- **UTC consistency for shard naming** — `bn init` now uses UTC (matching
  shard rotation and event timestamps) instead of local time. Removes a
  month-boundary inconsistency where init could create a shard named for
  a different month than the one the rotator would select.

## v0.24.3 - 2026-04-28

### Fixed

- **Hardened TUI event writes** — event-writing actions now preserve user
  input and error state more reliably across selection, filtering, and search
  flows.
- **Improved data integrity checks** — import, undo, parser, and reporting
  paths now reject more malformed inputs and surface clearer failures instead
  of silently accepting inconsistent state.
- **Fixed cache and projection edge cases** — binary cache freshness,
  projection dirty-state handling, incremental replay, and triage graph
  construction now handle stale or inconsistent derived data more defensively.
- **Repaired create/block and ITC state handling** — `bn create` block-link
  flows and ITC path state updates now behave correctly in nested and
  multi-event cases.
- **Hardened projection and golden-file tests** — projection selection and
  canonical parser fixtures now fail more predictably when assumptions drift.
- **Anchored triage feedback and compaction locking** — triage support output
  is tied to the selected item, and compaction now respects lock ownership
  more consistently.
- **Made text truncation Unicode-safe** — CLI graph/list rendering, event
  display, validation output, and shard range reads no longer split inside
  multi-byte characters.
- **Validated search threshold configuration** — invalid semantic search
  threshold values are now rejected during config parsing instead of producing
  surprising runtime behavior.
- **Saturated cache range reads** — `CacheReader::read_range` no longer
  overflows in debug builds when passed an extremely large count.

## v0.24.2 - 2026-04-27

### Changed

- **`bn` binary now uses mimalloc by default** — long-running TUI sessions
  on larger corpora were exhibiting RSS bloat (1.3+ GB on glibc) due to
  per-thread arena fragmentation that does not deflate after spikes.
  mimalloc returns freed pages to the OS more aggressively. Selecting
  `jemalloc` or `dhat-heap` at build time still takes precedence, so
  profiling builds continue to work without `--no-default-features`.

### Fixed

- **`bn doctor` now hints at `--fix` for missing-manifest failures** —
  matching the existing UX for `projection_drift` and `stale_symlink`.
  The repair logic was already wired through `verify_repository`'s
  `regenerate_missing` parameter; only the user-facing hint was missing.

## v0.24.1 - 2026-04-26

### Added

- **Chief provider JSON contract** — adds a strict machine-readable JSON
  contract for the chief provider, enabling stable downstream parsing.

### Fixed

- **TUI search now matches bone IDs** — typing a literal ID (e.g.
  `bn-002` or `001`) reliably surfaces the matching bone in the list
  view and the dedicated search view, even when the semantic ranker
  did not surface it. Direct ID matches (exact, prefix, or substring)
  outrank lexical/semantic results.
- **Tier-2 search failures no longer wipe Tier-1 results** in the
  dedicated search view — when semantic refinement errors out, the
  foreground lexical/structural results stay visible instead of being
  replaced by an empty set.
- **Backend search errors are visible by default** — bumped
  `hybrid_search_fast` failure logs from `debug!` to `warn!` so a
  broken FTS/SQLite layer surfaces in default logs.

## v0.24.0 - 2026-04-11

### Extreme performance pass

Measurement-driven performance sweep inspired by techniques from
[fff.nvim](https://github.com/fff-nvim). Every change was benchmarked
before/after with criterion; speculative optimizations were rejected.

**Headline numbers (10k-item corpus, criterion benchmarks):**

| path | before | after | speedup |
|------|--------|-------|---------|
| `bn triage` end-to-end | ~104 ms | ~18 ms | **5.8x** |
| PageRank (full, 10k) | 64.7 ms | 2.53 ms | **25.6x** |
| `bn admin rebuild` (Tier M) | 4.76 s | 2.39 s | **2.0x** |
| `NormalizedGraph::from_raw` (10k) | 39.9 ms | 13.8 ms | **2.9x** |

**What changed:**

- **PageRank CSR inner loop** — rewrote power iteration to use a flat
  CSR (compressed sparse row) adjacency view built once, replacing
  per-iteration petgraph neighbor walks and `.count()` calls. Folded
  dangling-node mass into the teleport term as a single scalar instead
  of an O(n) inner loop per dangling source.
- **Bitset-based transitive reduction** — replaced
  `HashMap<NodeIndex, HashSet<NodeIndex>>` reachability sets with
  `Vec<FixedBitSet>` (one bit per node). Cache-friendly O(n/64) union
  operations instead of hash-set insertions.
- **SQLite bulk-load pragmas for rebuild** — during `bn admin rebuild`,
  set `synchronous=OFF`, `journal_mode=OFF`, `locking_mode=EXCLUSIVE`
  (safe: the DB is throwaway on failure). Disabled FTS5 maintenance
  triggers during bulk insert, then repopulated the FTS5 index in a
  single query at the end.
- **TUI list sort** — switched three sort sites from `sort_by` to
  `sort_unstable_by` (safe because all comparators tiebreak on
  unique `item_id`).
- **Release profile flip** — `[profile.release]` moved from
  `opt-level = "z"` (size) to `opt-level = 3` + `lto = "thin"`.
  Binary grows from 28.9 MB to 34.6 MB (+19.7%), but all the
  above speedups now reach end users instead of being
  benchmark-only.

### New features

- **`mimalloc` allocator option** — `cargo install bones-cli --features
  mimalloc` for a ~2% latency win at the cost of higher RSS. Measured
  and documented in `benches/ALLOCATORS.md`.
- **Triage benchmarks** — new criterion bench suite in `bones-triage`
  covering `NormalizedGraph::from_raw`, `pagerank`, `composite_score`,
  and a composed end-to-end pipeline. Tiered at 1k/10k items.
- **Flamegraph script** — `scripts/flamegraph.sh` wraps `samply record`
  for triage/search/list/rebuild profiling.
- **`[profile.bench]`** — explicit bench profile pinned to opt-level=3
  so criterion numbers are representative of shipped performance.

### Infrastructure

- `benches/BASELINE.md` — captured baseline numbers with SLO targets.
- `benches/ALLOCATORS.md` — system vs mimalloc vs jemalloc A/B data.
- `benches/RELEASE_PROFILE.md` — opt-level trade-off measurements.

## v0.23.3 - 2026-04-07

- Downgraded PageRank incremental fallback warnings to info level.
- Refactored TUI list.rs into focused modules.
- Added Windows CI jobs to GitHub Actions.

## v0.23.2 - 2026-03-22

- `cargo install bones-cli` now just works on Windows — semantic backend is auto-selected per platform via target-specific dependencies. No special flags needed.

## v0.23.1 - 2026-03-22

### Two-tier progressive search in TUI

- TUI search now returns instant results from FTS5/BM25 + structural similarity, then refines with semantic search in a background thread. No more UI blocking during search.
- Search results stay visible while typing — no flash on keystroke.
- Spinning indicator shows when background semantic refinement is in progress.
- Search results display in flat rank order (best match first) instead of being reshuffled by hierarchy.

### Semantic search improvements

- Lowered semantic score thresholds (0.60 → 0.15) so semantic search actually bridges vocabulary gaps (e.g. "authentication" now finds "auth" items).
- Added `--semantic-threshold` flag to `bn search` for experimenting with threshold values.
- Added `hybrid_search_fast()` and `hybrid_search_with_threshold()` to the public API.

### Fixes

- Fixed TUI auto-refresh re-triggering search every 1-2s when a query was active, causing result flashing.

## v0.23.0 - 2026-03-22

### Windows support

- Made `semantic-ort` (ONNX Runtime) an opt-in feature, resolving CRT linking conflicts on Windows (MD vs MT `RuntimeLibrary` mismatch between `ort` and `esaxx-rs`).
- Added `model2vec` embedding backend (`safetensors` + `tokenizers`, no ONNX) for Windows-friendly semantic search with real embeddings.
- Added hash embedder as a zero-dependency semantic baseline — always available regardless of feature flags.
- Added `windows` convenience feature: `cargo install bones-cli --no-default-features --features windows`.
- Backend priority chain: ORT > model2vec > hash embed, with automatic fallback.

### Fixes

- Fixed hardcoded 384-dimension requirement in `knn_search` that rejected non-ORT embedding backends.
- Content hash now includes backend ID, so switching backends triggers re-embedding instead of silently using stale vectors.
- Embedding dimensions are now dynamic per-backend instead of hardcoded to MiniLM's 384.

## v0.22.11 - 2026-03-11

- Added `bn create --from-file <path>` with YAML, JSON, and TOML support for creating one or many bones from structured input files.
- Stopped surfacing goal bones in executable triage recommendations so `bn triage` and `bn next` focus on actionable work items.
