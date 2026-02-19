# ADR-002: Event File Sharding Strategy

## Status
Accepted

## Context
Event files store the append-only TSJSON event log. As projects grow, a single file
becomes unwieldy for git (large diffs, merge conflicts). The sharding strategy determines
how events are split across files and how they are discovered during replay.

## Decision
Use time-based monthly sharding: `.bones/events/YYYY-MM.events`.

### File Naming
- Active shard: `.bones/events/YYYY-MM.events` (current month)
- Symlink: `.bones/events/current.events` -> `YYYY-MM.events` (latest active shard)
- Example: `.bones/events/2026-02.events`, `.bones/events/2026-03.events`

### Rotation Policy
- New shard created on first event of each calendar month (UTC)
- Previous month's shard becomes frozen (never modified after rotation)
- Frozen shards are immutable -- any modification is a corruption signal

### Replay Ordering
1. Discover all `.events` files in `.bones/events/`
2. Sort by filename (lexicographic = chronological for YYYY-MM format)
3. Within each shard, events are ordered by line position (append order)
4. Cross-shard ordering: earlier shard < later shard
5. For concurrent events (different agents, same timeframe): ITC provides causal ordering

### Shard Manifests
Each frozen shard has a companion `.manifest` file:
- `.bones/events/2026-01.manifest` for `.bones/events/2026-01.events`
- Contains: event count, hash, first/last timestamp, agent list
- Used by `bn verify` to detect corruption without parsing full shard

## Alternatives Considered

### Agent-Sharded (one file per agent)
- Pros: no merge conflicts (each agent owns their file)
- Cons: replay requires interleaving across files by timestamp,
  hot-file contention gone but discovery complexity increases
- Rejected: increases replay complexity, does not match git's line-based merge model

### Size-Based Rotation (rotate at 1MB)
- Pros: predictable file sizes
- Cons: unpredictable shard boundaries, harder to reason about time ranges,
  race condition if two agents hit threshold simultaneously
- Rejected: unpredictable naming makes discovery harder

### Single File
- Pros: simple
- Cons: grows unbounded, terrible for git (every commit touches same file),
  merge conflicts guaranteed with multiple agents
- Rejected: does not scale

## Consequences
- Monthly rotation means at most ~30 days of events per shard
- Git diffs only touch the current month's shard (older shards frozen)
- Merge conflicts limited to current shard (append-only helps git merge)
- Recovery can rebuild from any subset of shards (replay is idempotent)

## References
- Related beads: bn-1t8, bn-x2e, bn-8hi, bn-26o, bn-3rr.1
- Related ADRs: ADR-001
