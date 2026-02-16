# Bones: Definitive Design & Implementation Plan

*Synthesized from initial design, advanced research, storage format analysis, search architecture, and conversation decisions.*

---

## What Bones Is

Bones is a CRDT-first, git-native issue tracker for AI agent swarms. The command is `bn`. Written in Rust.

**Core philosophy:**
1. **Events are truth.** Everything else is a projection.
2. **CRDTs over coordination.** Never ask "who wins?" — everyone does.
3. **The graph knows priority.** Don't ask humans to guess; compute it.
4. **Simple verbs, powerful engine.** `bn next` hides PageRank, HITS, and critical path analysis behind a single word.
5. **Agents first, humans welcome.** Every command has `--json`. The triage engine is the API.
6. **Git-native, not git-dependent.** The event log works with git but doesn't require daemons, hooks, or merge drivers.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                       CLI (bn)                               │
│  Commands: create, do, done, next, triage, plan, graph, ...  │
│  TUI: bn ui (ratatui-based, ported from beads-tui)           │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                    Event Engine                               │
│                                                              │
│  ┌──────────────────┐    ┌──────────────────┐                │
│  │  CRDT State       │◄──│  Event Log       │                │
│  │  (Eg-Walker DAG   │   │  (.events TSJSON │                │
│  │   replay, ITC)    │   │   + .bin cache)  │                │
│  └────────┬─────────┘    └────────┬─────────┘                │
│           │                       │                          │
│           ▼                       ▼                          │
│  ┌──────────────────┐    ┌──────────────────┐                │
│  │  SQLite Projection│    │  Git Sync        │                │
│  │  (relational +    │    │  (append-only    │                │
│  │   FTS5 + vec)     │    │   sharded files) │                │
│  └──────────────────┘    └──────────────────┘                │
│                                                              │
│  ┌──────────────────────────────────────────────────────┐    │
│  │  Triage Engine                                        │    │
│  │  DF-PageRank · Betweenness · HITS · Critical Path      │    │
│  │  Composite Score · Thompson Feedback · Whittle/Fallback│    │
│  └──────────────────────────────────────────────────────┘    │
│                                                              │
│  ┌──────────────────────────────────────────────────────┐    │
│  │  Search Engine                                        │    │
│  │  FTS5 BM25 · Semantic Vectors · Structural Similarity │    │
│  │  RRF Fusion · Duplicate Prevention                    │    │
│  └──────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────┘
```

### Data Flow

1. Every mutation produces an **immutable event** appended to `.bones/events/YYYY-MM.events` (TSJSON format).
2. Events are **replayed** via Eg-Walker DAG into CRDT state, **projected** into SQLite for indexing and search.
3. On `git pull`, new remote events are appended. CRDTs guarantee convergence regardless of event ordering.
4. SQLite is fully disposable — `bn rebuild` reconstructs it from the event log.
5. A binary columnar cache (`.bones/cache/events.bin`) accelerates replay. Also disposable.

---

## The Work Item Model

### Kinds (3)

| Kind | When to Use |
|------|-------------|
| `task` | Default. A discrete unit of work. |
| `goal` | A container of children working toward an outcome. Replaces "epic." |
| `bug` | Something broken that needs fixing. Carries implicit triage weight. |

Identity test:
- Has children and represents an outcome? → **goal**
- Describes something broken? → **bug**
- Everything else → **task**

### States (4)

```
       ┌─── open ───┐
       │             │
       ▼             │
     doing ──────► done
       │             │
       │             ▼
       └──────► archived
