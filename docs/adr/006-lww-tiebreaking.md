# ADR-006: LWW Tie-Breaking

## Status
Accepted

## Context
Last-Write-Wins (LWW) registers need a total order to resolve concurrent writes consistently across all replicas.

## Decision
Use a 4-step lexicographic tie-breaking chain for all LWW-based state resolutions.

### Tie-Breaking Order
1. **ITC Dominance**: If one event is a causal descendant of another (determined by ITC), the descendant wins.
2. **Wall Timestamp (`wall_ts`)**: If causality is ambiguous (concurrent writes), the event with the higher RFC3339 timestamp wins.
3. **Agent ID (`agent_id`)**: If wall timestamps are identical (rare), the event with the lexicographically higher agent ID wins.
4. **Event Hash (`event_hash`)**: If agent IDs are also identical (extremely rare, possibly same-agent clock skew), the lexicographical higher event hash wins.

### Benefits
- **Determinism**: Every replica will resolve the same set of concurrent events to the same winner.
- **Stability**: Wall time and agent IDs provide reasonable human intuition for conflict resolution.
- **Robustness**: The 4-step chain ensures a total order even in pathological cases.

## Alternatives Considered

### Wall Clock Only
- Pros: Simple to understand.
- Cons: Clock skew across machines makes this unreliable as the sole tie-breaker.
- Rejected because: Concurrent writes are common in decentralized systems, and wall clock alone is not enough for consistency.

### Random Tie-Break
- Pros: Simple.
- Cons: Non-deterministic unless the random seed is synchronized across replicas.
- Rejected because: Reproducibility is a core requirement for bones.

## Consequences
- All replicas must implement the identical 4-step comparison logic.
- ITC, wall timestamps, and agent IDs must be consistently formatted in the event log to ensure lexicographical comparison works as expected.
- Sorting concurrent events becomes slightly more complex but remains O(N log N).

## References
- Related beads: bn-3rr.1, bn-2jr
- Related ADRs: ADR-004 (ITC), ADR-005 (DAG)
