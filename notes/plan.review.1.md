# Bones Plan Review v1: Hardening for Correctness, Scale, and Alien-Math Reliability

## Scope and method

I reviewed `ws/default/notes/plan.md` against:

- the advanced math proposal in `ws/default/notes/original-convo/01-bones-advanced-research.md`
- practical systems constraints (distributed correctness, operability, observability)
- external references on dynamic PageRank, Whittle indexability, directed sparsification, directed persistent homology, FTS/vector search, transparency logs, and deterministic simulation.

This review is intentionally critical and additive. The current plan is excellent in ambition and direction; the changes below are about making it provably correct, robust under adversarial conditions, and realistic under performance pressure.

---

## Executive assessment

### What is already exceptional

1. **Event-sourced + disposable projection architecture** is the right foundation.
2. **CRDT-first convergence goal** is exactly right for multi-agent workflows.
3. **Integrated triage math** (critical path, centrality, scheduling) is a meaningful differentiator.
4. **Deterministic simulation and semilattice testing** is the right reliability backbone.

### Highest-risk gaps (must fix)

1. **Some asymptotic claims are too strong for directed dynamic graphs** and should be reframed as empirical/conditional guarantees.
2. **Whittle scheduling is presented as broadly optimal without indexability gates**.
3. **Directed spectral sparsification and directed persistent homology are mathematically subtle and should be staged behind strict activation thresholds**.
4. **LWW/ITC tie-breaking and state transition semantics need fully explicit deterministic rules**.
5. **Performance claims (bytes/event, sub-ms latency) need benchmark gates, not fixed promises**.

---

## Assumption validation (web + research)

| Plan assumption | Verdict | Evidence | Required change |
|---|---|---|---|
| DF-PageRank yields major practical speedups | Mostly valid in practice, not a universal worst-case claim | DF/DF-P papers report strong speedups on tested workloads ([W3]) | Keep as optimization; add fallback and adversarial-case guardrails |
| Dynamic directed PageRank can be maintained cheaply in general | Overstated | ICALP 2024 shows hard lower bounds for explicit multiplicative approximation in directed setting ([W4]) | Reframe guarantees; use approximate/incremental best-effort with SLA bounds |
| Whittle index is broadly optimal for scheduler | Conditional only | Whittle needs indexability; computation can be nontrivial ([W5]) | Add indexability checks + fallback scheduler |
| Directed sparsification behaves like undirected BSS sparsifiers | Not directly transferable | Directed results exist but are more specialized (Eulerian, stronger constraints) ([W7], [W8]) | Gate directed sparsifier usage; define prerequisites |
| Persistent homology directly applies to directed dependencies | Not by default | Standard PH ignores asymmetry; directed variants use path homology ([W6]) | Use directed path-homology only in advanced mode |
| Merkle-DAG gives O(log N) proof workflows | Valid | Transparent log and Merkle-CRDT literature support this ([W9], [W10]) | Adopt explicit proof APIs and signed checkpoints |
| Deterministic simulation at scale is practical | Valid | FoundationDB reports deterministic simulation as core strategy ([W11]) | Elevate simulator to phase-gating requirement |
| 2-4 bytes/event average for mixed issue-tracker payloads | Highly optimistic | Real payloads include natural language text; even efficient CRDT systems show compression wins but not universal ultra-low bytes/op for rich payloads ([W12], [W16]) | Replace with measured targets by event type percentile |
| FTS5 + BM25 + tokenizer strategy is sound | Valid | SQLite FTS5 supports BM25, tokenizers, prefixes, ranking hooks ([W1]) | Keep; add domain-specific analyzer test corpus |
| all-MiniLM-L6-v2 is suitable for offline semantic layer | Valid | 384-d embedding model, widely deployed sentence encoder ([W2]) | Keep; add calibration/evaluation and quantized fallback |
| sqlite-vec is safe default forever | Useful, but pre-v1 caution | sqlite-vec explicitly pre-v1 ([W13]) | Pin versions + provide flat-file fallback path |
| ITC solves dynamic actor growth concerns | Valid conceptually | ITC designed for dynamic participants ([W14]) | Define canonical encoding and normalization rules |

---

## Concrete proposed changes

## P0 (must include before implementation begins)

