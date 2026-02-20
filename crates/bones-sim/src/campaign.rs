//! Campaign runner for deterministic simulation campaigns.
//!
//! Executes many seeds across configurable parameters, collecting pass/fail
//! results and identifying the first failing seed for replay.

use std::ops::Range;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::oracle::{ConvergenceOracle, InvariantViolation, OracleResult};
use crate::rng::DeterministicRng;
use crate::{SimulationConfig, SimulationResult, Simulator};

/// Campaign-level configuration controlling how many seeds to run and
/// what simulation parameters to use for each seed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CampaignConfig {
    /// Range of seeds to execute, e.g., `0..100`.
    pub seed_range: Range<u64>,
    /// Number of simulated agents per seed.
    pub agent_count: usize,
    /// Number of simulation rounds per seed.
    pub rounds: u64,
    /// Number of peers each emitter sends each event to.
    pub fanout: usize,
    /// Network fault probability for random message drops (percent, 0–100).
    pub fault_drop_percent: u8,
    /// Network fault probability for message duplication (percent, 0–100).
    pub fault_duplicate_percent: u8,
    /// Network fault probability for message reordering (percent, 0–100).
    pub fault_reorder_percent: u8,
    /// Network fault probability for partition toggling (percent, 0–100).
    pub fault_partition_percent: u8,
    /// Maximum delivery delay in rounds.
    pub fault_max_delay: u8,
    /// Clock freeze probability (percent, 0–100).
    pub fault_freeze_percent: u8,
    /// Clock freeze duration in rounds.
    pub fault_freeze_duration: u8,
}

impl Default for CampaignConfig {
    fn default() -> Self {
        Self {
            seed_range: 0..100,
            agent_count: 5,
            rounds: 24,
            fanout: 2,
            fault_drop_percent: 10,
            fault_duplicate_percent: 5,
            fault_reorder_percent: 10,
            fault_partition_percent: 5,
            fault_max_delay: 3,
            fault_freeze_percent: 5,
            fault_freeze_duration: 2,
        }
    }
}

impl CampaignConfig {
    /// Build a [`SimulationConfig`] for a specific seed.
    #[must_use]
    pub fn sim_config_for_seed(&self, seed: u64) -> SimulationConfig {
        use crate::network::FaultConfig;
        SimulationConfig {
            seed,
            agent_count: self.agent_count,
            rounds: self.rounds,
            fanout: self.fanout,
            fault: FaultConfig {
                max_delay_rounds: self.fault_max_delay,
                drop_rate_percent: self.fault_drop_percent,
                duplicate_rate_percent: self.fault_duplicate_percent,
                reorder_rate_percent: self.fault_reorder_percent,
                partition_rate_percent: self.fault_partition_percent,
                freeze_rate_percent: self.fault_freeze_percent,
                freeze_duration_rounds: self.fault_freeze_duration,
            },
            clock: Default::default(),
        }
    }

    /// Validate configuration before running.
    ///
    /// # Errors
    ///
    /// Returns an error if any parameter is out of valid range.
    pub fn validate(&self) -> Result<()> {
        if self.seed_range.is_empty() {
            bail!("seed_range must not be empty");
        }
        if self.agent_count == 0 {
            bail!("agent_count must be > 0");
        }
        if self.rounds == 0 {
            bail!("rounds must be > 0");
        }
        Ok(())
    }
}

/// Failure details for a single seed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeedFailure {
    /// The seed that failed.
    pub seed: u64,
    /// Invariant violations found.
    pub violations: Vec<String>,
}

/// Aggregate report produced by a campaign run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CampaignReport {
    /// Total seeds executed.
    pub seeds_run: usize,
    /// Seeds that passed all invariants.
    pub seeds_passed: usize,
    /// First seed that failed (for prioritized replay).
    pub first_failure: Option<u64>,
    /// All seed failures with violation details.
    pub failures: Vec<SeedFailure>,
    /// Whether at least one seed reached an interesting fault state.
    pub interesting_states_reached: usize,
}

