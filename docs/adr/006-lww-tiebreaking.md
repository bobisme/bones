# ADR-006: LWW Tie-Breaking

## Status
Accepted, amended 2026-09-25 (bn-t694): step 1 (ITC dominance) removed. See "Amendment" below.

## Context
Last-Write-Wins (LWW) registers need a total order to resolve concurrent writes consistently across all replicas.

## Decision
Use a 3-step lexicographic comparison of `(wall_ts, agent_id, event_hash)` for all LWW-based state resolutions.

### Tie-Breaking Order
1. **Wall Timestamp (`wall_ts`)**: The event with the higher timestamp wins.
2. **Agent ID (`agent_id`)**: If wall timestamps are identical (rare), the event with the lexicographically higher agent ID wins.
3. **Event Hash (`event_hash`)**: If agent IDs are also identical (same agent, same microsecond), the lexicographically higher event hash wins. Hashes are unique per event, so no two distinct writes tie.

This is the same order that replay (`dag/replay.rs`) and the event merge driver (`sync/merge.rs`) use to sort events, so merging registers and replaying events agree.

### Benefits
- **Determinism**: Every replica will resolve the same set of concurrent events to the same winner.
- **Stability**: Wall time and agent IDs provide reasonable human intuition for conflict resolution.
- **Robustness**: A lexicographic comparison of scalar keys is a strict total order for all inputs, including logs written under clock skew.

## Alternatives Considered

### ITC Dominance First (original decision, withdrawn)
- Pros: A causally later write always wins, even when its wall clock is behind.
- Cons: Causality is a partial order. Placing it ahead of the wall clock makes the chain non-transitive under clock skew: if `a` happens before `c` but `c.wall_ts < b.wall_ts < a.wall_ts` and `b` is concurrent with both, then `c` beats `a`, `a` beats `b`, and `b` beats `c`. Merge is then not associative, and replicas that merge in different orders diverge.
- Withdrawn because: convergence is a hard requirement, and a pairwise comparison must be a total order for any inputs, including existing skewed logs.

### Random Tie-Break
- Pros: Simple.
- Cons: Non-deterministic unless the random seed is synchronized across replicas.
- Rejected because: Reproducibility is a core requirement for bones.

## Amendment (2026-09-25, bn-t694)
The original step 1 (ITC dominance) was removed; see "ITC Dominance First" above. A property test that draws causal histories and wall clocks independently (`crates/bones-core/tests/proptest_semilattice.rs`) found the associativity violation after 24 cases.

Cost: when a machine's clock lags, a causally later edit could lose to an earlier one. Since bn-52i6 the local clock follows the hybrid-logical-clock receive rule: rebuild and incremental apply call `ShardManager::observe_timestamp` with the newest event they applied, and `next_timestamp` returns a value above it. Write commands bring the projection up to date first, so a new write orders after every event its author has seen. Observed timestamps are capped at `MAX_OBSERVED_CLOCK_LEAD_US` (one hour) ahead of the local wall clock, so one far-future clock cannot drag every replica forward. Causal order therefore holds for clock skew below one hour.

Since bn-1dy8 the ITC implementation and the register stamp are removed, and the property test draws write histories without stamps.

## Consequences
- All replicas must implement the identical 3-step comparison logic.
- Wall timestamps, agent IDs and event hashes must be consistently formatted in the event log to ensure lexicographical comparison works as expected.
- Sorting concurrent events becomes slightly more complex but remains O(N log N).

## References
- Related beads: bn-3rr.1, bn-2jr
- Related ADRs: ADR-004 (ITC), ADR-005 (DAG)