### P0-1. Make merge semantics fully deterministic and auditable

**Current state**

- Plan says "LWW by ITC" and mentions tie-breaking, but not as a normative protocol section.

**Problem**

- If two replicas differ in tie-breaking implementation details, convergence can fail despite CRDT intent.

**Proposed change**

Add a normative merge spec section with total order:

1. Compare causal dominance using ITC partial order.
2. If concurrent, compare `wall_ts`.
3. If equal, compare `actor_id` lexicographically.
4. If equal, compare `event_hash` lexicographically.

Require this exact order for all LWW registers.

**Acceptance criteria**

- Property test: 1M random concurrent updates across 32 simulated actors, all replicas bit-identical.
- Cross-language conformance tests (Rust core + any bindings).

---

### P0-2. Replace "state LWW with validation" with an epoch-aware semilattice

**Current state**

- `state` is LWW plus "invalid transitions rejected".

**Problem**

- "Rejected" transitions can diverge across replicas unless modeled as data.

**Proposed change**

Represent state as `(epoch, phase)`:

- `phase in {open, doing, done, archived}` with monotone rank within epoch.
- `reopen` increments `epoch` and resets `phase=open`.
- Join operation: max epoch, then max phase rank.

This preserves semilattice laws and avoids reject/accept divergence.

**Acceptance criteria**

- Formal quickcheck of associativity/commutativity/idempotence for state join.
- Deterministic replay of concurrent done/reopen/update races.

---

### P0-3. Add anti-equivocation and provenance signing

**Current state**

- Merkle hash chain is proposed, but actor authenticity is not strongly specified.

**Problem**

- Without signatures, any actor name can be spoofed in event streams.

**Proposed change**

- Add optional but first-class per-actor keypair signatures on events.
- Add checkpoint signatures for shard roots.
- Include `prev_checkpoint_hash` for consistency proofs.

**Acceptance criteria**

- `bn verify` validates event signatures, shard Merkle roots, and checkpoint consistency.
- Negative tests: forged actor/event rejected.

---

### P0-4. Reframe performance promises into benchmark-backed SLO tiers

**Current state**

- Hard targets like `bn next < 1ms` and "2-4 bytes/event".

**Problem**

- Risk of overpromising before workload characterization.

**Proposed change**

Define SLO tiers with dataset profiles:

- **Tier S**: 1k items / 50k events
- **Tier M**: 10k items / 500k events
- **Tier L**: 100k items / 5M events

Track p50/p95/p99 for `create`, `next`, `search`, `rebuild` and storage bytes/event by percentile and event type.

**Acceptance criteria**

- Bench CI pipeline publishes trend dashboard per commit.
- Claims in docs auto-generated from latest green benchmark set.

---

### P0-5. Add mathematically explicit conditions for advanced graph modules

**Current state**

- Plan presents directed sparsification and homology as broadly applicable.

**Problem**

- Directed variants are constrained and costly; misuse can degrade output quality.

**Proposed change**

Add activation contract:

- Directed spectral sparsification requires graph eulerianization or approved directed Laplacian mode ([W7], [W8]).
- Persistent topology defaults to SCC/cycle metrics; path homology enabled only in `--topology=advanced` mode ([W6]).

**Acceptance criteria**

- Runtime checks emit "advanced math preconditions not met" with fallback path.
- No silent activation of unsupported assumptions.

---

### P0-6. Put Whittle behind indexability checks and fallback schedulers

**Current state**

- Whittle described as asymptotically optimal and default for parallel assignments.

**Problem**

- Indexability is not universal; computing indices can itself be expensive ([W5]).

**Proposed change**

- Add "Whittle eligibility" test per workload class.
- If failed, fallback to:
  1. constrained min-cost max-flow assignment, then
  2. contextual bandit tie-break.

**Acceptance criteria**

- `bn plan --explain` reports chosen assignment regime and reason.
- Regression suite compares throughput and starvation metrics across regimes.

---

### P0-7. Expand deletion/redaction semantics beyond G-Set comments

**Current state**

- Comments are append-only G-Set with no deletion semantics.

**Problem**

- Real projects need secret removal, legal erasure, and accidental paste cleanup.

**Proposed change**

