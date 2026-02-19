use serde::{Deserialize, Serialize};

/// Configuration for generating per-agent simulated clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockConfig {
    /// Base timestamp in milliseconds.
    pub base_millis: i64,
    /// Logical tick size in milliseconds per simulation round.
    pub tick_millis: i64,
    /// Maximum absolute drift in parts-per-million assigned per agent.
    pub max_abs_drift_ppm: i32,
    /// Maximum absolute skew in milliseconds assigned per agent.
    pub max_abs_skew_millis: i64,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            base_millis: 1_700_000_000_000,
            tick_millis: 100,
            max_abs_drift_ppm: 100,
            max_abs_skew_millis: 25,
        }
    }
}

/// Concrete per-agent clock specification (assigned from [`ClockConfig`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockSpec {
    /// Base timestamp in milliseconds.
    pub base_millis: i64,
    /// Tick size in milliseconds per simulation round.
    pub tick_millis: i64,
    /// Drift in parts-per-million.
    pub drift_ppm: i32,
    /// Constant offset from baseline.
    pub skew_millis: i64,
}

/// Simulated wall clock with drift, skew, and freeze controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulatedClock {
    spec: ClockSpec,
    frozen_at: Option<i64>,
}

impl SimulatedClock {
    /// Create a simulated clock from an assigned spec.
    #[must_use]
    pub fn new(spec: ClockSpec) -> Self {
        Self {
            spec,
            frozen_at: None,
        }
    }

    /// Return the assigned spec.
    #[must_use]
    pub fn spec(&self) -> ClockSpec {
        self.spec
    }

    /// Return current wall time in milliseconds for a simulation round.
    #[must_use]
    pub fn now_millis(&self, round: u64) -> i64 {
        if let Some(frozen) = self.frozen_at {
            return frozen;
        }

        let round_i64 = i64::try_from(round).unwrap_or(i64::MAX);
        let base_progress = self.spec.tick_millis.saturating_mul(round_i64);
        let drift_adjust = base_progress
            .saturating_mul(i64::from(self.spec.drift_ppm))
            .saturating_div(1_000_000);

        self.spec
            .base_millis
            .saturating_add(self.spec.skew_millis)
            .saturating_add(base_progress)
            .saturating_add(drift_adjust)
    }

    /// Freeze this clock at the current time.
    pub fn freeze(&mut self, round: u64) {
        self.frozen_at = Some(self.now_millis(round));
    }

    /// Unfreeze this clock.
    pub fn unfreeze(&mut self) {
        self.frozen_at = None;
    }

    /// Whether this clock is currently frozen.
    #[must_use]
    pub fn is_frozen(&self) -> bool {
        self.frozen_at.is_some()
    }
}
