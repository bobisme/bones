//! ITC operations: fork, join, event, peek, leq, concurrent.
//!
//! Implements the core Interval Tree Clock operations from:
//! Almeida, Baquero & Fonte (2008) "Interval Tree Clocks".
//!
//! These operations form the public API consumed by the CRDT layer
//! for causal comparison and LWW tie-breaking.

use super::itc::{Event, Id, Stamp};

// ===========================================================================
// Id operations (split / sum)
// ===========================================================================

/// Split an ID tree into two halves. The original interval is partitioned
/// so that left ∪ right = original and left ∩ right = ∅.
fn split_id(id: &Id) -> (Id, Id) {
    match id {
        Id::Zero => (Id::zero(), Id::zero()),
        Id::One => (
            Id::branch(Id::one(), Id::zero()),
            Id::branch(Id::zero(), Id::one()),
        ),
        Id::Branch(l, r) => {
            if !l.is_zero() && r.is_zero() {
                // Only left has ownership — split the left subtree
                let (ll, lr) = split_id(l);
                (Id::branch(ll, Id::zero()), Id::branch(lr, Id::zero()))
            } else if l.is_zero() && !r.is_zero() {
                // Only right has ownership — split the right subtree
                let (rl, rr) = split_id(r);
                (Id::branch(Id::zero(), rl), Id::branch(Id::zero(), rr))
            } else {
                // Both have ownership — give left to one, right to the other
                (
                    Id::branch((**l).clone(), Id::zero()),
                    Id::branch(Id::zero(), (**r).clone()),
                )
            }
        }
    }
}

/// Merge two ID trees into one (the union of their intervals).
fn sum_id(a: &Id, b: &Id) -> Id {
    match (a, b) {
        (Id::Zero, _) => b.clone(),
        (_, Id::Zero) => a.clone(),
        (Id::Branch(al, ar), Id::Branch(bl, br)) => {
            Id::branch(sum_id(al, bl), sum_id(ar, br))
        }
        // (One, anything-non-zero) or (anything-non-zero, One) — both own some
        // part, and since ITC invariant says intervals don't overlap, this
        // can only happen when merging complementary halves → result is One.
        _ => Id::one(),
    }
}

// ===========================================================================
// Event operations (join, leq, fill, grow)
// ===========================================================================

/// Join (merge) two event trees, taking the pointwise maximum.
fn join_event(a: &Event, b: &Event) -> Event {
    match (a, b) {
        (Event::Leaf(na), Event::Leaf(nb)) => Event::leaf((*na).max(*nb)),
        (Event::Leaf(na), Event::Branch(nb, bl, br)) => {
            if *na >= b.max_value() {
                Event::leaf(*na)
            } else {
                let lifted_a_l = Event::leaf(na.saturating_sub(*nb));
                let lifted_a_r = Event::leaf(na.saturating_sub(*nb));
                Event::branch(
                    *nb,
                    join_event(&lifted_a_l, bl),
                    join_event(&lifted_a_r, br),
                )
            }
        }
        (Event::Branch(na, al, ar), Event::Leaf(nb)) => {
            if *nb >= a.max_value() {
                Event::leaf(*nb)
            } else {
                let lifted_b_l = Event::leaf(nb.saturating_sub(*na));
                let lifted_b_r = Event::leaf(nb.saturating_sub(*na));
                Event::branch(
                    *na,
                    join_event(al, &lifted_b_l),
                    join_event(ar, &lifted_b_r),
                )
            }
        }
        (Event::Branch(na, al, ar), Event::Branch(nb, bl, br)) => {
            if na >= nb {
                // a's base is higher — lift a's children by the diff,
                // use b's base (the lower), and merge.
                let diff = na - nb;
                let lifted_al = (**al).clone().lift(diff);
                let lifted_ar = (**ar).clone().lift(diff);
                Event::branch(*nb, join_event(&lifted_al, bl), join_event(&lifted_ar, br))
            } else {
                // b's base is higher — lift b's children by the diff,
                // use a's base (the lower), and merge.
                let diff = nb - na;
                let lifted_bl = (**bl).clone().lift(diff);
                let lifted_br = (**br).clone().lift(diff);
                Event::branch(*na, join_event(al, &lifted_bl), join_event(ar, &lifted_br))
            }
        }
    }
}