- Add `item.redact` events with cryptographic tombstone references.
- Projection hides redacted payload by default while preserving auditability.
- Optional encrypted payload chunks with key revocation for hard privacy boundaries.

**Acceptance criteria**

- Redaction preserves convergence and Merkle integrity.
- `bn export --public` strips redacted bodies deterministically.

---

## P1 (high impact, should include in core roadmap)

### P1-1. Introduce a graph normalization pipeline before centrality

**Proposed**

Before computing PageRank/betweenness:

1. collapse SCCs into condensation DAG,
2. compute transitive reduction for scheduling edges,
3. preserve original graph for display/explanations.

**Why**

- More stable metrics, faster computation, fewer spurious bottlenecks.

**Acceptance criteria**

- Metric variance decreases under small edge perturbations.

---

### P1-2. Upgrade composite priority to uncertainty-aware objective

**Current formula**

`P(v) = alpha*CP + beta*PR + gamma*BC + delta*U + epsilon*D`

**Proposed extension**

`P_star(v) = E[P(v)] - lambda*sqrt(Var[P(v)]) + mu*CVaR_alpha(blocking_delay(v))`

- `Var[P(v)]` estimated from bootstrap perturbations.
- `CVaR` penalizes catastrophic unblock-delay tail risk.

**Why**

- Prioritizes robust unblocking under uncertainty, not only mean score.

**Acceptance criteria**

- Reduced high-percentile downstream wait time in simulator.

---

### P1-3. Add search/duplicate evaluation corpus and calibration loop

**Proposed**

- Build gold dataset of (duplicate, related, unrelated) item pairs.
- Track NDCG@k, duplicate precision/recall, and false-block rate for create-time warnings.
- Add online threshold adaptation with bounded exploration.

**Why**

- Prevents semantic layer drift and false positive fatigue.

**Acceptance criteria**

- CI fails if duplicate F1 regresses beyond threshold.

---

### P1-4. Add deterministic simulation as a release gate, not a feature

**Current state**

- Simulator appears in later phases.

**Proposed**

- Move simulator to Phase 0/1 release gate.
- Include seed replay, fault matrix, shrinking/minimization of failing traces.

**Why**

- FoundationDB-style deterministic simulation is a major reliability multiplier ([W11]).

**Acceptance criteria**

- Every release has reproducible nightly simulation campaign with tracked invariants.

---

### P1-5. Add checkpointed shard manifest and corruption recovery protocol

**Proposed**

- Per-shard manifest: record count, root hash, byte length, crc.
- Startup verifies manifest before replay.
- Recovery command rebuilds from last trusted checkpoint and remote proofs.

**Acceptance criteria**

- Injected corruption in shard body detected before projection build.

---

### P1-6. Add proof-carrying `bn next --proof`

**Proposed**

`bn next --proof` emits:

- feature vector values,
- metric versions and graph hash,
- ranking decomposition,
- deterministic replay token.

**Why**

- Makes triage decisions inspectable, reproducible, and debuggable.

**Acceptance criteria**

- Same proof token recomputes same ranked result on another replica.

---

## P2 (inventive, high upside, optional for MVP)

### P2-1. Algebraic invariant compiler for CRDT fields

Generate merge/test code from declarative semilattice specs:

- field lattice definition,
- monotonicity constraints,
- auto-generated property tests and counterexample shrinkers.

Outcome: less hand-written merge code, fewer subtle law violations.

---

### P2-2. Multi-agent scheduler with fairness and anti-collision guarantees

Hybrid objective:

- maximize expected unblock value,
- enforce fairness constraint across agents,
- penalize duplicate assignment probability.

Implement as constrained optimization fallback when Whittle not eligible.

---

### P2-3. Two-speed topology engine

- **Fast path** (always on): SCCs, cycle basis, edge-cut pressure, bridge score.
- **Alien path** (opt-in): directed persistent path homology over sampled filtrations.

Keeps day-to-day latency low while preserving advanced diagnostics.

---

### P2-4. Transparency federation for multi-repo Bones

Use cross-repo signed checkpoint exchange:

- inclusion proofs for imported events,
- consistency proofs across checkpoint lineage,
- auditable federation root for org-wide work graph.

---

## Proposed roadmap changes (concrete reorder)

## New Phase 0: Spec, proof obligations, and harnesses

