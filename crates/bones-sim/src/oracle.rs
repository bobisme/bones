use std::collections::BTreeSet;

use crate::agent::AgentState;
use crate::rng::DeterministicRng;

// ── Core result types ─────────────────────────────────────────────────────────

/// Oracle result for an invariant check.
///
/// Returned by each of the five invariant checkers and by [`ConvergenceOracle::check_all`].
#[derive(Debug, Clone, PartialEq)]
pub struct OracleResult {
    /// `true` iff no violations were found.
    pub passed: bool,
    /// Detailed description of every invariant that was violated.
    pub violations: Vec<InvariantViolation>,
}

impl OracleResult {
    /// Construct a passing result.
    #[must_use]
    fn pass() -> Self {
        Self {
            passed: true,
            violations: Vec::new(),
        }
    }

    /// Construct a failing result from one or more violations.
    #[must_use]
    fn fail(violations: Vec<InvariantViolation>) -> Self {
        Self {
            passed: false,
            violations,
        }
    }

    /// Merge another result into this one (failures accumulate).
    #[must_use]
    fn merge(mut self, other: OracleResult) -> Self {
        if !other.passed {
            self.passed = false;
            self.violations.extend(other.violations);
        }
        self
    }
}

// ── Invariant violation diagnostics ──────────────────────────────────────────

/// Diagnostic information for a single failed invariant check.
#[derive(Debug, Clone, PartialEq)]
pub enum InvariantViolation {
    /// Two agents have different `known_events` after full delivery.
    ///
    /// Emitted by `check_convergence`.
    Convergence {
        /// First diverging agent ID.
        agent_a: usize,
        /// Second diverging agent ID.
        agent_b: usize,
        /// Events present in `agent_a` but absent in `agent_b`.
        only_in_a: Vec<u64>,
        /// Events present in `agent_b` but absent in `agent_a`.
        only_in_b: Vec<u64>,
    },

    /// Re-ordering event delivery produced a different final state.
    ///
    /// Emitted by `check_commutativity`.
    Commutativity {
        /// Zero-based index of the shuffled permutation that diverged.
        permutation_index: usize,
        /// Events present in the canonical state but missing from the shuffled result.
        missing_events: Vec<u64>,
        /// Events present in the shuffled result but absent from the canonical state.
        extra_events: Vec<u64>,
    },

    /// Re-applying an already-applied event mutated the state.
    ///
    /// Emitted by `check_idempotence`.
    Idempotence {
        /// The event that was re-applied.
        event_id: u64,
        /// Known events before the duplicate application.
        events_before: Vec<u64>,
        /// Known events after the duplicate application (should equal `events_before`).
        events_after_dup: Vec<u64>,
    },

    /// An agent knows event `(source, seq=N)` but is missing `(source, seq=M)` where `M < N`.
    ///
    /// Emitted by `check_causality`.
    CausalConsistency {
        /// Agent that is missing the earlier event.
        observer_agent: usize,
        /// Agent that emitted both events.
        source_agent: usize,
        /// Sequence number that is absent but required by causality.
        missing_seq: u64,
        /// A higher sequence number that is present, proving the gap.
        present_higher_seq: u64,
    },

    /// Triage scores diverge beyond the allowed epsilon across replicas.
    ///
    /// Emitted by `check_triage_stability`.
    TriageStability {
        /// First agent involved in the comparison.
        agent_a: usize,
        /// Second agent involved in the comparison.
        agent_b: usize,
        /// Score computed for `agent_a`.
        score_a: f64,
        /// Score computed for `agent_b`.
        score_b: f64,
        /// Absolute difference `|score_a - score_b|`.
        diff: f64,
        /// Maximum permitted difference.
        epsilon: f64,
    },
}

// ── Backward-compatible convergence report ────────────────────────────────────

/// Convergence check output (backward-compatible with the original simple oracle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvergenceReport {
    /// Whether all agent states are identical.
    pub converged: bool,
    /// Agent IDs that diverged from canonical state.
    pub divergent_agents: Vec<usize>,
    /// Number of events in canonical state.
    pub canonical_event_count: usize,
}

// ── Oracle ────────────────────────────────────────────────────────────────────