impl CampaignReport {
    /// True if every seed passed.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Detailed trace produced by replaying a single seed.
#[derive(Debug, Clone)]
pub struct DetailedTrace {
    /// The simulation result including full trace and convergence info.
    pub result: SimulationResult,
    /// Oracle check result with violation details.
    pub oracle: OracleResult,
    /// All event IDs produced during the simulation.
    pub all_events: Vec<u64>,
}

/// Run a full campaign across all seeds in the config.
///
/// # Errors
///
/// Returns an error if config validation fails or a simulation encounters
/// an internal error.
pub fn run_campaign(config: &CampaignConfig) -> Result<CampaignReport> {
    config.validate()?;

    let mut seeds_run = 0_usize;
    let mut seeds_passed = 0_usize;
    let mut first_failure: Option<u64> = None;
    let mut failures = Vec::new();
    let mut interesting_states_reached = 0_usize;

    for seed in config.seed_range.clone() {
        seeds_run += 1;

        match run_single_seed(seed, config)? {
            Ok(()) => {
                seeds_passed += 1;
            }
            Err(violations) => {
                if first_failure.is_none() {
                    first_failure = Some(seed);
                }
                failures.push(SeedFailure {
                    seed,
                    violations: violations.iter().map(format_violation).collect(),
                });
            }
        }

        // Track interesting states separately by replaying
        let sim_config = config.sim_config_for_seed(seed);
        let mut sim = Simulator::new(sim_config)?;
        let result = sim.run()?;
        if result.interesting_state_reached {
            interesting_states_reached += 1;
        }
    }

    Ok(CampaignReport {
        seeds_run,
        seeds_passed,
        first_failure,
        failures,
        interesting_states_reached,
    })
}

/// Run a single seed and return Ok(()) on pass, Err(violations) on failure.
///
/// # Errors
///
/// Returns an `anyhow::Error` if the simulation itself encounters an internal
/// error (invalid config, etc). The inner `Result` distinguishes pass from
/// invariant violations.
pub fn run_single_seed(
    seed: u64,
    config: &CampaignConfig,
) -> Result<std::result::Result<(), Vec<InvariantViolation>>> {
    let sim_config = config.sim_config_for_seed(seed);
    let mut simulator = Simulator::new(sim_config)?;
    let result = simulator.run()?;

    // Collect all event IDs from the trace for oracle checks.
    let all_events = collect_emitted_events(&result);

    // Run the full oracle suite.
    let mut oracle_rng = DeterministicRng::new(seed.wrapping_add(0xDEAD));
    let oracle_result =
        ConvergenceOracle::check_all(&result.states, &all_events, &mut oracle_rng);

    if oracle_result.passed {
        Ok(Ok(()))
    } else {
        Ok(Err(oracle_result.violations))
    }
}

/// Replay a single seed with full trace details for debugging.
///
/// # Errors
///
/// Returns an error when config validation or simulation fails.
pub fn replay_seed(seed: u64, config: &CampaignConfig) -> Result<DetailedTrace> {
    config.validate()?;

    let sim_config = config.sim_config_for_seed(seed);
    let mut simulator = Simulator::new(sim_config)?;
    let result = simulator.run()?;

    let all_events = collect_emitted_events(&result);

    let mut oracle_rng = DeterministicRng::new(seed.wrapping_add(0xDEAD));
    let oracle =
        ConvergenceOracle::check_all(&result.states, &all_events, &mut oracle_rng);

    Ok(DetailedTrace {
        result,
        oracle,
        all_events,
    })
}

/// Extract all emitted event IDs from a simulation result's trace.
fn collect_emitted_events(result: &SimulationResult) -> Vec<u64> {
    result
        .trace
        .iter()
        .filter_map(|te| match te.kind {
            crate::TraceEventKind::Emit { event_id, .. } => Some(event_id),
            _ => None,
        })
        .collect()
}

/// Format an invariant violation into a human-readable string.
fn format_violation(v: &InvariantViolation) -> String {
    match v {
        InvariantViolation::Convergence {
            agent_a,
            agent_b,
            only_in_a,
            only_in_b,
        } => {
            format!(
                "Convergence: agents {agent_a} and {agent_b} diverge \
                 (only_in_a={only_in_a:?}, only_in_b={only_in_b:?})"
            )
        }
        InvariantViolation::Commutativity {
            permutation_index,
            missing_events,
            extra_events,
        } => {
            format!(
                "Commutativity: permutation {permutation_index} diverges \
                 (missing={missing_events:?}, extra={extra_events:?})"
            )
        }
        InvariantViolation::Idempotence {
            event_id,
            events_before,
            events_after_dup,
        } => {
            format!(
                "Idempotence: re-applying event {event_id} mutated state \
                 (before={} events, after={} events)",
                events_before.len(),
                events_after_dup.len()
            )
        }
        InvariantViolation::CausalConsistency {
            observer_agent,
            source_agent,
            missing_seq,
            present_higher_seq,
        } => {
            format!(
                "CausalConsistency: agent {observer_agent} has seq={present_higher_seq} \
                 from source {source_agent} but is missing seq={missing_seq}"
            )
        }
        InvariantViolation::TriageStability {
            agent_a,
            agent_b,
            score_a,
            score_b,
            diff,
            epsilon,
        } => {
            format!(
                "TriageStability: agents {agent_a} and {agent_b} scores \
                 diverge ({score_a:.6} vs {score_b:.6}, diff={diff:.6} > epsilon={epsilon:.6})"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn campaign_config_default_is_valid() {
        let config = CampaignConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn campaign_config_empty_seed_range_rejected() {
        let config = CampaignConfig {
            seed_range: 5..5,
            ..CampaignConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn campaign_config_zero_agents_rejected() {
        let config = CampaignConfig {
            agent_count: 0,
            ..CampaignConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn campaign_config_zero_rounds_rejected() {
        let config = CampaignConfig {
            rounds: 0,
            ..CampaignConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn sim_config_for_seed_uses_correct_seed() {
        let config = CampaignConfig::default();
        let sim = config.sim_config_for_seed(42);
        assert_eq!(sim.seed, 42);
        assert_eq!(sim.agent_count, config.agent_count);
        assert_eq!(sim.rounds, config.rounds);
    }

    #[test]
    fn run_single_seed_passes_with_correct_crdt() {
        // Use non-destructive faults only (no drops/partitions) so all events
        // reach all agents via the final drain and convergence is guaranteed.
        let config = CampaignConfig {
            seed_range: 0..1,
            agent_count: 3,
            rounds: 16,
            fanout: 2,
            fault_drop_percent: 0,
            fault_duplicate_percent: 3,
            fault_reorder_percent: 5,
            fault_partition_percent: 0,
            fault_max_delay: 2,
            fault_freeze_percent: 2,
            fault_freeze_duration: 2,
        };
        let result = run_single_seed(0, &config).expect("sim should not error");
        assert!(result.is_ok(), "seed 0 should pass: {result:?}");
    }

    #[test]
    fn run_campaign_all_seeds_pass() {
        // Non-destructive faults: delay, reorder, duplicate are fine (events
        // still arrive). Drop and partition permanently lose events, preventing
        // convergence without a sync protocol.
        let config = CampaignConfig {
            seed_range: 0..10,
            agent_count: 3,
            rounds: 12,
            fanout: 2,
            fault_drop_percent: 0,
            fault_duplicate_percent: 3,
            fault_reorder_percent: 5,
            fault_partition_percent: 0,
            fault_max_delay: 2,
            fault_freeze_percent: 2,
            fault_freeze_duration: 2,
        };
        let report = run_campaign(&config).expect("campaign should not error");
        assert_eq!(report.seeds_run, 10);
        assert_eq!(report.seeds_passed, 10);
        assert!(report.all_passed());
        assert!(report.first_failure.is_none());
        assert!(report.failures.is_empty());
    }

    #[test]
    fn run_campaign_100_seeds_pass() {
        // The acceptance criterion: 100+ seeds without failure.
        // Non-destructive faults only — the final drain delivers all pending
        // events so all agents converge. Fanout = agent_count - 1 (broadcast)
        // to ensure every event reaches every agent.
        let config = CampaignConfig {
            seed_range: 0..100,
            agent_count: 4,
            rounds: 16,
            fanout: 3, // broadcast to all peers (agent_count - 1)
            fault_drop_percent: 0,
            fault_duplicate_percent: 5,
            fault_reorder_percent: 10,
            fault_partition_percent: 0,
            fault_max_delay: 3,
            fault_freeze_percent: 5,
            fault_freeze_duration: 2,
        };
        let report = run_campaign(&config).expect("campaign should not error");
        assert_eq!(report.seeds_run, 100);
        assert!(
            report.all_passed(),
            "campaign failed: {} failures, first at seed {:?}",
            report.failures.len(),
            report.first_failure,
        );
    }

    #[test]
    fn replay_seed_produces_detailed_trace() {
        // Non-destructive faults only so oracle passes.
        let config = CampaignConfig {
            seed_range: 0..1,
            agent_count: 3,
            rounds: 12,
            fault_drop_percent: 0,
            fault_partition_percent: 0,
            ..CampaignConfig::default()
        };
        let trace = replay_seed(42, &config).expect("replay should not error");
        assert!(!trace.result.trace.is_empty());
        assert!(!trace.all_events.is_empty());
        // With correct CRDT and no destructive faults, oracle should pass
        assert!(trace.oracle.passed, "oracle should pass: {:?}", trace.oracle.violations);
    }

    #[test]
    fn replay_is_deterministic() {
        let config = CampaignConfig {
            seed_range: 0..1,
            agent_count: 4,
            rounds: 16,
            ..CampaignConfig::default()
        };

        let trace1 = replay_seed(7, &config).expect("replay 1");
        let trace2 = replay_seed(7, &config).expect("replay 2");

        assert_eq!(trace1.result.trace, trace2.result.trace);
        assert_eq!(trace1.result.states, trace2.result.states);
        assert_eq!(trace1.all_events, trace2.all_events);
    }

    #[test]
    fn campaign_report_serializes_to_json() {
        let report = CampaignReport {
            seeds_run: 10,
            seeds_passed: 9,
            first_failure: Some(7),
            failures: vec![SeedFailure {
                seed: 7,
                violations: vec!["Convergence: agents 0 and 1 diverge".into()],
            }],
            interesting_states_reached: 5,
        };
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(json.contains("\"seeds_run\":10"));
        assert!(json.contains("\"first_failure\":7"));
    }

    #[test]
    fn campaign_reaches_interesting_states() {
        let config = CampaignConfig {
            seed_range: 0..20,
            agent_count: 4,
            rounds: 16,
            fault_drop_percent: 20,
            fault_duplicate_percent: 15,
            fault_reorder_percent: 20,
            fault_partition_percent: 15,
            fault_max_delay: 3,
            fault_freeze_percent: 15,
            fault_freeze_duration: 2,
            ..CampaignConfig::default()
        };
        let report = run_campaign(&config).expect("campaign should not error");
        assert!(
            report.interesting_states_reached > 0,
            "expected some seeds to reach interesting fault states"
        );
    }

    #[test]
    fn format_violation_produces_readable_strings() {
        let v = InvariantViolation::Convergence {
            agent_a: 0,
            agent_b: 1,
            only_in_a: vec![42],
            only_in_b: vec![],
        };
        let s = format_violation(&v);
        assert!(s.contains("Convergence"));
        assert!(s.contains("agents 0 and 1"));
    }
}
