#![forbid(unsafe_code)]
//! Deterministic simulation harness for multi-agent Bones behavior.

pub mod agent;
pub mod clock;
pub mod network;
pub mod oracle;
pub mod rng;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::agent::{AgentId, AgentState, SimulatedAgent};
use crate::clock::{ClockConfig, ClockSpec, SimulatedClock};
use crate::network::{FaultConfig, NetworkMessage, SimulatedNetwork};
use crate::oracle::{ConvergenceOracle, ConvergenceReport};
use crate::rng::DeterministicRng;

/// Reason a message was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DropReason {
    /// Dropped due to random loss injection.
    RandomLoss,
    /// Dropped because sender or receiver is partitioned.
    Partition,
}

/// A single deterministic simulation trace entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEvent {
    /// Simulation round.
    pub round: u64,
    /// Event payload.
    pub kind: TraceEventKind,
}

/// Trace event payload variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraceEventKind {
    /// A local event emission.
    Emit {
        /// Emitting agent.
        agent: AgentId,
        /// Event ID.
        event_id: u64,
        /// Sender-local sequence.
        seq: u64,
    },
    /// Successful network enqueue.
    Send {
        /// Sender.
        from: AgentId,
        /// Receiver.
        to: AgentId,
        /// Event ID.
        event_id: u64,
        /// Assigned network delay.
        delay_rounds: u8,
        /// Whether duplicated.
        duplicated: bool,
    },
    /// Message dropped by fault injector.
    Drop {
        /// Sender.
        from: AgentId,
        /// Receiver.
        to: AgentId,
        /// Event ID.
        event_id: u64,
        /// Why it dropped.
        reason: DropReason,
    },
    /// Message delivered to receiver.
    Deliver {
        /// Sender.
        from: AgentId,
        /// Receiver.
        to: AgentId,
        /// Event ID.
        event_id: u64,
    },
    /// Ready messages were reordered before delivery.
    Reorder {
        /// Number of delivered messages reordered.
        delivered_count: usize,
    },
    /// A network partition toggled for one agent.
    Partition {
        /// Agent affected.
        agent: AgentId,
        /// New isolation state.
        isolated: bool,
    },
    /// A clock was frozen for a bounded period.
    ClockFreeze {
        /// Agent clock frozen.
        agent: AgentId,
        /// Freeze expires at this round (exclusive).
        until_round: u64,
    },
}

/// Top-level simulation configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationConfig {
    /// RNG seed controlling all nondeterminism.
    pub seed: u64,
    /// Number of simulated agents.
    pub agent_count: usize,
    /// Number of simulation rounds before final drain.
    pub rounds: u64,
    /// Number of peers each emitter sends each event to.
    pub fanout: usize,
    /// Fault injection configuration.
    pub fault: FaultConfig,
    /// Clock modeling configuration.
    pub clock: ClockConfig,
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            agent_count: 4,
            rounds: 24,
            fanout: 2,
            fault: FaultConfig::default(),
            clock: ClockConfig::default(),
        }
    }
}

/// Deterministic replay descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedReplay {
    /// Full simulation config required for exact replay.
    pub config: SimulationConfig,
}

impl SeedReplay {
    /// Create replay metadata from config.
    #[must_use]
    pub fn from_config(config: &SimulationConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }

    /// Execute replay and return a deterministic result.
    ///
    /// # Errors
    ///
    /// Returns an error when config validation fails.
    pub fn replay(&self) -> Result<SimulationResult> {
        let mut simulator = Simulator::new(self.config.clone())?;
        simulator.run()
    }
}

/// Completed simulation run with trace and convergence output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationResult {
    /// Full deterministic execution trace.
    pub trace: Vec<TraceEvent>,
    /// Final per-agent states after drain.
    pub states: Vec<AgentState>,
    /// Convergence report from oracle.
    pub convergence: ConvergenceReport,
    /// Whether at least one interesting fault state occurred.
    pub interesting_state_reached: bool,
}

impl SimulationResult {
    /// Stable fingerprint for comparing traces across reruns.
    #[must_use]
    pub fn trace_fingerprint(&self) -> u64 {
        self.trace.iter().fold(0_u64, |acc, item| {
            let encoded = format!("{:?}", item);
            encoded.as_bytes().iter().fold(acc, |inner, byte| {
                inner.wrapping_mul(131).wrapping_add(u64::from(*byte))
            })
        })
    }
}

