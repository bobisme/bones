use std::collections::HashSet;
use serde::{Deserialize, Serialize};

/// Grow-only Set (G-Set) CRDT.
///
/// Elements are typically event hashes (Strings) referencing comment content.
/// The merge operation is a simple set union.
///
/// Satisfies semilattice properties:
/// - Commutative: a ∪ b = b ∪ a
/// - Associative: (a ∪ b) ∪ c = a ∪ (b ∪ c)
/// - Idempotent: a ∪ a = a
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GSet<T: Eq + std::hash::Hash + Clone> {
    pub elements: HashSet<T>,
}

impl<T: Eq + std::hash::Hash + Clone> GSet<T> {
    /// Create a new empty G-Set.
    pub fn new() -> Self {
        Self {
            elements: HashSet::new(),
        }
    }

    /// Insert an element into the set.
    pub fn insert(&mut self, element: T) {
        self.elements.insert(element);
    }

    /// Merge another G-Set into this one (set union).
    pub fn merge(&mut self, other: &Self) {
        for element in &other.elements {
            self.elements.insert(element.clone());
        }
    }

    /// Returns true if the set contains the element.
    pub fn contains(&self, element: &T) -> bool {
        self.elements.contains(element)
    }

    /// Returns the number of elements in the set.
    pub fn len(&self) -> usize {
        self.elements.len()
    }

    /// Returns true if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gset_insert() {
        let mut set = GSet::new();
        set.insert("hash1".to_string());
        set.insert("hash2".to_string());
        assert_eq!(set.len(), 2);
        assert!(set.contains(&"hash1".to_string()));
        assert!(set.contains(&"hash2".to_string()));
    }

    #[test]
    fn test_gset_idempotent_insert() {
        let mut set = GSet::new();
        set.insert("hash1".to_string());
        set.insert("hash1".to_string());
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn test_gset_merge() {
        let mut a = GSet::new();
        a.insert("hash1".to_string());

        let mut b = GSet::new();
        b.insert("hash2".to_string());

        a.merge(&b);
        assert_eq!(a.len(), 2);
        assert!(a.contains(&"hash1".to_string()));
        assert!(a.contains(&"hash2".to_string()));
    }

    #[test]
    fn test_gset_semilattice_commutative() {
        let mut a = GSet::new();
        a.insert("hash1".to_string());
        let mut b = GSet::new();
        b.insert("hash2".to_string());

        let mut a_merge_b = a.clone();
        a_merge_b.merge(&b);

        let mut b_merge_a = b.clone();
        b_merge_a.merge(&a);

        assert_eq!(a_merge_b, b_merge_a);
    }

    #[test]
    fn test_gset_semilattice_associative() {
        let mut a = GSet::new();
        a.insert("hash1".to_string());
        let mut b = GSet::new();
        b.insert("hash2".to_string());
        let mut c = GSet::new();
        c.insert("hash3".to_string());

        let mut left = a.clone();
        left.merge(&b);
        left.merge(&c);

        let mut right_inner = b.clone();
        right_inner.merge(&c);
        let mut right = a.clone();
        right.merge(&right_inner);

        assert_eq!(left, right);
    }

    #[test]
    fn test_gset_semilattice_idempotent() {
        let mut a = GSet::new();
        a.insert("hash1".to_string());

        let mut a_merge_a = a.clone();
        a_merge_a.merge(&a);

        assert_eq!(a, a_merge_a);
    }
}
