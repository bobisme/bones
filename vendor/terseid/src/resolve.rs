use crate::error::{Result, TerseIdError};
use crate::parse::parse_id;

/// Configuration for the ID resolver.
#[derive(Debug, Clone)]
pub struct ResolverConfig {
    /// Default prefix to prepend when normalizing IDs without a dash.
    pub default_prefix: String,
    /// List of allowed prefixes for validation.
    pub allowed_prefixes: Vec<String>,
    /// Whether to allow substring matching in resolution.
    pub allow_substring_match: bool,
}

impl ResolverConfig {
    /// Creates a new resolver configuration with default settings.
    pub fn new(default_prefix: impl Into<String>) -> Self {
        Self {
            default_prefix: default_prefix.into(),
            allowed_prefixes: vec![],
            allow_substring_match: true,
        }
    }
}

/// The type of match found during ID resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchType {
    /// Exact match of the input ID.
    Exact,
    /// Match after prefix normalization (prepending default prefix).
    PrefixNormalized,
    /// Match via substring search on hash portion.
    Substring,
}

/// A resolved ID with match information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedId {
    /// The resolved ID string.
    pub id: String,
    /// How the ID was matched.
    pub match_type: MatchType,
    /// The original input string.
    pub original_input: String,
}

/// Resolver for fuzzy ID matching.
pub struct IdResolver {
    config: ResolverConfig,
}

impl IdResolver {
    /// Creates a new ID resolver with the given configuration.
    #[must_use]
    pub const fn new(config: ResolverConfig) -> Self {
        Self { config }
    }

    /// Resolves a user input to an ID using fuzzy matching.
    ///
    /// Resolution order:
    /// 1. Exact match — input (lowercased, trimmed) matches via `exists_fn`
    /// 2. Prefix normalization — if no dash in input, prepend `default_prefix` + "-" and retry `exists_fn`
    /// 3. Substring match — call `substring_match_fn` with input, exactly one match succeeds,
    ///    multiple matches -> `AmbiguousId` error
    /// 4. Not found -> `NotFound` error
    ///
    /// # Errors
    ///
    /// Returns `AmbiguousId` if multiple IDs match the substring.
    /// Returns `NotFound` if no match is found at any stage.
    pub fn resolve<F, G>(
        &self,
        input: &str,
        exists_fn: F,
        substring_match_fn: G,
    ) -> Result<ResolvedId>
    where
        F: Fn(&str) -> bool,
        G: Fn(&str) -> Vec<String>,
    {
        let original_input = input.to_string();
        let normalized = input.to_lowercase().trim().to_string();

        // Stage 1: Try exact match
        if exists_fn(&normalized) {
            return Ok(ResolvedId {
                id: normalized,
                match_type: MatchType::Exact,
                original_input,
            });
        }

        // Stage 2: Try prefix normalization (if no dash in input)
        if !normalized.contains('-') {
            let prefixed = format!("{}-{}", self.config.default_prefix, normalized);
            if exists_fn(&prefixed) {
                return Ok(ResolvedId {
                    id: prefixed,
                    match_type: MatchType::PrefixNormalized,
                    original_input,
                });
            }
        }

        // Stage 3: Try substring match
        if self.config.allow_substring_match {
            let matches = substring_match_fn(&normalized);
            match matches.len() {
                0 => {
                    // Fall through to not found
                }
                1 => {
                    return Ok(ResolvedId {
                        id: matches[0].clone(),
                        match_type: MatchType::Substring,
                        original_input,
                    });
                }
                _ => {
                    return Err(TerseIdError::AmbiguousId {
                        partial: normalized,
                        matches,
                    });
                }
            }
        }

        // Stage 4: Not found
        Err(TerseIdError::NotFound { id: normalized })
    }
}