```

| State | Meaning |
|-------|---------|
| `open` | Exists, not yet started. |
| `doing` | Someone is actively working on it. |
| `done` | Completed. Still visible, still queryable. |
| `archived` | Out of sight. Memory-decayed or irrelevant. |

No "blocked" state — blocking is a relationship, not a state. An item in `open` with unresolved `blocked_by` links won't appear in `bn next`.

### Urgency (3)

| Urgency | Meaning |
|---------|---------|
| `urgent` | Do this NOW regardless of what the graph says. Overrides computed priority. |
| `default` | Normal. Priority determined by graph metrics. |
| `punt` | Don't work on this. Visible but excluded from `bn next`. Like beads's "deferred." |

This replaces numeric priority (P0-P4). **Priority is computed from the dependency graph**, not assigned by hand. Urgency is the human override valve.

### Size (optional, 7)

| Size | Label |
|------|-------|
| `xxs` | Extra extra small |
| `xs` | Extra small |
| `s` | Small |
| `m` | Medium |
| `l` | Large |
| `xl` | Extra large |
| `xxl` | Extra extra large |

Sizes are **optional**. No timelines attached — agents get estimates wrong. If an agent sets a size, fine. If not, nothing breaks. The triage engine doesn't use size for priority; it's purely informational for humans planning capacity.

### Parent-Child Containment (Goals)

Goals are **containers**, not dependency roots. The parent-child edge is a containment relationship, distinct from the blocking relationship.

```bash
bn create "Phase 1: Auth Migration" --kind goal
bn create "Implement JWT rotation" --parent bn-p1
bn create "Update OIDC provider" --parent bn-p1
bn create "Write migration runbook" --parent bn-p1
```

```
Phase 1: Auth Migration [goal, open]
  Progress: 1/3 (33%)
  ├── ✓ bn-a3f8  Implement JWT rotation      [done]
  ├──   bn-c7d2  Update OIDC provider        [doing]
  └──   bn-e5f6  Write migration runbook      [open]
```

**Two orthogonal graph types:**
- **Parent-child** (containment): "this task is part of this goal." Determines completion. No ordering implied.
- **Blocks** (dependency): "this task must finish before that task can start." Determines scheduling. No containment implied.

A task can be part of Goal A and blocked by a task in Goal B. Cross-goal dependencies are valid and surfaced by triage.

**Goal auto-completion** (default: enabled, configurable):
- When the last child of a goal reaches `done`, Bones emits a system event closing the goal: `agent: bones, reason: "all children complete"`.
- Adding a new child to a closed goal reopens it automatically.
- Config: `goals.auto_complete: true` (default) or `false` for manual close.

Nested goals are supported. Completion rolls up through the tree.

---

## Dependencies

### Two Link Types

| Link | Meaning | Affects Triage? |
|------|---------|-----------------|
| `blocks` | A must complete before B can start. | Yes. Drives critical path, PageRank, ready-work detection. |
| `relates` | A and B are connected context. | No. Informational only. |

**Causation** is captured on the event (`causation` field), not as a link. When an agent discovers a new issue while working on X, the `item.create` event records `causation: "bn-x1y2"`. Preserves audit trail without polluting the dependency graph.

---

## Event Format: TSJSON

The canonical event log format is **TSJSON** — tab-separated fixed fields with a JSON payload. File extension: `.events`.

```
# bones event log v1
# fields: timestamp \t agent \t type \t item_id \t data
1708012200	claude-abc	item.create	bn-a3f8	{"title":"Fix auth retry","kind":"task","effort":"m","labels":["backend"]}
1708012201	claude-abc	item.move	bn-a3f8	{"state":"doing"}
1708012202	gemini-xyz	item.create	bn-c7d2	{"title":"Payment schema migration","kind":"task","effort":"l"}
1708012215	claude-abc	item.link	bn-a3f8	{"blocks":"bn-c7d2"}
1708012230	claude-abc	item.comment	bn-a3f8	{"body":"Root cause is a race in token refresh."}
1708012300	claude-abc	item.move	bn-a3f8	{"state":"done","reason":"Shipped in commit 9f3a2b1"}
```

### Why TSJSON

- ~40% more compact than JSONL (positional fixed fields eliminate repeated key names)
- Git diffs are clean — each added line is a self-contained event
- Grep/awk/sort work natively on the positional fields without a JSON parser
- Partial parse: can extract timestamp/agent/type/item_id without touching JSON payload
- One event = one line invariant preserved

### Event Type Catalog

```
item.create     — Create a new work item
item.update     — Update fields (title, description, size, labels, etc.)
item.move       — Transition to a new state (open → doing → done → archived)
item.assign     — Assign/unassign an agent
item.comment    — Add a comment or note
item.link       — Add a dependency or relationship
item.unlink     — Remove a dependency or relationship
item.delete     — Soft-delete (tombstone)
item.compact    — Replace description with summary (memory decay)
item.snapshot   — Lattice-compacted state for a completed item
item.redact     — Replace event payload with [redacted] in projection (secret removal, legal erasure)
```

### File Organization: Time-Sharded

```
.bones/
├── events/
│   ├── 2026-01.events     # Frozen, never modified
│   ├── 2026-02.events     # Active shard, append-only
│   └── current.events     # Symlink to active shard
├── bones.db               # Gitignored — SQLite projection + FTS5 + vectors
├── config.toml            # Committed — project config
├── feedback.jsonl          # Gitignored — local triage feedback
└── cache/
    ├── events.bin          # Gitignored — binary columnar cache
    └── triage.json         # Gitignored — cached graph metrics
