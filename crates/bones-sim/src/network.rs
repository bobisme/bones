use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::agent::AgentId;
use crate::rng::DeterministicRng;

/// Fault injection configuration for simulated network delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultConfig {
    /// Maximum delivery delay in rounds.
    pub max_delay_rounds: u8,
    /// Percentage of sends dropped.
    pub drop_rate_percent: u8,
    /// Percentage of sends duplicated.
    pub duplicate_rate_percent: u8,
    /// Percentage chance of reordering ready messages at each tick.
    pub reorder_rate_percent: u8,
    /// Percentage chance per round to toggle a random network partition.
    pub partition_rate_percent: u8,
    /// Percentage chance per round to freeze a random clock.
    pub freeze_rate_percent: u8,
    /// Number of rounds to keep a frozen clock frozen.
    pub freeze_duration_rounds: u8,
}

impl Default for FaultConfig {
    fn default() -> Self {
        Self {
            max_delay_rounds: 2,
            drop_rate_percent: 5,
            duplicate_rate_percent: 3,
            reorder_rate_percent: 5,
            partition_rate_percent: 2,
            freeze_rate_percent: 2,
            freeze_duration_rounds: 2,
        }
    }
}

/// Message carried by the simulated network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkMessage {
    /// Sender.
    pub from: AgentId,
    /// Receiver.
    pub to: AgentId,
    /// Event identifier being replicated.
    pub event_id: u64,
    /// Sender-local sequence.
    pub seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingMessage {
    deliver_at_round: u64,
    message: NetworkMessage,
}

/// Result of a send attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendOutcome {
    /// Message dropped before enqueue.
    pub dropped: bool,
    /// Message was duplicated.
    pub duplicated: bool,
    /// Delay assigned for primary enqueue.
    pub delay_rounds: u8,
}

/// Result of delivering all ready messages for a round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliverOutcome {
    /// Messages delivered this tick.
    pub delivered: Vec<NetworkMessage>,
    /// Whether delivery order was shuffled.
    pub reordered: bool,
}

/// Deterministic fault-injecting network model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedNetwork {
    pending: Vec<PendingMessage>,
    partitioned_agents: BTreeSet<AgentId>,
    fault: FaultConfig,
}

impl SimulatedNetwork {
    /// Create a new network with fault injection.
    #[must_use]
    pub fn new(fault: FaultConfig) -> Self {
        Self {
            pending: Vec::new(),
            partitioned_agents: BTreeSet::new(),
            fault,
        }
    }

    /// Return configured fault options.
    #[must_use]
    pub fn fault_config(&self) -> FaultConfig {
        self.fault
    }

    /// Isolate or reconnect an agent from the network.
    pub fn set_partitioned(&mut self, agent: AgentId, isolated: bool) {
        if isolated {
            self.partitioned_agents.insert(agent);
        } else {
            self.partitioned_agents.remove(&agent);
        }
    }

    /// Test whether an agent is currently partitioned.
    #[must_use]
    pub fn is_partitioned(&self, agent: AgentId) -> bool {
        self.partitioned_agents.contains(&agent)
    }

    /// Number of queued in-flight messages.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Send a message with fault injection.
    #[must_use]
    pub fn send(
        &mut self,
        message: NetworkMessage,
        round: u64,
        rng: &mut DeterministicRng,
    ) -> SendOutcome {
        if self.is_partitioned(message.from) || self.is_partitioned(message.to) {
            return SendOutcome {
                dropped: true,
                duplicated: false,
                delay_rounds: 0,
            };
        }

        if rng.hit_rate_percent(self.fault.drop_rate_percent) {
            return SendOutcome {
                dropped: true,
                duplicated: false,
                delay_rounds: 0,
            };
        }

        let delay_bound = u64::from(self.fault.max_delay_rounds).saturating_add(1);
        let primary_delay_u64 = rng.next_bounded(delay_bound);
        let primary_delay = u8::try_from(primary_delay_u64).unwrap_or(self.fault.max_delay_rounds);

        self.pending.push(PendingMessage {
            deliver_at_round: round.saturating_add(u64::from(primary_delay)),
            message,
        });

        let duplicated = rng.hit_rate_percent(self.fault.duplicate_rate_percent);
        if duplicated {
            self.pending.push(PendingMessage {
                deliver_at_round: round.saturating_add(u64::from(primary_delay)),
                message,
            });
        }

        SendOutcome {
            dropped: false,
            duplicated,
            delay_rounds: primary_delay,
        }
    }

    /// Deliver all messages whose delivery round has arrived.
    #[must_use]
    pub fn deliver_ready(&mut self, round: u64, rng: &mut DeterministicRng) -> DeliverOutcome {
        let mut ready = Vec::new();
        let mut future = Vec::new();

        for pending in self.pending.drain(..) {
            if pending.deliver_at_round <= round {
                ready.push(pending.message);
            } else {
                future.push(pending);
            }
        }

        self.pending = future;

        let should_reorder =
            ready.len() > 1 && rng.hit_rate_percent(self.fault.reorder_rate_percent);
        if should_reorder {
            ready.reverse();
        }

        DeliverOutcome {
            delivered: ready,
            reordered: should_reorder,
        }
    }
}
