//! Child ID functions for managing hierarchical relationships between terseid IDs.
//!
//! Terseid supports hierarchical child IDs by appending dot-separated numbers to a parent ID.
//! For example, `child_id("bd-a7x", 1)` returns `"bd-a7x.1"`, and `child_id("bd-a7x.1", 3)` returns `"bd-a7x.1.3"`.

use crate::parse::parse_id;

/// Creates a child ID from a parent ID and child number.
///
/// Appends `.{child_number}` to the parent ID to create a child ID.
/// Supports arbitrary nesting depth.
///
/// # Examples
///
/// ```
/// use terseid::children::child_id;
///
/// assert_eq!(child_id("bd-a7x", 1), "bd-a7x.1");
/// assert_eq!(child_id("bd-a7x.1", 3), "bd-a7x.1.3");
/// assert_eq!(child_id("bd-a7x.1.3", 7), "bd-a7x.1.3.7");
/// ```
#[must_use]
pub fn child_id(parent_id: &str, child_number: u32) -> String {
    format!("{parent_id}.{child_number}")
}

/// Checks if an ID is a child ID (has a child path).
///
/// Returns true if the ID contains one or more dot-separated child path segments.
/// Returns false for root IDs that have no child path.
///
/// # Examples
///
/// ```
/// use terseid::children::is_child_id;
///
/// assert!(!is_child_id("bd-a7x"));        // Root ID
/// assert!(is_child_id("bd-a7x.1"));       // Direct child
/// assert!(is_child_id("bd-a7x.1.3"));     // Grandchild
/// ```
#[must_use]
pub fn is_child_id(id: &str) -> bool {
    parse_id(id).is_ok_and(|parsed| !parsed.child_path.is_empty())
}

