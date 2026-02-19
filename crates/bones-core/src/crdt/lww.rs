//! Last-Writer-Wins (LWW) Register CRDT.
//!
//! LWW Register is the CRDT for scalar fields: title, description, kind,
//! size, urgency, parent. The merge uses a deterministic 4-step tie-breaking
//! chain that guarantees bit-identical convergence across all replicas.
//!
//! # Tie-Breaking Chain
//!
//! Given two `LwwRegister<T>` values `a` and `b`:
//!
//! 1. **ITC causal dominance**: If `a.stamp.leq(&b.stamp)` and they are
//!    not concurrent, the causally later one wins.
//! 2. **Wall-clock timestamp**: If concurrent, higher `wall_ts` wins.
//! 3. **Agent ID**: If wall clocks are equal, lexicographically greater
//!    `agent_id` wins.
//! 4. **Event hash**: If agent IDs are equal (same agent, concurrent writes),
//!    lexicographically greater `event_hash` wins. This step guarantees
//!    uniqueness — no ties are possible.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::clock::itc::Stamp;

// ---------------------------------------------------------------------------
// LwwRegister
// ---------------------------------------------------------------------------

/// A Last-Writer-Wins register holding a value of type `T`.
///
/// Each write records the value along with metadata used for deterministic
/// merge: an ITC stamp for causal ordering, a wall-clock timestamp, the
/// writing agent's ID, and the event hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LwwRegister<T> {
    /// The current value of the register.
    pub value: T,
    /// ITC stamp for causal ordering.
    pub stamp: Stamp,
    /// Wall-clock timestamp in microseconds since Unix epoch.
    pub wall_ts: u64,
    /// Agent identifier (e.g., "alice", "bot-1").
    pub agent_id: String,
    /// BLAKE3 hash of the event that wrote this value.
    pub event_hash: String,
}

impl<T> LwwRegister<T> {
    /// Create a new LWW register with the given value and metadata.
    pub fn new(
        value: T,
        stamp: Stamp,
        wall_ts: u64,
        agent_id: String,
        event_hash: String,
    ) -> Self {
        Self {
            value,
            stamp,
            wall_ts,
            agent_id,
            event_hash,
        }
    }
}

impl<T: Clone> LwwRegister<T> {
    /// Merge another register into this one, keeping the "winning" value.
    ///
    /// The 4-step tie-breaking chain:
    /// 1. ITC causal dominance (non-concurrent: later wins)
    /// 2. Wall-clock timestamp (concurrent: higher wins)
    /// 3. Agent ID (lexicographic: greater wins)
    /// 4. Event hash (lexicographic: greater wins — guaranteed unique)
    ///
    /// After merge, `self` contains the winning value.
    pub fn merge(&mut self, other: &Self) {
        if self.wins_over(other) {
            // Keep self
        } else {
            self.value = other.value.clone();
            self.stamp = other.stamp.clone();
            self.wall_ts = other.wall_ts;
            self.agent_id = other.agent_id.clone();
            self.event_hash = other.event_hash.clone();
        }
    }

    /// Returns `true` if `self` wins over `other` in the tie-breaking chain.
    fn wins_over(&self, other: &Self) -> bool {
        // Step 1: ITC causal dominance
        let self_leq_other = self.stamp.leq(&other.stamp);
        let other_leq_self = other.stamp.leq(&self.stamp);

        match (self_leq_other, other_leq_self) {
            (true, false) => {
                // other causally dominates self → other wins
                return false;
            }
            (false, true) => {
                // self causally dominates other → self wins
                return true;
            }
            (true, true) => {
                // They are equal (both leq each other) → self wins (idempotent)
                return true;
            }
            (false, false) => {
                // Concurrent — fall through to tie-breaking
            }
        }

        // Step 2: Wall-clock timestamp (higher wins)
        match self.wall_ts.cmp(&other.wall_ts) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => {}
        }

        // Step 3: Agent ID (lexicographically greater wins)
        match self.agent_id.cmp(&other.agent_id) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => {}
        }

        // Step 4: Event hash (lexicographically greater wins — guaranteed unique)
        self.event_hash >= other.event_hash
    }
}

