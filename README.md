# bones

bones is a CRDT-native issue tracker for distributed human and agent collaboration.

![The Ossuary of the Unfinished](images/bones-embed.jpg)

It is designed for teams where multiple people and coding agents are editing the same backlog concurrently, and where machine-readable CLI output matters as much as human UX.

![bones UI](images/ui.webp)

## Why bones exists

bones is heavily inspired by great prior work:

- [`beads`](https://github.com/steveyegge/beads) by Steve Yegge
- [`beads_viewer`](https://github.com/Dicklesworthstone/beads_viewer) by Jeffrey Emanuel

Those tools proved that agent-oriented issue tracking and robot triage workflows are practical. bones builds on that direction with a different storage/convergence architecture and a unified CLI/TUI surface.

The short version:

- **beads**: strong issue/workflow model.
- **beads_viewer**: rich `--robot-*` triage/reporting interface.
- **bones**: append-only event log + CRDT convergence + graph-native triage + consistent `--format json` contracts.

## Project goal: eliminate merge conflicts in tracker data

bones is built around an append-only event log in `.bones/events/*.events`, then replayed into disposable projections (`.bones/bones.db`, caches).

Design goal: **eliminate backlog merge conflicts as a normal mode of operation** by making writes additive and convergence-driven.

- events are immutable facts
- projection state is rebuildable
- concurrent writes converge via CRDT semantics
- git diffs stay mostly line-append operations

In other words: fewer painful "who wins this edit?" moments, more "everyone can keep moving."

## Built-in duplicate detection

When you create a bone, bones automatically searches the existing backlog and surfaces potential duplicates before committing. This combines full-text search, semantic vector similarity, and structural matching (shared labels, parents, dependencies) fused with reciprocal rank fusion.

This matters most in agent workflows. Agents create bones aggressively and don't naturally pause to check whether something similar already exists. With bones, duplicate detection is part of the create path — not a separate cleanup step.

```bash
# search explicitly
bn search "retry budget"

# duplicates surfaced automatically on create
bn create --title "Add retry budget to queue writer"

# find bones similar to an existing one
bn triage similar bn-abc
```

## Why CRDTs

Traditional issue trackers assume a central server arbitrates writes. That breaks down when you have multiple agents and humans editing the same backlog concurrently across git branches, workspaces, and offline contexts. You get merge conflicts in tracker state, lost updates, and manual conflict resolution that interrupts flow.

CRDTs (Conflict-free Replicated Data Types) solve this at the data model level. Every write is an immutable event appended to a log. Any two replicas that have seen the same set of events will compute the same state, regardless of the order they received them. No coordination, no locking, no conflict resolution UI.

bones uses a grow-only event set with deterministic merge and tie-break rules. This means:

- **Branches can diverge freely** — each workspace appends events independently
- **Merging is always safe** — union the event logs, replay, done
- **No data loss** — every write from every agent is preserved
- **Offline-first** — sync when convenient, converge automatically

The tradeoff is that the data model must be designed so all operations commute. bones achieves this with append-only events and last-writer-wins tie-breaking on derived fields.

## The math and algorithms under the hood

See `notes/plan.md` for the full design reference. Highlights:

- **CRDT/event layer**: event DAG replay, ITC clocks, deterministic merge/tie-break rules.
- **Graph triage**: SCC condensation, transitive reduction, PageRank, betweenness, HITS/eigenvector signals, critical-path influence.
- **Composite ranking**: urgency override + graph metrics + decay signals.
- **Search fusion**: FTS5 lexical scoring + semantic vectors + structural similarity, merged with RRF.

You can use bones without caring about these internals, but they are why `bn next` and triage outputs are graph-aware instead of flat priority sorting.

## Typical agent workflow with `bn`

```bash
# one-time setup per repo
bn init

# set identity for attribution
export AGENT=bones-dev

# create and link work
bn create --title "Add retry budget to queue writer" --kind task --label reliability
bn create --title "Queue durability hardening" --kind goal
bn bone move bn-abc --parent bn-goal1

# get next assignments
bn next
bn next 3 # multi-slot
bn next --take # assign the next to yourself
bn next --assign-to agent-1 --assign-to agent-2 # delegate

# execute work and leave traceable notes
bn do bn-abc
bn bone comment add bn-abc "Found race in retry loop; patch in progress"
bn done bn-abc

# machine-readable reporting
bn triage # show top bones to work on
bn triage plan <goal-id> # suggest a parallel execution plan for a goal
```

## Migration from beads

Import an existing beads project with:

```bash
bn data migrate-from-beads --beads-db .beads/beads.db
```

or:

```bash
bn data migrate-from-beads --beads-jsonl export.jsonl
```

## Installation

```bash
cargo install bones-cli
```

Works on Linux, macOS, and Windows. The semantic search backend is auto-selected per platform (ONNX Runtime on Linux/macOS, model2vec on Windows).

## Shell completions

Generate shell completions with:

```bash
bn completions bash
bn completions zsh
bn completions fish
```

## Development

```bash
just check
just install
```

## Deterministic simulation testing

bones includes a simulation harness (`bones-sim`) that verifies CRDT correctness under adversarial network conditions. Instead of hoping the convergence logic is correct, we prove it across thousands of randomized scenarios.

The sim models multiple agents emitting events over a lossy network with configurable fault injection: message drops, reordering, duplication, network partitions, and clock drift. After the network drains, a reconciliation phase models the real sync protocol (pairwise gossip + set union). An oracle then checks five invariants:

- **Convergence** — all agents end up with identical state
- **Commutativity** — event application order doesn't matter
- **Idempotence** — re-applying events is a no-op
- **Causal consistency** — no gaps in per-source sequences
- **Triage stability** — derived scores agree across replicas

Every seed is deterministic: same seed, same trace, same result. When a seed fails, you replay it for a full execution trace showing exactly which message was dropped or reordered and how it cascaded.

```bash
# run 100 seeds with default fault rates
bn dev sim run --seeds 100

# replay a failing seed for debugging
bn dev sim replay --seed 42

# verify reconciliation is necessary (disable it, watch failures)
bn dev sim run --seeds 100 --reconciliation-rounds 0
```

The default configuration passes 100,000+ seeds with 10% fault rates.

## Semantic acceleration

- `sqlite-vec` is bundled at build time and auto-registered as a SQLite extension.
- When available, `bn` reports vector acceleration in capability/health output.
- If unavailable, semantic search still works via Rust-side KNN over stored embeddings.
- Set `BONES_SQLITE_VEC_AUTO=0` to disable auto-registration for troubleshooting.
