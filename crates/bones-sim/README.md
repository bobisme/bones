# bones-sim

Deterministic simulation harness for testing CRDT correctness in [bones](https://github.com/bobisme/bones) under adversarial network conditions.

## What this crate provides

The sim models multiple agents emitting events over a configurable fault-injected network. After the network drains, a reconciliation phase models real sync (pairwise gossip + set union). An oracle then checks five invariants:

- **Convergence** — all agents end up with identical state
- **Commutativity** — event application order doesn't matter
- **Idempotence** — re-applying events is a no-op
- **Causal consistency** — no gaps in per-source sequences
- **Triage stability** — derived scores agree across replicas

Fault modes: message drops, reordering, duplication, network partitions, clock drift.

Every seed is deterministic: same seed → same trace → same result. When a seed fails, you get a full execution trace showing exactly which message was dropped or reordered and how it cascaded.

## Usage

This crate is used by the `bn dev sim` subcommand in [`bones-cli`](https://crates.io/crates/bones-cli):

```bash
# run 100 seeds with default fault rates
bn dev sim run --seeds 100

# replay a failing seed
bn dev sim replay --seed 42
```

See the [bones repository](https://github.com/bobisme/bones) for the full project.
