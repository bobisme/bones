//! Interval Tree Clock (ITC) data structures.
//!
//! Implements the ID tree, Event tree, and Stamp types from:
//! Almeida, Baquero & Fonte (2008) "Interval Tree Clocks".
//!
//! - [`Id`] represents a partition of the interval \[0, 1) among agents.
//! - [`Event`] represents causal history as a binary tree of counters.
//! - [`Stamp`] combines an ID tree and Event tree into an ITC stamp.
//!
//! Trees are automatically normalized to their minimal representation.
//! Operations (fork, join, event, peek, leq) are in a separate module.

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// ID tree
// ---------------------------------------------------------------------------

/// An ITC identity tree, partitioning \[0, 1) among participants.
///
/// Leaves are either `0` (not owned) or `1` (owned). Interior nodes
/// split the interval into left and right halves. Normalization collapses
/// degenerate branches: `Branch(0, 0) → Zero`, `Branch(1, 1) → One`.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Id {
    /// Leaf 0 — this portion of the interval is not owned.
    Zero,
    /// Leaf 1 — this portion of the interval is owned.
    One,
    /// Branch splitting the interval into left and right halves.
    Branch(Box<Self>, Box<Self>),
}

impl Id {
    /// Create an anonymous (unowned) identity: leaf 0.
    #[must_use]
    pub const fn zero() -> Self {
        Self::Zero
    }

    /// Create a seed (fully-owned) identity: leaf 1.
    #[must_use]
    pub const fn one() -> Self {
        Self::One
    }

    /// Create a branch, automatically normalizing degenerate cases.
    #[must_use]
    pub fn branch(left: Self, right: Self) -> Self {
        match (&left, &right) {
            (Self::Zero, Self::Zero) => Self::Zero,
            (Self::One, Self::One) => Self::One,
            _ => Self::Branch(Box::new(left), Box::new(right)),
        }
    }

    /// Returns `true` if this identity owns no interval (is all zeros).
    #[must_use]
    pub fn is_zero(&self) -> bool {
        *self == Self::Zero
    }

    /// Returns `true` if this identity owns the entire interval (is all ones).
    #[must_use]
    pub fn is_one(&self) -> bool {
        *self == Self::One
    }

    /// Returns `true` if this is a leaf node (0 or 1).
    #[must_use]
    pub const fn is_leaf(&self) -> bool {
        matches!(self, Self::Zero | Self::One)
    }

    /// Returns `true` if this is a branch node.
    #[must_use]
    pub const fn is_branch(&self) -> bool {
        matches!(self, Self::Branch(..))
    }

    /// Depth of the tree (0 for leaves).
    #[must_use]
    pub fn depth(&self) -> usize {
        match self {
            Self::Zero | Self::One => 0,
            Self::Branch(l, r) => 1 + l.depth().max(r.depth()),
        }
    }

    /// Number of nodes in the tree (leaves + branches).
    #[must_use]
    pub fn node_count(&self) -> usize {
        match self {
            Self::Zero | Self::One => 1,
            Self::Branch(l, r) => 1 + l.node_count() + r.node_count(),
        }
    }

    /// Normalize the tree to its minimal representation.
    ///
    /// This collapses `Branch(0, 0) → 0` and `Branch(1, 1) → 1`
    /// recursively. Already-normalized trees are returned unchanged.
    #[must_use]
    pub fn normalize(self) -> Self {
        match self {
            Self::Zero | Self::One => self,
            Self::Branch(l, r) => {
                let nl = l.normalize();
                let nr = r.normalize();
                Self::branch(nl, nr)
            }
        }
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => write!(f, "0"),
            Self::One => write!(f, "1"),
            Self::Branch(l, r) => write!(f, "({l:?}, {r:?})"),
        }
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Display uses the same compact format as Debug
        fmt::Debug::fmt(self, f)
    }
}

// ---------------------------------------------------------------------------
// Event tree
// ---------------------------------------------------------------------------

