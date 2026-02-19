# Event Format Compatibility Policy

This document defines the versioning scheme for bones event log shard files
and the backward/forward compatibility guarantees bones provides.

## Version Header

Every event shard file begins with:

```
# bones event log v<N>
```

where `N` is a positive integer representing the format version. As of this
writing, the current version is **v1**.

The second line of every shard is a field-description comment:

```
# fields: wall_ts_us \t agent \t itc \t parents \t type \t item_id \t data \t event_hash
```

The current TSJSON format (v1) has **8 tab-separated fields** per event line:

| # | Field | Description |
|---|-------|-------------|
| 1 | `wall_ts_us` | Wall-clock timestamp, microseconds since Unix epoch (i64) |
| 2 | `agent` | Agent/user identifier |
| 3 | `itc` | Interval Tree Clock stamp (canonical text encoding) |
| 4 | `parents` | Comma-separated parent hashes (`blake3:<hex>`) or empty |
| 5 | `type` | Event type (`item.<verb>`) |
| 6 | `item_id` | Target work-item ID |
| 7 | `data` | Canonical JSON payload (keys sorted, compact) |
| 8 | `event_hash` | BLAKE3 content hash (`blake3:<hex>`) |

## Backward Compatibility (new `bn` reads old events)

**All prior format versions are always readable.**

A newer version of bones will always be able to read event log files written
by older versions. Version-specific parsing logic is dispatched via the
version number returned by `detect_version()`. No data migration is required
when upgrading bones.

## Forward Compatibility (old `bn` reads new events)

When a shard file's version number is **higher** than what this build
supports, bones returns an error with actionable upgrade instructions rather
than silently misinterpreting data:

```
Event log version N is newer than this version of bones (supports up to v1).
Please upgrade bones: cargo install bones-cli
Or download the latest release from: https://github.com/bobisme/bones/releases
```

This prevents silent data corruption from partial reads of an incompatible
format.

### Unknown Event Types

Adding a **new event type** (e.g., `item.archive`) does **not** require a
version bump. An older reader encountering an unknown `item.<verb>` will:

1. Emit a `WARN`-level log message identifying the line number and event type.
2. **Skip** the line and continue parsing.
3. **Not** return an error or abort.

This means a repository written with a newer bones can be opened by an older
bones; the older reader will simply ignore event types it doesn't understand.

### Unknown JSON Fields

Adding a **new optional field** to an existing event type's JSON payload does
**not** require a version bump. The serde deserialization uses
`#[serde(flatten)]` to capture and preserve unknown fields in a
`BTreeMap<String, serde_json::Value>`. These extra fields round-trip cleanly
through `EventData` and are omitted from canonical JSON on write.

This means new optional metadata added to existing events is invisible to
older readers but is not lost.

## What Requires a Version Bump

The format version **must** be incremented when any of the following change:

| Change | Reason |
|--------|--------|
| Number or order of tab-separated fields | Old readers hard-code 8-field split |
| Hash algorithm (currently BLAKE3) | Old readers cannot verify integrity |
| Canonical JSON rules (key order, escaping) | Hash verification would fail |
| Removing a field from an existing event type | Old readers may require it |
| Changing the semantic meaning of a field | Projection replay would be wrong |
| Changing the `item_id` format | Old readers may reject valid IDs |

When a version bump is required, the new format must be implemented alongside
migration tooling (`bn migrate`) so that existing repositories can be upgraded
without data loss.

## What Does NOT Require a Version Bump

The following changes are safe to deploy without a format version bump:

| Change | Reason |
|--------|--------|
| Adding a new `item.<verb>` event type | Old readers skip unknown types |
| Adding a new optional field to an event's JSON payload | Old readers ignore unknown fields |
| Adding new comment lines (`#`) to shard headers | Comment lines are always skipped |
| Changing the ITC clock algorithm (text encoding unchanged) | Field is opaque to the parser |
| Adding new `item_id` prefixes or namespaces | Old readers pass IDs through opaquely |

## Version Detection Implementation

```rust
use bones_core::event::{CURRENT_VERSION, detect_version};

// Check a shard header line
match detect_version("# bones event log v1") {
    Ok(version) => println!("format version: {version}"),
    Err(msg) => eprintln!("cannot read shard: {msg}"),
}
```

The `detect_version` function:

1. Validates the header prefix (`# bones event log v`).
2. Parses the version number as a `u32`.
3. Returns `Ok(version)` if `version <= CURRENT_VERSION`.
4. Returns `Err(upgrade_message)` if `version > CURRENT_VERSION`.

This function is called automatically by `parse_lines()` when the first
matching comment line is encountered.

## Upgrade Path

When a format version bump is required:

1. Implement the new format in `crates/bones-core/src/event/parser.rs`,
   dispatching on the detected version.
2. Implement a `bn migrate` sub-command that rewrites v(N-1) shards to v(N).
3. Update `CURRENT_VERSION` in `parser.rs`.
4. Update the `SHARD_HEADER` constant in both `parser.rs` and `writer.rs`.
5. Document the change in this file and in the release notes.
6. Add backward compatibility tests in `crates/bones-core/tests/` confirming
   that v(N-1) shards still parse correctly after the upgrade.

## Relationship to Dependent Features

| Feature | Depends on compatibility guarantee |
|---------|------------------------------------|
| `bn-36gv` Backward compatibility tests | Version detection + `parse_lines` skipping |
| `bn-37o8` Event format migration | Version detection error → migration prompt |
| `bn-x2e` TSJSON parser/writer | Canonical field layout (v1) |