/// Deterministic simulator state machine.
pub struct Simulator {
    config: SimulationConfig,
    agents: Vec<SimulatedAgent>,
    clocks: Vec<SimulatedClock>,
    clock_unfreeze_round: Vec<Option<u64>>,
    network: SimulatedNetwork,
    partitioned: Vec<bool>,
    rng: DeterministicRng,
}

impl Simulator {
    /// Build a simulator from config.
    ///
    /// # Errors
    ///
    /// Returns an error if configuration is invalid.
    pub fn new(config: SimulationConfig) -> Result<Self> {
        if config.agent_count == 0 {
            bail!("agent_count must be > 0");
        }
        if config.rounds == 0 {
            bail!("rounds must be > 0");
        }

        let mut rng = DeterministicRng::new(config.seed);
        let mut clocks = Vec::with_capacity(config.agent_count);

        for _ in 0..config.agent_count {
            let drift_ppm = sample_signed_i32(&mut rng, config.clock.max_abs_drift_ppm);
            let skew_millis = sample_signed_i64(&mut rng, config.clock.max_abs_skew_millis);
            clocks.push(SimulatedClock::new(ClockSpec {
                base_millis: config.clock.base_millis,
                tick_millis: config.clock.tick_millis,
                drift_ppm,
                skew_millis,
            }));
        }

        let agents = (0..config.agent_count)
            .map(SimulatedAgent::new)
            .collect::<Vec<_>>();

        Ok(Self {
            clock_unfreeze_round: vec![None; config.agent_count],
            partitioned: vec![false; config.agent_count],
            network: SimulatedNetwork::new(config.fault),
            config,
            agents,
            clocks,
            rng,
        })
    }

    /// Execute simulation rounds and final drain.
    ///
    /// # Errors
    ///
    /// Returns an error when internal invariants fail.
    pub fn run(&mut self) -> Result<SimulationResult> {
        let mut trace = Vec::new();

        for round in 0..self.config.rounds {
            self.progress_clock_freezes(round);
            self.maybe_toggle_partition(round, &mut trace);
            self.maybe_freeze_clock(round, &mut trace);

            for agent_idx in 0..self.agents.len() {
                let emitted = self.agents[agent_idx].emit_event();
                trace.push(TraceEvent {
                    round,
                    kind: TraceEventKind::Emit {
                        agent: emitted.source,
                        event_id: emitted.event_id,
                        seq: emitted.seq,
                    },
                });

                let targets = self.pick_targets(agent_idx);
                for target in targets {
                    let message = NetworkMessage {
                        from: agent_idx,
                        to: target,
                        event_id: emitted.event_id,
                        seq: emitted.seq,
                    };

                    let partition_blocked = self.network.is_partitioned(message.from)
                        || self.network.is_partitioned(message.to);

                    let outcome = self.network.send(message, round, &mut self.rng);

                    if outcome.dropped {
                        trace.push(TraceEvent {
                            round,
                            kind: TraceEventKind::Drop {
                                from: message.from,
                                to: message.to,
                                event_id: message.event_id,
                                reason: if partition_blocked {
                                    DropReason::Partition
                                } else {
                                    DropReason::RandomLoss
                                },
                            },
                        });
                    } else {
                        trace.push(TraceEvent {
                            round,
                            kind: TraceEventKind::Send {
                                from: message.from,
                                to: message.to,
                                event_id: message.event_id,
                                delay_rounds: outcome.delay_rounds,
                                duplicated: outcome.duplicated,
                            },
                        });
                    }
                }
            }

            self.deliver_round(round, &mut trace);
        }

        self.final_drain(&mut trace);

        let states = self
            .agents
            .iter()
            .map(SimulatedAgent::snapshot)
            .collect::<Vec<_>>();

        let convergence = ConvergenceOracle::evaluate(&states);
        let interesting_state_reached = trace.iter().any(|event| {
            matches!(
                event.kind,
                TraceEventKind::Drop { .. }
                    | TraceEventKind::Reorder { .. }
                    | TraceEventKind::Partition { .. }
                    | TraceEventKind::ClockFreeze { .. }
            )
        });

        Ok(SimulationResult {
            trace,
            states,
            convergence,
            interesting_state_reached,
        })
    }