```

Frozen shards are immutable. Git stores them once and never diffs them again. Only the active shard appears in diffs.

### Shard Manifests

Each shard has a companion manifest (committed to git) recording event count, root Merkle hash, and byte length. On startup, Bones verifies the manifest before replaying events. Corruption is detected before it reaches the projection layer. `bn verify` checks all shard manifests and reports integrity status.

### Interoperability

`bn export --jsonl` produces standard JSONL from the event log. `bn log --json` outputs structured JSON to stdout. The event log format is internal; the public API is the CLI with `--json` output.

---

## Item IDs: terseid

All item IDs are generated by [terseid](https://github.com/bobisme/terseid) — an adaptive-length, collision-resistant short ID library.

**Format:** `bn-<base36hash>[.<child>.<path>]` (e.g., `bn-a7x`, `bn-c7d2`, `bn-a3f8.1.3`)

**How it works:**
- SHA256 seed → first 8 bytes → base36 → truncate to adaptive length
- Length grows with collection size via birthday problem math (3 chars for ≤100 items, 4 for ~200, 5 for ~7k, etc.)
- 4-tier collision avoidance: nonce escalation → length extension → long fallback → desperate fallback
- Caller provides `exists` closure — terseid never touches storage directly

**Integration:**
```rust
use terseid::{IdConfig, IdGenerator, IdResolver, ResolverConfig};

// Generation: seed from title + nonce, collision check against SQLite
let gen = IdGenerator::new(IdConfig::new("bn"));
let id = gen.generate(
    |nonce| format!("{title}|{nonce}").into_bytes(),
    item_count,
    |candidate| db.id_exists(candidate),
);

// CLI resolution: partial input → full ID
let resolver = IdResolver::new(ResolverConfig::new("bn"));
let resolved = resolver.resolve("a7x", |id| db.id_exists(id), |s| db.find_matching(s));

