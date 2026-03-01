# bones-core

Core data structures, CRDT event model, locking, error types, and projection engine for the [bones](https://github.com/bobisme/bones) issue tracker.

## What this crate provides

- **Event model**: immutable append-only events with ITC vector clocks and deterministic hash addressing
- **CRDT projection**: replay events into a SQLite projection database with last-writer-wins tie-breaking
- **Item model**: bones (tasks, goals, bugs) with state, urgency, labels, parents, and dependencies
- **FTS5 search**: BM25 full-text index built into the projection
- **Locking**: file-based advisory locks for concurrent access safety
- **Error types**: structured error hierarchy with machine-readable codes and hints
- **Config**: per-project and per-user configuration loading

## Usage

This crate is an internal dependency of [`bones-cli`](https://crates.io/crates/bones-cli). It is not intended as a standalone library, but the API is public for tooling built on top of bones.

See the [bones repository](https://github.com/bobisme/bones) for the full project.
