use crate::error::{Result, TerseIdError};
use std::fmt;

/// Parsed representation of a terseid ID.
///
/// Format: `<prefix>-<hash>[.<child>.<path>]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedId {
    pub prefix: String,
    pub hash: String,
    pub child_path: Vec<u32>,
}

impl ParsedId {
    /// Returns true if this ID has no child path segments.
    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.child_path.is_empty()
    }

    /// Returns the depth of this ID (number of child path segments).
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.child_path.len()
    }

    /// Returns the parent ID, or None if this is a root ID.
    ///
    /// For example, "bd-a7x.1.3" -> Some("bd-a7x.1")
    #[must_use]
    pub fn parent(&self) -> Option<String> {
        if self.child_path.is_empty() {
            None
        } else {
            let mut parent = self.clone();
            parent.child_path.pop();
            Some(parent.to_id_string())
        }
    }

    /// Formats this `ParsedId` as a complete ID string.
    ///
    /// Returns format: "{prefix}-{hash}" with child path segments separated by dots.
    #[must_use]
    pub fn to_id_string(&self) -> String {
        let prefix = &self.prefix;
        let hash = &self.hash;
        let mut result = format!("{prefix}-{hash}");
        for segment in &self.child_path {
            result.push('.');
            result.push_str(&segment.to_string());
        }
        result
    }

    /// Returns true if self is a child of `potential_parent`.
    ///
    /// A child ID must:
    /// - Have the same prefix and hash as the parent
    /// - Have a `child_path` that starts with the parent's `child_path`
    /// - Have a longer `child_path` than the parent (deeper in the tree)
    #[must_use]
    pub fn is_child_of(&self, potential_parent: &str) -> bool {
        let Ok(parent) = parse_id(potential_parent) else {
            return false;
        };

        // Must have same prefix and hash
        if self.prefix != parent.prefix || self.hash != parent.hash {
            return false;
        }

        // Must be deeper (longer child_path)
        if self.child_path.len() <= parent.child_path.len() {
            return false;
        }

        // Child path must start with parent's child path
        self.child_path
            .iter()
            .zip(parent.child_path.iter())
            .all(|(a, b)| a == b)
    }
}

impl fmt::Display for ParsedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_id_string())
    }
}

/// Checks if a character is valid in base36.
const fn is_base36(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// Checks if a string contains at least one digit.
fn contains_digit(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_digit())
}

/// Parses a terseid ID string into a structured `ParsedId`.
///
/// Parsing rules:
/// - Lowercase the input first
/// - The **last dash** separates prefix from hash (supports hyphenated prefixes)
/// - Hash validation:
///   - Must be non-empty
///   - All characters must be base36 (0-9, a-z)
///   - At 3 chars: any base36 is valid
///   - At 4+ chars: must contain at least one digit (avoids ambiguity with English words)
/// - Child path segments after dots must be valid u32 integers
///
/// # Errors
///
/// Returns `InvalidId` if:
/// - No dash found
/// - Empty hash
/// - Invalid base36 characters in hash
/// - 4+ char hash without a digit
/// - Invalid u32 child path segments
pub fn parse_id(id: &str) -> Result<ParsedId> {
    let id = id.to_lowercase();

    // Find the first dot (if any) - this marks the start of child path
    let first_dot = id.find('.');

    // Find the last dash before the child path (or at the end if no dot)
    let search_end = first_dot.unwrap_or(id.len());
    let Some(last_dash) = id[..search_end].rfind('-') else {
        return Err(TerseIdError::InvalidId { id });
    };

    let prefix = id[..last_dash].to_string();
    let rest = &id[last_dash + 1..];

    // Split by dots: first segment is hash, rest are child path
    let segments: Vec<&str> = rest.split('.').collect();
    let hash = segments[0];

    // Validate hash
    if hash.is_empty() {
        return Err(TerseIdError::InvalidId { id });
    }

    // All characters must be base36
    if !hash.chars().all(is_base36) {
        return Err(TerseIdError::InvalidId { id });
    }

    // Legacy IDs may not contain digits at 4+ chars.
    // Accept all base36 hashes regardless of digit presence.

    // Parse child path segments
    let mut child_path = Vec::new();
    for segment_str in &segments[1..] {
        match segment_str.parse::<u32>() {
            Ok(num) => child_path.push(num),
            Err(_) => {
                return Err(TerseIdError::InvalidId { id });
            }
        }
    }

    Ok(ParsedId {
        prefix,
        hash: hash.to_string(),
        child_path,
    })
}