/// Causal ordering: returns true if event tree `a` ≤ event tree `b`
/// (every position in `a` has count ≤ the corresponding position in `b`).
fn leq_event(a: &Event, b: &Event) -> bool {
    match (a, b) {
        (Event::Leaf(na), Event::Leaf(nb)) => na <= nb,
        (Event::Leaf(na), Event::Branch(nb, bl, br)) => {
            // a is flat at *na; b has base *nb plus children.
            // We need na ≤ nb + bl[pos] and na ≤ nb + br[pos] for all positions.
            // Since na is flat: if na ≤ nb, then certainly na ≤ nb + child for all children (children ≥ 0).
            // Otherwise, we need (na - nb) ≤ every value in child.
            if *na <= *nb {
                true
            } else {
                let remainder = na - nb;
                leq_event(&Event::leaf(remainder), bl)
                    && leq_event(&Event::leaf(remainder), br)
            }
        }
        (Event::Branch(_na, _al, _ar), Event::Leaf(nb)) => {
            // a has structure, b is flat.
            // Need na + al[pos] ≤ nb and na + ar[pos] ≤ nb for all positions.
            // This means a.max_value() ≤ nb.
            a.max_value() <= *nb
        }
        (Event::Branch(na, al, ar), Event::Branch(nb, bl, br)) => {
            // We need: na + al(pos) <= nb + bl(pos) for all positions.
            // Equivalently: al(pos) <= bl(pos) + (nb - na) when nb >= na,
            // or: al(pos) + (na - nb) <= bl(pos) when na > nb.
            if na <= nb {
                let diff = nb - na;
                // Lift b's children by diff: al <= lift(bl, diff)
                let lifted_bl = (**bl).clone().lift(diff);
                let lifted_br = (**br).clone().lift(diff);
                leq_event(al, &lifted_bl) && leq_event(ar, &lifted_br)
            } else {
                let diff = na - nb;
                // Lift a's children by diff: lift(al, diff) <= bl
                let lifted_al = (**al).clone().lift(diff);
                let lifted_ar = (**ar).clone().lift(diff);
                leq_event(&lifted_al, bl) && leq_event(&lifted_ar, br)
            }
        }
    }
}

/// Fill the event tree where the ID owns the interval, up to the
/// maximum possible without exceeding the existing counters.
///
/// Returns `(filled_event, did_change)`.
fn fill(id: &Id, event: &Event) -> (Event, bool) {
    match (id, event) {
        (Id::Zero, _) => (event.clone(), false),
        (Id::One, Event::Leaf(_)) => (event.clone(), false),
        (Id::One, Event::Branch(n, l, r)) => {
            // Fill everything — find the max of the children and collapse
            let max_child = l.max_value().max(r.max_value());
            (Event::leaf(n + max_child), true)
        }
        (Id::Branch(il, ir), Event::Leaf(n)) => {
            // Expand the leaf into a branch so we can fill selectively
            let (el, changed_l) = fill(il, &Event::leaf(0));
            let (er, changed_r) = fill(ir, &Event::leaf(0));
            if changed_l || changed_r {
                (Event::branch(*n, el, er), true)
            } else {
                (Event::Leaf(*n), false)
            }
        }
        (Id::Branch(il, ir), Event::Branch(n, el, er)) => {
            let (new_l, changed_l) = fill(il, el);
            let (new_r, changed_r) = fill(ir, er);
            if changed_l || changed_r {
                (Event::branch(*n, new_l, new_r), true)
            } else {
                (event.clone(), false)
            }
        }
    }
}

