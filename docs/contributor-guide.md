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

## Kani Proofs

`crates/bones-core/src/cache/codec.rs` and `crates/bones-core/src/crdt/lww.rs` have Kani harnesses in `mod kani_proofs` (`#[cfg(kani)]`). They prove the varint, zigzag, timestamp-delta and RLE codecs, and the LWW merge order, for all inputs, or for a bound stated on each harness.

- Install once: `cargo install --locked kani-verifier && cargo kani setup`.
- Run: `just kani` (all harnesses) or `just kani varint_round_trips`.
- Always run through `just kani`. It runs one harness at a time in a systemd scope with a memory cap (`KANI_MEM`, default 12G). A bad harness once used 72 GB and took down the host.
- Keep slice lengths fixed in harnesses, and never loop over a symbolic count. Both make CBMC's memory use explode.
- Before trusting a new harness, break the code it covers and confirm the harness fails.
- To add a file with harnesses, list it in `FILES` in `scripts/kani.sh`.
- `just kani` is not part of `just check`.

## Verus Proofs

`crates/bones-verified` holds the merge decisions that `bones-core` calls: the LWW key order, the epoch/phase join and the item timestamp joins. Verus proves them correct for all inputs, with no size bound. A plain `cargo build` erases the proofs and compiles the executable code only, so building bones does not need Verus.

- Install: download the Verus release that matches the `vstd` version pinned in `crates/bones-verified/Cargo.toml` from https://github.com/verus-lang/verus/releases and unpack it to `~/.local/verus/` (or set `VERUS_DIR`).
- Run: `just verus`. It cleans the crate first, because cargo-verus prints nothing on a cache hit, and fails unless Verus reports zero errors.
- To upgrade Verus, change the `vstd` pin and the installed release together.
- Before trusting a new proof, break the code it covers and confirm `just verus` fails.
- `just verus` is not part of `just check`.

## Conventions and Style

- Prefer small, composable modules and pure functions where practical.
- Keep interfaces explicit and avoid hidden global state.
- Maintain backward compatibility for persisted/on-disk formats.
- For risky behavioral changes, add an ADR under `docs/adr/`.

## First Task Suggestions for New Contributors

- Improve CLI help text and examples.
- Add parser/writer test coverage for edge cases.
- Add docs for a missing command workflow.
