use crate::config::IdConfig;

/// ID generator with adaptive length and collision avoidance.
pub struct IdGenerator {
    config: IdConfig,
}

impl IdGenerator {
    /// Create a new ID generator with the given config.
    #[must_use]
    pub const fn new(config: IdConfig) -> Self {
        Self { config }
    }

    /// Get the prefix for this generator.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.config.prefix
    }

    /// Compute optimal hash length using the birthday problem approximation.
    ///
    /// Finds the shortest length where `P(collision) = 1 - e^(-n^2 / 2d) < max_collision_prob`,
    /// where `d = 36^length` (the size of the ID space at that length).
    ///
    /// Starting from `min_hash_length`, returns the first length that satisfies the threshold.
    /// If no length up to `max_hash_length` satisfies it, returns `max_hash_length`.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    pub fn optimal_length(&self, item_count: usize) -> usize {
        let n = item_count as f64;

        for length in self.config.min_hash_length..=self.config.max_hash_length {
            // length is bounded by max_hash_length (default 8), safe to cast
            let d = 36_f64.powi(length as i32);
            let exponent = -((n.powi(2)) / (2.0 * d));
            let p_collision = 1.0 - exponent.exp();

            if p_collision < self.config.max_collision_prob {
                return length;
            }
        }

        // If no length up to max found, return max_hash_length
        self.config.max_hash_length
    }

    /// Generate a candidate ID at a specific hash length.
    ///
    /// Returns a string formatted as `{prefix}-{hash}`, where hash is the base36
    /// hash of the seed bytes truncated/padded to the specified length.
    pub fn candidate(&self, seed: impl AsRef<[u8]>, hash_length: usize) -> String {
        let hash_str = crate::hash::hash(seed, hash_length);
        let prefix = &self.config.prefix;
        format!("{prefix}-{hash_str}")
    }

    /// Generate an ID with full collision avoidance.
    ///
    /// Uses a multi-tier strategy:
    /// 1. Nonce escalation: try nonces 0-9 at optimal length
    /// 2. Length extension: increment length, repeat up to `max_hash_length`
    /// 3. Long fallback: 12-char hashes, nonces 0-1000
    /// 4. Desperate fallback: append nonce number to hash
    ///
    /// `seed_fn` is called with the nonce (0, 1, 2, ...) and should return seed bytes.
    /// `item_count` is the current number of existing items.
    /// `exists` returns true if a candidate ID is already taken.
    pub fn generate<S, F>(&self, seed_fn: S, item_count: usize, exists: F) -> String
    where
        S: Fn(u32) -> Vec<u8>,
        F: Fn(&str) -> bool,
    {
        // Phase 1: Nonce escalation at optimal length
        let optimal = self.optimal_length(item_count);
        for nonce in 0..10 {
            let seed = seed_fn(nonce);
            let candidate = self.candidate(&seed, optimal);
            if !exists(&candidate) {
                return candidate;
            }
        }

        // Phase 2: Length extension (increment length, repeat up to max_hash_length)
        for length in (optimal + 1)..=self.config.max_hash_length {
            for nonce in 0..10 {
                let seed = seed_fn(nonce);
                let candidate = self.candidate(&seed, length);
                if !exists(&candidate) {
                    return candidate;
                }
            }
        }

        // Phase 3: Long fallback (12-char hashes, nonces 0-1000)
        for nonce in 0..=1000 {
            let seed = seed_fn(nonce);
            let candidate = self.candidate(&seed, 12);
            if !exists(&candidate) {
                return candidate;
            }
        }

        // Phase 4: Desperate fallback (append nonce number to the hash)
        // This guarantees uniqueness since we're appending the nonce directly
        let seed = seed_fn(0);
        let hash_str = crate::hash::hash(&seed, 12);
        let prefix = &self.config.prefix;
        for nonce in 0..=10000 {
            let desperate = format!("{prefix}-{hash_str}{nonce}");
            if !exists(&desperate) {
                return desperate;
            }
        }

        // Absolute fallback: should never reach here in practice
        let fallback_hash = crate::hash::hash(seed_fn(0), 12);
        format!("{prefix}-{fallback_hash}.fallback")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_new_creates_generator() {
        let config = IdConfig::new("bd");
        let generator = IdGenerator::new(config);
        assert_eq!(generator.prefix(), "bd");
    }

    #[test]
    fn test_prefix_accessor() {
        let generator = IdGenerator::new(IdConfig::new("tk"));
        assert_eq!(generator.prefix(), "tk");
    }

    #[test]
    fn test_optimal_length_small_count() {
        let generator = IdGenerator::new(IdConfig::new("bd"));
        // For 50 items, should use 3 chars (36^3 = 46,656)
        let len = generator.optimal_length(50);
        assert_eq!(len, 3);
    }

    #[test]
    fn test_optimal_length_medium_count() {
        let generator = IdGenerator::new(IdConfig::new("bd"));
        // For 200 items, should use 4 chars (36^4 = 1,679,616)
        let len = generator.optimal_length(200);
        assert_eq!(len, 4);
    }

    #[test]
    fn test_optimal_length_large_count() {
        let generator = IdGenerator::new(IdConfig::new("bd"));
        // For 7000 items, should use 6 chars
        // (36^5 = 60M gives p ≈ 0.33; 36^6 = 2.1B gives p ≈ 0.01)
        let len = generator.optimal_length(7000);
        assert_eq!(len, 6);
    }

    #[test]
    fn test_optimal_length_respects_min() {
        let generator = IdGenerator::new(IdConfig::new("bd").min_hash_length(4));
        // Even for 0 items, should use at least min_hash_length
        let len = generator.optimal_length(0);
        assert_eq!(len, 4);
    }

    #[test]
    fn test_optimal_length_respects_max() {
        let generator = IdGenerator::new(IdConfig::new("bd").max_hash_length(5));
        // For very large item count, should not exceed max_hash_length
        let len = generator.optimal_length(1_000_000_000);
        assert_eq!(len, 5);
    }

    #[test]
    fn test_optimal_length_custom_collision_prob() {
        let generator_strict = IdGenerator::new(
            IdConfig::new("bd").max_collision_prob(0.01), // stricter threshold
        );
        let len_strict = generator_strict.optimal_length(100);

        let generator_loose = IdGenerator::new(
            IdConfig::new("bd").max_collision_prob(0.50), // looser threshold
        );
        let len_loose = generator_loose.optimal_length(100);

        // Stricter should be >= looser
        assert!(len_strict >= len_loose);
    }

    #[test]
    fn test_candidate_format() {
        let generator = IdGenerator::new(IdConfig::new("bd"));
        let seed = b"test seed";
        let candidate = generator.candidate(seed, 6);

        // Should be in format "prefix-hash"
        assert!(candidate.starts_with("bd-"));
        let parts: Vec<&str> = candidate.split('-').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "bd");
        assert_eq!(parts[1].len(), 6);
    }

    #[test]
    fn test_candidate_deterministic() {
        let generator = IdGenerator::new(IdConfig::new("bd"));
        let seed = b"consistent seed";

        let c1 = generator.candidate(seed, 8);
        let c2 = generator.candidate(seed, 8);
        assert_eq!(c1, c2);
    }

    #[test]
    fn test_candidate_different_seeds_different_hashes() {
        let generator = IdGenerator::new(IdConfig::new("bd"));

        let c1 = generator.candidate(b"seed1", 8);
        let c2 = generator.candidate(b"seed2", 8);
        assert_ne!(c1, c2);
    }

    #[test]
    fn test_candidate_different_lengths() {
        let generator = IdGenerator::new(IdConfig::new("bd"));
        let seed = b"test";

        let c3 = generator.candidate(seed, 3);
        let c8 = generator.candidate(seed, 8);

        assert_eq!(c3.len(), "bd-".len() + 3);
        assert_eq!(c8.len(), "bd-".len() + 8);
    }

    #[test]
    fn test_generate_no_collisions_simple() {
        let generator = IdGenerator::new(IdConfig::new("bd"));
        let mut generated = vec![];

        for i in 0..10 {
            let id = generator.generate(
                |nonce| format!("seed{i}-{nonce}").into_bytes(),
                0,
                |candidate| generated.contains(&candidate.to_string()),
            );

            generated.push(id);
        }

        // Check uniqueness
        let unique_count = {
            let mut set = std::collections::HashSet::new();
            for id in &generated {
                set.insert(id.clone());
            }
            set.len()
        };
        assert_eq!(unique_count, 10);
    }

    #[test]
    fn test_generate_deterministic_seed() {
        let generator = IdGenerator::new(IdConfig::new("bd"));

        let id = generator.generate(
            |_nonce| b"fixed-seed".to_vec(),
            0,
            |_| false,
        );

        assert!(id.starts_with("bd-"));
        assert!(!id.is_empty());
    }

    #[test]
    fn test_generate_with_high_item_count() {
        let generator = IdGenerator::new(IdConfig::new("bd"));

        let id = generator.generate(
            |nonce| format!("content-{nonce}").into_bytes(),
            250_000,            // High item count should use longer hashes
            |_candidate| false, // No collisions
        );

        // Should generate a valid ID
        assert!(id.starts_with("bd-"));

        // Optimal length for 250k items should be 6 or more
        let optimal = generator.optimal_length(250_000);
        assert!(optimal >= 5);
    }

    #[test]
    fn test_generate_format() {
        let generator = IdGenerator::new(IdConfig::new("test"));

        let id = generator.generate(
            |nonce| format!("data-{nonce}").into_bytes(),
            10,
            |_| false,
        );

        // Check format: prefix-hash
        assert!(id.starts_with("test-"));
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts[0], "test");
    }

    #[test]
    fn test_optimal_length_boundary_100() {
        let generator = IdGenerator::new(IdConfig::new("bd"));
        // Just under 100
        assert_eq!(generator.optimal_length(99), 3);
        // At 100
        let len_100 = generator.optimal_length(100);
        assert!(len_100 >= 3);
    }

    #[test]
    fn test_optimal_length_boundary_200() {
        let generator = IdGenerator::new(IdConfig::new("bd"));
        // Around 200 should transition to 4
        let len_200 = generator.optimal_length(200);
        assert!(len_200 >= 3);
    }

    #[test]
    fn test_multiple_generators_independent() {
        let gen1 = IdGenerator::new(IdConfig::new("bd"));
        let gen2 = IdGenerator::new(IdConfig::new("tk"));

        assert_eq!(gen1.prefix(), "bd");
        assert_eq!(gen2.prefix(), "tk");

        let opt1 = gen1.optimal_length(100);
        let opt2 = gen2.optimal_length(100);

        // Should have same optimal length (same config)
        assert_eq!(opt1, opt2);
    }

    #[test]
    fn test_generate_with_string_seed() {
        let generator = IdGenerator::new(IdConfig::new("bd"));

        let id = generator.generate(
            |nonce| format!("user-description-{nonce}").into_bytes(),
            50,
            |_| false,
        );

        assert!(id.starts_with("bd-"));
    }

    #[test]
    fn test_candidate_all_valid_base36() {
        let generator = IdGenerator::new(IdConfig::new("bd"));

        for seed_val in 0..100 {
            let candidate = generator.candidate(format!("seed-{seed_val}").as_bytes(), 6);

            // Extract hash part
            let parts: Vec<&str> = candidate.split('-').collect();
            assert_eq!(parts.len(), 2);

            let hash = parts[1];
            for ch in hash.chars() {
                assert!(
                    ch.is_ascii_digit() || ch.is_ascii_lowercase(),
                    "Invalid base36 char in {candidate}: {ch}"
                );
            }
        }
    }

    #[test]
    fn test_generate_always_returns_valid_format() {
        let generator = IdGenerator::new(IdConfig::new("prefix"));

        for item_count in &[0, 1, 10, 100, 1000, 10000] {
            let id = generator.generate(
                |nonce| format!("seed-{nonce}").into_bytes(),
                *item_count,
                |_| false,
            );

            // Basic format validation
            assert!(id.starts_with("prefix-"));
            let hash_part = &id[7..]; // Skip "prefix-"
            assert!(!hash_part.is_empty());

            // All chars should be valid base36 or our fallback format
            for ch in hash_part.chars() {
                assert!(
                    ch.is_ascii_digit() || ch.is_ascii_lowercase(),
                    "Invalid char in {id}: {ch}"
                );
            }
        }
    }

    // ========== Phase 2: Length extension tests ==========

    #[test]
    fn test_generate_phase2_length_extension() {
        // min=3, max=5 so phase 2 can extend to lengths 4 and 5.
        let generator = IdGenerator::new(IdConfig::new("bd").min_hash_length(3).max_hash_length(5));

        // Collect all phase 1 candidates (nonces 0-9 at optimal length 3)
        let optimal = generator.optimal_length(0);
        assert_eq!(optimal, 3);

        let mut phase1_candidates: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for nonce in 0..10 {
            let seed = format!("seed-{nonce}").into_bytes();
            let candidate = generator.candidate(&seed, optimal);
            phase1_candidates.insert(candidate);
        }

        // exists_fn rejects all phase 1 candidates, forcing phase 2
        let id = generator.generate(
            |nonce| format!("seed-{nonce}").into_bytes(),
            0,
            |candidate| phase1_candidates.contains(candidate),
        );

        // The result should have a longer hash (4 or 5 chars, not 3)
        assert!(id.starts_with("bd-"));
        let hash_part = &id["bd-".len()..];
        assert!(
            hash_part.len() > optimal,
            "Expected hash longer than {} chars, got '{}' ({})",
            optimal,
            hash_part,
            hash_part.len()
        );
    }

    #[test]
    fn test_generate_phase2_exhausts_multiple_lengths() {
        // min=3, max=5: phase 1 at length 3, phase 2 tries lengths 4 and 5
        let generator = IdGenerator::new(IdConfig::new("bd").min_hash_length(3).max_hash_length(5));

        // Collect all candidates for lengths 3 AND 4 (nonces 0-9 each)
        let mut reject: std::collections::HashSet<String> = std::collections::HashSet::new();
        for length in 3..=4 {
            for nonce in 0..10 {
                let seed = format!("seed-{nonce}").into_bytes();
                let candidate = generator.candidate(&seed, length);
                reject.insert(candidate);
            }
        }

        let id = generator.generate(
            |nonce| format!("seed-{nonce}").into_bytes(),
            0,
            |candidate| reject.contains(candidate),
        );

        // Must have length 5 hash (skipped past 3 and 4)
        assert!(id.starts_with("bd-"));
        let hash_part = &id["bd-".len()..];
        assert_eq!(
            hash_part.len(),
            5,
            "Expected 5-char hash, got '{}' ({})",
            hash_part,
            hash_part.len()
        );
    }

    // ========== Phase 3: Long fallback tests ==========

    #[test]
    fn test_generate_phase3_long_fallback() {
        // min=max=3 so phase 2 has no room to extend, forcing phase 3
        let generator = IdGenerator::new(IdConfig::new("bd").min_hash_length(3).max_hash_length(3));

        // Collect all phase 1 candidates (nonces 0-9 at length 3)
        let mut reject: std::collections::HashSet<String> = std::collections::HashSet::new();
        for nonce in 0..10 {
            let seed = format!("seed-{nonce}").into_bytes();
            let candidate = generator.candidate(&seed, 3);
            reject.insert(candidate);
        }
        // Phase 2 range is (3+1)..=3 which is empty, so we jump straight to phase 3.

        let id = generator.generate(
            |nonce| format!("seed-{nonce}").into_bytes(),
            0,
            |candidate| reject.contains(candidate),
        );

        // Phase 3 generates 12-char hashes
        assert!(id.starts_with("bd-"));
        let hash_part = &id["bd-".len()..];
        assert_eq!(
            hash_part.len(),
            12,
            "Expected 12-char hash from phase 3, got '{}' ({})",
            hash_part,
            hash_part.len()
        );
    }

    // ========== Phase 4: Desperate fallback tests ==========

    #[test]
    fn test_generate_phase4_desperate_fallback() {
        // min=max=3 so phase 2 is empty
        let generator = IdGenerator::new(IdConfig::new("bd").min_hash_length(3).max_hash_length(3));

        // Reject ALL candidates from phases 1-3.
        // Phase 1: nonces 0-9 at length 3
        // Phase 2: empty (min=max=3)
        // Phase 3: nonces 0-1000 at length 12
        let mut reject: std::collections::HashSet<String> = std::collections::HashSet::new();
        for nonce in 0..10 {
            let seed = format!("seed-{nonce}").into_bytes();
            reject.insert(generator.candidate(&seed, 3));
        }
        for nonce in 0..=1000 {
            let seed = format!("seed-{nonce}").into_bytes();
            reject.insert(generator.candidate(&seed, 12));
        }

        let id = generator.generate(
            |nonce| format!("seed-{nonce}").into_bytes(),
            0,
            |candidate| reject.contains(candidate),
        );

        // Phase 4 appends the nonce number to a 12-char hash: "bd-{12chars}{nonce}"
        assert!(id.starts_with("bd-"));
        let hash_part = &id["bd-".len()..];
        // Must be longer than 12 (12-char hash + nonce digits)
        assert!(
            hash_part.len() > 12,
            "Expected >12-char hash from phase 4, got '{}' ({})",
            hash_part,
            hash_part.len()
        );
        // Verify it ends with a digit (the appended nonce)
        assert!(
            hash_part.ends_with(|c: char| c.is_ascii_digit()),
            "Phase 4 ID should end with nonce digit, got '{hash_part}'"
        );
    }

    // ========== Absolute fallback test ==========

    #[test]
    fn test_generate_absolute_fallback() {
        // min=max=3, reject EVERYTHING
        let generator = IdGenerator::new(IdConfig::new("bd").min_hash_length(3).max_hash_length(3));

        let id = generator.generate(
            |nonce| format!("seed-{nonce}").into_bytes(),
            0,
            |_| true, // reject all candidates
        );

        // Absolute fallback format: "bd-{12chars}.fallback"
        assert!(
            id.ends_with(".fallback"),
            "Expected '.fallback' suffix, got '{id}'"
        );
        assert!(id.starts_with("bd-"));
    }

    // ========== Phase transition tracking test ==========

    #[test]
    fn test_generate_phase_transitions_are_ordered() {
        use std::cell::Cell;

        // min=3, max=4 — phase 1 at 3, phase 2 at 4, then phase 3 at 12
        let generator = IdGenerator::new(IdConfig::new("bd").min_hash_length(3).max_hash_length(4));

        let call_count = Cell::new(0usize);

        // Let the 15th candidate through (0-indexed):
        // Phase 1: 10 candidates (nonces 0-9 at length 3) → indices 0-9
        // Phase 2: 10 candidates (nonces 0-9 at length 4) → indices 10-19
        // We accept at index 15, which is phase 2 nonce 5 at length 4
        let id = generator.generate(
            |nonce| format!("seed-{nonce}").into_bytes(),
            0,
            |_candidate| {
                let n = call_count.get();
                call_count.set(n + 1);
                n < 15
            },
        );

        assert_eq!(call_count.get(), 16); // 15 rejections + 1 acceptance
        assert!(id.starts_with("bd-"));
        let hash_part = &id["bd-".len()..];
        assert_eq!(hash_part.len(), 4, "Should be phase 2 (length 4)");
    }
}
