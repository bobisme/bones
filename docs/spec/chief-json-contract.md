# Chief-Facing JSON Contract

This document defines the JSON surfaces intended for orchestration tools such
as `chief`. These contracts are versioned independently from the event-log
format documented in `docs/spec/compatibility.md`.

## Schema Version Policy

Chief-facing JSON payloads use a top-level `schema_version` integer. The
current version is `1`.

Compatible changes do not increment the version:

- Adding optional fields.
- Adding enum values that older consumers can ignore.
- Adding advisory metadata under `provenance`.

Breaking changes require a version increment:

- Removing or renaming fields documented here.
- Changing a field's type or semantic meaning.
- Changing lifecycle/status filtering behavior.

Consumers should accept the current version and the previous version, warn on
newer versions, and reject older unsupported versions for mutating commands.

## Context Snapshot

`bn context --format json` emits a single provider snapshot for hot-path
orchestrator use:

```json
{
  "schema_version": 1,
  "generated_at": "2026-04-24T00:00:00+00:00",
  "provider": "bones",
  "command": "bn context --format json",
  "summary": {
    "open_count": 12,
    "doing_count": 1,
    "blocked_count": 2,
    "stale_count": 1
  },
  "recommended_next": {
    "id": "bn-abc",
    "title": "Ship migration scaffold",
    "kind": "task",
    "state": "open",
    "urgency": "urgent",
    "score": 1.25,
    "why": ["Driven by critical-path and pagerank."]
  },
  "blocked": [
    {
      "id": "bn-def",
      "title": "Decide provider schema",
      "kind": "task",
      "state": "open",
      "urgency": "default",
      "blocked_by": ["bn-123"]
    }
  ],
  "active_goals": [
    {
      "id": "bn-goal",
      "title": "Chief provider transition",
      "state": "open",
      "urgency": "default"
    }
  ],
  "provenance": {
    "provider": "bones",
    "command": "bn context --format json",
    "generated_at": "2026-04-24T00:00:00+00:00",
    "projection_schema_version": 2,
    "projection_last_event_offset": 12345,
    "projection_last_event_hash": "blake3:...",
    "projection_last_rebuild_at_us": 1770000000000000,
    "event_log_bytes": 12345
  }
}
```

`blocked_count` and `blocked` are derived from unresolved dependency edges and
inherited blocked goal state. `blocked_by` lists direct active blockers when
present.

## Write Responses

Mutation commands include `schema_version: 1` in JSON mode.

`bn create --title <title> --kind task --format json` returns a single item:

```json
{
  "schema_version": 1,
  "id": "bn-rx94",
  "title": "Plan chief provider transition",
  "kind": "task",
  "state": "open",
  "previous_state": null,
  "urgency": "default",
  "agent": "alice",
  "event_hash": "blake3:..."
}
```

`bn do <id> --format json` and `bn done <id> --format json` return batch
wrappers, even for one ID:

```json
{
  "schema_version": 1,
  "results": [
    {
      "id": "bn-rx94",
      "ok": true,
      "previous_state": "open",
      "state": "doing",
      "new_state": "doing",
      "event_hash": "blake3:..."
    }
  ]
}
```

`state` is the current state alias for chief. `new_state` is retained for
existing callers.

`bn bone comment add <id> <text> --format json` returns:

```json
{
  "schema_version": 1,
  "ok": true,
  "id": "bn-rx94",
  "item_id": "bn-rx94",
  "agent": "alice",
  "body": "progress note",
  "ts": 1770000000000000,
  "event_hash": "blake3:..."
}
```

`bn bone move <child> --parent <goal> --format json` returns:

```json
{
  "schema_version": 1,
  "ok": true,
  "id": "bn-rx94",
  "item_id": "bn-rx94",
  "previous_parent_id": null,
  "parent_id": "bn-goal"
}
```

## Status Filtering

`bn list` supports lifecycle states and the virtual `blocked` status through
either `--state` or its alias `--status`.

Supported values:

- `open`
- `doing`
- `done`
- `archived`
- `blocked`

The flag may be repeated or comma-separated:

```bash
bn list --format json --status open --status doing --status blocked
bn list --format json --status open,doing,blocked
bn list --format json --state doing
```

Multiple statuses use OR semantics. `blocked` does not rewrite the item's
lifecycle state in output; blocked items still report their real `state`.

`bn context --format json` always returns the chief-oriented active context:
open/doing counts, blocked work, stale in-progress work, recommended next, and
active goals. It intentionally has no filtering flags.
