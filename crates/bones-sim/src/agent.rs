use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Stable identifier for a simulated agent.
pub type AgentId = usize;

/// Immutable snapshot of an agent's local state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentState {
    /// Agent identity.
    pub id: AgentId,
    /// Event IDs the agent has observed (including its own emitted events).
    pub known_events: BTreeSet<u64>,
}

/// Event emitted by an agent during simulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmittedEvent {
    /// Emitting agent.
    pub source: AgentId,
    /// Per-agent monotonic sequence.
    pub seq: u64,
    /// Global event identifier used in network delivery.
    pub event_id: u64,
}

/// Minimal CRDT model for simulation: each agent tracks a grow-only set of event IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedAgent {
    id: AgentId,
    next_seq: u64,
    known_events: BTreeSet<u64>,
}

impl SimulatedAgent {
    /// Create a new simulated agent with empty state.
    #[must_use]
    pub fn new(id: AgentId) -> Self {
        Self {
            id,
            next_seq: 0,
            known_events: BTreeSet::new(),
        }
    }

    /// Return this agent's ID.
    #[must_use]
    pub fn id(&self) -> AgentId {
        self.id
    }

    /// Emit a new event and apply it to local state immediately.
    #[must_use]
    pub fn emit_event(&mut self) -> EmittedEvent {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);

        let source_u64 = u64::try_from(self.id).unwrap_or(u64::MAX);
        let event_id = (source_u64 << 32) | (seq & 0xFFFF_FFFF);

        self.known_events.insert(event_id);

        EmittedEvent {
            source: self.id,
            seq,
            event_id,
        }
    }

    /// Apply a delivered event.
    pub fn observe_event(&mut self, event_id: u64) {
        self.known_events.insert(event_id);
    }

    /// Get an immutable snapshot of current local state.
    #[must_use]
    pub fn snapshot(&self) -> AgentState {
        AgentState {
            id: self.id,
            known_events: self.known_events.clone(),
        }
    }
}