/// An ITC event tree, tracking causal history as a binary tree of counters.
///
/// The effective count at any position is the sum of counters along the
/// root-to-leaf path. Interior nodes carry a base counter that is shared
/// by both subtrees. Normalization lifts the minimum of two children's
/// leaf values into the parent and collapses uniform branches.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Event {
    /// Leaf with a counter value.
    Leaf(u32),
    /// Branch with a base counter and left/right subtrees.
    ///
    /// The effective value at any leaf in this subtree is `base` plus
    /// the value accumulated through the child path.
    Branch(u32, Box<Self>, Box<Self>),
}

impl Event {
    /// Create a leaf node with the given counter value.
    #[must_use]
    pub const fn leaf(value: u32) -> Self {
        Self::Leaf(value)
    }

    /// Create a zero leaf (no events recorded).
    #[must_use]
    pub const fn zero() -> Self {
        Self::Leaf(0)
    }

    /// Create a branch, automatically normalizing degenerate cases.
    ///
    /// Normalization rules:
    /// - `Branch(n, Leaf(a), Leaf(b))` where `a == b` → `Leaf(n + a)`
    /// - Otherwise, lifts the minimum of leaf children into the base:
    ///   `Branch(n, l, r)` → `Branch(n + min, l - min, r - min)`
    #[must_use]
    pub fn branch(base: u32, left: Self, right: Self) -> Self {
        match (&left, &right) {
            (Self::Leaf(a), Self::Leaf(b)) if a == b => Self::Leaf(base + a),
            _ => {
                let min_val = left.min_value().min(right.min_value());
                if min_val > 0 {
                    Self::Branch(
                        base + min_val,
                        Box::new(left.subtract_base(min_val)),
                        Box::new(right.subtract_base(min_val)),
                    )
                } else {
                    Self::Branch(base, Box::new(left), Box::new(right))
                }
            }
        }
    }

    /// Returns `true` if this is a leaf node.
    #[must_use]
    pub const fn is_leaf(&self) -> bool {
        matches!(self, Self::Leaf(_))
    }

    /// Returns `true` if this is a branch node.
    #[must_use]
    pub const fn is_branch(&self) -> bool {
        matches!(self, Self::Branch(..))
    }

    /// The base value at this node.
    ///
    /// For leaves, this is the counter value. For branches, the base counter.
    #[must_use]
    pub const fn value(&self) -> u32 {
        match self {
            Self::Leaf(n) | Self::Branch(n, _, _) => *n,
        }
    }

    /// The minimum effective value in the entire subtree.
    ///
    /// For leaves, this is just the counter. For branches, it is the base
    /// plus the minimum of the two children's minimums.
    #[must_use]
    pub fn min_value(&self) -> u32 {
        match self {
            Self::Leaf(n) => *n,
            Self::Branch(n, l, r) => n + l.min_value().min(r.min_value()),
        }
    }

    /// The maximum effective value in the entire subtree.
    ///
    /// For leaves, this is just the counter. For branches, it is the base
    /// plus the maximum of the two children's maximums.
    #[must_use]
    pub fn max_value(&self) -> u32 {
        match self {
            Self::Leaf(n) => *n,
            Self::Branch(n, l, r) => n + l.max_value().max(r.max_value()),
        }
    }

    /// Depth of the tree (0 for leaves).
    #[must_use]
    pub fn depth(&self) -> usize {
        match self {
            Self::Leaf(_) => 0,
            Self::Branch(_, l, r) => 1 + l.depth().max(r.depth()),
        }
    }