/// Finds IDs matching a hash substring.
///
/// Given a list of full IDs and a hash substring, returns all IDs whose hash portion
/// (after the last dash, before the first dot) contains the substring.
/// Both the IDs and the substring are compared case-insensitively.
pub fn find_matching_ids(all_ids: &[impl AsRef<str>], hash_substring: &str) -> Vec<String> {
    let needle = hash_substring.to_lowercase();
    all_ids
        .iter()
        .filter_map(|id| match parse_id(id.as_ref()) {
            Ok(parsed) => {
                if parsed.hash.contains(&needle) {
                    Some(parsed.to_id_string())
                } else {
                    None
                }
            }
            Err(_) => None,
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::similar_names)]
mod tests {
    use super::*;

    // ========== ResolverConfig tests ==========

    #[test]
    fn test_resolver_config_new() {
        let config = ResolverConfig::new("bd");
        assert_eq!(config.default_prefix, "bd");
        assert!(config.allow_substring_match);
        assert!(config.allowed_prefixes.is_empty());
    }

    #[test]
    fn test_match_type_variants() {
        assert_eq!(MatchType::Exact, MatchType::Exact);
        assert_ne!(MatchType::Exact, MatchType::PrefixNormalized);
        assert_ne!(MatchType::PrefixNormalized, MatchType::Substring);
    }

    #[test]
    fn test_resolved_id_creation() {
        let id = ResolvedId {
            id: "bd-a7x".to_string(),
            match_type: MatchType::Exact,
            original_input: "BD-A7X".to_string(),
        };
        assert_eq!(id.id, "bd-a7x");
        assert_eq!(id.match_type, MatchType::Exact);
        assert_eq!(id.original_input, "BD-A7X");
    }

    // ========== Exact match tests ==========

    #[test]
    fn test_resolve_exact_match_lowercase() {
        let config = ResolverConfig::new("bd");
        let resolver = IdResolver::new(config);

        let result = resolver.resolve("bd-a7x", |id| id == "bd-a7x", |_| vec![]);
        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert_eq!(resolved.id, "bd-a7x");
        assert_eq!(resolved.match_type, MatchType::Exact);
        assert_eq!(resolved.original_input, "bd-a7x");
    }

    #[test]
    fn test_resolve_exact_match_uppercase_normalized() {
        let config = ResolverConfig::new("bd");
        let resolver = IdResolver::new(config);

        let result = resolver.resolve("BD-A7X", |id| id == "bd-a7x", |_| vec![]);
        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert_eq!(resolved.id, "bd-a7x");
        assert_eq!(resolved.match_type, MatchType::Exact);
        assert_eq!(resolved.original_input, "BD-A7X");
    }

    #[test]
    fn test_resolve_exact_match_with_whitespace() {
        let config = ResolverConfig::new("bd");
        let resolver = IdResolver::new(config);

        let result = resolver.resolve("  bd-a7x  ", |id| id == "bd-a7x", |_| vec![]);
        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert_eq!(resolved.id, "bd-a7x");
        assert_eq!(resolved.original_input, "  bd-a7x  ");
    }

    // ========== Prefix normalization tests ==========

    #[test]
    fn test_resolve_prefix_normalization() {
        let config = ResolverConfig::new("bd");
        let resolver = IdResolver::new(config);

        let result = resolver.resolve("a7x", |id| id == "bd-a7x", |_| vec![]);
        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert_eq!(resolved.id, "bd-a7x");
        assert_eq!(resolved.match_type, MatchType::PrefixNormalized);
        assert_eq!(resolved.original_input, "a7x");
    }

    #[test]
    fn test_resolve_prefix_normalization_uppercase() {
        let config = ResolverConfig::new("bd");
        let resolver = IdResolver::new(config);

        let result = resolver.resolve("A7X", |id| id == "bd-a7x", |_| vec![]);
        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert_eq!(resolved.id, "bd-a7x");
        assert_eq!(resolved.match_type, MatchType::PrefixNormalized);
    }

    #[test]
    fn test_resolve_prefix_normalization_skipped_with_dash() {
        let config = ResolverConfig::new("bd");
        let resolver = IdResolver::new(config);

        // Input has a dash, so prefix normalization is skipped
        let result = resolver.resolve("bd-a7x", |id| id == "bd-xyz", |_| vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_prefix_normalization_custom_prefix() {
        let config = ResolverConfig::new("my-proj");
        let resolver = IdResolver::new(config);

        let result = resolver.resolve("a7x3q9", |id| id == "my-proj-a7x3q9", |_| vec![]);
        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert_eq!(resolved.id, "my-proj-a7x3q9");
        assert_eq!(resolved.match_type, MatchType::PrefixNormalized);
    }

    // ========== Substring match tests ==========

    #[test]
    fn test_resolve_substring_match_unique() {
        let config = ResolverConfig::new("bd");
        let resolver = IdResolver::new(config);

        let substring_fn = |_: &str| vec!["bd-a7x".to_string()];

        let result = resolver.resolve("a7", |id| id == "nonexistent", substring_fn);
        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert_eq!(resolved.id, "bd-a7x");
        assert_eq!(resolved.match_type, MatchType::Substring);
        assert_eq!(resolved.original_input, "a7");
    }

    #[test]
    fn test_resolve_substring_match_ambiguous() {
        let config = ResolverConfig::new("bd");
        let resolver = IdResolver::new(config);

        let substring_fn = |_: &str| vec!["bd-a7x".to_string(), "bd-a7y".to_string()];

        let result = resolver.resolve("a7", |id| id == "nonexistent", substring_fn);
        assert!(result.is_err());
        match result.unwrap_err() {
            TerseIdError::AmbiguousId { partial, matches } => {
                assert_eq!(partial, "a7");
                assert_eq!(matches.len(), 2);
            }
            _ => panic!("Expected AmbiguousId error"),
        }
    }

    #[test]
    fn test_resolve_substring_match_disabled() {
        let mut config = ResolverConfig::new("bd");
        config.allow_substring_match = false;
        let resolver = IdResolver::new(config);

        let substring_fn = |_: &str| vec!["bd-a7x".to_string()];

        let result = resolver.resolve("a7", |id| id == "nonexistent", substring_fn);
        assert!(result.is_err());
        match result.unwrap_err() {
            TerseIdError::NotFound { id } => {
                assert_eq!(id, "a7");
            }
            _ => panic!("Expected NotFound error"),
        }
    }

    // ========== Not found tests ==========

    #[test]
    fn test_resolve_not_found() {
        let config = ResolverConfig::new("bd");
        let resolver = IdResolver::new(config);

        let result = resolver.resolve("nonexistent", |_| false, |_| vec![]);
        assert!(result.is_err());
        match result.unwrap_err() {
            TerseIdError::NotFound { id } => {
                assert_eq!(id, "nonexistent");
            }
            _ => panic!("Expected NotFound error"),
        }
    }

    #[test]
    fn test_resolve_not_found_after_prefix_normalization_attempt() {
        let config = ResolverConfig::new("bd");
        let resolver = IdResolver::new(config);

        let result = resolver.resolve("a7x", |id| id == "other-id", |_| vec![]);
        assert!(result.is_err());
        match result.unwrap_err() {
            TerseIdError::NotFound { id } => {
                assert_eq!(id, "a7x");
            }
            _ => panic!("Expected NotFound error"),
        }
    }

    // ========== Resolution order tests ==========

    #[test]
    fn test_resolve_exact_takes_precedence_over_prefix_normalized() {
        let config = ResolverConfig::new("bd");
        let resolver = IdResolver::new(config);

        // Both exact and prefix-normalized exist, exact should win
        let exists_fn = |id: &str| id == "a7x" || id == "bd-a7x";
        let result = resolver.resolve("a7x", exists_fn, |_| vec![]);
        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert_eq!(resolved.id, "a7x");
        assert_eq!(resolved.match_type, MatchType::Exact);
    }

    #[test]
    fn test_resolve_prefix_normalized_takes_precedence_over_substring() {
        let config = ResolverConfig::new("bd");
        let resolver = IdResolver::new(config);

        let substring_fn = |_: &str| vec!["bd-xyz".to_string()];
        let result = resolver.resolve("a7x", |id| id == "bd-a7x", substring_fn);
        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert_eq!(resolved.id, "bd-a7x");
        assert_eq!(resolved.match_type, MatchType::PrefixNormalized);
    }

    // ========== find_matching_ids tests ==========

    #[test]
    fn test_find_matching_ids_single_match() {
        let all_ids = vec!["bd-a7x", "bd-b8y", "bd-c9z"];
        let matches = find_matching_ids(&all_ids, "a7");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], "bd-a7x");
    }

    #[test]
    fn test_find_matching_ids_multiple_matches() {
        let all_ids = vec!["bd-a7x", "bd-a7y", "bd-b8z"];
        let matches = find_matching_ids(&all_ids, "a7");
        assert_eq!(matches.len(), 2);
        assert!(matches.contains(&"bd-a7x".to_string()));
        assert!(matches.contains(&"bd-a7y".to_string()));
    }

    #[test]
    fn test_find_matching_ids_no_matches() {
        let all_ids = vec!["bd-a7x", "bd-b8y", "bd-c9z"];
        let matches = find_matching_ids(&all_ids, "xyz");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_find_matching_ids_with_child_path() {
        let all_ids = vec!["bd-a7x", "bd-a7x.1", "bd-a7x.1.2"];
        let matches = find_matching_ids(&all_ids, "a7");
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn test_find_matching_ids_substring_in_hash() {
        let all_ids = vec!["bd-abc123", "bd-def456", "bd-ghi123"];
        let matches = find_matching_ids(&all_ids, "123");
        assert_eq!(matches.len(), 2);
        assert!(matches.contains(&"bd-abc123".to_string()));
        assert!(matches.contains(&"bd-ghi123".to_string()));
    }

    #[test]
    fn test_find_matching_ids_ignores_invalid_ids() {
        let all_ids = vec!["bd-a7x", "invalid", "bd-b8y"];
        let matches = find_matching_ids(&all_ids, "a7");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], "bd-a7x");
    }

    #[test]
    fn test_find_matching_ids_case_insensitive() {
        let all_ids = vec!["BD-A7X", "bd-a7x", "BD-a7x"];
        let matches = find_matching_ids(&all_ids, "a7");
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn test_find_matching_ids_hyphenated_prefix() {
        let all_ids = vec!["my-proj-a7x3q9", "my-proj-b8y4r0"];
        let matches = find_matching_ids(&all_ids, "a7");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], "my-proj-a7x3q9");
    }

    // ========== Integration tests ==========

    #[test]
    fn test_full_resolution_workflow() {
        let all_ids = vec!["bd-a7x", "bd-b8y", "bd-c9z"];
        let config = ResolverConfig::new("bd");
        let resolver = IdResolver::new(config);

        // Test 1: exact match
        let exists_fn = |id: &str| all_ids.contains(&id);
        let substring_fn = |sub: &str| find_matching_ids(&all_ids, sub);
        let result = resolver.resolve("bd-a7x", exists_fn, substring_fn);
        assert_eq!(result.unwrap().match_type, MatchType::Exact);

        // Test 2: prefix normalized
        let result = resolver.resolve("a7x", exists_fn, substring_fn);
        assert_eq!(result.unwrap().match_type, MatchType::PrefixNormalized);

        // Test 3: substring match
        let result = resolver.resolve("a7", exists_fn, substring_fn);
        assert_eq!(result.unwrap().match_type, MatchType::Substring);
    }

    #[test]
    fn test_resolved_id_clone_and_equality() {
        let id1 = ResolvedId {
            id: "bd-a7x".to_string(),
            match_type: MatchType::Exact,
            original_input: "bd-a7x".to_string(),
        };
        let id2 = id1.clone();
        assert_eq!(id1, id2);
    }
}
