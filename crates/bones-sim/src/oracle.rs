use crate::agent::AgentState;

/// Convergence check output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvergenceReport {
    /// Whether all agent states are identical.
    pub converged: bool,
    /// Agent IDs that diverged from canonical state.
    pub divergent_agents: Vec<usize>,
    /// Number of events in canonical state.
    pub canonical_event_count: usize,
}

/// Oracle for convergence checking after simulation drain.
pub struct ConvergenceOracle;

impl ConvergenceOracle {
    /// Compare all agent states and detect divergence.
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
}
