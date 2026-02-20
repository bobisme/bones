# Contributor Guide

This guide is for contributors adding functionality to bones.

## Development Setup

### Requirements

- Rust toolchain (stable) with `cargo`
- `just` (optional helper tasks)
- SQLite available locally (for projection-related work)

### Helpful environment variables

- `AGENT` or `BONES_AGENT`: identity used for event attribution in local flows
- `RUST_LOG`: tracing verbosity, e.g. `RUST_LOG=info`

### Common commands

```bash
cargo build
cargo test
cargo fmt --all
cargo clippy --all-targets --all-features
```

## Event Merge Conflict Workaround (maw + jj)

Until maw supports union-style auto resolution for `.bones/events` (`bd-17vr`), resolve event-file conflicts with the bones merge tool:

```bash
bn merge-tool --setup
jj resolve --tool bones
```

Notes:
- `.beads/**` can still use take-main auto resolution.
- Do **not** restore `.bones/events` from main; that can discard local events.

## How to Add a New Event Type

1. Add/extend the event type definition in `bones-core`.
2. Add parse + write support for the TSJSON/event-log format.
3. Update CRDT/state transition handling for the new event.
4. Update projection logic so derived views include the new behavior.
5. Add tests:
   - parser/writer round-trip test
   - state transition test
   - projection regression test (if applicable)

### Testing strategy

- Unit tests: parser, validation, and state transitions
- Property tests (where relevant): monotonicity/idempotence semantics
- Integration tests: replay event log and assert projected state

## How to Add a New CLI Command

1. Create a command module in `crates/bones-cli/src/cmd/` (or extend existing structure).
2. Define clap args/options and user-facing help text.
3. Wire the subcommand into the CLI dispatch in `main.rs` / command registry.
4. Keep output contract explicit (`--format pretty|text|json`; hidden `--json` alias for compatibility).
5. Add tests for happy path and user-error path.

### Command quality checklist

- Clear one-line summary and examples
- Stable exit codes and actionable error messages
- Deterministic output for scripting

## How to Add a New Metric

Metrics can live in triage/search crates depending on scope.

1. Implement metric computation in the relevant crate (`bones-triage` or `bones-search`).
2. Wire into composite scoring/ranking pipeline.
3. Add regression tests with small hand-verified fixtures.
4. Document tradeoffs and thresholds in code comments or ADRs when behavior is non-obvious.

## Conventions and Style

- Prefer small, composable modules and pure functions where practical.
- Keep interfaces explicit and avoid hidden global state.
- Maintain backward compatibility for persisted/on-disk formats.
- For risky behavioral changes, add an ADR under `docs/adr/`.

## First Task Suggestions for New Contributors

- Improve CLI help text and examples.
- Add parser/writer test coverage for edge cases.
- Add docs for a missing command workflow.
