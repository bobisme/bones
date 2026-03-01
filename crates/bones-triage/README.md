# bones-triage

Prioritization and scoring engine for the [bones](https://github.com/bobisme/bones) issue tracker.

## What this crate provides

- **PageRank**: graph-based importance scoring over the dependency DAG
- **Betweenness centrality**: identifies bottleneck items on the critical path
- **HITS/eigenvector signals**: hub/authority decomposition for multi-signal ranking
- **Critical-path influence**: how many downstream items are blocked by each item
- **Urgency decay**: time-weighted urgency signals that decay toward defaults
- **Composite ranking**: urgency override + graph metrics + decay, whittle-scored
- **Dependency management**: cycle detection, transitive reduction, SCC condensation
- **Triage scoring**: `bn next` and `bn triage` dispatch ranking

## Usage

This crate is an internal dependency of [`bones-cli`](https://crates.io/crates/bones-cli). See the [bones repository](https://github.com/bobisme/bones) for the full project.
