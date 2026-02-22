# ADR-007: ITC Sparse Base64 Text Encoding

## Status
Accepted

## Context

ITC stamps are stored on every event line in the TSJSON log. The previous text format
(`itc:v1:<hex>`) hex-encoded the compact binary stamp. In real repositories this was a
significant fraction of event-log size.

Measured on current project data:

- `ws/default/.bones/events`: 3608 events, ITC text avg `190.5` chars/event.
- `~/repos/mcp_agent_mail_rust/.bones/events`: 9344 events, ITC text avg `190.8` chars/event.

Compression estimates on those corpora:

- Base64-only over compact bytes: ~`31.9%` reduction.
- Sparse event-value encoding + base64: ~`82%` reduction.

The project is pre-general-availability and can take an immediate format upgrade without
a staged rollout.

## Decision

Use sparse base64 ITC text encoding as the default write format.

- New text prefix: `itc:v3:`
- Legacy read support remains for `itc:v1:` values.
- New writes always emit `v3`.

### `itc:v3` payload (base64url, no padding)

After base64url decode, payload bytes are:

1. `wire_version` (`u8`, currently `1`)
2. `id_bit_len` (varint)
3. `id_bits` (packed bits, `ceil(id_bit_len/8)` bytes)
4. `event_bit_len` (varint)
5. `event_bits` (packed bits, `ceil(event_bit_len/8)` bytes)
6. `non_zero_count` (varint)
7. repeated `non_zero_count` entries:
   - `index_delta` (varint, first entry uses absolute index)
   - `value` (varint, must be non-zero)

Reconstruction fills all unspecified event-value positions with zero and rebuilds the
compact stamp before normal ITC decode.

### Canonical constraints

To guarantee deterministic event hashes for equivalent stamps:

- non-zero entries are emitted in strictly increasing index order,
- deltas are computed from that canonical order,
- zero values are omitted from sparse entries,
- duplicate or out-of-range indices are invalid on decode.

## Alternatives Considered

### Compact-bytes base64 only (`itc:v2`)

- Pros: small implementation delta, immediate ~32% gain.
- Cons: leaves most size savings unrealized because event-value streams are mostly zeros.
- Rejected because sparse representation yields materially better reduction on observed data.

### Keep `itc:v1` hex

- Pros: zero implementation risk.
- Cons: persistent log bloat on every event.
- Rejected because storage overhead is avoidable.

### Omit ITC from events

- Pros: maximum size reduction.
- Cons: breaks causal ordering semantics and weakens LWW tie-breaking quality.
- Rejected because causality metadata is required by design.

## Consequences

- Event logs shrink significantly in active repositories.
- Codec complexity increases (sparse packing + versioned decoding).
- `stamp_from_text` must continue to support legacy `v1` decode for existing shards.
- Test coverage must include malformed sparse payloads, canonical ordering validation,
  and byte-preserving sparse↔compact roundtrips.

## References

- Related beads: bn-v0z8
- Related ADRs: ADR-004 (ITC), ADR-006 (LWW)
