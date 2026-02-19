# ADR-004: ITC Choice

## Status
Accepted

## Context
Need causal ordering for multi-agent CRDT operations in a decentralized system where agents can fork and join at any time without a central registry.

## Decision
Use Interval Tree Clocks (ITC) for logical time and causality tracking.

### Rationale
- **Dynamic Membership**: No need to know the number of agents upfront.
- **O(1) Fork/Join**: Efficiently handles agent spawning and workspace merging.
- **Compact Representation**: Small size that only grows with causal complexity.
- **Total Ordering**: Provides stable tie-breaking for concurrent operations when combined with wall time and agent IDs.

### Implementation
- Use a custom or embedded ITC library (e.g., `terseid` or a dedicated crate).
- Serialize ITC stamps as compact strings in the TSJSON event log.

## Alternatives Considered

### Vector Clocks
- Pros: Simple and well-understood.
- Cons: Size grows linearly with the number of agents.
- Rejected because: Managing the agent registry and the unbounded growth of the clock array is not suitable for decentralized bones deployments.

### Hybrid Logical Clocks (HLC)
- Pros: Close to wall time, compact.
- Cons: Only provides partial ordering. Does not capture full causality (fork/join) as explicitly as ITC.
- Rejected because: Causal precision is required for our Eg-Walker DAG replay model.

## Consequences
- ITC library must be embedded or referenced as a stable dependency.
- Serialization format must remain stable for backward compatibility.
- Agents must correctly fork and join their ITC stamps when creating and merging workspaces.

## References
- Related beads: bn-3rr.1, bn-2jr
- Related ADRs: ADR-005 (DAG), ADR-006 (LWW)