    fn deliver_round(&mut self, round: u64, trace: &mut Vec<TraceEvent>) {
        let delivered = self.network.deliver_ready(round, &mut self.rng);

        if delivered.reordered {
            trace.push(TraceEvent {
                round,
                kind: TraceEventKind::Reorder {
                    delivered_count: delivered.delivered.len(),
                },
            });
        }

        for message in delivered.delivered {
            if let Some(agent) = self.agents.get_mut(message.to) {
                agent.observe_event(message.event_id);
            }
            trace.push(TraceEvent {
                round,
                kind: TraceEventKind::Deliver {
                    from: message.from,
                    to: message.to,
                    event_id: message.event_id,
                },
            });
        }
    }

    fn final_drain(&mut self, trace: &mut Vec<TraceEvent>) {
        let mut drain_round = self.config.rounds;
        let drain_limit = self.config.rounds.saturating_add(1_000);

        while self.network.pending_len() > 0 && drain_round < drain_limit {
            self.deliver_round(drain_round, trace);
            drain_round = drain_round.saturating_add(1);
        }
    }

    fn pick_targets(&mut self, source: AgentId) -> Vec<AgentId> {
        if self.agents.len() <= 1 {
            return Vec::new();
        }

        let max_targets = self.config.fanout.min(self.agents.len().saturating_sub(1));
        let mut targets = Vec::new();

        while targets.len() < max_targets {
            let len_u64 = u64::try_from(self.agents.len()).unwrap_or(1);
            let candidate_u64 = self.rng.next_bounded(len_u64);
            let candidate = usize::try_from(candidate_u64).unwrap_or(0);
            if candidate != source && !targets.contains(&candidate) {
                targets.push(candidate);
            }
        }

        targets
    }

    fn maybe_toggle_partition(&mut self, round: u64, trace: &mut Vec<TraceEvent>) {
        if !self
            .rng
            .hit_rate_percent(self.config.fault.partition_rate_percent)
        {
            return;
        }

        let len_u64 = u64::try_from(self.partitioned.len()).unwrap_or(1);
        let idx_u64 = self.rng.next_bounded(len_u64);
        let idx = usize::try_from(idx_u64).unwrap_or(0);

        self.partitioned[idx] = !self.partitioned[idx];
        self.network.set_partitioned(idx, self.partitioned[idx]);

        trace.push(TraceEvent {
            round,
            kind: TraceEventKind::Partition {
                agent: idx,
                isolated: self.partitioned[idx],
            },
        });
    }

    fn progress_clock_freezes(&mut self, round: u64) {
        for (idx, maybe_until) in self.clock_unfreeze_round.iter_mut().enumerate() {
            if let Some(until_round) = *maybe_until
                && round >= until_round
            {
                if let Some(clock) = self.clocks.get_mut(idx) {
                    clock.unfreeze();
                }
                *maybe_until = None;
            }
        }
    }

    fn maybe_freeze_clock(&mut self, round: u64, trace: &mut Vec<TraceEvent>) {
        if self.config.fault.freeze_duration_rounds == 0 {
            return;
        }
        if !self
            .rng
            .hit_rate_percent(self.config.fault.freeze_rate_percent)
        {
            return;
        }

        let len_u64 = u64::try_from(self.clocks.len()).unwrap_or(1);
        let idx_u64 = self.rng.next_bounded(len_u64);
        let idx = usize::try_from(idx_u64).unwrap_or(0);

        if let Some(clock) = self.clocks.get_mut(idx)
            && !clock.is_frozen()
        {
            clock.freeze(round);
            let until_round =
                round.saturating_add(u64::from(self.config.fault.freeze_duration_rounds));
            self.clock_unfreeze_round[idx] = Some(until_round);
            trace.push(TraceEvent {
                round,
                kind: TraceEventKind::ClockFreeze {
                    agent: idx,
                    until_round,
                },
            });
        }
    }
}

/// "Sometimes" assertion helper: verify interesting fault states are reachable.
///
/// # Errors
///
/// Returns an error when any attempted simulation has invalid config.
pub fn sometimes_reaches_interesting_state(base: &SimulationConfig, attempts: u32) -> Result<bool> {
    let mut seed = base.seed;

    for _ in 0..attempts {
        let mut config = base.clone();
        config.seed = seed;

        let mut simulator = Simulator::new(config)?;
        let result = simulator.run()?;
        if result.interesting_state_reached {
            return Ok(true);
        }

        seed = seed.saturating_add(1);
    }

    Ok(false)
}

