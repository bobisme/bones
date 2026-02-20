# `bn sim` Resilience Usage Plan

## Purpose

Define how `bn sim` should be used in development to systematically improve Bones resilience, and close the gap between having a simulation tool and using it as an engineering feedback loop.

## Current Gap

- The simulator exists and produces deterministic failures, but there is no explicit team contract for what should pass vs what is exploratory.
- Default settings are adversarial (partial fanout + faults), while the oracle checks strict convergence-style invariants.
- Result output is too verbose/noisy for routine usage, so findings are not consistently converted into fixes.
- CI policy does not clearly separate hard-gated correctness checks from stress tracking.

## Operating Model

Use `bn sim` in two explicit lanes:

1. **Sanity lane (hard gate)**
   - Goal: detect regressions in baseline deterministic behavior.
   - Configuration: no faults, full fanout (`fanout = agents - 1`), fixed seed corpus.
   - Policy: must pass in PR/merge validation.

2. **Stress lane (resilience improvement lane)**
   - Goal: expose robustness gaps under faults and partial connectivity.
   - Configuration: adversarial settings (faults, delays, partitions, partial fanout), fixed core seeds plus optional rotating seeds.
   - Policy: not an immediate hard gate; used to drive prioritized remediation and trend tracking.

## Required Tooling Changes

### CLI ergonomics

- Add explicit run modes (for example: `--mode sanity|stress|custom`).
- Make mode semantics visible in output and help text.
- Print concise summaries by default:
  - seeds run/passed/failed
  - violations grouped by invariant
  - top failing seeds
  - replay command hints
- Keep full violation payloads behind verbose/detail flags.

### Oracle signal quality

- Avoid cascading/noisy failure reporting where one root cause explodes into redundant diagnostics.
- Ensure each invariant check reports meaningful, non-duplicative evidence.
- Improve idempotence and convergence interaction so the output remains actionable when convergence is already broken.

### Replay-first workflow support

- Ensure every campaign report surfaces first failing seed and direct replay command.
- Add stable, comparable fingerprints for replay traces.
- Keep deterministic behavior across runs for identical settings and seed inputs.

## CI and Development Workflow

### PR workflow

- Run sanity lane as required status check.
- Fail fast with clear explanation when sanity fails.

### Nightly/periodic workflow

- Run stress lane across a known seed corpus.
- Publish artifacts:
  - invariant violation counts
  - recurring failing seeds
  - trend snapshots relative to previous runs

### Developer loop

For each recurring stress failure:

1. replay seed,
2. isolate root cause,
3. implement fix,
4. add regression coverage (unit/integration/sim seed case),
5. rerun sanity + affected stress subset,
6. update failure trend.

## Tracking and Metrics

Track these as first-class resilience indicators:

- sanity pass rate,
- stress pass rate,
- failure count by invariant,
- recurring failing seed count,
- median time from seed failure discovery to fix,
- number of stabilized seeds (previously failing, now consistently passing).

## Simulator Fidelity Roadmap

If Bones is expected to converge under lossy networks, evolve the simulation model to reflect intended recovery mechanics (for example anti-entropy/repair behavior) before converting stress lane into a strict gate.

Until then:

- sanity lane is release-critical,
- stress lane is resilience-discovery and trend management.

## Exit Criteria for Full Operationalization

- Team documentation clearly defines sanity vs stress expectations.
- PRs are gated on deterministic sanity checks.
- Stress runs produce compact, actionable reports and stable artifacts.
- Recurring failures are actively burned down via replay-driven fixes.
- Trend metrics demonstrate improving resilience over time.