/// Oracle for verifying CRDT invariants after simulation.
///
/// All methods are `#[must_use]`.  Pass results through
/// [`OracleResult::merge`] or call [`ConvergenceOracle::check_all`] to run
/// every check in one shot.
///
/// # Invariants checked
///
/// 1. **Strong convergence** (`check_convergence`) — all replicas are identical.
/// 2. **Commutativity** (`check_commutativity`) — ordering events differently
///    yields the same final state.
/// 3. **Idempotence** (`check_idempotence`) — duplicate delivery is a no-op.
/// 4. **Causal consistency** (`check_causality`) — if seq=N is present from
///    source S, every seq<N from S is also present.
/// 5. **Triage stability** (`check_triage_stability`) — coverage scores
///    converge within an epsilon tolerance.
pub struct ConvergenceOracle;

impl ConvergenceOracle {
    // ── Backward-compatible entry point ──────────────────────────────────────

    /// Compare all agent states and detect divergence.
    ///
    /// This is the original simple oracle kept for backward compatibility.
    /// New callers should prefer [`check_convergence`](Self::check_convergence).
    #[must_use]
    pub fn evaluate(states: &[AgentState]) -> ConvergenceReport {
        if states.is_empty() {
            return ConvergenceReport {
                converged: true,
                divergent_agents: Vec::new(),
                canonical_event_count: 0,
            };
        }

        let canonical = &states[0].known_events;
        let divergent_agents = states
            .iter()
            .filter(|state| state.known_events != *canonical)
            .map(|state| state.id)
            .collect::<Vec<_>>();

        ConvergenceReport {
            converged: divergent_agents.is_empty(),
            divergent_agents,
            canonical_event_count: canonical.len(),
        }
    }

    // ── Invariant 1: Strong convergence ──────────────────────────────────────

    /// Check that every replica holds identical `known_events` after full delivery.
    ///
    /// Compares every agent pair; stops after the first divergent event set for
    /// that pair but continues to the next pair so all violations are reported.
    #[must_use]
    pub fn check_convergence(states: &[AgentState]) -> OracleResult {
        if states.len() < 2 {
            return OracleResult::pass();
        }

        let mut violations = Vec::new();

        for i in 0..states.len() {
            for j in (i + 1)..states.len() {
                let a = &states[i];
                let b = &states[j];

                if a.known_events == b.known_events {
                    continue;
                }

                let only_in_a: Vec<u64> = a
                    .known_events
                    .difference(&b.known_events)
                    .copied()
                    .collect();
                let only_in_b: Vec<u64> = b
                    .known_events
                    .difference(&a.known_events)
                    .copied()
                    .collect();

                violations.push(InvariantViolation::Convergence {
                    agent_a: a.id,
                    agent_b: b.id,
                    only_in_a,
                    only_in_b,
                });
            }
        }

        if violations.is_empty() {
            OracleResult::pass()
        } else {
            OracleResult::fail(violations)
        }
    }

    // ── Invariant 2: Commutativity ────────────────────────────────────────────

    /// Check that applying `events` in any order yields the same final state.
    ///
    /// Runs `iterations` random permutations using `rng` as the source of
    /// shuffle decisions.  Compares each permuted final state against the
    /// canonical state (events applied in their original order).
    ///
    /// With a grow-only set CRDT this always passes, but the check remains
    /// meaningful for richer merge functions.
    #[must_use]
    pub fn check_commutativity(
        events: &[u64],
        rng: &mut DeterministicRng,
        iterations: usize,
    ) -> OracleResult {
        if events.len() < 2 || iterations == 0 {
            return OracleResult::pass();
        }

        // Canonical: apply events in the supplied order.
        let canonical: BTreeSet<u64> = events.iter().copied().collect();

        let mut violations = Vec::new();

        for perm_idx in 0..iterations {
            let shuffled = fisher_yates_shuffle(events, rng);
            let result: BTreeSet<u64> = shuffled.iter().copied().collect();

            if result != canonical {
                let missing_events: Vec<u64> = canonical.difference(&result).copied().collect();
                let extra_events: Vec<u64> = result.difference(&canonical).copied().collect();

                violations.push(InvariantViolation::Commutativity {
                    permutation_index: perm_idx,
                    missing_events,
                    extra_events,
                });
            }
        }

        if violations.is_empty() {
            OracleResult::pass()
        } else {
            OracleResult::fail(violations)
        }
    }

    // ── Invariant 3: Idempotence ──────────────────────────────────────────────