// Child IDs for goal children
let child = terseid::child_id("bn-a7x", 1);  // "bn-a7x.1"
```

**Dependency:** `terseid = { git = "https://github.com/bobisme/terseid.git" }`

---

## CRDT Layer

### Eg-Walker Event DAG

Instead of classical OR-Sets with per-element metadata, Bones uses an **Eg-Walker-inspired event DAG** (from Diamond Types / Loro). Events store raw operations with parent hash(es) encoding causality implicitly in the DAG structure.

- Event size drops ~40% vs. embedded vector clocks
- Merge cost is O(divergent events), not O(all events)
- Tombstone accumulation eliminated

### Interval Tree Clocks (ITC)

Replace vector clocks with **Interval Tree Clocks** (Almeida et al. 2008). Clock size adapts to currently active agents, not historical total — critical for AI swarms where agents are ephemeral.

- O(currently active agents) size vs. O(all historical agents) for vector clocks
- Fork/join/event/peek are O(log n)
- Serializes as ~20-50 bytes vs. hundreds for vector clocks

### Field-Level CRDT Types

| Field | CRDT Type | Merge Semantics |
|-------|-----------|-----------------|
| `title` | LWW Register | Last-writer-wins by ITC |
| `description` | LWW Register | Last-writer-wins by ITC |
| `kind` | LWW Register | Last-writer-wins by ITC |
| `state` | LWW Register with state machine validation | LWW, invalid transitions rejected |
| `size` | LWW Register | Last-writer-wins by ITC |
| `urgency` | LWW Register | Last-writer-wins by ITC |
| `assignees` | OR-Set (via DAG replay) | Add/remove converge without conflicts |
| `labels` | OR-Set (via DAG replay) | Add/remove converge without conflicts |
| `blocked_by` | OR-Set of item IDs | Add/remove converge without conflicts |
| `related_to` | OR-Set of item IDs | Add/remove converge without conflicts |
| `parent` | LWW Register (nullable) | Last-writer-wins by ITC |
| `comments` | G-Set (Grow-only Set) | Comments never deleted, only appended |

### Redaction

`item.redact` events reference a prior event by hash and replace its payload in the projection with `[redacted]`. The original event remains in the log (Merkle integrity preserved), but projections hide the content. Handles accidental secret exposure and legal erasure without breaking convergence or audit trails.

### Deterministic LWW Tie-Breaking (Normative)

To guarantee bit-identical convergence across replicas and implementations, all LWW fields must use this exact total order:

1. Compare ITC causal dominance.
2. If concurrent, compare `wall_ts`.
3. If equal, compare `agent_id` lexicographically.
4. If equal, compare `event_hash` lexicographically.

Any implementation that diverges from this order is non-conformant.

### State Merge Semantics (Epoch + Phase)

For deterministic reopen/close behavior under concurrency, `state` is represented as `(epoch, phase)`:

- `phase in {open, doing, done, archived}`
- `reopen` increments `epoch` and sets `phase=open`
- Join is `max(epoch)`, then `max(phase_rank)` within epoch

This preserves semilattice laws while supporting reopen without reject/accept divergence.

### Merkle-DAG Event Integrity

Structure the event DAG as a Merkle-DAG where each event's hash includes its causal parent hashes. Provides tamper evidence, O(log N) sync diffing, and selective verification via inclusion proofs.

### Formal Properties

The merge operator (S, ⊔) forms a join-semilattice:
1. **Associativity**: (a ⊔ b) ⊔ c = a ⊔ (b ⊔ c)
2. **Commutativity**: a ⊔ b = b ⊔ a
3. **Idempotence**: a ⊔ a = a

These laws guarantee convergence in any asynchronous network with any message ordering, duplication, or delay.

---

## Triage Engine

### Graph Normalization Pipeline

Before computing centrality metrics, the dependency graph is normalized:

1. Collapse SCCs into a condensation DAG (removes cycles from metric computation)
2. Compute transitive reduction for scheduling edges (removes redundant edges)
3. Preserve original graph for display and explanations

This produces more stable metrics, faster computation, and fewer spurious bottleneck signals.

### Computed Priority (Bones Composite Score)

```
P(v) = α·CP(v) + β·PR(v) + γ·BC(v) + δ·U(v) + ε·D(v)

  CP(v) = Critical Path Centrality
  PR(v) = PageRank
  BC(v) = Betweenness Centrality
  U(v)  = Urgency signal (urgent override, punt suppression)
  D(v)  = Decay factor (items in "doing" too long get boosted)
```

Weights (α,β,γ,δ,ε) are learned from feedback via Thompson Sampling (contextual bandit).

### Graph Metrics

**Phase 1 (synchronous, < 20ms):**
- Degree centrality (in/out)
- Topological sort
- Graph density

**Phase 2 (incremental, cached):**
- DF-PageRank (incremental, best-effort; falls back to full recompute when approximation or stability checks fail)
- Betweenness centrality
- HITS (hubs/authorities)
- Eigenvector centrality
- Critical path analysis
- Cycle detection

Results cached with content hash of event log. Cache invalidates only when new events arrive.

### Multi-Agent Scheduling (Whittle Index)

When multiple agents ask `bn next` simultaneously, Bones uses Whittle Index assignment only when indexability checks pass for the active workload class. Otherwise, Bones falls back to constrained optimization assignment (min-cost flow style) with contextual tie-breaks.

Assignment objective accounts for:
- Immediate value of completing it (unblocking downstream)
- Opportunity cost of not working on other items
- Expected completion time (from size estimate)
- Dynamic state (blocked by something in progress?)

`bn plan --explain` reports which assignment regime was used and why.

The fallback optimizer also enforces fairness constraints (bounded agent starvation) and penalizes duplicate assignment probability.

### Topological Health (Persistent Homology)

Default health diagnostics are directed-graph native and always on:

- SCC structure (independent work streams)
- Cycle diagnostics and feedback-edge pressure
- Bridge/cut pressure for fragility detection

Advanced topology mode (`bn health --topology=advanced`) enables directed path-homology analysis and sampled filtrations when preconditions are satisfied.

### Feedback Loop

```bash
bn did bn-a3f8     # "I worked on this"
bn skip bn-c7d2    # "I skipped this recommendation"
```

Feedback stored in `.bones/feedback.jsonl` (gitignored, local). Adjusts triage weights per-agent over time.

---

## Search Engine (Three-Layer Hybrid)

### Layer 1: FTS5 Lexical (BM25)

SQLite FTS5 with porter stemming, unicode support, prefix indexes. Weighted: title 3×, description 2×, labels 1×.

Catches exact keyword matches, IDs, error codes. Sub-1ms.

### Layer 2: Semantic Vectors (MiniLM-L6-v2)

384-dim embeddings via ONNX Runtime (int8 quantized, ~23MB model). Stored in sqlite-vec. CPU inference ~5ms per sentence. Fully offline.

Catches vocabulary-gap matches: "auth timeout" ↔ "authentication fails after 30 seconds."

### Layer 3: Structural Graph Similarity

Jaccard similarity on labels, dependencies, agents, parent, and graph neighborhood proximity.

Catches structurally related items even when text differs.

### Fusion: Reciprocal Rank Fusion (RRF, K=60)

Rank-based combination of all three layers. No score calibration needed.

### Duplicate Prevention at Creation Time

```bash
$ bn create "Fix authentication timeout in payment service"