    /// Number of nodes in the tree (leaves + branches).
    #[must_use]
    pub fn node_count(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Branch(_, l, r) => 1 + l.node_count() + r.node_count(),
        }
    }

    /// Normalize the tree to its minimal representation.
    ///
    /// Recursively normalizes children, then applies:
    /// - `Branch(n, Leaf(a), Leaf(b))` where `a == b` → `Leaf(n + a)`
    /// - Lifts the common minimum into the base counter.
    #[must_use]
    pub fn normalize(self) -> Self {
        match self {
            Self::Leaf(_) => self,
            Self::Branch(n, l, r) => {
                let nl = l.normalize();
                let nr = r.normalize();
                Self::branch(n, nl, nr)
            }
        }
    }

    /// Lift the tree by adding `delta` to the base/leaf value.
    #[must_use]
    pub fn lift(self, delta: u32) -> Self {
        match self {
            Self::Leaf(n) => Self::Leaf(n + delta),
            Self::Branch(n, l, r) => Self::Branch(n + delta, l, r),
        }
    }

    /// Subtract `delta` from the base/leaf value.
    ///
    /// # Panics
    ///
    /// Panics if `delta` exceeds the current base/leaf value. This is
    /// an internal invariant — callers must ensure `delta <= self.value()`.
    #[must_use]
    fn subtract_base(self, delta: u32) -> Self {
        match self {
            Self::Leaf(n) => {
                assert!(delta <= n, "subtract_base: delta {delta} > leaf value {n}");
                Self::Leaf(n - delta)
            }
            Self::Branch(n, l, r) => {
                assert!(delta <= n, "subtract_base: delta {delta} > branch base {n}");
                Self::Branch(n - delta, l, r)
            }
        }
    }
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Leaf(n) => write!(f, "{n}"),
            Self::Branch(n, l, r) => write!(f, "({n}, {l:?}, {r:?})"),
        }
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

// ---------------------------------------------------------------------------
// Stamp
// ---------------------------------------------------------------------------

/// An ITC stamp: a pair of (ID tree, Event tree).
///
/// The stamp is the fundamental unit of causality tracking in ITC.
/// Each agent holds a stamp; operations like fork, join, and event
/// modify the stamp to track causal history.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Stamp {
    /// The identity partition owned by this stamp.
    pub id: Id,
    /// The causal history recorded by this stamp.
    pub event: Event,
}

impl Stamp {
    /// Create a new stamp with the given ID and event trees.
    #[must_use]
    pub const fn new(id: Id, event: Event) -> Self {
        Self { id, event }
    }

    /// Create the initial seed stamp: owns the full interval with zero events.
    ///
    /// This is the starting point for an ITC system. The seed stamp
    /// owns the entire \[0, 1) interval and has recorded no events.
    #[must_use]
    pub const fn seed() -> Self {
        Self {
            id: Id::one(),
            event: Event::zero(),
        }
    }

    /// Create an anonymous stamp: owns nothing, zero events.
    ///
    /// An anonymous stamp cannot record events but can receive causality
    /// information through join operations.
    #[must_use]
    pub const fn anonymous() -> Self {
        Self {
            id: Id::zero(),
            event: Event::zero(),
        }
    }

    /// Returns `true` if this stamp owns no interval.
    #[must_use]
    pub fn is_anonymous(&self) -> bool {
        self.id.is_zero()
    }

    /// Normalize both the ID and event trees to their minimal representations.
    #[must_use]
    pub fn normalize(self) -> Self {
        Self {
            id: self.id.normalize(),
            event: self.event.normalize(),
        }
    }
}