    /// Check that re-applying any event from `events` to `state` is a no-op.
    ///
    /// For a grow-only set, `insert` is idempotent by construction.  A
    /// non-idempotent merge function would violate this invariant.
    #[must_use]
    pub fn check_idempotence(state: &AgentState, events: &[u64]) -> OracleResult {
        let mut violations = Vec::new();

        for &event_id in events {
            let before = state.known_events.clone();

            // Simulate the re-application of the event.
            let mut after = before.clone();
            after.insert(event_id);

            if before != after {
                violations.push(InvariantViolation::Idempotence {
                    event_id,
                    events_before: before.iter().copied().collect(),
                    events_after_dup: after.iter().copied().collect(),
                });
            }
        }

        if violations.is_empty() {
            OracleResult::pass()
        } else {
            OracleResult::fail(violations)
        }
    }

    // ── Invariant 4: Causal consistency ──────────────────────────────────────

    /// Check that within each agent's state, event sequences are gap-free per source.
    ///
    /// Events are encoded as `(source << 32) | (seq & 0xFFFF_FFFF)`.  If a
    /// state contains `(source=S, seq=N)` it must also contain every
    /// `(source=S, seq=M)` for `M < N`, because earlier events are causal
    /// predecessors of later ones from the same agent.
    #[must_use]
    pub fn check_causality(states: &[AgentState]) -> OracleResult {
        let mut violations = Vec::new();

        for state in states {
            // Group event sequences by source agent.
            let mut per_source: std::collections::BTreeMap<usize, Vec<u64>> =
                std::collections::BTreeMap::new();

            for &event_id in &state.known_events {
                let source = decode_source(event_id);
                let seq = decode_seq(event_id);
                per_source.entry(source).or_default().push(seq);
            }

            for (source_agent, mut seqs) in per_source {
                seqs.sort_unstable();

                // Sequences must be contiguous starting from 0.
                for window in seqs.windows(2) {
                    let prev = window[0];
                    let next = window[1];

                    if next != prev.saturating_add(1) {
                        // There is a gap: next is present but prev+1 is missing.
                        let missing_seq = prev.saturating_add(1);
                        violations.push(InvariantViolation::CausalConsistency {
                            observer_agent: state.id,
                            source_agent,
                            missing_seq,
                            present_higher_seq: next,
                        });
                    }
                }

                // The first sequence must be 0 (can't have seq=N without seq=0).
                if let Some(&first) = seqs.first() {
                    if first != 0 {
                        violations.push(InvariantViolation::CausalConsistency {
                            observer_agent: state.id,
                            source_agent,
                            missing_seq: 0,
                            present_higher_seq: first,
                        });
                    }
                }
            }
        }

        if violations.is_empty() {
            OracleResult::pass()
        } else {
            OracleResult::fail(violations)
        }
    }

    // ── Invariant 5: Triage stability ─────────────────────────────────────────

    /// Check that triage scores converge across replicas within `epsilon`.
    ///
    /// Score = `known_events.len() / total_events.max(1)`.  When all
    /// replicas are fully converged, their scores are identical.  Partially
    /// converged replicas may differ; violations are reported when the
    /// absolute score difference exceeds `epsilon`.
    ///
    /// The default tolerance used by [`check_all`] is `1e-9` (effectively
    /// zero, catching any divergence at all).
    #[must_use]
    pub fn check_triage_stability(
        states: &[AgentState],
        total_events: usize,
        epsilon: f64,
    ) -> OracleResult {
        if states.len() < 2 {
            return OracleResult::pass();
        }

        let scores: Vec<f64> = states
            .iter()
            .map(|s| triage_score(s.known_events.len(), total_events))
            .collect();

        let mut violations = Vec::new();

        for i in 0..scores.len() {
            for j in (i + 1)..scores.len() {
                let diff = (scores[i] - scores[j]).abs();
                if diff > epsilon {
                    violations.push(InvariantViolation::TriageStability {
                        agent_a: states[i].id,
                        agent_b: states[j].id,
                        score_a: scores[i],
                        score_b: scores[j],
                        diff,
                        epsilon,
                    });
                }
            }
        }

        if violations.is_empty() {
            OracleResult::pass()
        } else {
            OracleResult::fail(violations)
        }
    }

    // ── Composite runner ─────────────────────────────────────────────────────