⚠ Similar items found:
  bn-a3f8  "Payment service auth fails after 30s"     (92% match, state: open)
  bn-c7d2  "Auth token expiry causes payment drops"    (78% match, state: doing)

Create anyway? [y/N/link]
```

Thresholds:
- ≥ 0.90 → `likely_duplicate`
- 0.70–0.89 → `possibly_related`
- 0.50–0.69 → `maybe_related`
- < 0.50 → no warning

All thresholds configurable in `.bones/config.toml`.

---

## CLI Design

### Core Commands

```bash
# Creating work
bn create "Title"                    # Create a task (default)
bn create "Title" --kind goal        # Create a goal
bn create "Title" --kind bug         # Create a bug
bn create "Title" -s m               # Set size
bn create "Title" --parent bn-a3f8   # Create as child of a goal
bn create "Title" --blocks bn-c7d2   # Create with a blocking link
bn create "Title" --urgent           # Set urgency to urgent
bn create "Title" --punt             # Set urgency to punt

# Viewing work
bn list                              # All open items
bn list --state doing                # Filter by state
bn list --label backend              # Filter by label
bn show bn-a3f8                      # Full details
bn graph bn-a3f8                     # ASCII dependency tree
bn progress bn-p1                    # Goal completion status
bn log bn-a3f8                       # Event history for an item

# Doing work
bn do bn-a3f8                        # Move to "doing"
bn done bn-a3f8                      # Move to "done"
bn done bn-a3f8 --reason "Shipped"   # Move with closing note

# Triage
bn next                              # What should I do right now?
bn next --agent 3                    # Multi-agent assignment (Whittle or fallback)
bn triage                            # Full triage report
bn plan                              # Parallel execution plan
bn plan bn-p1                        # Plan for a specific goal's children
bn health                            # Project health dashboard
bn health --topology                 # Persistent homology analysis
bn cycles                            # Show dependency cycles

# Search
bn search "auth timeout"             # Hybrid search (all three layers)
bn search "query" --semantic         # Semantic-only
bn search "CVE-2024" --lexical       # Lexical-only
bn similar bn-a3f8                   # Find similar items
bn dedup                             # Bulk duplicate scan

# Dependencies
bn link bn-a3f8 --blocks bn-c7d2    # A blocks B
bn link bn-a3f8 --relates bn-c7d2   # A relates to B
bn unlink bn-a3f8 bn-c7d2           # Remove link

# Labels
bn tag bn-a3f8 backend security     # Add labels
bn untag bn-a3f8 security           # Remove label

# Hierarchy
bn move bn-a3f8 --parent bn-p1      # Reparent into a goal

# Maintenance
bn rebuild                           # Rebuild SQLite + embeddings from events
bn compact                           # Lattice compaction for old done items
bn verify                            # Verify shard manifests and Merkle integrity
bn stats                             # Event log statistics
bn export --jsonl                    # Export events as JSONL

# Migration
bn migrate-from-beads                # Import beads database into Bones

# TUI
bn ui                                # Launch ratatui TUI