/// Grow the event tree by inflating at a position where the ID owns the interval.
///
/// Returns `(new_event, cost)` where cost is the increase in node count
/// (used to pick the minimal growth). Returns `None` if growth isn't possible
/// (e.g., anonymous ID).
fn grow(id: &Id, event: &Event) -> Option<(Event, u32)> {
    match (id, event) {
        (Id::One, Event::Leaf(n)) => {
            // Simple increment at a fully-owned leaf
            Some((Event::leaf(n + 1), 0))
        }
        (Id::Zero, _) => None,
        (Id::One, Event::Branch(n, l, r)) => {
            // We own everything — try growing left or right, pick cheapest
            let opt_l = grow(&Id::one(), l);
            let opt_r = grow(&Id::one(), r);
            match (opt_l, opt_r) {
                (Some((new_l, cl)), Some((new_r, cr))) => {
                    if cl <= cr {
                        Some((Event::branch(*n, new_l, (**r).clone()), cl))
                    } else {
                        Some((Event::branch(*n, (**l).clone(), new_r), cr))
                    }
                }
                (Some((new_l, cl)), None) => {
                    Some((Event::branch(*n, new_l, (**r).clone()), cl))
                }
                (None, Some((new_r, cr))) => {
                    Some((Event::branch(*n, (**l).clone(), new_r), cr))
                }
                (None, None) => None,
            }
        }
        (Id::Branch(il, ir), Event::Leaf(n)) => {
            // Event is a leaf but ID has structure — expand into branch
            let opt_l = grow(il, &Event::leaf(0));
            let opt_r = grow(ir, &Event::leaf(0));
            match (opt_l, opt_r) {
                (Some((new_l, cl)), Some((new_r, cr))) => {
                    if cl < cr {
                        Some((Event::branch(*n, new_l, Event::leaf(0)), cl + 1_000))
                    } else {
                        Some((Event::branch(*n, Event::leaf(0), new_r), cr + 1_000))
                    }
                }
                (Some((new_l, cl)), None) => {
                    Some((Event::branch(*n, new_l, Event::leaf(0)), cl + 1_000))
                }
                (None, Some((new_r, cr))) => {
                    Some((Event::branch(*n, Event::leaf(0), new_r), cr + 1_000))
                }
                (None, None) => None,
            }
        }
        (Id::Branch(il, ir), Event::Branch(n, el, er)) => {
            let opt_l = grow(il, el);
            let opt_r = grow(ir, er);
            match (opt_l, opt_r) {
                (Some((new_l, cl)), Some((new_r, cr))) => {
                    if cl <= cr {
                        Some((Event::branch(*n, new_l, (**er).clone()), cl))
                    } else {
                        Some((Event::branch(*n, (**el).clone(), new_r), cr))
                    }
                }
                (Some((new_l, cl)), None) => {
                    Some((Event::branch(*n, new_l, (**er).clone()), cl))
                }
                (None, Some((new_r, cr))) => {
                    Some((Event::branch(*n, (**el).clone(), new_r), cr))
                }
                (None, None) => None,
            }
        }
    }
}

// ===========================================================================
// Public Stamp operations
// ===========================================================================

impl Stamp {
    /// Split this stamp's ID interval into two halves.
    ///
    /// Used when a new agent forks from an existing one.
    /// Returns `(left, right)` where `left` and `right` partition
    /// the original interval: `left ∪ right = original`, `left ∩ right = ∅`.
    ///
    /// Both stamps share the same event tree (causal history at time of fork).
    ///
    /// # Panics
    ///
    /// Panics if this stamp is anonymous (owns no interval to split).
    #[must_use]
    pub fn fork(&self) -> (Stamp, Stamp) {
        assert!(
            !self.id.is_zero(),
            "cannot fork an anonymous stamp (owns no interval)"
        );
        let (id_l, id_r) = split_id(&self.id);
        (
            Stamp::new(id_l, self.event.clone()).normalize(),
            Stamp::new(id_r, self.event.clone()).normalize(),
        )
    }

    /// Merge two stamps into one, combining their ID intervals and
    /// taking the pointwise maximum of their event trees.
    ///
    /// Used when an agent retires and donates its interval back, or
    /// when synchronizing causality information between agents.
    #[must_use]
    pub fn join(a: &Stamp, b: &Stamp) -> Stamp {
        let id = sum_id(&a.id, &b.id);
        let event = join_event(&a.event, &b.event);
        Stamp::new(id, event).normalize()
    }

