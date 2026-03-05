# Fuzz Targets

This directory contains `cargo-fuzz` harnesses for high-risk bones primitives.

Targets:
- `parse_line` — TSJSON parser robustness.
- `replay_state` — CRDT replay/state transitions under arbitrary event streams.
- `project_event` — SQLite projection path resilience for parsed events.

Run locally:

```bash
cargo install cargo-fuzz
cargo fuzz run parse_line -- -max_total_time=30
cargo fuzz run replay_state -- -max_total_time=30
cargo fuzz run project_event -- -max_total_time=30
```