# Every command supports --json for agents
bn next --json
bn triage --json
bn plan --json
```

### Agent Identity

Every mutating command requires an agent for attribution. Resolution order:

1. `--agent <name>` flag (highest priority, per-command override)
2. `BONES_AGENT` environment variable (bones-specific, set by launchers/hooks)
3. `AGENT` environment variable (generic agent env, e.g. from botbox)
4. `USER` environment variable — **only** if stdin is a TTY (interactive human session)

If none are set, mutating commands **error** with a clear message explaining how to set one. Read-only commands (`list`, `show`, `search`, etc.) do not require an agent.

The TTY guard on `USER` prevents scripts, CI, cron, and agent subprocesses from silently attributing events to a Unix username. Config files do not participate in agent resolution.

The agent string is free-form (e.g., `bones-dev`, `claude-abc`, `gemini-xyz`). It is recorded in every event's `agent` field and is the permanent audit trail for who did what.

---

## Storage: Binary Columnar Cache

The gitignored `.bones/cache/events.bin` uses an Automerge-inspired columnar format:

- Timestamps: delta-encoded varints
- Agent IDs: interned table, RLE-encoded references
- Event types: 4-bit enum, RLE-encoded
- Item IDs: dictionary-encoded
- Values: type-specific (FSST strings, varint ints, packed booleans)

Compression is measured per event class and reported by percentile (p50/p95/p99). Structural events are expected to compress far more than payload-heavy text events. This cache is purely a performance optimization and is rebuilt on demand from `.events` files.

---

## Prolly Tree Sync (Future)

For multi-repo or non-git sync scenarios, organize events into a Prolly Tree keyed by `(item_id, event_timestamp)`. Two replicas diff in O(log N) by comparing root hashes. Enables efficient sync over MCP, HTTP, or USB drives.

---

## Deterministic Simulation Testing

TigerBeetle/FoundationDB-style simulation as a first-class component:

- Seeded deterministic RNG
- Simulated agents with independent event queues
- Configurable network: latency, partition, reorder, duplication
- Configurable clocks: drift, skew, freeze
- Convergence oracle checking after all events delivered

**Invariants:**
1. Strong convergence: all replicas produce identical state
2. Commutativity: any event ordering yields same result
3. Idempotence: duplicate events don't change state
4. Causal consistency: dependent events maintain order
5. Triage stability: metrics converge regardless of event ordering

**Property tests** (semilattice laws via quickcheck):
- merge_is_commutative
- merge_is_associative
- merge_is_idempotent

---

## Memory Architecture

Arena allocator for event replay, mmap for binary event file, pre-allocated graph structures for triage. Zero allocation on hot paths where practical.

Latency targets are benchmark-gated by dataset tier and reported as p50/p95/p99 instead of fixed universal promises.

### Benchmark Tiers (SLO Reporting)

- **Tier S:** 1k items / 50k events
- **Tier M:** 10k items / 500k events
- **Tier L:** 100k items / 5M events

Tracked operations: `bn create`, `bn next`, `bn search`, `bn rebuild`, plus bytes/event by event class.

---

## Log Compaction (Lattice-Based)

Replace event sequences for completed items with a single `item.snapshot` event (the semilattice join of all events). Coordination-free — each replica compacts independently and still converges.

Policy: compact items in `done`/`archived` for > 30 days. Projected savings: ~70% reduction for mature projects.

---

## TUI: `bn ui`

Ratatui-based terminal UI, ported from [beads-tui](https://github.com/bobisme/beads-tui) (`~/src/beads-tui/`). The existing beads-tui is a Rust binary (`bu`) using ratatui 0.29, crossterm 0.28, tui-textarea, rusqlite, tokio, clap, and chrono.

The port adapts beads-tui's UI patterns to Bones' data model:
- List view with filtering by state, kind, label, urgency
- Detail view with full item info, comments, dependency graph
- Triage view showing `bn next` recommendations with scores
- Goal progress view with tree of children
- Search with live duplicate detection
- Keyboard-driven workflow matching the CLI verbs

---

## Migration: `bn migrate-from-beads`

Utility to import an existing beads database into Bones:
- Read beads SQLite database (or JSONL export)
- Map beads issue types → Bones kinds (bug→bug, feature/task/chore→task, epic→goal)
- Map beads statuses → Bones states (open→open, in_progress→doing, closed/verified/wontfix→done, deferred→open+punt)
- Map beads priorities → Bones urgency (P0→urgent, P1-P4→default)
- Map beads dependencies → Bones blocks/relates links
- Preserve comments, labels, assignees
- Emit Bones events with original timestamps and agents
- Build initial SQLite projection

---

---

## Configuration

```toml
# .bones/config.toml