    /// Record a new event, inflating the event tree.
    ///
    /// Called on every event emission. The event counter is monotonically
    /// increased at a position owned by this stamp's ID. Uses the
    /// fill-then-grow strategy from the ITC paper for minimal tree growth.
    ///
    /// # Panics
    ///
    /// Panics if this stamp is anonymous (cannot record events without
    /// owning part of the interval).
    pub fn event(&mut self) {
        assert!(
            !self.id.is_zero(),
            "cannot record event on an anonymous stamp"
        );

        // First try to fill — collapse structure where we own everything
        let (filled, changed) = fill(&self.id, &self.event);
        if changed {
            self.event = filled;
            // Normalize after fill
            *self = self.clone().normalize();
            return;
        }

        // Fill didn't help — grow the event tree
        if let Some((grown, _cost)) = grow(&self.id, &self.event) {
            self.event = grown;
            *self = self.clone().normalize();
        } else {
            // This should not happen if the stamp owns an interval
            panic!("event: could not grow event tree (internal error)");
        }
    }

    /// Read the current event tree without incrementing.
    #[must_use]
    pub fn peek(&self) -> &Event {
        &self.event
    }

    /// Causal dominance: returns `true` if `self` happened-before-or-equal `other`.
    ///
    /// This means every event recorded by `self` is also recorded by `other`.
    #[must_use]
    pub fn leq(&self, other: &Stamp) -> bool {
        leq_event(&self.event, &other.event)
    }

