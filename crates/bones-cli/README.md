# bones

bones is a CRDT-native issue tracker for distributed human and agent collaboration.

It is designed for teams where multiple people and coding agents are editing the same backlog concurrently, and where machine-readable CLI output matters as much as human UX.

## Installation

```bash
cargo install bones-cli
```

This installs the `bn` command-line tool.

## Quick start

```bash
# initialize a repo
bn init

# set identity
export AGENT=my-agent

# create work items
bn create --title "Add retry budget to queue writer" --kind task --label reliability

# get prioritized next items
bn next
bn next 3

# search
bn search "retry budget"

# machine-readable output
bn triage --format json
bn search auth --format json
```

## Shell completions

```bash
bn completions bash > ~/.local/share/bash-completion/completions/bn
bn completions zsh > ~/.zfunc/_bn
bn completions fish > ~/.config/fish/completions/bn.fish
```

## Features

- **CRDT event log**: append-only writes, deterministic merge, no conflicts
- **Hybrid search**: lexical BM25 + semantic embeddings + structural graph proximity fused with RRF
- **Graph-aware triage**: PageRank, critical-path, dependency-weighted scoring
- **Duplicate detection**: automatic on create, explicit via `bn triage similar`
- **Machine-readable**: `--format json` on every command for agent workflows
- **TUI**: interactive interface alongside the CLI

## Documentation

See the [bones repository](https://github.com/bobisme/bones) for full documentation, including:
- Architecture overview
- Agent workflow guide
- Simulation testing details
- Migration from beads