- Normative merge spec (including tie-breaking)
- Invariant catalog and conformance suite
- Deterministic simulator skeleton and seed replay
- Benchmark corpus and SLO definitions

## Revised Phase 1: Event core + projection + verification

- TSJSON event engine + shard manifests
- CRDT merges with epoch-state semantics
- Merkle checkpoints + optional signatures
- `bn verify` baseline

## Revised Phase 2: Search and dedupe early

- FTS5 + embedding + fusion + dedupe UX
- Gold dataset and threshold calibration

## Revised Phase 3: Triage baseline

- Critical path + centrality on normalized graph
- Composite score with uncertainty penalties
- `bn next --proof`

## Revised Phase 4: Advanced graph math (gated)

- Incremental PageRank optimizations
- Directed sparsifier only when preconditions hold
- Advanced topology mode (path homology)

## Revised Phase 5: Multi-agent scheduling

- Whittle (eligibility-gated)
- min-cost flow fallback
- fairness/starvation protections

## Revised Phase 6: Distribution and federation

- Prolly-tree experiments
- cross-repo transparency federation
- MCP and TUI hardening

---

## Specific wording changes recommended in `plan.md`

These are high-value textual edits to avoid overclaiming:

1. Replace "Whittle Index provides asymptotically optimal allocation" with:
   "Whittle Index is used when indexability conditions are met; otherwise Bones uses a constrained optimization fallback."

2. Replace absolute latency claims (for example `<1ms`) with:
   "Latency targets are benchmark-gated by dataset tier (S/M/L) and reported as p50/p95/p99."

3. Replace "2-4 bytes per event average" with:
   "Binary cache compression target is measured per event class; payload-heavy events are expected to remain significantly larger than structural events."

4. Replace broad directed sparsifier statements with:
   "Directed sparsification is activated only under validated directed-Laplacian preconditions; otherwise full-graph or sampled approximations are used."

5. Replace unconditional persistent homology health analysis with:
   "Default health uses SCC/cycle diagnostics; directed persistent path homology is available as an advanced asynchronous analysis mode."

---

## Why this keeps the alien-artifact spirit (while becoming bulletproof)

This revision does not remove ambition. It makes ambition reliable:

- keep the semilattice math, but make every rule executable and testable;
- keep the high-end graph science, but gate it behind mathematically valid preconditions;
- keep extreme optimization, but tie claims to repeatable benchmark evidence;
- keep multi-agent scheduling innovation, but avoid overfitting to one theoretical policy.

That is how Bones becomes both futuristic and production-grade.

---

## References

- [W1] SQLite FTS5 documentation: https://sqlite.org/fts5.html
- [W2] all-MiniLM-L6-v2 model card: https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2
- [W3] DF* PageRank (Sahu, 2024): https://arxiv.org/abs/2401.15870
- [W4] Dynamic PageRank lower bounds (ICALP 2024): https://drops.dagstuhl.de/entities/document/10.4230/LIPIcs.ICALP.2024.90
- [W5] Whittle indexability conditions, O(K^3) computation: https://arxiv.org/abs/2008.06111
- [W6] Persistent Path Homology of Directed Networks: https://arxiv.org/abs/1701.00565
- [W7] Unified directed spectral sparsification: https://arxiv.org/abs/1812.04165
- [W8] Better sparsifiers for directed Eulerian graphs (ICALP 2024): https://drops.dagstuhl.de/entities/document/10.4230/LIPIcs.ICALP.2024.119
- [W9] Merkle-CRDTs: https://arxiv.org/abs/2004.00107
- [W10] Transparent logs and O(log N) proofs (Russ Cox): https://research.swtch.com/tlog
- [W11] FoundationDB simulation and deterministic testing: https://apple.github.io/foundationdb/testing.html
- [W12] Automerge 2.0 performance and storage discussion: https://automerge.org/blog/automerge-2/
- [W13] sqlite-vec (pre-v1 status and architecture): https://github.com/asg017/sqlite-vec
- [W14] Interval Tree Clocks citation: https://doi.org/10.1007/978-3-540-92221-6_18
- [W15] Reciprocal Rank Fusion citation: https://doi.org/10.1145/1571941.1572114
- [W16] Diamond Types implementation notes: https://github.com/josephg/diamond-types