impl<T: fmt::Display> fmt::Display for LwwRegister<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.value)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::itc::{Event, Id, Stamp};

    /// Helper: create a stamp with a specific event counter (seed identity).
    fn make_stamp(counter: u64) -> Stamp {
        let mut s = Stamp::seed();
        for _ in 0..counter {
            s.event();
        }
        s
    }

    /// Helper: create a stamp from a fork (anonymous identity, specific event).
    fn make_forked_stamps(counter_a: u64, counter_b: u64) -> (Stamp, Stamp) {
        let seed = Stamp::seed();
        let (mut a, mut b) = seed.fork();
        for _ in 0..counter_a {
            a.event();
        }
        for _ in 0..counter_b {
            b.event();
        }
        (a, b)
    }

    fn reg(value: &str, stamp: Stamp, wall_ts: u64, agent: &str, hash: &str) -> LwwRegister<String> {
        LwwRegister::new(
            value.to_string(),
            stamp,
            wall_ts,
            agent.to_string(),
            hash.to_string(),
        )
    }

    // === Step 1: ITC causal dominance ===

    #[test]
    fn causal_later_wins() {
        let s1 = make_stamp(1);
        let s2 = make_stamp(2);
        // s1 is causally before s2 (same lineage, s2 has more events)
        assert!(s1.leq(&s2));
        assert!(!s2.leq(&s1));

        let mut a = reg("old", s1, 100, "alice", "aaa");
        let b = reg("new", s2, 100, "alice", "aaa");
        a.merge(&b);
        assert_eq!(a.value, "new");
    }

    #[test]
    fn causal_earlier_loses() {
        let s1 = make_stamp(1);
        let s2 = make_stamp(2);

        let mut a = reg("new", s2, 100, "alice", "aaa");
        let b = reg("old", s1, 100, "alice", "aaa");
        a.merge(&b);
        assert_eq!(a.value, "new"); // a (later) wins
    }

    // === Step 2: Concurrent, wall_ts tie-break ===

    #[test]
    fn concurrent_higher_wall_ts_wins() {
        let (sa, sb) = make_forked_stamps(1, 1);
        // sa and sb are concurrent (forked, both have events)
        assert!(sa.concurrent(&sb));

        let mut a = reg("alice-val", sa, 200, "alice", "aaa");
        let b = reg("bob-val", sb, 300, "bob", "bbb");
        a.merge(&b);
        assert_eq!(a.value, "bob-val"); // higher wall_ts wins
    }

    #[test]
    fn concurrent_lower_wall_ts_loses() {
        let (sa, sb) = make_forked_stamps(1, 1);

        let mut a = reg("alice-val", sa, 300, "alice", "aaa");
        let b = reg("bob-val", sb, 200, "bob", "bbb");
        a.merge(&b);
        assert_eq!(a.value, "alice-val"); // a has higher wall_ts
    }

    // === Step 3: Concurrent, same wall_ts, agent_id tie-break ===

    #[test]
    fn concurrent_same_ts_higher_agent_wins() {
        let (sa, sb) = make_forked_stamps(1, 1);

        let mut a = reg("alice-val", sa, 100, "alice", "aaa");
        let b = reg("bob-val", sb, 100, "bob", "bbb");
        a.merge(&b);
        assert_eq!(a.value, "bob-val"); // "bob" > "alice" lexicographically
    }

    #[test]
    fn concurrent_same_ts_lower_agent_loses() {
        let (sa, sb) = make_forked_stamps(1, 1);

        let mut a = reg("bob-val", sa, 100, "bob", "bbb");
        let b = reg("alice-val", sb, 100, "alice", "aaa");
        a.merge(&b);
        assert_eq!(a.value, "bob-val"); // "bob" > "alice"
    }

    // === Step 4: Concurrent, same ts, same agent, event_hash tie-break ===

    #[test]
    fn concurrent_same_agent_higher_hash_wins() {
        let (sa, sb) = make_forked_stamps(1, 1);

        let mut a = reg("val-a", sa, 100, "alice", "hash-aaa");
        let b = reg("val-b", sb, 100, "alice", "hash-zzz");
        a.merge(&b);
        assert_eq!(a.value, "val-b"); // "hash-zzz" > "hash-aaa"
    }

    #[test]
    fn concurrent_same_agent_lower_hash_loses() {
        let (sa, sb) = make_forked_stamps(1, 1);

        let mut a = reg("val-a", sa, 100, "alice", "hash-zzz");
        let b = reg("val-b", sb, 100, "alice", "hash-aaa");
        a.merge(&b);
        assert_eq!(a.value, "val-a"); // "hash-zzz" > "hash-aaa"
    }

    // === Semilattice properties ===

    #[test]
    fn semilattice_commutative() {
        let (sa, sb) = make_forked_stamps(1, 1);

        let a = reg("val-a", sa.clone(), 100, "alice", "hash-a");
        let b = reg("val-b", sb.clone(), 200, "bob", "hash-b");

        let mut ab = a.clone();
        ab.merge(&b);

        let mut ba = b.clone();
        ba.merge(&a);

        assert_eq!(ab, ba);
    }

    #[test]
    fn semilattice_associative() {
        let seed = Stamp::seed();
        let (left, right) = seed.fork();
        let (mut sa, sb) = left.fork();
        let (mut sc, _) = right.fork();
        sa.event();
        // sb stays as is (concurrent with sa)
        sc.event();

        let a = reg("val-a", sa, 100, "alice", "hash-a");
        let b = reg("val-b", sb, 200, "bob", "hash-b");
        let c = reg("val-c", sc, 150, "carol", "hash-c");

        // (a merge b) merge c
        let mut left_merge = a.clone();
        left_merge.merge(&b);
        left_merge.merge(&c);

        // a merge (b merge c)
        let mut bc = b.clone();
        bc.merge(&c);
        let mut right_merge = a.clone();
        right_merge.merge(&bc);

        assert_eq!(left_merge, right_merge);
    }

    #[test]
    fn semilattice_idempotent_self_merge() {
        let s = make_stamp(3);
        let a = reg("value", s, 500, "agent", "hash-123");
        let mut m = a.clone();
        m.merge(&a);
        assert_eq!(m, a);
    }

    // === Edge cases ===

    #[test]
    fn equal_stamps_are_idempotent() {
        // Two registers with identical stamps (both leq each other)
        let s = make_stamp(2);
        let a = reg("same", s.clone(), 100, "agent", "hash");
        let mut m = a.clone();
        m.merge(&a);
        assert_eq!(m, a);
    }

    #[test]
    fn identical_timestamps_different_agents() {
        let (sa, sb) = make_forked_stamps(1, 1);

        let a = reg("alice-val", sa.clone(), 999, "alice", "hash-same");
        let b = reg("bob-val", sb.clone(), 999, "bob", "hash-same");

        let mut ab = a.clone();
        ab.merge(&b);
        assert_eq!(ab.value, "bob-val"); // "bob" > "alice"

        let mut ba = b.clone();
        ba.merge(&a);
        assert_eq!(ba.value, "bob-val");

        assert_eq!(ab, ba); // commutative
    }

    #[test]
    fn same_agent_concurrent_writes() {
        // Same agent can have concurrent writes if forked
        let (sa, sb) = make_forked_stamps(1, 1);

        let a = reg("write-1", sa, 100, "alice", "hash-111");
        let b = reg("write-2", sb, 100, "alice", "hash-222");

        let mut ab = a.clone();
        ab.merge(&b);

        let mut ba = b.clone();
        ba.merge(&a);

        assert_eq!(ab, ba); // commutative
        assert_eq!(ab.value, "write-2"); // "hash-222" > "hash-111"
    }

    #[test]
    fn display_shows_value() {
        let s = make_stamp(1);
        let r = reg("Hello, World!", s, 0, "agent", "hash");
        assert_eq!(r.to_string(), "Hello, World!");
    }

    #[test]
    fn serde_roundtrip() {
        let s = make_stamp(2);
        let r = reg("test-value", s, 42, "agent-1", "blake3:abc");
        let json = serde_json::to_string(&r).unwrap();
        let deserialized: LwwRegister<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(r, deserialized);
    }

    #[test]
    fn numeric_value_type() {
        let s = make_stamp(1);
        let mut a = LwwRegister::new(42u64, s.clone(), 100, "alice".to_string(), "h1".to_string());
        let s2 = make_stamp(2);
        let b = LwwRegister::new(99u64, s2, 200, "bob".to_string(), "h2".to_string());
        a.merge(&b);
        assert_eq!(a.value, 99);
    }

    #[test]
    fn merge_chain_converges() {
        // Multiple agents writing concurrently, all merge in different orders
        let seed = Stamp::seed();
        let (left, right) = seed.fork();
        let (mut s1, mut s2) = left.fork();
        let (mut s3, _) = right.fork();
        s1.event();
        s2.event();
        s3.event();

        let r1 = reg("v1", s1, 100, "alice", "h1");
        let r2 = reg("v2", s2, 200, "bob", "h2");
        let r3 = reg("v3", s3, 200, "carol", "h3");

        // Order 1: r1, r2, r3
        let mut m1 = r1.clone();
        m1.merge(&r2);
        m1.merge(&r3);

        // Order 2: r3, r1, r2
        let mut m2 = r3.clone();
        m2.merge(&r1);
        m2.merge(&r2);

        // Order 3: r2, r3, r1
        let mut m3 = r2.clone();
        m3.merge(&r3);
        m3.merge(&r1);

        assert_eq!(m1, m2);
        assert_eq!(m2, m3);
    }
}