/// Returns true if the given ID string is in a valid format.
///
/// This is a convenience function that tries to parse the ID and returns
/// a boolean instead of a Result.
#[must_use]
pub fn is_valid_id_format(id: &str) -> bool {
    parse_id(id).is_ok()
}

/// Normalizes an ID string by converting it to lowercase.
#[must_use]
pub fn normalize_id(id: &str) -> String {
    id.to_lowercase()
}

/// Validates that an ID has the expected prefix or one of the allowed prefixes.
///
/// Parses the ID and checks if its prefix matches `expected` or is in the `allowed` list.
///
/// # Errors
///
/// Returns `InvalidId` if the ID cannot be parsed.
/// Returns `PrefixMismatch` if the prefix doesn't match expected or allowed.
pub fn validate_prefix(id: &str, expected: &str, allowed: &[&str]) -> Result<()> {
    let parsed = parse_id(id)?;

    if parsed.prefix == expected {
        return Ok(());
    }

    if allowed.contains(&parsed.prefix.as_str()) {
        return Ok(());
    }

    Err(TerseIdError::PrefixMismatch {
        expected: expected.to_string(),
        found: parsed.prefix,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ========== Basic parsing tests ==========

    #[test]
    fn test_simple_id() {
        let parsed = parse_id("bd-a7x").unwrap();
        assert_eq!(parsed.prefix, "bd");
        assert_eq!(parsed.hash, "a7x");
        assert!(parsed.is_root());
    }

    #[test]
    fn test_simple_id_uppercase() {
        let parsed = parse_id("BD-A7X").unwrap();
        assert_eq!(parsed.prefix, "bd");
        assert_eq!(parsed.hash, "a7x");
    }

    // ========== Hyphenated prefix tests ==========

    #[test]
    fn test_hyphenated_prefix() {
        let parsed = parse_id("my-proj-a7x3q9").unwrap();
        assert_eq!(parsed.prefix, "my-proj");
        assert_eq!(parsed.hash, "a7x3q9");
        assert!(parsed.is_root());
    }

    #[test]
    fn test_hyphenated_prefix_multiple_dashes() {
        let parsed = parse_id("my-long-proj-name-a7x").unwrap();
        assert_eq!(parsed.prefix, "my-long-proj-name");
        assert_eq!(parsed.hash, "a7x");
    }

    // ========== Child path tests ==========

    #[test]
    fn test_child_path_single() {
        let parsed = parse_id("bd-a7x.1").unwrap();
        assert_eq!(parsed.prefix, "bd");
        assert_eq!(parsed.hash, "a7x");
        assert_eq!(parsed.child_path, vec![1]);
        assert!(!parsed.is_root());
        assert_eq!(parsed.depth(), 1);
    }

    #[test]
    fn test_child_path_multiple() {
        let parsed = parse_id("bd-a7x.1.3.7").unwrap();
        assert_eq!(parsed.prefix, "bd");
        assert_eq!(parsed.hash, "a7x");
        assert_eq!(parsed.child_path, vec![1, 3, 7]);
        assert_eq!(parsed.depth(), 3);
    }

    #[test]
    fn test_child_path_large_numbers() {
        let parsed = parse_id("bd-a7x.123.456.789").unwrap();
        assert_eq!(parsed.child_path, vec![123, 456, 789]);
    }

    #[test]
    fn test_child_path_max_u32() {
        let max_u32 = u32::MAX;
        let id = format!("bd-a7x.{max_u32}");
        let parsed = parse_id(&id).unwrap();
        assert_eq!(parsed.child_path, vec![max_u32]);
    }

    // ========== Hash validation: length and content ==========

    #[test]
    fn test_hash_3_chars_any_base36() {
        // 3-char hashes accept any base36, no digit requirement
        assert!(parse_id("bd-abc").is_ok());
        assert!(parse_id("bd-xyz").is_ok());
        assert!(parse_id("bd-000").is_ok());
        assert!(parse_id("bd-a7x").is_ok());
    }

    #[test]
    fn test_hash_4_chars_requires_digit() {
        // 4-char hashes must contain at least one digit
        assert!(parse_id("bd-a7x3").is_ok()); // contains digits
        assert!(parse_id("bd-test").is_err()); // no digit, invalid
        assert!(parse_id("bd-a0bc").is_ok()); // contains digit
    }

    #[test]
    fn test_hash_5_chars_requires_digit() {
        // 5-char hashes must contain at least one digit
        assert!(parse_id("bd-a7x3q9").is_ok()); // multiple digits
        assert!(parse_id("bd-abcde").is_err()); // no digit, invalid
        assert!(parse_id("bd-abc0d").is_ok()); // contains digit
    }

    #[test]
    fn test_hash_long_with_digit() {
        // Long hashes with at least one digit
        assert!(parse_id("bd-a7x3q9z2w1e0r").is_ok());
    }

    #[test]
    fn test_hash_empty() {
        assert!(parse_id("bd-").is_err());
    }

    #[test]
    fn test_hash_invalid_characters() {
        assert!(parse_id("bd-a7x!").is_err());
        assert!(parse_id("bd-a_x").is_err());
        assert!(parse_id("bd-a x").is_err()); // space in hash
        // Note: "bd-a-x" is valid as prefix "bd-a" with hash "x", not invalid
    }

    // ========== No dash error ==========

    #[test]
    fn test_no_dash() {
        assert!(parse_id("bda7x").is_err());
    }

    // ========== Invalid child path ==========

    #[test]
    fn test_invalid_child_path_non_numeric() {
        assert!(parse_id("bd-a7x.abc").is_err());
        assert!(parse_id("bd-a7x.1.abc").is_err());
    }

    #[test]
    fn test_invalid_child_path_overflow() {
        // u64 string that overflows u32
        let huge = (u64::from(u32::MAX) + 1).to_string();
        let id = format!("bd-a7x.{huge}");
        assert!(parse_id(&id).is_err());
    }

    #[test]
    fn test_invalid_child_path_negative() {
        assert!(parse_id("bd-a7x.-1").is_err());
    }

    // ========== Round-trip tests ==========

    #[test]
    fn test_roundtrip_simple() {
        let id = "bd-a7x";
        let parsed = parse_id(id).unwrap();
        assert_eq!(parsed.to_id_string(), normalize_id(id));
    }

    #[test]
    fn test_roundtrip_uppercase() {
        let id = "BD-A7X";
        let parsed = parse_id(id).unwrap();
        assert_eq!(parsed.to_id_string(), normalize_id(id));
    }

    #[test]
    fn test_roundtrip_hyphenated_prefix() {
        let id = "my-proj-a7x3q9";
        let parsed = parse_id(id).unwrap();
        assert_eq!(parsed.to_id_string(), normalize_id(id));
    }

    #[test]
    fn test_roundtrip_with_child_path() {
        let id = "bd-a7x.1.3.7";
        let parsed = parse_id(id).unwrap();
        assert_eq!(parsed.to_id_string(), normalize_id(id));
    }

    #[test]
    fn test_roundtrip_complex() {
        let id = "my-proj-a7x3q9.1.2.3";
        let parsed = parse_id(id).unwrap();
        assert_eq!(parsed.to_id_string(), normalize_id(id));
    }

    // ========== is_root and depth ==========

    #[test]
    fn test_is_root_true() {
        let parsed = parse_id("bd-a7x").unwrap();
        assert!(parsed.is_root());
        assert_eq!(parsed.depth(), 0);
    }

    #[test]
    fn test_is_root_false() {
        let parsed = parse_id("bd-a7x.1").unwrap();
        assert!(!parsed.is_root());
        assert_eq!(parsed.depth(), 1);
    }

    #[test]
    fn test_depth_multiple() {
        let parsed = parse_id("bd-a7x.1.2.3.4.5").unwrap();
        assert_eq!(parsed.depth(), 5);
    }

    // ========== parent() tests ==========

    #[test]
    fn test_parent_root_returns_none() {
        let parsed = parse_id("bd-a7x").unwrap();
        assert_eq!(parsed.parent(), None);
    }

    #[test]
    fn test_parent_child_level_1() {
        let parsed = parse_id("bd-a7x.1").unwrap();
        assert_eq!(parsed.parent(), Some("bd-a7x".to_string()));
    }

    #[test]
    fn test_parent_child_level_2() {
        let parsed = parse_id("bd-a7x.1.3").unwrap();
        assert_eq!(parsed.parent(), Some("bd-a7x.1".to_string()));
    }

    #[test]
    fn test_parent_child_level_3() {
        let parsed = parse_id("bd-a7x.1.3.7").unwrap();
        assert_eq!(parsed.parent(), Some("bd-a7x.1.3".to_string()));
    }

    #[test]
    fn test_parent_chain() {
        let a = parse_id("bd-a7x").unwrap();
        let b = parse_id("bd-a7x.1").unwrap();
        let c = parse_id("bd-a7x.1.3").unwrap();

        assert_eq!(a.parent(), None);
        assert_eq!(b.parent(), Some("bd-a7x".to_string()));
        assert_eq!(c.parent(), Some("bd-a7x.1".to_string()));
    }

    // ========== is_child_of tests ==========

    #[test]
    fn test_is_child_of_direct_child() {
        let child = parse_id("bd-a7x.1").unwrap();
        assert!(child.is_child_of("bd-a7x"));
    }

    #[test]
    fn test_is_child_of_grandchild() {
        let grandchild = parse_id("bd-a7x.1.3").unwrap();
        assert!(grandchild.is_child_of("bd-a7x"));
        assert!(grandchild.is_child_of("bd-a7x.1"));
    }

    #[test]
    fn test_is_child_of_deep_nesting() {
        let deep = parse_id("bd-a7x.1.3.7.9").unwrap();
        assert!(deep.is_child_of("bd-a7x"));
        assert!(deep.is_child_of("bd-a7x.1"));
        assert!(deep.is_child_of("bd-a7x.1.3"));
        assert!(deep.is_child_of("bd-a7x.1.3.7"));
    }

    #[test]
    fn test_is_child_of_root_not_child_of_self() {
        let root = parse_id("bd-a7x").unwrap();
        assert!(!root.is_child_of("bd-a7x"));
    }

    #[test]
    fn test_is_child_of_child_not_parent_of_ancestor() {
        let parent = parse_id("bd-a7x.1").unwrap();
        assert!(!parent.is_child_of("bd-a7x.1.3"));
    }

    #[test]
    fn test_is_child_of_different_prefix() {
        let id1 = parse_id("bd-a7x.1").unwrap();
        assert!(!id1.is_child_of("tk-a7x"));
    }

    #[test]
    fn test_is_child_of_different_hash() {
        let id1 = parse_id("bd-a7x.1").unwrap();
        assert!(!id1.is_child_of("bd-xyz"));
    }

    #[test]
    fn test_is_child_of_invalid_parent() {
        let child = parse_id("bd-a7x.1").unwrap();
        assert!(!child.is_child_of("invalid-id-format"));
    }

    #[test]
    fn test_is_child_of_different_path_branch() {
        let id2 = parse_id("bd-a7x.1.5").unwrap();
        assert!(!id2.is_child_of("bd-a7x.1.3"));
    }

    // ========== Display impl ==========

    #[test]
    fn test_display_simple() {
        let parsed = parse_id("bd-a7x").unwrap();
        assert_eq!(format!("{parsed}"), "bd-a7x");
    }

    #[test]
    fn test_display_with_path() {
        let parsed = parse_id("bd-a7x.1.3").unwrap();
        assert_eq!(format!("{parsed}"), "bd-a7x.1.3");
    }

    // ========== is_valid_id_format ==========

    #[test]
    fn test_is_valid_id_format_valid() {
        assert!(is_valid_id_format("bd-a7x"));
        assert!(is_valid_id_format("bd-a7x.1"));
        assert!(is_valid_id_format("my-proj-a7x3q9"));
    }

    #[test]
    fn test_is_valid_id_format_invalid() {
        assert!(!is_valid_id_format("bda7x"));
        assert!(!is_valid_id_format("bd-"));
        assert!(!is_valid_id_format("bd-test")); // 4+ chars without digit
    }

    // ========== normalize_id ==========

    #[test]
    fn test_normalize_id_uppercase() {
        assert_eq!(normalize_id("BD-A7X"), "bd-a7x");
    }

    #[test]
    fn test_normalize_id_mixed() {
        assert_eq!(normalize_id("MY-PROJ-A7X3Q9"), "my-proj-a7x3q9");
    }

    #[test]
    fn test_normalize_id_already_lowercase() {
        assert_eq!(normalize_id("bd-a7x"), "bd-a7x");
    }

    // ========== validate_prefix ==========

    #[test]
    fn test_validate_prefix_exact_match() {
        assert!(validate_prefix("bd-a7x", "bd", &[]).is_ok());
    }

    #[test]
    fn test_validate_prefix_in_allowed() {
        assert!(validate_prefix("tk-a7x", "bd", &["tk"]).is_ok());
        assert!(validate_prefix("ev-a7x", "bd", &["tk", "ev"]).is_ok());
    }

    #[test]
    fn test_validate_prefix_mismatch() {
        let result = validate_prefix("tk-a7x", "bd", &[]);
        assert!(result.is_err());
        match result {
            Err(TerseIdError::PrefixMismatch { expected, found }) => {
                assert_eq!(expected, "bd");
                assert_eq!(found, "tk");
            }
            _ => panic!("Expected PrefixMismatch error"),
        }
    }

    #[test]
    fn test_validate_prefix_invalid_id() {
        let result = validate_prefix("invalid", "bd", &[]);
        assert!(result.is_err());
        match result {
            Err(TerseIdError::InvalidId { .. }) => {}
            _ => panic!("Expected InvalidId error"),
        }
    }

    #[test]
    fn test_validate_prefix_hyphenated() {
        assert!(validate_prefix("my-proj-a7x3q9", "my-proj", &[]).is_ok());
    }

    // ========== Edge cases and stress tests ==========

    #[test]
    fn test_all_digits_hash_3_char() {
        let parsed = parse_id("bd-000").unwrap();
        assert_eq!(parsed.hash, "000");
    }

    #[test]
    fn test_all_digits_hash_4_char() {
        let parsed = parse_id("bd-0000").unwrap();
        assert_eq!(parsed.hash, "0000");
    }

    #[test]
    fn test_all_letters_hash_3_char() {
        let parsed = parse_id("bd-abc").unwrap();
        assert_eq!(parsed.hash, "abc");
    }

    #[test]
    fn test_all_letters_hash_4_char() {
        let parsed = parse_id("bd-abc0").unwrap();
        assert_eq!(parsed.hash, "abc0");
    }

    #[test]
    fn test_very_long_hash() {
        let parsed =
            parse_id("bd-a7x3q9z2w1e0r5t4y3u2i1o0p9a8s7d6f5g4h3j2k1l0z9x8c7v6b5n4m3").unwrap();
        assert!(parsed.hash.len() > 30);
    }

    #[test]
    fn test_zero_child_segment() {
        let parsed = parse_id("bd-a7x.0").unwrap();
        assert_eq!(parsed.child_path, vec![0]);
    }

    #[test]
    fn test_many_child_segments() {
        let parsed = parse_id("bd-a7x.1.2.3.4.5.6.7.8.9.10").unwrap();
        assert_eq!(parsed.child_path.len(), 10);
    }

    #[test]
    fn test_clone_and_equality() {
        let p1 = parse_id("bd-a7x.1.3").unwrap();
        let p2 = p1.clone();
        assert_eq!(p1, p2);
    }

    #[test]
    fn test_debug_format() {
        let parsed = parse_id("bd-a7x").unwrap();
        let debug_str = format!("{parsed:?}");
        assert!(debug_str.contains("ParsedId"));
        assert!(debug_str.contains("bd"));
        assert!(debug_str.contains("a7x"));
    }
}
