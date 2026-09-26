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

This network model tracks a grow-only set of event IDs, so it checks delivery, not bones semantics.

## Projection simulation (`replica` module)

`bones_sim::replica` runs the real system. Each agent owns a real `.bones` directory, writes real events (creates, field updates, labels, assignees, links, comments, deletes, redactions) with a skewed and drifting clock, and pulls other agents' logs the way git does: union merge (ours first) or rebase (theirs first). Faults: duplicated log lines and deleted projection databases.

After every step, two oracles check the real projection:

- **Incremental equals rebuild** — the acting agent's incrementally applied projection equals a full rebuild of its log.
- **Convergence** — agents that hold the same events have identical projections. After a final all-to-all sync, all must agree.

Failing seeds are shrunk to a minimal plan. Plans are plain JSON; shrunk plans live in `tests/fixtures/` as named regressions.

```bash
cargo test -p bones-sim --test projection_sim                    # 48 seeds
BONES_SIM_SEEDS=1000 cargo test --release -p bones-sim --test projection_sim campaign_converges
```

The campaign found two bugs when it was introduced (bn-2fs6): after a rebase pull rewrote the log, incremental apply resumed at a stale byte offset and either skipped events silently or failed on a partial line.

## Usage

This crate is used by the `bn dev sim` subcommand in [`bones-cli`](https://crates.io/crates/bones-cli):

```bash
# run 100 seeds with default fault rates
bn dev sim run --seeds 100

# replay a failing seed
bn dev sim replay --seed 42
```

See the [bones repository](https://github.com/bobisme/bones) for the full project.