fn sample_signed_i32(rng: &mut DeterministicRng, max_abs: i32) -> i32 {
    if max_abs <= 0 {
        return 0;
    }

    let span = i64::from(max_abs)
        .saturating_mul(2)
        .saturating_add(1)
        .max(1);
    let span_u64 = u64::try_from(span).unwrap_or(1);
    let sampled = i64::try_from(rng.next_bounded(span_u64)).unwrap_or(0) - i64::from(max_abs);

    i32::try_from(sampled).unwrap_or(0)
}

fn sample_signed_i64(rng: &mut DeterministicRng, max_abs: i64) -> i64 {
    if max_abs <= 0 {
        return 0;
    }

    let span = max_abs.saturating_mul(2).saturating_add(1).max(1);
    let span_u64 = u64::try_from(span).unwrap_or(1);
    let sampled = i64::try_from(rng.next_bounded(span_u64)).unwrap_or(0);

    sampled.saturating_sub(max_abs)
}

#[cfg(test)]
mod tests {
    use crate::agent::AgentState;
    use crate::oracle::ConvergenceOracle;

    use super::{
        FaultConfig, SeedReplay, SimulationConfig, Simulator, sometimes_reaches_interesting_state,
    };

    #[test]
    fn same_seed_produces_identical_trace() {
        let config = SimulationConfig {
            seed: 7,
            rounds: 16,
            ..SimulationConfig::default()
        };

        let mut left = Simulator::new(config.clone()).expect("valid config");
        let mut right = Simulator::new(config).expect("valid config");

        let left_result = left.run().expect("run left");
        let right_result = right.run().expect("run right");

        assert_eq!(left_result.trace, right_result.trace);
        assert_eq!(
            left_result.trace_fingerprint(),
            right_result.trace_fingerprint()
        );
    }

    #[test]
    fn seed_replay_reproduces_execution() {
        let config = SimulationConfig {
            seed: 1234,
            rounds: 20,
            ..SimulationConfig::default()
        };

        let mut sim = Simulator::new(config.clone()).expect("valid config");
        let original = sim.run().expect("original run");

        let replay = SeedReplay::from_config(&config);
        let replayed = replay.replay().expect("replayed run");

        assert_eq!(original.trace, replayed.trace);
        assert_eq!(original.states, replayed.states);
    }

    #[test]
    fn network_faults_are_observable() {
        let config = SimulationConfig {
            seed: 99,
            rounds: 12,
            fanout: 3,
            fault: FaultConfig {
                max_delay_rounds: 3,
                drop_rate_percent: 40,
                duplicate_rate_percent: 30,
                reorder_rate_percent: 40,
                partition_rate_percent: 30,
                freeze_rate_percent: 30,
                freeze_duration_rounds: 2,
            },
            ..SimulationConfig::default()
        };

        let mut simulator = Simulator::new(config).expect("valid config");
        let result = simulator.run().expect("run");

        assert!(result.interesting_state_reached);
    }

    #[test]
    fn convergence_oracle_detects_divergence() {
        let state_a = AgentState {
            id: 0,
            known_events: [1_u64, 2_u64, 3_u64].into_iter().collect(),
        };
        let state_b = AgentState {
            id: 1,
            known_events: [1_u64, 3_u64].into_iter().collect(),
        };

        let report = ConvergenceOracle::evaluate(&[state_a, state_b]);
        assert!(!report.converged);
        assert_eq!(report.divergent_agents, vec![1]);
    }

    #[test]
    fn sometimes_assertion_reaches_interesting_state() {
        let base = SimulationConfig {
            seed: 500,
            rounds: 8,
            fault: FaultConfig {
                max_delay_rounds: 2,
                drop_rate_percent: 20,
                duplicate_rate_percent: 15,
                reorder_rate_percent: 20,
                partition_rate_percent: 15,
                freeze_rate_percent: 15,
                freeze_duration_rounds: 2,
            },
            ..SimulationConfig::default()
        };

        let seen = sometimes_reaches_interesting_state(&base, 12).expect("sometimes assertion");
        assert!(seen);
    }
}