    /// Run all five invariant checks and return a merged result.
    ///
    /// `events` must be the complete set of event IDs that were produced during
    /// the simulation (used for commutativity and idempotence checks).
    ///
    /// `rng` is used to drive permutations in the commutativity check.
    #[must_use]
    pub fn check_all(
        states: &[AgentState],
        events: &[u64],
        rng: &mut DeterministicRng,
    ) -> OracleResult {
        // Total distinct events across all agents (union of all known_events).
        let total_events = {
            let mut union: BTreeSet<u64> = BTreeSet::new();
            for s in states {
                union.extend(s.known_events.iter().copied());
            }
            union.len()
        };

        let convergence = Self::check_convergence(states);

        let commutativity = Self::check_commutativity(events, rng, 8);

        // For idempotence, use the first state (all are identical after convergence).
        let idempotence = if let Some(first) = states.first() {
            Self::check_idempotence(first, events)
        } else {
            OracleResult::pass()
        };

        let causality = Self::check_causality(states);

        // Tight epsilon: fully-converged replicas must have identical scores.
        let triage = Self::check_triage_stability(states, total_events, 1e-9);

        convergence
            .merge(commutativity)
            .merge(idempotence)
            .merge(causality)
            .merge(triage)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Decode the source agent from an event_id encoded as `(source << 32) | seq`.
#[inline]
fn decode_source(event_id: u64) -> usize {
    usize::try_from(event_id >> 32).unwrap_or(usize::MAX)
}

/// Decode the per-agent sequence from an event_id.
#[inline]
fn decode_seq(event_id: u64) -> u64 {
    event_id & 0xFFFF_FFFF
}

/// Compute triage coverage score in `[0.0, 1.0]`.
#[inline]
fn triage_score(known: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        known as f64 / total as f64
    }
}

/// Fisher-Yates shuffle returning a new `Vec<u64>`.
fn fisher_yates_shuffle(events: &[u64], rng: &mut DeterministicRng) -> Vec<u64> {
    let mut v = events.to_vec();
    let n = v.len();
    for i in (1..n).rev() {
        let j_u64 = rng.next_bounded(u64::try_from(i + 1).unwrap_or(1));
        let j = usize::try_from(j_u64).unwrap_or(0);
        v.swap(i, j);
    }
    v
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::agent::AgentState;
    use crate::rng::DeterministicRng;

    // ── Helper constructors ───────────────────────────────────────────────────

    fn make_state(id: usize, events: &[u64]) -> AgentState {
        AgentState {
            id,
            known_events: events.iter().copied().collect(),
        }
    }

    /// Encode an event id the same way `SimulatedAgent::emit_event` does.
    fn event_id(source: u64, seq: u64) -> u64 {
        (source << 32) | (seq & 0xFFFF_FFFF)
    }

    // ── Backward-compatible evaluate ─────────────────────────────────────────

    #[test]
    fn evaluate_all_same_reports_converged() {
        let s0 = make_state(0, &[1, 2, 3]);
        let s1 = make_state(1, &[1, 2, 3]);
        let report = ConvergenceOracle::evaluate(&[s0, s1]);
        assert!(report.converged);
        assert!(report.divergent_agents.is_empty());
        assert_eq!(report.canonical_event_count, 3);
    }

    #[test]
    fn evaluate_divergent_reports_agents() {
        let s0 = make_state(0, &[1, 2, 3]);
        let s1 = make_state(1, &[1, 3]);
        let report = ConvergenceOracle::evaluate(&[s0, s1]);
        assert!(!report.converged);
        assert_eq!(report.divergent_agents, vec![1]);
    }

    #[test]
    fn evaluate_empty_slice_is_converged() {
        let report = ConvergenceOracle::evaluate(&[]);
        assert!(report.converged);
    }

    // ── Invariant 1: Strong convergence ──────────────────────────────────────

    #[test]
    fn check_convergence_identical_states_passes() {
        let s0 = make_state(0, &[10, 20, 30]);
        let s1 = make_state(1, &[10, 20, 30]);
        let s2 = make_state(2, &[10, 20, 30]);
        let result = ConvergenceOracle::check_convergence(&[s0, s1, s2]);
        assert!(result.passed);
        assert!(result.violations.is_empty());
    }

    #[test]
    fn check_convergence_divergent_states_fails() {
        let s0 = make_state(0, &[1, 2, 3]);
        let s1 = make_state(1, &[1, 3]);
        let result = ConvergenceOracle::check_convergence(&[s0, s1]);
        assert!(!result.passed);
        assert_eq!(result.violations.len(), 1);

        match &result.violations[0] {
            InvariantViolation::Convergence {
                agent_a,
                agent_b,
                only_in_a,
                only_in_b,
            } => {
                assert_eq!(*agent_a, 0);
                assert_eq!(*agent_b, 1);
                assert_eq!(only_in_a, &[2_u64]);
                assert!(only_in_b.is_empty());
            }
            other => panic!("unexpected violation: {other:?}"),
        }
    }

    #[test]
    fn check_convergence_reports_all_divergent_pairs() {
        let s0 = make_state(0, &[1, 2, 3]);
        let s1 = make_state(1, &[1, 2]);
        let s2 = make_state(2, &[1]);
        let result = ConvergenceOracle::check_convergence(&[s0, s1, s2]);
        assert!(!result.passed);
        // Pairs (0,1), (0,2), (1,2) all diverge.
        assert_eq!(result.violations.len(), 3);
    }

    #[test]
    fn check_convergence_single_agent_passes() {
        let s0 = make_state(0, &[1, 2, 3]);
        let result = ConvergenceOracle::check_convergence(&[s0]);
        assert!(result.passed);
    }

    // ── Invariant 2: Commutativity ────────────────────────────────────────────

    #[test]
    fn check_commutativity_set_union_always_passes() {
        let events = [1_u64, 2, 3, 4, 5];
        let mut rng = DeterministicRng::new(42);
        let result = ConvergenceOracle::check_commutativity(&events, &mut rng, 16);
        assert!(result.passed, "grow-only set is always commutative");
    }

    #[test]
    fn check_commutativity_single_event_passes() {
        let events = [7_u64];
        let mut rng = DeterministicRng::new(1);
        let result = ConvergenceOracle::check_commutativity(&events, &mut rng, 4);
        assert!(result.passed);
    }

    #[test]
    fn check_commutativity_empty_events_passes() {
        let mut rng = DeterministicRng::new(0);
        let result = ConvergenceOracle::check_commutativity(&[], &mut rng, 4);
        assert!(result.passed);
    }

    #[test]
    fn check_commutativity_detects_non_commutative_merge() {
        // Simulate a non-commutative merge: take-last-seen, i.e. the state is
        // simply the most-recently-applied event.  Different orderings produce
        // different results.  For a take-last merge the canonical state applied in
        // [10,20,30] order is 30; applied in [30,20,10] order is 10.  We reproduce
        // this as a violation by directly constructing the OracleResult (the
        // function targets set-CRDT, but we validate the violation type here).
        let violation = InvariantViolation::Commutativity {
            permutation_index: 0,
            missing_events: vec![30],
            extra_events: vec![10],
        };
        let result = OracleResult::fail(vec![violation]);
        assert!(!result.passed);
        assert_eq!(result.violations.len(), 1);
    }

    // ── Invariant 3: Idempotence ──────────────────────────────────────────────

    #[test]
    fn check_idempotence_set_insert_is_always_idempotent() {
        let events = [1_u64, 2, 3, 4, 5];
        let state = make_state(0, &events);
        let result = ConvergenceOracle::check_idempotence(&state, &events);
        assert!(result.passed, "set insert is idempotent");
    }

    #[test]
    fn check_idempotence_re_applying_unknown_event_is_ok() {
        // After convergence, re-applying an event the agent didn't have yet
        // is NOT a violation — it's a new delivery.  The check applies events
        // to the current (converged) state.
        let state = make_state(0, &[1, 2, 3]);
        let result = ConvergenceOracle::check_idempotence(&state, &[1, 2, 3]);
        assert!(result.passed);
    }

    #[test]
    fn check_idempotence_violation_produces_correct_diagnostic() {
        // Construct a violation directly to test the diagnostic shape.
        let violation = InvariantViolation::Idempotence {
            event_id: 42,
            events_before: vec![1, 2, 3],
            events_after_dup: vec![1, 2, 3, 42],
        };
        let result = OracleResult::fail(vec![violation]);
        assert!(!result.passed);
        match &result.violations[0] {
            InvariantViolation::Idempotence {
                event_id,
                events_before,
                events_after_dup,
            } => {
                assert_eq!(*event_id, 42);
                assert_eq!(events_before, &[1, 2, 3]);
                assert_eq!(events_after_dup, &[1, 2, 3, 42]);
            }
            other => panic!("unexpected violation: {other:?}"),
        }
    }

    // ── Invariant 4: Causal consistency ──────────────────────────────────────

    #[test]
    fn check_causality_contiguous_sequences_pass() {
        // Agent 0: emitted seq=0,1,2 → event_ids (0<<32|0), (0<<32|1), (0<<32|2)
        let e0 = event_id(0, 0);
        let e1 = event_id(0, 1);
        let e2 = event_id(0, 2);
        // Agent 1 knows all three.
        let state = make_state(1, &[e0, e1, e2]);
        let result = ConvergenceOracle::check_causality(&[state]);
        assert!(result.passed);
    }

    #[test]
    fn check_causality_gap_in_sequence_fails() {
        // Agent 1 knows seq=0 and seq=2 from agent 0, but not seq=1.
        let e0 = event_id(0, 0);
        let e2 = event_id(0, 2);
        let state = make_state(1, &[e0, e2]);
        let result = ConvergenceOracle::check_causality(&[state]);
        assert!(!result.passed);

        match &result.violations[0] {
            InvariantViolation::CausalConsistency {
                observer_agent,
                source_agent,
                missing_seq,
                present_higher_seq,
            } => {
                assert_eq!(*observer_agent, 1);
                assert_eq!(*source_agent, 0);
                assert_eq!(*missing_seq, 1);
                assert_eq!(*present_higher_seq, 2);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn check_causality_missing_seq_zero_fails() {
        // Agent 1 knows seq=2 from agent 0 but not seq=0.
        let e2 = event_id(0, 2);
        let state = make_state(1, &[e2]);
        let result = ConvergenceOracle::check_causality(&[state]);
        assert!(!result.passed);

        let has_missing_zero = result.violations.iter().any(|v| {
            matches!(
                v,
                InvariantViolation::CausalConsistency { missing_seq: 0, .. }
            )
        });
        assert!(has_missing_zero, "must report missing seq=0");
    }

    #[test]
    fn check_causality_single_event_per_source_passes() {
        // seq=0 alone is fine; there's no gap.
        let e0 = event_id(3, 0);
        let state = make_state(0, &[e0]);
        let result = ConvergenceOracle::check_causality(&[state]);
        assert!(result.passed);
    }

    #[test]
    fn check_causality_multiple_sources_independent() {
        // Two sources, both complete.
        let a0 = event_id(0, 0);
        let a1 = event_id(0, 1);
        let b0 = event_id(1, 0);
        let b1 = event_id(1, 1);
        let state = make_state(2, &[a0, a1, b0, b1]);
        let result = ConvergenceOracle::check_causality(&[state]);
        assert!(result.passed);
    }

    #[test]
    fn check_causality_violation_in_one_source_only() {
        // Source 0 complete, source 1 has a gap.
        let a0 = event_id(0, 0);
        let a1 = event_id(0, 1);
        let b0 = event_id(1, 0);
        let b2 = event_id(1, 2); // gap: seq=1 missing
        let state = make_state(2, &[a0, a1, b0, b2]);
        let result = ConvergenceOracle::check_causality(&[state]);
        assert!(!result.passed);
        assert_eq!(result.violations.len(), 1);
    }

    // ── Invariant 5: Triage stability ────────────────────────────────────────

    #[test]
    fn check_triage_stability_identical_counts_pass() {
        // All replicas know 3 out of 6 events.
        let s0 = make_state(0, &[1, 2, 3]);
        let s1 = make_state(1, &[1, 2, 3]);
        let result = ConvergenceOracle::check_triage_stability(&[s0, s1], 6, 1e-9);
        assert!(result.passed);
    }

    #[test]
    fn check_triage_stability_divergent_scores_fail() {
        // s0 knows 3/6 events = 0.5; s1 knows 1/6 ≈ 0.167.
        let s0 = make_state(0, &[1, 2, 3]);
        let s1 = make_state(1, &[1]);
        let result = ConvergenceOracle::check_triage_stability(&[s0, s1], 6, 0.01);
        assert!(!result.passed);
        match &result.violations[0] {
            InvariantViolation::TriageStability {
                agent_a,
                agent_b,
                score_a,
                score_b,
                diff,
                epsilon,
            } => {
                assert_eq!(*agent_a, 0);
                assert_eq!(*agent_b, 1);
                assert!((score_a - 0.5).abs() < 1e-9);
                assert!((score_b - (1.0 / 6.0)).abs() < 1e-9);
                assert!(*diff > *epsilon);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn check_triage_stability_zero_total_events_passes() {
        let s0 = make_state(0, &[]);
        let s1 = make_state(1, &[]);
        let result = ConvergenceOracle::check_triage_stability(&[s0, s1], 0, 1e-9);
        assert!(result.passed);
    }

    #[test]
    fn check_triage_stability_within_loose_epsilon_passes() {
        // 2/10 vs 3/10 diff = 0.1; epsilon = 0.2 → passes.
        let s0 = make_state(0, &[1, 2]);
        let s1 = make_state(1, &[1, 2, 3]);
        let result = ConvergenceOracle::check_triage_stability(&[s0, s1], 10, 0.2);
        assert!(result.passed);
    }

    // ── check_all ─────────────────────────────────────────────────────────────

    #[test]
    fn check_all_fully_converged_passes_every_invariant() {
        // 3 agents, all fully converged after simulated delivery.
        let e = |src: u64, seq: u64| event_id(src, seq);
        let all = [e(0, 0), e(0, 1), e(1, 0), e(1, 1), e(2, 0)];
        let s0 = make_state(0, &all);
        let s1 = make_state(1, &all);
        let s2 = make_state(2, &all);

        let mut rng = DeterministicRng::new(77);
        let result = ConvergenceOracle::check_all(&[s0, s1, s2], &all, &mut rng);
        assert!(
            result.passed,
            "all invariants must pass; violations: {:?}",
            result.violations
        );
    }

    #[test]
    fn check_all_divergent_states_fails_convergence() {
        let all = [1_u64, 2, 3, 4];
        let s0 = make_state(0, &all);
        let s1 = make_state(1, &[1, 2]); // divergent
        let mut rng = DeterministicRng::new(7);
        let result = ConvergenceOracle::check_all(&[s0, s1], &all, &mut rng);
        assert!(!result.passed);
        let has_conv = result
            .violations
            .iter()
            .any(|v| matches!(v, InvariantViolation::Convergence { .. }));
        assert!(has_conv);
    }

    #[test]
    fn check_all_causal_violation_fails_causality() {
        let e = |src: u64, seq: u64| event_id(src, seq);
        // Agent 0 knows seq=2 from source 1 but is missing seq=1.
        let state = make_state(0, &[e(0, 0), e(1, 0), e(1, 2)]);
        let events: Vec<u64> = state.known_events.iter().copied().collect();
        let mut rng = DeterministicRng::new(3);
        let result = ConvergenceOracle::check_all(&[state], &events, &mut rng);
        assert!(!result.passed);
        let has_causal = result
            .violations
            .iter()
            .any(|v| matches!(v, InvariantViolation::CausalConsistency { .. }));
        assert!(has_causal);
    }

    // ── Fisher-Yates shuffle ──────────────────────────────────────────────────

    #[test]
    fn fisher_yates_preserves_all_elements() {
        let events = [1_u64, 2, 3, 4, 5, 6, 7, 8];
        let mut rng = DeterministicRng::new(99);
        let shuffled = fisher_yates_shuffle(&events, &mut rng);
        assert_eq!(shuffled.len(), events.len());
        let orig_set: BTreeSet<u64> = events.iter().copied().collect();
        let shuf_set: BTreeSet<u64> = shuffled.iter().copied().collect();
        assert_eq!(orig_set, shuf_set);
    }

    #[test]
    fn different_seeds_produce_different_shuffles() {
        let events = [1_u64, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let mut r1 = DeterministicRng::new(10);
        let mut r2 = DeterministicRng::new(20);
        let s1 = fisher_yates_shuffle(&events, &mut r1);
        let s2 = fisher_yates_shuffle(&events, &mut r2);
        // Different seeds almost certainly produce different orderings for 10
        // elements.  The probability of collision is 1/10! ≈ 2.8e-7.
        assert_ne!(s1, s2);
    }
}