impl fmt::Display for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.id, self.event)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // === Id construction ====================================================

    #[test]
    fn test_id_zero() {
        let id = Id::zero();
        assert!(id.is_zero());
        assert!(!id.is_one());
        assert!(id.is_leaf());
        assert!(!id.is_branch());
        assert_eq!(id.depth(), 0);
        assert_eq!(id.node_count(), 1);
    }

    #[test]
    fn test_id_one() {
        let id = Id::one();
        assert!(!id.is_zero());
        assert!(id.is_one());
        assert!(id.is_leaf());
        assert!(!id.is_branch());
        assert_eq!(id.depth(), 0);
        assert_eq!(id.node_count(), 1);
    }

    #[test]
    fn test_id_branch_distinct_children() {
        let id = Id::branch(Id::one(), Id::zero());
        assert!(!id.is_zero());
        assert!(!id.is_one());
        assert!(!id.is_leaf());
        assert!(id.is_branch());
        assert_eq!(id.depth(), 1);
        assert_eq!(id.node_count(), 3);
    }

    // === Id normalization ===================================================

    #[test]
    fn test_id_branch_both_zero_normalizes() {
        let id = Id::branch(Id::zero(), Id::zero());
        assert_eq!(id, Id::Zero);
        assert!(id.is_zero());
    }

    #[test]
    fn test_id_branch_both_one_normalizes() {
        let id = Id::branch(Id::one(), Id::one());
        assert_eq!(id, Id::One);
        assert!(id.is_one());
    }

    #[test]
    fn test_id_nested_normalization() {
        // Branch(Branch(0,0), Branch(1,1)) should normalize to Branch(0, 1)
        let id = Id::branch(
            Id::branch(Id::zero(), Id::zero()),
            Id::branch(Id::one(), Id::one()),
        );
        assert_eq!(id, Id::branch(Id::zero(), Id::one()));
    }

    #[test]
    fn test_id_deep_normalization() {
        // (((1,1),(1,1)), ((1,1),(1,1))) → all ones → 1
        let inner = Id::branch(Id::one(), Id::one()); // → 1
        let mid = Id::branch(inner.clone(), inner); // → 1
        let outer = Id::branch(mid.clone(), mid); // → 1
        assert_eq!(outer, Id::One);
    }

    #[test]
    fn test_id_normalize_method() {
        // Manually construct un-normalized tree
        let id = Id::Branch(
            Box::new(Id::Branch(Box::new(Id::Zero), Box::new(Id::Zero))),
            Box::new(Id::One),
        );
        let normalized = id.normalize();
        assert_eq!(normalized, Id::branch(Id::zero(), Id::one()));
    }

    #[test]
    fn test_id_normalize_already_minimal() {
        let id = Id::branch(Id::one(), Id::zero());
        let normalized = id.clone().normalize();
        assert_eq!(id, normalized);
    }

    // === Id display =========================================================

    #[test]
    fn test_id_display_zero() {
        assert_eq!(format!("{}", Id::zero()), "0");
    }

    #[test]
    fn test_id_display_one() {
        assert_eq!(format!("{}", Id::one()), "1");
    }

    #[test]
    fn test_id_display_branch() {
        let id = Id::branch(Id::one(), Id::zero());
        assert_eq!(format!("{id}"), "(1, 0)");
    }

    #[test]
    fn test_id_display_nested() {
        let id = Id::branch(Id::branch(Id::one(), Id::zero()), Id::zero());
        assert_eq!(format!("{id}"), "((1, 0), 0)");
    }

    // === Id equality and clone ==============================================

    #[test]
    fn test_id_equality() {
        assert_eq!(Id::zero(), Id::zero());
        assert_eq!(Id::one(), Id::one());
        assert_ne!(Id::zero(), Id::one());

        let a = Id::branch(Id::one(), Id::zero());
        let b = Id::branch(Id::one(), Id::zero());
        assert_eq!(a, b);

        let c = Id::branch(Id::zero(), Id::one());
        assert_ne!(a, c);
    }

    #[test]
    fn test_id_clone() {
        let id = Id::branch(Id::one(), Id::branch(Id::zero(), Id::one()));
        let cloned = id.clone();
        assert_eq!(id, cloned);
    }

    // === Id complex structures ==============================================

    #[test]
    fn test_id_represents_left_half_partition() {
        // Agent owns left half of [0,1): (1, 0)
        let id = Id::branch(Id::one(), Id::zero());
        assert!(!id.is_zero());
        assert!(!id.is_one());
        assert_eq!(id.depth(), 1);
    }

    #[test]
    fn test_id_represents_quarter_partition() {
        // Agent owns first quarter: ((1,0), 0)
        let id = Id::branch(Id::branch(Id::one(), Id::zero()), Id::zero());
        assert_eq!(id.depth(), 2);
        assert_eq!(id.node_count(), 5);
    }

    // === Event construction =================================================

    #[test]
    fn test_event_zero() {
        let e = Event::zero();
        assert!(e.is_leaf());
        assert!(!e.is_branch());
        assert_eq!(e.value(), 0);
        assert_eq!(e.min_value(), 0);
        assert_eq!(e.max_value(), 0);
        assert_eq!(e.depth(), 0);
        assert_eq!(e.node_count(), 1);
    }

    #[test]
    fn test_event_leaf() {
        let e = Event::leaf(5);
        assert!(e.is_leaf());
        assert_eq!(e.value(), 5);
        assert_eq!(e.min_value(), 5);
        assert_eq!(e.max_value(), 5);
    }

    #[test]
    fn test_event_branch_distinct_children() {
        // Branch(1, Leaf(2), Leaf(3))
        let e = Event::branch(1, Event::leaf(2), Event::leaf(3));
        // Since leaf values differ, branch is kept
        assert!(e.is_branch());
        assert_eq!(e.value(), 3); // base 1 + lifted min 2
        assert_eq!(e.min_value(), 3); // 3 + 0 (left after subtraction)
        assert_eq!(e.max_value(), 4); // 3 + 1 (right after subtraction)
    }

    // === Event normalization ================================================

    #[test]
    fn test_event_branch_equal_leaves_normalizes() {
        // Branch(2, Leaf(3), Leaf(3)) → Leaf(5)
        let e = Event::branch(2, Event::leaf(3), Event::leaf(3));
        assert_eq!(e, Event::Leaf(5));
    }

    #[test]
    fn test_event_branch_zero_leaves_normalizes() {
        // Branch(0, Leaf(0), Leaf(0)) → Leaf(0)
        let e = Event::branch(0, Event::leaf(0), Event::leaf(0));
        assert_eq!(e, Event::Leaf(0));
    }

    #[test]
    fn test_event_branch_lifts_common_minimum() {
        // Branch(0, Leaf(3), Leaf(5))
        // min = 3, so becomes Branch(3, Leaf(0), Leaf(2))
        let e = Event::branch(0, Event::leaf(3), Event::leaf(5));
        assert_eq!(
            e,
            Event::Branch(3, Box::new(Event::Leaf(0)), Box::new(Event::Leaf(2)))
        );
    }

    #[test]
    fn test_event_branch_base_plus_lift() {
        // Branch(2, Leaf(3), Leaf(5))
        // min of children = 3, so base becomes 2+3=5
        // Children become Leaf(0), Leaf(2)
        let e = Event::branch(2, Event::leaf(3), Event::leaf(5));
        assert_eq!(
            e,
            Event::Branch(5, Box::new(Event::Leaf(0)), Box::new(Event::Leaf(2)))
        );
    }

    #[test]
    fn test_event_branch_one_child_zero_no_lift() {
        // Branch(0, Leaf(0), Leaf(3)) — min is 0, no lift needed
        let e = Event::branch(0, Event::leaf(0), Event::leaf(3));
        assert_eq!(
            e,
            Event::Branch(0, Box::new(Event::Leaf(0)), Box::new(Event::Leaf(3)))
        );
    }

    #[test]
    fn test_event_normalize_method() {
        // Manually construct un-normalized tree
        let e = Event::Branch(
            0,
            Box::new(Event::Branch(
                0,
                Box::new(Event::Leaf(2)),
                Box::new(Event::Leaf(2)),
            )),
            Box::new(Event::Leaf(2)),
        );
        let normalized = e.normalize();
        // Inner branch collapses: Branch(0, Leaf(2), Leaf(2)) → Leaf(2)
        // Then outer: Branch(0, Leaf(2), Leaf(2)) → Leaf(2)
        assert_eq!(normalized, Event::Leaf(2));
    }

    #[test]
    fn test_event_normalize_partial_collapse() {
        // Branch(0, Branch(0, Leaf(1), Leaf(1)), Leaf(3))
        // Inner → Leaf(1)
        // Outer: Branch(0, Leaf(1), Leaf(3)) → Branch(1, Leaf(0), Leaf(2))
        let e = Event::Branch(
            0,
            Box::new(Event::Branch(
                0,
                Box::new(Event::Leaf(1)),
                Box::new(Event::Leaf(1)),
            )),
            Box::new(Event::Leaf(3)),
        );
        let normalized = e.normalize();
        assert_eq!(
            normalized,
            Event::Branch(1, Box::new(Event::Leaf(0)), Box::new(Event::Leaf(2)))
        );
    }

    // === Event value calculations ===========================================

    #[test]
    fn test_event_min_value_branch() {
        // Branch(2, Leaf(1), Leaf(3)) → min = 2 + min(1, 3) = 3
        let e = Event::Branch(2, Box::new(Event::Leaf(1)), Box::new(Event::Leaf(3)));
        assert_eq!(e.min_value(), 3);
    }

    #[test]
    fn test_event_max_value_branch() {
        // Branch(2, Leaf(1), Leaf(3)) → max = 2 + max(1, 3) = 5
        let e = Event::Branch(2, Box::new(Event::Leaf(1)), Box::new(Event::Leaf(3)));
        assert_eq!(e.max_value(), 5);
    }

    #[test]
    fn test_event_min_max_deep() {
        // Branch(1, Branch(2, Leaf(0), Leaf(3)), Leaf(1))
        // Left subtree: min=1+2+0=3, max=1+2+3=6
        // Right subtree: min=max=1+1=2
        // Overall: min=2, max=6
        let e = Event::Branch(
            1,
            Box::new(Event::Branch(
                2,
                Box::new(Event::Leaf(0)),
                Box::new(Event::Leaf(3)),
            )),
            Box::new(Event::Leaf(1)),
        );
        assert_eq!(e.min_value(), 2);
        assert_eq!(e.max_value(), 6);
    }

    // === Event lift =========================================================

    #[test]
    fn test_event_lift_leaf() {
        let e = Event::leaf(3).lift(2);
        assert_eq!(e, Event::Leaf(5));
    }

    #[test]
    fn test_event_lift_branch() {
        let e = Event::Branch(1, Box::new(Event::Leaf(0)), Box::new(Event::Leaf(2)));
        let lifted = e.lift(3);
        assert_eq!(
            lifted,
            Event::Branch(4, Box::new(Event::Leaf(0)), Box::new(Event::Leaf(2)))
        );
    }

    #[test]
    fn test_event_lift_zero() {
        let e = Event::leaf(5);
        let lifted = e.lift(0);
        assert_eq!(lifted, Event::Leaf(5));
    }

    // === Event display ======================================================

    #[test]
    fn test_event_display_leaf() {
        assert_eq!(format!("{}", Event::leaf(7)), "7");
    }

    #[test]
    fn test_event_display_branch() {
        let e = Event::Branch(1, Box::new(Event::Leaf(0)), Box::new(Event::Leaf(2)));
        assert_eq!(format!("{e}"), "(1, 0, 2)");
    }

    // === Event equality and clone ===========================================

    #[test]
    fn test_event_equality() {
        assert_eq!(Event::zero(), Event::zero());
        assert_eq!(Event::leaf(3), Event::leaf(3));
        assert_ne!(Event::leaf(3), Event::leaf(4));
    }

    #[test]
    fn test_event_clone() {
        let e = Event::Branch(
            1,
            Box::new(Event::Branch(
                0,
                Box::new(Event::Leaf(1)),
                Box::new(Event::Leaf(2)),
            )),
            Box::new(Event::Leaf(3)),
        );
        let cloned = e.clone();
        assert_eq!(e, cloned);
    }

    // === Event depth and node count =========================================

    #[test]
    fn test_event_depth_nested() {
        let e = Event::Branch(
            0,
            Box::new(Event::Branch(
                0,
                Box::new(Event::Leaf(0)),
                Box::new(Event::Leaf(1)),
            )),
            Box::new(Event::Leaf(0)),
        );
        assert_eq!(e.depth(), 2);
    }

    #[test]
    fn test_event_node_count_nested() {
        // Branch(0, Branch(0, Leaf(0), Leaf(1)), Leaf(0))
        // = 1 + (1 + 1 + 1) + 1 = 5
        let e = Event::Branch(
            0,
            Box::new(Event::Branch(
                0,
                Box::new(Event::Leaf(0)),
                Box::new(Event::Leaf(1)),
            )),
            Box::new(Event::Leaf(0)),
        );
        assert_eq!(e.node_count(), 5);
    }

    // === Stamp construction =================================================

    #[test]
    fn test_stamp_seed() {
        let s = Stamp::seed();
        assert_eq!(s.id, Id::One);
        assert_eq!(s.event, Event::Leaf(0));
        assert!(!s.is_anonymous());
    }

    #[test]
    fn test_stamp_anonymous() {
        let s = Stamp::anonymous();
        assert_eq!(s.id, Id::Zero);
        assert_eq!(s.event, Event::Leaf(0));
        assert!(s.is_anonymous());
    }

    #[test]
    fn test_stamp_new() {
        let id = Id::branch(Id::one(), Id::zero());
        let event = Event::leaf(3);
        let s = Stamp::new(id.clone(), event.clone());
        assert_eq!(s.id, id);
        assert_eq!(s.event, event);
    }

    #[test]
    fn test_stamp_normalize() {
        let id = Id::Branch(Box::new(Id::One), Box::new(Id::One));
        let event = Event::Branch(0, Box::new(Event::Leaf(2)), Box::new(Event::Leaf(2)));
        let s = Stamp::new(id, event).normalize();
        assert_eq!(s.id, Id::One);
        assert_eq!(s.event, Event::Leaf(2));
    }

    // === Stamp display ======================================================

    #[test]
    fn test_stamp_display_seed() {
        let s = Stamp::seed();
        assert_eq!(format!("{s}"), "(1, 0)");
    }

    #[test]
    fn test_stamp_display_complex() {
        let s = Stamp::new(
            Id::branch(Id::one(), Id::zero()),
            Event::Branch(1, Box::new(Event::Leaf(0)), Box::new(Event::Leaf(2))),
        );
        assert_eq!(format!("{s}"), "((1, 0), (1, 0, 2))");
    }

    // === Stamp equality and clone ===========================================

    #[test]
    fn test_stamp_equality() {
        assert_eq!(Stamp::seed(), Stamp::seed());
        assert_eq!(Stamp::anonymous(), Stamp::anonymous());
        assert_ne!(Stamp::seed(), Stamp::anonymous());
    }

    #[test]
    fn test_stamp_clone() {
        let s = Stamp::new(
            Id::branch(Id::one(), Id::branch(Id::zero(), Id::one())),
            Event::Branch(2, Box::new(Event::Leaf(1)), Box::new(Event::Leaf(3))),
        );
        assert_eq!(s, s.clone());
    }

    // === Serde roundtrip ====================================================

    #[test]
    fn test_id_serde_roundtrip_zero() {
        let id = Id::zero();
        let json = serde_json::to_string(&id).unwrap();
        let deser: Id = serde_json::from_str(&json).unwrap();
        assert_eq!(id, deser);
    }

    #[test]
    fn test_id_serde_roundtrip_one() {
        let id = Id::one();
        let json = serde_json::to_string(&id).unwrap();
        let deser: Id = serde_json::from_str(&json).unwrap();
        assert_eq!(id, deser);
    }

    #[test]
    fn test_id_serde_roundtrip_branch() {
        let id = Id::branch(Id::one(), Id::branch(Id::zero(), Id::one()));
        let json = serde_json::to_string(&id).unwrap();
        let deser: Id = serde_json::from_str(&json).unwrap();
        assert_eq!(id, deser);
    }

    #[test]
    fn test_event_serde_roundtrip_leaf() {
        let e = Event::leaf(42);
        let json = serde_json::to_string(&e).unwrap();
        let deser: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(e, deser);
    }

    #[test]
    fn test_event_serde_roundtrip_branch() {
        let e = Event::Branch(
            3,
            Box::new(Event::Leaf(0)),
            Box::new(Event::Branch(
                1,
                Box::new(Event::Leaf(2)),
                Box::new(Event::Leaf(0)),
            )),
        );
        let json = serde_json::to_string(&e).unwrap();
        let deser: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(e, deser);
    }

    #[test]
    fn test_stamp_serde_roundtrip() {
        let s = Stamp::new(
            Id::branch(Id::one(), Id::branch(Id::zero(), Id::one())),
            Event::Branch(2, Box::new(Event::Leaf(1)), Box::new(Event::Leaf(3))),
        );
        let json = serde_json::to_string(&s).unwrap();
        let deser: Stamp = serde_json::from_str(&json).unwrap();
        assert_eq!(s, deser);
    }

    #[test]
    fn test_stamp_serde_roundtrip_seed() {
        let s = Stamp::seed();
        let json = serde_json::to_string(&s).unwrap();
        let deser: Stamp = serde_json::from_str(&json).unwrap();
        assert_eq!(s, deser);
    }

    // === Normalization invariants ===========================================

    #[test]
    fn test_normalization_idempotent_id() {
        let id = Id::branch(
            Id::branch(Id::one(), Id::zero()),
            Id::branch(Id::zero(), Id::one()),
        );
        let n1 = id.clone().normalize();
        let n2 = n1.clone().normalize();
        assert_eq!(n1, n2);
    }

    #[test]
    fn test_normalization_idempotent_event() {
        let e = Event::Branch(
            0,
            Box::new(Event::Branch(
                1,
                Box::new(Event::Leaf(2)),
                Box::new(Event::Leaf(3)),
            )),
            Box::new(Event::Leaf(5)),
        );
        let n1 = e.clone().normalize();
        let n2 = n1.clone().normalize();
        assert_eq!(n1, n2);
    }

    #[test]
    fn test_normalization_preserves_semantics() {
        // Normalizing should not change min/max values
        let e = Event::Branch(
            0,
            Box::new(Event::Branch(
                0,
                Box::new(Event::Leaf(2)),
                Box::new(Event::Leaf(2)),
            )),
            Box::new(Event::Leaf(5)),
        );
        let min_before = e.min_value();
        let max_before = e.max_value();
        let normalized = e.normalize();
        assert_eq!(normalized.min_value(), min_before);
        assert_eq!(normalized.max_value(), max_before);
    }

    // === Edge cases =========================================================

    #[test]
    fn test_event_branch_with_nested_branches() {
        // Complex nested structure that partially normalizes
        let e = Event::branch(
            1,
            Event::branch(0, Event::leaf(2), Event::leaf(4)),
            Event::leaf(3),
        );
        // Inner: branch(0, leaf(2), leaf(4)) → Branch(2, Leaf(0), Leaf(2))
        // Outer: branch(1, Branch(2, Leaf(0), Leaf(2)), Leaf(3))
        //   min of children: min(Branch(2, L0, L2).min_value(), 3) = min(2, 3) = 2
        //   base: 1 + 2 = 3
        //   left: Branch(2-2=0, L0, L2) = Branch(0, L0, L2)
        //   right: Leaf(3-2=1)
        assert_eq!(e.min_value(), 3);
        assert_eq!(e.max_value(), 5);
    }

    #[test]
    fn test_event_large_values() {
        let e = Event::leaf(u32::MAX - 1);
        assert_eq!(e.value(), u32::MAX - 1);
        assert_eq!(e.min_value(), u32::MAX - 1);
        let lifted = e.lift(1);
        assert_eq!(lifted.value(), u32::MAX);
    }

    #[test]
    fn test_id_deeply_nested() {
        // Build a 10-level deep ID tree
        let mut id = Id::one();
        for _ in 0..10 {
            id = Id::branch(id, Id::zero());
        }
        assert_eq!(id.depth(), 10);
        // node_count: 10 branches + 10 zero leaves + 1 one leaf = 21
        assert_eq!(id.node_count(), 21);
    }

    #[test]
    fn test_stamp_with_complex_trees() {
        // Realistic stamp after several fork operations
        let id = Id::branch(Id::branch(Id::one(), Id::zero()), Id::zero());
        let event = Event::Branch(
            2,
            Box::new(Event::Branch(
                1,
                Box::new(Event::Leaf(0)),
                Box::new(Event::Leaf(1)),
            )),
            Box::new(Event::Leaf(0)),
        );
        let s = Stamp::new(id, event);
        assert!(!s.is_anonymous());
        assert_eq!(s.event.min_value(), 2);
        assert_eq!(s.event.max_value(), 4);
    }
}
