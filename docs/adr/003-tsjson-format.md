# ADR-003: TSJSON Format

## Status
Accepted

## Context
Need an append-only event format that is human-readable, git-mergeable, and parseable without full JSON overhead for indexing and discovery.

## Decision
Use TSJSON (Tab-Separated with JSON payload) for the event log.

### Format
Each line is a single event:
`wall_ts \t agent_id \t type \t event_id \t [parent_ids...] \t payload`

- `wall_ts`: RFC3339 timestamp with nanosecond precision (UTC)
- `agent_id`: The ID of the agent that created the event
- `type`: Event type string (e.g., `item_created`, `status_updated`)
- `event_id`: Unique event ID (ITC-based)
- `parent_ids`: Comma-separated list of causal parent event IDs (for DAG)
- `payload`: JSON object containing event-specific data

### Properties
- **Tab-Separated Fixed Fields**: Allows fast parsing/grepping of metadata without full JSON decoding.
- **Append-Only**: New events are always added to the end of the current shard.
- **Git-Friendly**: Tab-separated fields and one-event-per-line minimize merge conflicts.

## Alternatives Considered

### JSONL (JSON Lines)
- Pros: Simple, standard, great library support.
- Cons: Extracting even one field (like `wall_ts` for sorting) requires full JSON parsing. Grep/awk workflows are harder.
- Rejected because: Metadata extraction overhead and poorer support for low-level shell tools.

### Protobuf / Binary
- Pros: Extremely compact, fast.
- Cons: Not human-readable, requires special tools to inspect, not git-mergeable.
- Rejected because: Transparency and git-native integration are primary design goals.

## Consequences
- Parser must handle tab-in-JSON-value escaping (standard JSON string escape `\t`).
- Fixed field count is part of the format contract.
- Implementation must ensure tabs are only used as field separators outside the payload.

## References
- Related beads: bn-3rr.1, bn-x2e
- Related ADRs: ADR-002 (sharding)