[goals]
auto_complete = true          # Auto-close goals when all children done

[search]
semantic = true               # Enable semantic search (requires model)
model = "minilm-l6-v2-int8"  # Embedding model
duplicate_threshold = 0.85    # >= this = likely duplicate
related_threshold = 0.65      # >= this = possibly related
warn_on_create = true         # Show duplicate warnings during creation
block_on_create = false       # Block creation if duplicate found (strict)

[triage]
feedback_learning = true      # Adjust weights from bn did/skip feedback
```

---

## What Bones Drops from Beads (Intentionally)

| Beads Feature | Bones Position |
|---------------|----------------|
| Background daemon | Not needed. Append-only log + rebuild is fast enough. |
| Auto-commit to git | Explicit is better. Agents commit when ready. |
| Dolt backend | Unnecessary. CRDTs handle versioning at the data level. |
| Protected branch worktrees | Not needed. Append-only events don't conflict. |
| Sequential ID migration | Adaptive hash IDs via terseid from day one. |
| 8 status values | 4 states: open, doing, done, archived. |
| 5 priority levels | Computed from graph. 3-value urgency for overrides. |
| 5 issue types | 3 kinds: task, goal, bug. |
| Git hooks | Optional. System works without them. |
| Merge driver | Not needed. CRDTs + append-only = no conflicts. |
| Templates | `bn create` with flags is enough. |

---

## Implementation Phases

### Phase 0: Spec and Reliability Gates

- Normative merge spec (including deterministic LWW tie-breaking)
- Semilattice conformance/property test harness
- Deterministic simulator skeleton with seed replay
- Benchmark corpus and tiered SLO reporting

### Phase 1: Core (MVP)

- TSJSON event format with time-sharded files
- Event engine: append, replay, parse
- Eg-Walker DAG with ITC clocks
- CRDT state machine (LWW registers, OR-Sets via DAG replay)
- SQLite projection (relational tables, FTS5)
- Work item model: 3 kinds, 4 states, 3 urgencies, optional 7 sizes
- Parent-child containment with auto-complete
- Blocking/relates dependencies
- Shard manifests and `bn verify`
- `item.redact` event type
- CLI: create, list, show, do, done, search (lexical), tag, link, rebuild, verify
- `bn migrate-from-beads` utility

### Phase 2: Triage

- Dependency graph construction from SQLite
- Graph normalization: SCC condensation + transitive reduction
- Static metrics: degree centrality, topological sort, density
- DF-PageRank (incremental with stability/fallback checks)
- Betweenness centrality, HITS, eigenvector centrality
- Critical path analysis, cycle detection
- Composite priority score
- CLI: next, triage, plan, health, cycles
- Thompson Sampling feedback loop (bn did, bn skip)

### Phase 3: Search & Dedup

- MiniLM-L6-v2 ONNX embedding integration
- sqlite-vec for vector storage
- Semantic search layer
- Structural similarity layer
- RRF fusion
- Duplicate prevention at creation time
- CLI: search --semantic, similar, dedup

### Phase 4: Scale & Verification

- Binary columnar cache format
- Spectral graph sparsification for large projects
- Persistent homology for project health topology
- Full DST simulator (TigerBeetle-style)
- Semilattice property tests (quickcheck)
- Lattice-based log compaction
- Arena allocator + mmap for zero-allocation hot paths

### Phase 5: Multi-Agent & Distribution

- Whittle scheduling with indexability gate + optimization fallback
- Prolly Tree sync for non-git scenarios
- TUI (`bn ui`, ported from beads-tui)
- Multi-repo event aggregation

---

## Rust Dependencies (Expected)

| Crate | Purpose |
|-------|---------|
| `clap` | CLI parsing |
| `serde`, `serde_json` | Serialization |
| `rusqlite` | SQLite (bundled) |
| `petgraph` | Graph algorithms |
| `ort` | ONNX Runtime for embeddings |
| `ratatui`, `crossterm` | TUI |
| `tui-textarea` | TUI text input |
| `tokio` | Async runtime (for MCP, TUI) |
| `chrono` | Time handling |
| `anyhow` | Error handling |
| `quickcheck` / `proptest` | Property testing |
| `memmap2` | Memory-mapped files |
| `bumpalo` | Arena allocation |
