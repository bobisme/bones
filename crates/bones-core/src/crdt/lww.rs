//! Last-Writer-Wins (LWW) Register CRDT.
//!
//! LWW Register is the CRDT for scalar fields: title, description, kind,
//! size, urgency, parent. The merge keeps the greater of two writes under a
//! strict total order, which guarantees bit-identical convergence across all
//! replicas regardless of merge order.
//!
//! # Ordering
//!
//! Given two `LwwRegister<T>` values `a` and `b`, compare lexicographically:
//!
//! 1. **Wall-clock timestamp**: higher `wall_ts` wins.
//! 2. **Agent ID**: if wall clocks are equal, lexicographically greater
//!    `agent_id` wins.
//! 3. **Event hash**: if agent IDs are equal, lexicographically greater
//!    `event_hash` wins. Event hashes are unique per event, so no two
//!    distinct writes tie.
//!
//! This is the same `(wall_ts, agent, event_hash)` order that the event
//! merge driver and replay use to sort events.
//!
//! Registers carry no causal stamp. The ITC stamp was removed in bn-1dy8:
//! causality is a partial order, and putting it ahead of the wall clock made
//! the chain non-transitive under clock skew, so merge depended on merge
//! order. Instead, the local clock's receive rule
//! (`ShardManager::observe_timestamp`, bn-52i6) gives a causally later write
//! a later wall clock when the skew is under the cap.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::crdt::trace::{MergeTrace, TieBreakStep, merge_tracing_enabled};
use tracing::debug;

// ---------------------------------------------------------------------------
// LwwRegister
// ---------------------------------------------------------------------------

/// A Last-Writer-Wins register holding a value of type `T`.
///
/// Each write records the value along with metadata used for deterministic
/// merge: a wall-clock timestamp, the writing agent's ID, and the event
/// hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LwwRegister<T> {
    /// The current value of the register.
    pub value: T,
    /// Wall-clock timestamp in microseconds since Unix epoch.
    pub wall_ts: u64,
    /// Agent identifier (e.g., "alice", "bot-1").
    pub agent_id: String,
    /// BLAKE3 hash of the event that wrote this value.
    pub event_hash: String,
}

impl<T> LwwRegister<T> {
    /// Create a new LWW register with the given value and metadata.
    pub const fn new(value: T, wall_ts: u64, agent_id: String, event_hash: String) -> Self {
        Self {
            value,
            wall_ts,
            agent_id,
            event_hash,
        }
    }
}

impl<T: Clone> LwwRegister<T> {
    /// Merge another register into this one, keeping the "winning" value.
    ///
    /// The winner is the greater write under `(wall_ts, agent_id,
    /// event_hash)`; see the module docs for why no causal stamp is used.
    ///
    /// After merge, `self` contains the winning value.
    pub fn merge(&mut self, other: &Self) {
        if self.wins_over(other) {
            // Keep self
        } else {
            self.value = other.value.clone();
            self.wall_ts = other.wall_ts;
            self.agent_id.clone_from(&other.agent_id);
            self.event_hash.clone_from(&other.event_hash);
        }
    }

    /// Merge and optionally emit structured decision trace.
    ///
    /// If tracing is disabled via environment toggles, returns a no-op trace
    /// payload and preserves the normal low-overhead merge path.
    pub fn merge_with_trace(&mut self, other: &Self, field: &str) -> MergeTrace
    where
        T: fmt::Display,
    {
        let (self_wins, step) = self.compare(other);

        let trace = if merge_tracing_enabled() {
            let winner = if self_wins {
                self.value.to_string()
            } else {
                other.value.to_string()
            };

            let trace = MergeTrace {
                field: field.to_string(),
                values: (self.value.to_string(), other.value.to_string()),
                winner,
                step,
                correlation_id: format!("{}..{}", self.event_hash, other.event_hash),
                enabled: true,
            };

            debug!(
                target: "bones_core::crdt::merge_trace",
                field = trace.field,
                winner = trace.winner,
                step = ?trace.step,
                correlation_id = trace.correlation_id,
                "LWW merge decision"
            );

            trace
        } else {
            MergeTrace::disabled()
        };

        if !self_wins {
            self.value = other.value.clone();
            self.wall_ts = other.wall_ts;
            self.agent_id.clone_from(&other.agent_id);
            self.event_hash.clone_from(&other.event_hash);
        }

        trace
    }

