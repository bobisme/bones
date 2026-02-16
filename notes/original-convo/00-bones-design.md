# Bones: A CRDT-First Issue Tracker for AI Agent Swarms

## The Elevator Pitch

Bones is a reimagining of [beads](https://github.com/steveyegge/beads) that inverts the data model: **JSON events are the source of truth**, projected into SQLite for fast queries. By treating every mutation as an immutable, content-addressed event and using CRDTs for all mutable state, Bones eliminates merge conflicts entirely — across branches, machines, and concurrent agents. It bakes in the graph-theoretic triage intelligence from [beads_viewer](https://github.com/Dicklesworthstone/beads_viewer) as a first-class subsystem, and simplifies the issue model from Jira-like complexity to Asana-like focus.

The command is `bn`.

---

## Why Bones?

### The Problems Bones Solves

**1. Beads's sync is fragile.** Beads uses SQLite as primary storage and exports to JSONL for git portability. This creates a two-source-of-truth problem: the daemon, the JSONL, and the database can all diverge (especially on Windows, in worktrees, or across branches). Bones eliminates this by making the event log canonical and SQLite a disposable projection.

**2. Merge conflicts are still possible.** Even with hash-based IDs, concurrent JSONL edits can produce git conflicts. Beads's merge driver helps but adds complexity. Bones uses CRDTs so that any ordering of events produces the same final state — merge conflicts become structurally impossible.

**3. Triage is a separate tool.** beads_viewer's brilliant graph analysis (PageRank, critical path, HITS, cycle detection) lives in a separate binary. Agents must shell out to `bv --robot-triage` as a sidecar. Bones computes these metrics internally and exposes them through the same CLI, so `bn triage` is a single call.

**4. The model is overfit to Jira.** Beads has 5 issue types (bug/feature/task/epic/chore), 8 statuses, and 5 priority levels. For AI agent workflows, this is too many knobs. Bones simplifies to a model that's closer to how agents actually think about work.

---

## Core Architecture

```
┌─────────────────────────────────────────────────────────┐
│                     CLI (bn)                             │
│  Commands: create, do, done, triage, plan, graph, ...    │
└───────────────────────┬─────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│                  Event Engine                             │
│                                                          │
│  ┌──────────────┐    ┌──────────────┐                    │
│  │  CRDT State   │◄──│  Event Log   │                    │
│  │  (in-memory)  │   │  (.jsonl)    │                    │
│  └──────┬───────┘    └──────┬───────┘                    │
│         │                   │                            │
│         ▼                   ▼                            │
│  ┌──────────────┐    ┌──────────────┐                    │
│  │  SQLite       │    │  Git Sync    │                    │
│  │  (projection) │    │  (append)    │                    │
│  └──────────────┘    └──────────────┘                    │
│                                                          │
│  ┌──────────────────────────────────────────────────┐    │
│  │  Triage Engine (PageRank, Critical Path, HITS)    │    │
│  └──────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘
```

### Data Flow

1. Every mutation (create, update, close, add-dep, comment) produces an **immutable JSON event** appended to `.bones/events.jsonl`.
2. Events are **replayed** into CRDT state, which is **projected** into a local SQLite database for indexing.
3. On `git pull`, new remote events are appended. The CRDT guarantees convergence regardless of event ordering.
4. SQLite is fully disposable — `bn rebuild` reconstructs it from the event log in milliseconds.

---

## The Event Model

Every event is a self-contained, content-addressed JSON object:

```json
{
  "id": "evt_a7f3e2b1c9d4",
  "ts": "2026-02-15T14:30:00.000Z",
  "actor": "agent:claude-abc",
  "type": "item.create",
  "item_id": "bn-a3f8",
  "data": {
    "title": "Implement retry logic for database connections",
    "kind": "task",
    "effort": "medium",
    "labels": ["backend", "reliability"]
  },
  "causation": "bn-e2c1",
  "clock": { "agent:claude-abc": 42, "agent:gemini-xyz": 17 }
}
```

### Key Fields

| Field | Purpose |
|-------|---------|
| `id` | Content hash of (ts + actor + type + data). Globally unique, dedup-safe. |
| `ts` | Wall-clock timestamp. Used for display, not ordering. |
| `actor` | Who produced this event. Human, agent name, or system. |
| `type` | Event type (see catalog below). |
| `item_id` | The work item this event affects. |
| `data` | Type-specific payload. |
| `causation` | Optional: the item ID that led to discovering this work. |
| `clock` | Hybrid logical clock / vector clock for causal ordering. |

### Event Type Catalog

```
item.create          — Create a new work item
item.update          — Update fields (title, description, effort, labels, etc.)
item.move            — Transition to a new state (open → doing → done → archived)
item.assign          — Assign/unassign an actor
item.comment         — Add a comment or note
item.link            — Add a dependency or relationship
item.unlink          — Remove a dependency or relationship
item.delete          — Soft-delete (tombstone)
item.compact         — Replace description with summary (memory decay)
```

Events are append-only. There is no "update event" — you can never modify a past event. "Corrections" are new events with later timestamps.

---

## The CRDT Layer

This is where Bones diverges most sharply from beads. Every mutable field on a work item uses a specific CRDT type:

### Field-Level CRDTs

| Field | CRDT Type | Merge Semantics |
|-------|-----------|-----------------|
| `title` | LWW Register | Last-writer-wins by vector clock |
| `description` | LWW Register | Last-writer-wins by vector clock |
| `kind` | LWW Register | Last-writer-wins by vector clock |
| `state` | LWW Register with state machine validation | Last-writer-wins, invalid transitions rejected |
| `effort` | LWW Register | Last-writer-wins by vector clock |
| `assignees` | OR-Set (Observed-Remove Set) | Add/remove converge without conflicts |
| `labels` | OR-Set | Add/remove converge without conflicts |
| `blocked_by` | OR-Set of item IDs | Add/remove converge without conflicts |
| `related_to` | OR-Set of item IDs | Add/remove converge without conflicts |
| `parent` | LWW Register (nullable) | Last-writer-wins by vector clock |
| `comments` | G-Set (Grow-only Set) | Comments are never deleted, only appended |

### Why This Matters

Consider the scenario that breaks beads today:

> Agent A on branch `feature-auth` creates issue `bd-10` and adds label "backend".
> Agent B on branch `feature-payments` creates issue `bd-10` and adds label "frontend".
> Git merge → conflict on the same JSONL line.

With Bones:

> Agent A emits `{type: "item.create", item_id: "bn-a3f8", ...}` with label "backend".
> Agent B emits `{type: "item.create", item_id: "bn-c7d2", ...}` with label "frontend".
> (Different content → different hash → different item IDs.)
>
> Even if they *both* update the *same* item:
> Agent A emits `{type: "item.update", item_id: "bn-x1y2", data: {labels: {add: ["backend"]}}}`
> Agent B emits `{type: "item.update", item_id: "bn-x1y2", data: {labels: {add: ["frontend"]}}}`
>
> The OR-Set CRDT merges both: labels = {"backend", "frontend"}. No conflict. No merge driver.

### Vector Clocks for Causal Ordering

Each actor maintains a logical clock counter. Events carry the full vector clock state at emission time. This allows Bones to establish **causal ordering** (event A happened-before event B) even when wall-clock timestamps disagree due to clock skew.

For LWW fields, ties are broken by: (1) vector clock dominance, then (2) wall-clock timestamp, then (3) deterministic hash ordering. This makes convergence fully deterministic.

---

## The Simplified Work Item Model

### Asana-like, Not Jira-like

Beads has `bug | feature | task | epic | chore` types and `open | in_progress | blocked | deferred | needs_review | verified | wontfix | closed` statuses. This is too many concepts for agents to reason about well.

Bones has **two dimensions** that matter:

#### Kind (what it is)

| Kind | When to Use |
|------|-------------|
| `task` | Default. A discrete unit of work. |
| `goal` | A collection of tasks working toward an outcome (replaces "epic"). |
| `bug` | Something broken that needs fixing. |
| `note` | An observation, question, or decision record. Not actionable work. |

#### State (where it is)

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
| `doing` | Someone (human or agent) is actively working on it. |
| `done` | Completed. Still visible, still queryable. |
| `archived` | Out of sight. Memory-decayed or irrelevant. |

That's it. Four states. No "blocked" state — blocking is a *relationship*, not a state. An item in `open` that has unresolved `blocked_by` links simply won't appear in `bn next`. No "deferred" — just remove it from the active view with labels or let it age. No "wontfix" — close it and say why in a comment.

#### Effort (how big)

Instead of numeric priority (P0-P4), which conflates urgency with size, Bones separates them:

| Effort | T-shirt Size |
|--------|-------------|
| `tiny` | < 30 minutes. Quick fix, typo, config change. |
| `small` | 30 min – 2 hours. Well-scoped task. |
| `medium` | 2 – 8 hours. A meaningful chunk of work. |
| `large` | 1 – 3 days. Needs breakdown into smaller items. |

**Priority is computed, not assigned.** Bones computes priority from the dependency graph: items that block the most downstream work, have the highest PageRank, or sit on the critical path are automatically surfaced first. An agent doesn't need to guess that something is "P0" — the graph tells it.

This is the key insight from beads_viewer, promoted to a first-class design principle: **let the structure of work determine what matters, not human/agent guesswork about priority numbers.**

#### Optional: Urgency Override

Sometimes a human needs to say "do this NOW regardless of what the graph says." Bones supports an optional `urgent` flag (a boolean, not a scale) that overrides graph-computed priority. Urgent items always sort first.

---

## Built-In Triage Engine

### The Nine Metrics (from beads_viewer)

Bones computes these on every `bn triage` or `bn next` call, with the same two-phase architecture beads_viewer uses:

**Phase 1 (synchronous, < 20ms):**
- Degree centrality (in/out)
- Topological sort
- Graph density

**Phase 2 (async with caching):**
- PageRank
- Betweenness centrality
- HITS (hubs/authorities)
- Eigenvector centrality
- Critical path analysis
- Cycle detection

Results are cached with a content hash of the event log. Cache invalidates only when new events arrive.

### The Triage Command

```bash
# The mega-command: what should I do next?
bn triage --json

# Output:
{
  "next": [
    {
      "id": "bn-a3f8",
      "title": "Set up database schema",
      "why": "Critical path keystone. Unblocks 7 downstream items.",
      "scores": {
        "pagerank": 0.15,
        "betweenness": 0.42,
        "critical_path_depth": 8
      }
    }
  ],
  "quick_wins": [...],
  "bottlenecks": [...],
  "cycles": [...],
  "health": {
    "density": 0.04,
    "items_open": 23,
    "items_blocked": 5,
    "items_ready": 12
  }
}

# Just the single next pick:
bn next
# → bn-a3f8: Set up database schema (unblocks 7)

# Parallel execution plan for agent swarms:
bn plan --json
# → Returns parallel tracks that can be worked simultaneously
```

### Feedback Loop

Like beads_viewer's feedback system, agents can record what they actually worked on:

```bash
bn did bn-a3f8     # "I worked on this"
bn skip bn-c7d2    # "I skipped this recommendation"
```

This feedback adjusts triage weights over time, learning the project's actual priorities.

---

## Dependency Model

### Two Kinds of Links (Not Four)

Beads has four dependency types: `blocks`, `related`, `parent-child`, `discovered-from`. Bones simplifies to two:

| Link | Meaning | Affects Triage? |
|------|---------|-----------------|
| `blocks` | A must complete before B can start. | Yes. Blocking links drive the critical path, PageRank, and ready-work detection. |
| `relates` | A and B are connected context. | No. Informational only. Visible in item details but doesn't affect scheduling. |

**Parent-child** is expressed through the `parent` field on an item, not as a separate link type. A goal has children; children have `parent: "bn-a3f8"`. This is cleaner than a dependency link because it's a structural relationship, not a scheduling one.

**Discovered-from** is captured as the `causation` field on the *event*, not as a link. When Agent A is working on item X and discovers a new issue, the `item.create` event records `causation: "bn-x1y2"`. This preserves the audit trail without polluting the dependency graph.

---

## Git Integration

### The Event Log as Git-Native Format

`.bones/events.jsonl` is an append-only file. Each line is one event. This is the **only** file that needs to be committed to git.

Because it's append-only:
- Concurrent appends to different lines never conflict.
- Git's line-based merge handles concurrent appends naturally.
- Even if two agents append to the exact same position, git can auto-merge (both lines are retained).

The only case that *could* conflict is if two agents append at the exact same byte offset in the same commit — but since each event is a unique JSON line with a unique content hash, git's merge will accept both.

### What Lives in `.bones/`

```
.bones/
├── events.jsonl       # Committed to git. Append-only event log.
├── bones.db           # Gitignored. Disposable SQLite projection.
├── config.yaml        # Committed. Project-level configuration.
├── feedback.jsonl     # Gitignored. Local triage feedback.
└── cache/
    └── triage.json    # Gitignored. Cached graph metrics.
```

### Sync is Trivial

```bash
# Pull remote events
git pull

# Bones detects new events, replays them into SQLite
bn list  # Automatically up-to-date

# Create local work
bn create "Fix the auth bug"  # Appends event to events.jsonl

# Push
git add .bones/events.jsonl
git commit -m "bones: new issues"
git push
```

No daemon. No debounce timers. No worktree tricks. No merge driver configuration. Append, commit, push.

---

## CLI Design

### Commands

```bash
# Creating work
bn create "Title"                    # Create a task (default)
bn create "Title" --kind goal        # Create a goal
bn create "Title" --kind bug         # Create a bug
bn create "Title" -e small           # Set effort
bn create "Title" --parent bn-a3f8   # Create as child of a goal
bn create "Title" --blocks bn-c7d2   # Create with a blocking link

# Viewing work
bn list                              # All open items
bn list --state doing                # Filter by state
bn list --label backend              # Filter by label
bn show bn-a3f8                      # Full details
bn graph bn-a3f8                     # ASCII dependency tree

# Doing work
bn do bn-a3f8                        # Move to "doing"
bn done bn-a3f8                      # Move to "done"
bn done bn-a3f8 --reason "Shipped"   # Move to "done" with closing note

# Triage (the star of the show)
bn next                              # What should I do right now?
bn triage                            # Full triage report
bn plan                              # Parallel execution plan
bn health                            # Project health dashboard
bn cycles                            # Show dependency cycles

# Dependencies
bn link bn-a3f8 --blocks bn-c7d2    # A blocks B
bn link bn-a3f8 --relates bn-c7d2   # A relates to B
bn unlink bn-a3f8 bn-c7d2           # Remove link

# Labels
bn tag bn-a3f8 backend security     # Add labels
bn untag bn-a3f8 security           # Remove label

# Maintenance
bn rebuild                           # Rebuild SQLite from events
bn compact                           # Memory decay for old done items
bn stats                             # Event log statistics

# Every command supports --json for agents
bn next --json
bn triage --json
bn plan --json
```

### The Verbs are Human

Notice the verb choices: `do`, `done`, `next`, `tag`. Not `update --status in_progress`, not `close --reason`. Bones optimizes for the 80% case. The common operations should be single words.

---

## Implementation Language: Rust

Bones should be written in Rust for the same reasons beads_rust exists:

- **Small binary** (~5-8 MB vs ~30 MB for Go).
- **No GC pauses** — important for the triage engine computing graph metrics.
- **Strong type system** — the CRDT implementations benefit enormously from Rust's type system and ownership model.
- **Single static binary** — no runtime dependencies, trivial installation.
- **Ecosystem** — excellent CRDT libraries (e.g., `crdts`, `automerge`), great SQLite bindings (`rusqlite`), fast JSON parsing (`serde_json`).

The triage engine's graph algorithms (PageRank, betweenness centrality, etc.) can leverage the `petgraph` crate, which is production-grade and battle-tested.

---

## Advanced Ideas

### 1. Deterministic Simulation Testing

Given your interest in TigerBeetle-style deterministic simulation: the event-sourced architecture is *perfect* for this. You can:

- Record a sequence of events from multiple agents.
- Replay them in every possible interleaving.
- Assert that the CRDT state converges identically regardless of order.
- Inject network partitions, clock skew, and concurrent mutations.

This would be a genuinely novel testing approach for an issue tracker and would make the CRDT correctness provably bulletproof.

### 2. Event Log as Universal Connector

Because events.jsonl is the source of truth, you can project it into *anything*:

- SQLite (for local queries)
- A web dashboard (by tailing the log)
- GitHub Issues (by mapping event types to API calls)
- Linear or Asana (same idea)
- A Mermaid diagram (by reading the dependency graph)

This is the event sourcing / CQRS pattern. The write side (events) is completely decoupled from the read side (projections).

### 3. Multi-Repo Bones

Since events are content-addressed and carry full actor/clock metadata, you can merge event logs from multiple repositories into a unified view. A `bones-meta` repo could aggregate events across your entire project portfolio, with the triage engine computing cross-repo critical paths.

### 4. AI-Native Event Compression

The `item.compact` event type enables the same memory decay that beads has, but with a twist: because the original events are preserved, you can always "un-compact" by replaying the full history. The compacted summary is just another event that tells the projection layer "use this instead of replaying the full chain."

### 5. Causal Breadcrumbs

The `causation` field on events creates a discoverable trail: "Why does this item exist?" An agent can follow the causation chain to understand the genealogy of any piece of work. This is beads's `discovered-from` dependency type, but without polluting the dependency graph.

### 6. MCP Server Built-In

Rather than a separate integration (like beads-mcp), Bones could ship with `bn mcp` that starts an MCP server exposing the full API. This makes Bones instantly usable from any MCP-aware agent without CLI shelling.

---

## What Bones Drops (Intentionally)

| Beads Feature | Bones Position |
|---------------|----------------|
| Background daemon | Not needed. Append-only log + rebuild is fast enough. |
| Auto-commit to git | Explicit is better. Agents commit when ready. |
| Dolt backend | Unnecessary. CRDTs handle versioning at the data level. |
| Protected branch worktrees | Not needed. Append-only events don't conflict. |
| Sequential ID migration | Hash IDs from day one. No migration path needed. |
| 8 status values | 4 states: open, doing, done, archived. |
| 5 priority levels | Computed from graph. Optional `urgent` boolean for overrides. |
| Git hooks | Optional. The system works without them. |
| Merge driver | Not needed. CRDTs make merge conflicts structurally impossible. |
| Templates | Keep it simple. `bn create` with flags is enough. |

---

## Summary: The Bones Philosophy

1. **Events are truth.** Everything else is a projection.
2. **CRDTs over coordination.** Never ask "who wins?" — everyone does.
3. **The graph knows priority.** Don't ask humans to guess; compute it.
4. **Simple verbs, powerful engine.** `bn next` hides PageRank, HITS, and critical path analysis behind a single word.
5. **Agents first, humans welcome.** Every command has `--json`. The triage engine is the API.
6. **Git-native, not git-dependent.** The event log works with git but doesn't require daemons, hooks, or merge drivers to function correctly.

Bones takes the best ideas from beads (git-backed persistence, dependency graphs, agent-first design), the best ideas from beads_viewer (graph-theoretic triage, robot protocol, cognitive offloading), and the best ideas from beads_rust (minimal footprint, no surprises, explicit operations) — and unifies them on a foundation that makes distributed collaboration structurally sound instead of operationally fragile.
