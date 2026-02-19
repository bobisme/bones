# ADR-005: Eg-Walker DAG Replay

## Status
Accepted

## Context
Standard CRDTs (like OR-Sets) often use per-element metadata (tombstones) to track additions and removals. This metadata grows unbounded even for static set sizes if churn is high.

## Decision
Use Eg-Walker DAG replay for computing final set state from the event log.

### Mechanism
- All events are nodes in a Directed Acyclic Graph (DAG).
- Each event explicitly references its parent event IDs (captured via ITC fork/join).
- The state of an item (e.g., its status or priority) is computed by traversing the DAG from the last common ancestor (LCA) to the leaves.
- Merging two workspaces involves merging their event DAGs and re-evaluating the set state.

### Benefits
- **Bounded Metadata**: No need to store per-element tombstones in the items themselves.
- **Traceability**: The full history of how an item reached its state is preserved in the event log.
- **Idempotency**: Replaying the same event log always results in the same state.

## Alternatives Considered

### Classical OR-Set with Tombstones
- Pros: Well-documented, simple to implement for a single set.
- Cons: Unbounded metadata growth. Removing an item leaves a permanent tombstone that must be synced and stored forever.
- Rejected because: Managing metadata growth (e.g., via garbage collection) is complex and adds overhead in a decentralized system.

### Centralized SQL Model
- Pros: Simple queries, standard database features.
- Cons: Not suitable for offline-first, decentralized synchronization.
- Rejected because: `bones` is designed to work in disconnected environments without a central server.

## Consequences
- Replay requires DAG traversal from the LCA.
- Replay performance is critical; implementation must be optimized and potentially use snapshots (checkpoints) to avoid replaying the entire history.
- The event DAG must be kept consistent during git merges (using custom merge drivers).

## References
- Related beads: bn-3rr.1, bn-2jr
- Related ADRs: ADR-004 (ITC), ADR-006 (LWW)