/// Returns the depth of an ID (number of child path segments).
///
/// Returns 0 for root IDs (no child path).
/// Returns 1 for direct children, 2 for grandchildren, etc.
///
/// # Examples
///
/// ```
/// use terseid::children::id_depth;
///
/// assert_eq!(id_depth("bd-a7x"), 0);          // Root
/// assert_eq!(id_depth("bd-a7x.1"), 1);        // Depth 1
/// assert_eq!(id_depth("bd-a7x.1.3"), 2);      // Depth 2
/// assert_eq!(id_depth("bd-a7x.1.3.7"), 3);    // Depth 3
/// ```
#[must_use]
pub fn id_depth(id: &str) -> usize {
    parse_id(id).map_or(0, |parsed| parsed.depth())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ========== child_id tests ==========

    #[test]
    fn test_child_id_basic() {
        assert_eq!(child_id("bd-a7x", 1), "bd-a7x.1");
    }

    #[test]
    fn test_child_id_different_number() {
        assert_eq!(child_id("bd-a7x", 42), "bd-a7x.42");
    }

    #[test]
    fn test_child_id_zero() {
        assert_eq!(child_id("bd-a7x", 0), "bd-a7x.0");
    }

    #[test]
    fn test_child_id_max_u32() {
        let max = u32::MAX;
        assert_eq!(child_id("bd-a7x", max), format!("bd-a7x.{max}"));
    }

    #[test]
    fn test_child_id_nested_single_level() {
        assert_eq!(child_id("bd-a7x.1", 3), "bd-a7x.1.3");
    }

    #[test]
    fn test_child_id_nested_multiple_levels() {
        assert_eq!(child_id("bd-a7x.1.3", 7), "bd-a7x.1.3.7");
    }

    #[test]
    fn test_child_id_deeply_nested() {
        assert_eq!(child_id("bd-a7x.1.2.3.4.5", 99), "bd-a7x.1.2.3.4.5.99");
    }

    #[test]
    fn test_child_id_hyphenated_prefix() {
        assert_eq!(child_id("my-proj-a7x3q9", 1), "my-proj-a7x3q9.1");
    }

    #[test]
    fn test_child_id_chain() {
        let root = "bd-a7x";
        let child1 = child_id(root, 1);
        let child2 = child_id(&child1, 2);
        let child3 = child_id(&child2, 3);

        assert_eq!(child1, "bd-a7x.1");
        assert_eq!(child2, "bd-a7x.1.2");
        assert_eq!(child3, "bd-a7x.1.2.3");
    }

    // ========== is_child_id tests ==========

    #[test]
    fn test_is_child_id_root_false() {
        assert!(!is_child_id("bd-a7x"));
    }

    #[test]
    fn test_is_child_id_direct_child_true() {
        assert!(is_child_id("bd-a7x.1"));
    }

    #[test]
    fn test_is_child_id_grandchild_true() {
        assert!(is_child_id("bd-a7x.1.3"));
    }

    #[test]
    fn test_is_child_id_deep_nesting_true() {
        assert!(is_child_id("bd-a7x.1.2.3.4.5.6.7.8.9.10"));
    }

    #[test]
    fn test_is_child_id_invalid_format() {
        assert!(!is_child_id("invalid"));
        assert!(!is_child_id("bd-"));
        assert!(!is_child_id(""));
    }

    #[test]
    fn test_is_child_id_hyphenated_prefix_root() {
        assert!(!is_child_id("my-proj-a7x3q9"));
    }

    #[test]
    fn test_is_child_id_hyphenated_prefix_child() {
        assert!(is_child_id("my-proj-a7x3q9.1"));
    }

    #[test]
    fn test_is_child_id_child_with_zero() {
        assert!(is_child_id("bd-a7x.0"));
    }

    #[test]
    fn test_is_child_id_multiple_children_same_parent() {
        assert!(is_child_id("bd-a7x.1"));
        assert!(is_child_id("bd-a7x.2"));
        assert!(is_child_id("bd-a7x.42"));
    }

    // ========== id_depth tests ==========

    #[test]
    fn test_id_depth_root() {
        assert_eq!(id_depth("bd-a7x"), 0);
    }

    #[test]
    fn test_id_depth_direct_child() {
        assert_eq!(id_depth("bd-a7x.1"), 1);
    }

    #[test]
    fn test_id_depth_grandchild() {
        assert_eq!(id_depth("bd-a7x.1.3"), 2);
    }

    #[test]
    fn test_id_depth_three_levels() {
        assert_eq!(id_depth("bd-a7x.1.3.7"), 3);
    }

    #[test]
    fn test_id_depth_many_levels() {
        assert_eq!(id_depth("bd-a7x.1.2.3.4.5.6.7.8.9.10"), 10);
    }

    #[test]
    fn test_id_depth_invalid_returns_zero() {
        assert_eq!(id_depth("invalid"), 0);
        assert_eq!(id_depth("bd-"), 0);
        assert_eq!(id_depth(""), 0);
    }

    #[test]
    fn test_id_depth_hyphenated_prefix_root() {
        assert_eq!(id_depth("my-proj-a7x3q9"), 0);
    }

    #[test]
    fn test_id_depth_hyphenated_prefix_child() {
        assert_eq!(id_depth("my-proj-a7x3q9.1"), 1);
    }

    #[test]
    fn test_id_depth_hyphenated_prefix_nested() {
        assert_eq!(id_depth("my-proj-a7x3q9.1.2.3"), 3);
    }

    #[test]
    fn test_id_depth_child_with_zero() {
        assert_eq!(id_depth("bd-a7x.0"), 1);
    }

    #[test]
    fn test_id_depth_child_with_large_numbers() {
        assert_eq!(id_depth("bd-a7x.123.456.789"), 3);
    }

    #[test]
    fn test_id_depth_child_with_max_u32() {
        let max = u32::MAX;
        let id = format!("bd-a7x.{max}");
        assert_eq!(id_depth(&id), 1);
    }

    // ========== Integration tests ==========

    #[test]
    fn test_child_id_consistency() {
        let child = child_id("bd-a7x", 1);
        assert!(is_child_id(&child));
    }

    #[test]
    fn test_child_id_depth_consistency() {
        let child = child_id("bd-a7x", 1);
        assert_eq!(id_depth(&child), 1);

        let grandchild = child_id(&child, 3);
        assert_eq!(id_depth(&grandchild), 2);

        let great_grandchild = child_id(&grandchild, 7);
        assert_eq!(id_depth(&great_grandchild), 3);
    }

    #[test]
    fn test_parent_and_child_depth_relation() {
        let parent = "bd-a7x";
        let child = child_id(parent, 1);

        assert_eq!(id_depth(parent), 0);
        assert_eq!(id_depth(&child), 1);

        let grandchild = child_id(&child, 2);
        assert_eq!(id_depth(&grandchild), 2);
    }

    #[test]
    fn test_is_child_and_depth_consistency() {
        let root = "bd-a7x";
        let child = "bd-a7x.1";
        let grandchild = "bd-a7x.1.3";

        assert!(!is_child_id(root) && id_depth(root) == 0);
        assert!(is_child_id(child) && id_depth(child) > 0);
        assert!(is_child_id(grandchild) && id_depth(grandchild) > 0);
    }

    #[test]
    fn test_edge_case_uppercase_input() {
        assert!(!is_child_id("BD-A7X"));
        assert!(is_child_id("BD-A7X.1"));
        assert_eq!(id_depth("BD-A7X"), 0);
        assert_eq!(id_depth("BD-A7X.1"), 1);
    }

    #[test]
    fn test_child_id_output_can_be_parsed() {
        let parent = "bd-a7x";
        let child = child_id(parent, 5);
        assert!(is_child_id(&child));
        assert_eq!(id_depth(&child), 1);

        let grandchild = child_id(&child, 10);
        assert!(is_child_id(&grandchild));
        assert_eq!(id_depth(&grandchild), 2);
    }

    #[test]
    fn test_sequential_children_have_same_parent() {
        let child1 = child_id("bd-a7x", 1);
        let child2 = child_id("bd-a7x", 2);

        assert_eq!(id_depth(&child1), 1);
        assert_eq!(id_depth(&child2), 1);

        assert!(is_child_id(&child1));
        assert!(is_child_id(&child2));
    }
}
