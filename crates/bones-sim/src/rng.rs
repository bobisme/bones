use serde::{Deserialize, Serialize};

/// Tiny deterministic RNG used by the simulator.
///
/// This is intentionally simple and reproducible across platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    /// Create a new deterministic RNG from a seed.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// Next pseudo-random `u64`.
    #[must_use]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.state
    }

    /// Next value in `[0, upper_exclusive)`.
    #[must_use]
    pub fn next_bounded(&mut self, upper_exclusive: u64) -> u64 {
        if upper_exclusive == 0 {
            return 0;
        }
        self.next_u64() % upper_exclusive
    }

    /// Bernoulli trial with integer percent.
    #[must_use]
    pub fn hit_rate_percent(&mut self, percent: u8) -> bool {
        if percent == 0 {
            return false;
        }
        if percent >= 100 {
            return true;
        }
        self.next_bounded(100) < u64::from(percent)
    }
}
