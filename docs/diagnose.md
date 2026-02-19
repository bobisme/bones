# `bn diagnose`

`bn diagnose` summarizes repository health from two perspectives:

1. **Raw append-only event log** (`.bones/events/*.events`)
2. **SQLite projection state** (`.bones/bones.db`)

Use human output for quick debugging:

```bash
bn diagnose
```

Use JSON for automation:

```bash
bn diagnose --json
```

## Stable JSON schema

Top-level object:

- `generated_at_us` (integer)
- `shard_inventory` (object)
- `event_stats` (object)
- `integrity` (object)
- `projection` (object)
- `remediation_hints` (array of strings)

### `shard_inventory`

- `shard_count` (integer)
- `total_bytes` (integer)
- `shards` (array)
  - `shard_name` (string)
  - `path` (string)
  - `byte_size` (integer)
  - `event_count` (integer)
  - `parse_error_count` (integer)
  - `time_range.earliest_wall_ts_us` (integer|null)
  - `time_range.latest_wall_ts_us` (integer|null)
  - `read_error` (string|null)

### `event_stats`

- `total_events` (integer)
- `unique_event_hashes` (integer)
- `duplicate_event_hashes` (integer)
- `unique_items` (integer)
- `events_by_type` (object<string, integer>)
- `events_by_agent` (object<string, integer>)
- `time_range.earliest_wall_ts_us` (integer|null)
- `time_range.latest_wall_ts_us` (integer|null)

### `integrity`

- `parse_error_count` (integer)
- `parse_error_samples` (array)
  - `shard_name` (string)
  - `line_number` (integer)
  - `error` (string)
- `hash_anomalies` (object)
  - `invalid_event_hash_lines` (integer)
  - `hash_mismatch_lines` (integer)
  - `invalid_parent_hash_lines` (integer)
  - `unknown_parent_refs` (integer)
  - `unknown_parent_samples` (array)
    - `event_hash` (string)
    - `item_id` (string)
    - `missing_parent_hash` (string)
- `orphan_events` (object)
  - `orphan_event_count` (integer)
  - `orphan_item_count` (integer)
  - `orphan_item_samples` (array<string>)
- `warnings` (array<string>)

### `projection`

- `status` (string)
- `db_path` (string)
- `expected_offset` (integer)
- `expected_last_hash` (string|null)
- `cursor_offset` (integer|null)
- `cursor_hash` (string|null)
- `cursor_offset_matches_log` (bool|null)
- `cursor_hash_matches_log` (bool|null)
- `projected_events_table_present` (bool)
- `projected_event_count` (integer|null)
- `projected_events_match_log` (bool|null)
- `item_count` (integer|null)
- `placeholder_item_count` (integer|null)
- `incremental_safety_error` (string|null)
- `drift_indicators` (array<string>)

## Notes

- Diagnostics intentionally continue through malformed lines so the command is
  still useful on degraded repositories.
- Non-empty `remediation_hints` always provides next-step guidance.