    /// Returns `true` if `self` wins over `other` in the tie-breaking chain.
    fn wins_over(&self, other: &Self) -> bool {
        self.compare(other).0
    }

    fn compare(&self, other: &Self) -> (bool, TieBreakStep) {
        // Step 1: Wall-clock timestamp (higher wins)
        match self.wall_ts.cmp(&other.wall_ts) {
            std::cmp::Ordering::Greater => return (true, TieBreakStep::WallTimestamp),
            std::cmp::Ordering::Less => return (false, TieBreakStep::WallTimestamp),
            std::cmp::Ordering::Equal => {}
        }

        // Step 2: Agent ID (lexicographically greater wins)
        match self.agent_id.cmp(&other.agent_id) {
            std::cmp::Ordering::Greater => return (true, TieBreakStep::AgentId),
            std::cmp::Ordering::Less => return (false, TieBreakStep::AgentId),
            std::cmp::Ordering::Equal => {}
        }

        // Step 3: Event hash (lexicographically greater wins — unique per event)
        (self.event_hash >= other.event_hash, TieBreakStep::EventHash)
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

    fn reg(value: &str, wall_ts: u64, agent: &str, hash: &str) -> LwwRegister<String> {
        LwwRegister::new(
            value.to_string(),
            wall_ts,
            agent.to_string(),
            hash.to_string(),
        )
    }

    /// Regression: when causality ranked ahead of the wall clock, these three
    /// writes formed a cycle (a beats b, b beats c, c beats a) and each merge
    /// order produced a different winner. Now every order converges on the
    /// highest wall clock.
    #[test]
    fn causal_skew_cycle_converges_in_every_order() {
        let a = reg("a", 3, "agent-a", "blake3:a");
        let b = reg("b", 2, "agent-b", "blake3:b");
        let c = reg("c", 1, "agent-c", "blake3:c");

        let orders = [
            [&a, &b, &c],
            [&a, &c, &b],
            [&b, &a, &c],
            [&b, &c, &a],
            [&c, &a, &b],
            [&c, &b, &a],
        ];
        for order in orders {
            let mut merged = order[0].clone();
            merged.merge(order[1]);
            merged.merge(order[2]);
            assert_eq!(merged.value, "a", "order {order:?}");
        }
    }

    // === Step 1: wall_ts ===

    #[test]
    fn concurrent_higher_wall_ts_wins() {
        let mut a = reg("alice-val", 200, "alice", "aaa");
        let b = reg("bob-val", 300, "bob", "bbb");
        a.merge(&b);
        assert_eq!(a.value, "bob-val"); // higher wall_ts wins
    }

    #[test]
    fn concurrent_lower_wall_ts_loses() {
        let mut a = reg("alice-val", 300, "alice", "aaa");
        let b = reg("bob-val", 200, "bob", "bbb");
        a.merge(&b);
        assert_eq!(a.value, "alice-val"); // a has higher wall_ts
    }

    // === Step 2: same wall_ts, agent_id tie-break ===

    #[test]
    fn concurrent_same_ts_higher_agent_wins() {
        let mut a = reg("alice-val", 100, "alice", "aaa");
        let b = reg("bob-val", 100, "bob", "bbb");
        a.merge(&b);
        assert_eq!(a.value, "bob-val"); // "bob" > "alice" lexicographically
    }

    #[test]
    fn concurrent_same_ts_lower_agent_loses() {
        let mut a = reg("bob-val", 100, "bob", "bbb");
        let b = reg("alice-val", 100, "alice", "aaa");
        a.merge(&b);
        assert_eq!(a.value, "bob-val"); // "bob" > "alice"
    }

    // === Step 3: same ts, same agent, event_hash tie-break ===

    #[test]
    fn concurrent_same_agent_higher_hash_wins() {
        let mut a = reg("val-a", 100, "alice", "hash-aaa");
        let b = reg("val-b", 100, "alice", "hash-zzz");
        a.merge(&b);
        assert_eq!(a.value, "val-b"); // "hash-zzz" > "hash-aaa"
    }

    #[test]
    fn concurrent_same_agent_lower_hash_loses() {
        let mut a = reg("val-a", 100, "alice", "hash-zzz");
        let b = reg("val-b", 100, "alice", "hash-aaa");
        a.merge(&b);
        assert_eq!(a.value, "val-a"); // "hash-zzz" > "hash-aaa"
    }

    // === Semilattice properties ===

    #[test]
    fn semilattice_commutative() {
        let a = reg("val-a", 100, "alice", "hash-a");
        let b = reg("val-b", 200, "bob", "hash-b");

        let mut ab = a.clone();
        ab.merge(&b);

        let mut ba = b.clone();
        ba.merge(&a);

        assert_eq!(ab, ba);
    }

    #[test]
    fn semilattice_associative() {
        let a = reg("val-a", 100, "alice", "hash-a");
        let b = reg("val-b", 200, "bob", "hash-b");
        let c = reg("val-c", 150, "carol", "hash-c");

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
        let a = reg("value", 500, "agent", "hash-123");
        let mut m = a.clone();
        m.merge(&a);
        assert_eq!(m, a);
    }

    // === Edge cases ===

    #[test]
    fn identical_timestamps_different_agents() {
        let a = reg("alice-val", 999, "alice", "hash-same");
        let b = reg("bob-val", 999, "bob", "hash-same");

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
        let a = reg("write-1", 100, "alice", "hash-111");
        let b = reg("write-2", 100, "alice", "hash-222");

        let mut ab = a.clone();
        ab.merge(&b);

        let mut ba = b.clone();
        ba.merge(&a);

        assert_eq!(ab, ba); // commutative
        assert_eq!(ab.value, "write-2"); // "hash-222" > "hash-111"
    }

    #[test]
    fn display_shows_value() {
        let r = reg("Hello, World!", 0, "agent", "hash");
        assert_eq!(r.to_string(), "Hello, World!");
    }

    #[test]
    fn serde_roundtrip() {
        let r = reg("test-value", 42, "agent-1", "blake3:abc");
        let json = serde_json::to_string(&r).unwrap();
        let deserialized: LwwRegister<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(r, deserialized);
    }

    #[test]
    fn numeric_value_type() {
        let mut a = LwwRegister::new(42u64, 100, "alice".to_string(), "h1".to_string());
        let b = LwwRegister::new(99u64, 200, "bob".to_string(), "h2".to_string());
        a.merge(&b);
        assert_eq!(a.value, 99);
    }

    #[test]
    fn merge_with_trace_disabled_by_default_has_no_payload() {
        let mut a = reg("old", 100, "alice", "aaa");
        let b = reg("new", 100, "alice", "bbb");

        let trace = a.merge_with_trace(&b, "title");
        assert_eq!(a.value, "new");
        assert!(!trace.enabled);
        assert_eq!(trace.step, TieBreakStep::Equal);
        assert!(trace.field.is_empty());
    }

    #[test]
    fn merge_with_trace_reports_decisive_step_when_enabled() {
        if !merge_tracing_enabled() {
            return;
        }

        let mut a = reg("alice-val", 100, "alice", "aaa");
        let b = reg("bob-val", 200, "bob", "bbb");

        let trace = a.merge_with_trace(&b, "title");
        assert!(trace.enabled);
        assert_eq!(trace.field, "title");
        assert_eq!(trace.winner, "bob-val");
        assert_eq!(trace.step, TieBreakStep::WallTimestamp);
        assert!(!trace.correlation_id.is_empty());
    }

    #[test]
    fn merge_chain_converges() {
        // Multiple agents writing concurrently, all merge in different orders
        let r1 = reg("v1", 100, "alice", "h1");
        let r2 = reg("v2", 200, "bob", "h2");
        let r3 = reg("v3", 200, "carol", "h3");

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

/// Proofs that `wins_over` is a strict total order on distinct write keys.
///
/// Composition argument: `wins_over` compares the key
/// `(wall_ts, agent_id, event_hash)`. The harnesses prove it is a total
/// preorder (total and transitive) whose ties are exactly equal keys. So on
/// distinct keys it is a strict total order, and `merge(a, b)` keeps the
/// maximum. `max` under a total order is commutative, associative and
/// idempotent, so `merge` is a join semilattice with no induction over merge
/// sequences. An event hash is unique per event, so equal keys carry equal
/// values.
///
/// Bound: `wall_ts` is any `u64`. `agent_id` and `event_hash` are strings of
/// length 0 to 2 over `{a, b}`. `compare` uses only `Ord::cmp` on each field,
/// and this domain gives every Less/Equal/Greater outcome on each field,
/// including the proper-prefix case, so every path through the tie-break
/// chain is covered.
///
/// Registers carry no causal stamp since bn-1dy8, so the key above is the
/// whole of the merge order.
#[cfg(kani)]
mod kani_proofs {
    use super::LwwRegister;

    fn any_short_string() -> String {
        let len: u8 = kani::any_where(|&l| l <= 2);
        let mut s = String::new();
        for i in 0..2 {
            if i < len {
                s.push(if kani::any() { 'a' } else { 'b' });
            }
        }
        s
    }

    fn any_register() -> LwwRegister<u8> {
        LwwRegister::new(
            kani::any(),
            kani::any(),
            any_short_string(),
            any_short_string(),
        )
    }

    fn same_key(a: &LwwRegister<u8>, b: &LwwRegister<u8>) -> bool {
        a.wall_ts == b.wall_ts && a.agent_id == b.agent_id && a.event_hash == b.event_hash
    }

    /// Totality and antisymmetry: for any two registers at least one wins,
    /// and both win only when their keys are equal.
    #[kani::proof]
    #[kani::unwind(4)]
    fn wins_over_is_total_and_antisymmetric() {
        let a = any_register();
        let b = any_register();
        assert!(a.wins_over(&b) || b.wins_over(&a));
        if a.wins_over(&b) && b.wins_over(&a) {
            assert!(same_key(&a, &b));
        }
        kani::cover!(a.wall_ts == b.wall_ts && a.agent_id == b.agent_id && a.wins_over(&b));
    }

    /// Transitivity: `a` wins over `b` and `b` wins over `c` implies `a` wins
    /// over `c`.
    #[kani::proof]
    #[kani::unwind(4)]
    fn wins_over_is_transitive() {
        let a = any_register();
        let b = any_register();
        let c = any_register();
        if a.wins_over(&b) && b.wins_over(&c) {
            assert!(a.wins_over(&c));
        }
        kani::cover!(
            a.wins_over(&b)
                && b.wins_over(&c)
                && a.wall_ts == c.wall_ts
                && a.agent_id == c.agent_id
                && !same_key(&a, &c)
        );
    }

    /// `merge` keeps the winner, and merging a register with itself is a
    /// no-op.
    #[kani::proof]
    #[kani::unwind(4)]
    fn merge_keeps_the_winner() {
        let a = any_register();
        let b = any_register();
        let mut self_merged = a.clone();
        self_merged.merge(&a);
        assert!(self_merged == a);
        let mut merged = a.clone();
        merged.merge(&b);
        let winner = if a.wins_over(&b) { &a } else { &b };
        assert!(merged == *winner);
    }
}