    /// Returns `true` if neither stamp causally dominates the other.
    ///
    /// Two stamps are concurrent if there exist events in each that the
    /// other has not observed.
    #[must_use]
    pub fn concurrent(&self, other: &Stamp) -> bool {
        !self.leq(other) && !other.leq(self)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // === fork ===============================================================

    #[test]
    fn fork_seed_produces_two_halves() {
        let seed = Stamp::seed();
        let (left, right) = seed.fork();

        // Each half owns part of the interval
        assert!(!left.id.is_zero());
        assert!(!right.id.is_zero());
        // Neither owns the full interval
        assert!(!left.id.is_one());
        assert!(!right.id.is_one());
        // Both share the same (zero) event tree
        assert_eq!(left.event, Event::zero());
        assert_eq!(right.event, Event::zero());
    }

    #[test]
    fn fork_preserves_interval_coverage() {
        let seed = Stamp::seed();
        let (left, right) = seed.fork();
        // Joining the IDs should recover the full interval
        let reunited = sum_id(&left.id, &right.id);
        assert_eq!(reunited, Id::one());
    }

    #[test]
    fn fork_ids_are_disjoint() {
        let seed = Stamp::seed();
        let (left, right) = seed.fork();
        // The expected split of seed (1) is (1,0) and (0,1)
        assert_eq!(left.id, Id::branch(Id::one(), Id::zero()));
        assert_eq!(right.id, Id::branch(Id::zero(), Id::one()));
    }

    #[test]
    fn fork_of_half_further_splits() {
        let seed = Stamp::seed();
        let (left, _) = seed.fork();
        let (ll, lr) = left.fork();
        // ll and lr should subdivide the left half
        assert!(!ll.id.is_zero());
        assert!(!lr.id.is_zero());
        // Reuniting them gives back the original left ID
        let reunited = sum_id(&ll.id, &lr.id);
        assert_eq!(reunited, Id::branch(Id::one(), Id::zero()));
    }

    #[test]
    fn fork_preserves_event_history() {
        let mut s = Stamp::seed();
        s.event();
        s.event();
        let (left, right) = s.fork();
        // Both children inherit the parent's event tree
        assert_eq!(left.event, s.event);
        assert_eq!(right.event, s.event);
    }

    #[test]
    #[should_panic(expected = "cannot fork an anonymous stamp")]
    fn fork_anonymous_panics() {
        let anon = Stamp::anonymous();
        let _ = anon.fork();
    }

    // === join ===============================================================

    #[test]
    fn join_recovers_seed_from_fork() {
        let seed = Stamp::seed();
        let (left, right) = seed.fork();
        let joined = Stamp::join(&left, &right);
        assert_eq!(joined.id, Id::one());
        assert_eq!(joined.event, Event::zero());
    }

    #[test]
    fn join_merges_divergent_events() {
        let seed = Stamp::seed();
        let (mut a, mut b) = seed.fork();
        a.event();
        b.event();
        b.event();

        let joined = Stamp::join(&a, &b);
        // Joined should dominate both
        assert!(a.leq(&joined));
        assert!(b.leq(&joined));
    }

    #[test]
    fn join_with_anonymous() {
        let seed = Stamp::seed();
        let anon = Stamp::anonymous();
        let joined = Stamp::join(&seed, &anon);
        assert_eq!(joined.id, Id::one());
        assert_eq!(joined.event, Event::zero());
    }

    #[test]
    fn join_is_commutative() {
        let seed = Stamp::seed();
        let (mut a, mut b) = seed.fork();
        a.event();
        b.event();
        b.event();

        let ab = Stamp::join(&a, &b);
        let ba = Stamp::join(&b, &a);
        assert_eq!(ab, ba);
    }

    // === event ==============================================================

    #[test]
    fn event_monotonically_increases() {
        let mut s = Stamp::seed();
        let before = s.event.max_value();
        s.event();
        let after = s.event.max_value();
        assert!(after > before, "event must increase: {before} -> {after}");
    }

    #[test]
    fn event_multiple_increments() {
        let mut s = Stamp::seed();
        for i in 1..=10 {
            s.event();
            assert!(
                s.event.max_value() >= i,
                "after {i} events, max_value should be >= {i}"
            );
        }
    }

    #[test]
    fn event_on_forked_stamp() {
        let seed = Stamp::seed();
        let (mut a, _b) = seed.fork();
        a.event();
        assert!(a.event.max_value() >= 1);
    }

    #[test]
    #[should_panic(expected = "cannot record event on an anonymous stamp")]
    fn event_anonymous_panics() {
        let mut anon = Stamp::anonymous();
        anon.event();
    }

    // === peek ===============================================================

    #[test]
    fn peek_returns_event_ref() {
        let s = Stamp::seed();
        assert_eq!(s.peek(), &Event::zero());
    }

    #[test]
    fn peek_reflects_events() {
        let mut s = Stamp::seed();
        s.event();
        let peeked = s.peek();
        assert!(peeked.max_value() >= 1);
    }

    // === leq ===============================================================

    #[test]
    fn leq_identical_stamps() {
        let s = Stamp::seed();
        assert!(s.leq(&s));
    }

    #[test]
    fn leq_after_event() {
        let mut s = Stamp::seed();
        let before = s.clone();
        s.event();
        assert!(before.leq(&s), "before should be <= after event");
        assert!(!s.leq(&before), "after event should NOT be <= before");
    }

    #[test]
    fn leq_forked_then_diverged() {
        let seed = Stamp::seed();
        let (mut a, mut b) = seed.fork();
        a.event();
        b.event();
        // After independent events, neither dominates
        assert!(!a.leq(&b));
        assert!(!b.leq(&a));
    }

    #[test]
    fn leq_joined_dominates_parts() {
        let seed = Stamp::seed();
        let (mut a, mut b) = seed.fork();
        a.event();
        b.event();
        let joined = Stamp::join(&a, &b);
        assert!(a.leq(&joined));
        assert!(b.leq(&joined));
    }

    #[test]
    fn leq_zero_events() {
        let a = Stamp::seed();
        let b = Stamp::seed();
        assert!(a.leq(&b));
        assert!(b.leq(&a));
    }

    #[test]
    fn leq_transitive() {
        let mut s = Stamp::seed();
        let s0 = s.clone();
        s.event();
        let s1 = s.clone();
        s.event();
        let s2 = s.clone();

        assert!(s0.leq(&s1));
        assert!(s1.leq(&s2));
        assert!(s0.leq(&s2)); // transitivity
    }

    // === concurrent =========================================================

    #[test]
    fn concurrent_after_fork_and_events() {
        let seed = Stamp::seed();
        let (mut a, mut b) = seed.fork();
        a.event();
        b.event();
        assert!(a.concurrent(&b));
        assert!(b.concurrent(&a));
    }

    #[test]
    fn not_concurrent_when_dominated() {
        let mut s = Stamp::seed();
        let before = s.clone();
        s.event();
        assert!(!before.concurrent(&s));
        assert!(!s.concurrent(&before));
    }

    #[test]
    fn not_concurrent_when_equal() {
        let s = Stamp::seed();
        assert!(!s.concurrent(&s));
    }

    #[test]
    fn concurrent_is_symmetric() {
        let seed = Stamp::seed();
        let (mut a, mut b) = seed.fork();
        a.event();
        b.event();
        assert_eq!(a.concurrent(&b), b.concurrent(&a));
    }

    // === Multi-agent scenarios ==============================================

    #[test]
    fn two_agent_fork_work_retire() {
        let seed = Stamp::seed();
        let (mut a, mut b) = seed.fork();

        // Agent A does 3 events
        a.event();
        a.event();
        a.event();

        // Agent B does 2 events
        b.event();
        b.event();

        // They're concurrent
        assert!(a.concurrent(&b));

        // Retire: join them back
        let retired = Stamp::join(&a, &b);
        assert!(a.leq(&retired));
        assert!(b.leq(&retired));
        assert_eq!(retired.id, Id::one());
    }

    #[test]
    fn four_agent_scenario() {
        let seed = Stamp::seed();
        let (ab, cd) = seed.fork();
        let (mut a, mut b) = ab.fork();
        let (mut c, mut d) = cd.fork();

        // Each agent does some work
        a.event();
        b.event();
        b.event();
        c.event();
        c.event();
        c.event();
        d.event();

        // All pairs are concurrent
        assert!(a.concurrent(&b));
        assert!(a.concurrent(&c));
        assert!(a.concurrent(&d));
        assert!(b.concurrent(&c));
        assert!(b.concurrent(&d));
        assert!(c.concurrent(&d));

        // Merge a+b, c+d, then everything
        let ab_merged = Stamp::join(&a, &b);
        let cd_merged = Stamp::join(&c, &d);
        assert!(a.leq(&ab_merged));
        assert!(b.leq(&ab_merged));
        assert!(c.leq(&cd_merged));
        assert!(d.leq(&cd_merged));

        let all = Stamp::join(&ab_merged, &cd_merged);
        assert!(a.leq(&all));
        assert!(b.leq(&all));
        assert!(c.leq(&all));
        assert!(d.leq(&all));
        assert_eq!(all.id, Id::one());
    }

    #[test]
    fn eight_agent_fork_work_retire_cycle() {
        let seed = Stamp::seed();

        // Fork into 8 agents
        let (half_l, half_r) = seed.fork();
        let (q1, q2) = half_l.fork();
        let (q3, q4) = half_r.fork();
        let (mut a1, mut a2) = q1.fork();
        let (mut a3, mut a4) = q2.fork();
        let (mut a5, mut a6) = q3.fork();
        let (mut a7, mut a8) = q4.fork();

        let mut agents = [&mut a1, &mut a2, &mut a3, &mut a4, &mut a5, &mut a6, &mut a7, &mut a8];

        // Each agent does (i+1) events
        for (i, agent) in agents.iter_mut().enumerate() {
            for _ in 0..=(i as u32) {
                agent.event();
            }
        }

        // All agents should be pairwise concurrent
        let snapshots: Vec<Stamp> = [&a1, &a2, &a3, &a4, &a5, &a6, &a7, &a8]
            .iter()
            .map(|s| (*s).clone())
            .collect();

        for i in 0..8 {
            for j in (i + 1)..8 {
                assert!(
                    snapshots[i].concurrent(&snapshots[j]),
                    "agents {i} and {j} should be concurrent"
                );
            }
        }

        // Retire all agents back into one stamp
        let mut merged = snapshots[0].clone();
        for s in &snapshots[1..] {
            merged = Stamp::join(&merged, s);
        }

        // Merged dominates all
        for (i, s) in snapshots.iter().enumerate() {
            assert!(s.leq(&merged), "agent {i} should be <= merged");
        }

        // Merged stamp should own the full interval
        assert_eq!(merged.id, Id::one());
    }

    #[test]
    fn sixteen_agent_cycle() {
        let seed = Stamp::seed();

        // Fork tree to get 16 agents
        fn fork_n(stamp: Stamp, depth: u32) -> Vec<Stamp> {
            if depth == 0 {
                return vec![stamp];
            }
            let (l, r) = stamp.fork();
            let mut result = fork_n(l, depth - 1);
            result.extend(fork_n(r, depth - 1));
            result
        }

        let mut agents = fork_n(seed, 4);
        assert_eq!(agents.len(), 16);

        // Each agent does some work
        for (i, agent) in agents.iter_mut().enumerate() {
            for _ in 0..((i % 5) + 1) {
                agent.event();
            }
        }

        // Verify pairwise concurrency
        for i in 0..16 {
            for j in (i + 1)..16 {
                assert!(
                    agents[i].concurrent(&agents[j]),
                    "agents {i} and {j} should be concurrent"
                );
            }
        }

        // Merge all back
        let mut merged = agents[0].clone();
        for a in &agents[1..] {
            merged = Stamp::join(&merged, a);
        }

        for (i, a) in agents.iter().enumerate() {
            assert!(a.leq(&merged), "agent {i} should be <= merged");
        }
        assert_eq!(merged.id, Id::one());
    }

    // === Property tests =====================================================

    proptest! {
        #[test]
        fn prop_fork_join_roundtrip(events_before in 0u32..10) {
            let mut s = Stamp::seed();
            for _ in 0..events_before {
                s.event();
            }
            let original_event = s.event.clone();

            let (a, b) = s.fork();
            let joined = Stamp::join(&a, &b);

            // ID should be fully recovered
            prop_assert_eq!(&joined.id, &Id::one());
            // Event should be at least as advanced as the original
            prop_assert!(leq_event(&original_event, &joined.event));
        }

        #[test]
        fn prop_event_monotonic(n_events in 1u32..20) {
            let mut s = Stamp::seed();
            let mut prev_max = 0u32;
            for _ in 0..n_events {
                s.event();
                let new_max = s.event.max_value();
                prop_assert!(new_max > prev_max, "monotonicity violated: {} -> {}", prev_max, new_max);
                prev_max = new_max;
            }
        }

        #[test]
        fn prop_leq_reflexive(n_events in 0u32..10) {
            let mut s = Stamp::seed();
            for _ in 0..n_events {
                s.event();
            }
            prop_assert!(s.leq(&s));
        }

        #[test]
        fn prop_leq_antisymmetric(n_a in 0u32..5, n_b in 0u32..5) {
            let seed = Stamp::seed();
            let (mut a, mut b) = seed.fork();
            for _ in 0..n_a {
                a.event();
            }
            for _ in 0..n_b {
                b.event();
            }
            if a.leq(&b) && b.leq(&a) {
                prop_assert_eq!(a.event, b.event);
            }
        }

        #[test]
        fn prop_fork_preserves_coverage(n_events in 0u32..5) {
            let mut s = Stamp::seed();
            for _ in 0..n_events {
                s.event();
            }
            let (a, b) = s.fork();
            let reunited = sum_id(&a.id, &b.id);
            prop_assert_eq!(reunited, Id::one());
        }

        #[test]
        fn prop_join_dominates_both(n_a in 1u32..5, n_b in 1u32..5) {
            let seed = Stamp::seed();
            let (mut a, mut b) = seed.fork();
            for _ in 0..n_a {
                a.event();
            }
            for _ in 0..n_b {
                b.event();
            }
            let joined = Stamp::join(&a, &b);
            prop_assert!(a.leq(&joined), "a should be <= join(a,b)");
            prop_assert!(b.leq(&joined), "b should be <= join(a,b)");
        }

        #[test]
        fn prop_concurrent_symmetric(n_a in 1u32..5, n_b in 1u32..5) {
            let seed = Stamp::seed();
            let (mut a, mut b) = seed.fork();
            for _ in 0..n_a {
                a.event();
            }
            for _ in 0..n_b {
                b.event();
            }
            prop_assert_eq!(a.concurrent(&b), b.concurrent(&a));
        }

        #[test]
        fn prop_leq_transitive(n1 in 0u32..4, n2 in 1u32..4, n3 in 1u32..4) {
            let mut s = Stamp::seed();
            for _ in 0..n1 {
                s.event();
            }
            let s0 = s.clone();
            for _ in 0..n2 {
                s.event();
            }
            let s1 = s.clone();
            for _ in 0..n3 {
                s.event();
            }
            let s2 = s;

            prop_assert!(s0.leq(&s1));
            prop_assert!(s1.leq(&s2));
            prop_assert!(s0.leq(&s2));
        }
    }
}
