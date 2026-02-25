pub struct IdConfig {
    pub prefix: String,
    pub min_hash_length: usize,
    pub max_hash_length: usize,
    pub max_collision_prob: f64,
}

impl IdConfig {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            min_hash_length: 3,
            max_hash_length: 8,
            max_collision_prob: 0.25,
        }
    }

    #[must_use]
    pub const fn min_hash_length(mut self, len: usize) -> Self {
        self.min_hash_length = len;
        self
    }

    #[must_use]
    pub const fn max_hash_length(mut self, len: usize) -> Self {
        self.max_hash_length = len;
        self
    }

    #[must_use]
    pub const fn max_collision_prob(mut self, prob: f64) -> Self {
        self.max_collision_prob = prob;
        self
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn test_new_with_defaults() {
        let config = IdConfig::new("bd");
        assert_eq!(config.prefix, "bd");
        assert_eq!(config.min_hash_length, 3);
        assert_eq!(config.max_hash_length, 8);
        assert_eq!(config.max_collision_prob, 0.25);
    }

    #[test]
    fn test_builder_chain() {
        let config = IdConfig::new("tk")
            .min_hash_length(4)
            .max_collision_prob(0.10);
        assert_eq!(config.prefix, "tk");
        assert_eq!(config.min_hash_length, 4);
        assert_eq!(config.max_hash_length, 8);
        assert_eq!(config.max_collision_prob, 0.10);
    }

    #[test]
    fn test_builder_all_methods() {
        let config = IdConfig::new("test")
            .min_hash_length(2)
            .max_hash_length(10)
            .max_collision_prob(0.5);
        assert_eq!(config.prefix, "test");
        assert_eq!(config.min_hash_length, 2);
        assert_eq!(config.max_hash_length, 10);
        assert_eq!(config.max_collision_prob, 0.5);
    }

    #[test]
    fn test_inverted_min_max_still_generates() {
        // When min > max, optimal_length loop range is empty,
        // so it falls through to returning max_hash_length.
        let config = IdConfig::new("bd").min_hash_length(10).max_hash_length(5);

        let generator = crate::generate::IdGenerator::new(config);

        // optimal_length should return max (5) since the loop range 10..=5 is empty
        assert_eq!(generator.optimal_length(0), 5);

        // generate should still produce a valid ID
        let id = generator.generate(|nonce| format!("seed-{nonce}").into_bytes(), 0, |_| false);
        assert!(id.starts_with("bd-"));
        let hash_part = &id["bd-".len()..];
        assert_eq!(hash_part.len(), 5);
    }

    #[test]
    fn test_zero_length_config() {
        let config = IdConfig::new("bd").min_hash_length(0).max_hash_length(0);

        let generator = crate::generate::IdGenerator::new(config);
        assert_eq!(generator.optimal_length(0), 0);

        let candidate = generator.candidate(b"test", 0);
        assert_eq!(candidate, "bd-");
    }

    #[test]
    fn test_collision_prob_zero() {
        // max_collision_prob = 0.0 means no length satisfies the threshold
        // (P(collision) for n=0 is exactly 0.0, which is NOT < 0.0)
        let config = IdConfig::new("bd").max_collision_prob(0.0);

        let generator = crate::generate::IdGenerator::new(config);
        assert_eq!(generator.optimal_length(0), 8);
    }

    #[test]
    fn test_collision_prob_one() {
        // max_collision_prob = 1.0 means any length satisfies (P < 1.0 for finite n)
        let config = IdConfig::new("bd").max_collision_prob(1.0);

        let generator = crate::generate::IdGenerator::new(config);
        assert_eq!(generator.optimal_length(0), 3);
        assert_eq!(generator.optimal_length(10), 3);
    }
}
