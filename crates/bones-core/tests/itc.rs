//! Integration tests for Interval Tree Clocks (ITC).
//!
//! Covers: fork/join roundtrip, causal ordering, monotonicity,
//! transitivity, concurrency detection, multi-agent scenarios,
//! serialization roundtrip, edge cases, and size regressions.

use bones_core::clock::itc::{Event, Id, Stamp};
use proptest::prelude::*;

// ===========================================================================
// Fork / Join Roundtrip
// ===========================================================================

#[test]
fn fork_then_join_produces_equivalent_stamp() {
    let original = Stamp::seed();
    let (left, right) = original.fork();
    let joined = Stamp::join(&left, &right);
    // Joined ID should be the full interval
    assert_eq!(joined.id, Id::one());
    // Event tree should be equivalent to the original (both zero)
    assert_eq!(joined.event, original.event);
}

#[test]
fn fork_then_join_after_events_recovers_combined_history() {
    let seed = Stamp::seed();
    let (mut a, mut b) = seed.fork();

    // Each fork records independent events
    a.event();
    b.event();
    b.event();

    let joined = Stamp::join(&a, &b);

    // Joined must dominate both forks
    assert!(a.leq(&joined), "fork A should be <= joined");
    assert!(b.leq(&joined), "fork B should be <= joined");
    // Joined ID recovers full interval
    assert_eq!(joined.id, Id::one());
}

#[test]
fn fork_produces_equal_event_histories_initially() {
    let seed = Stamp::seed();
    let (left, right) = seed.fork();
    // Immediately after fork (before any events), both halves share the
    // same event tree (zero), so leq holds both ways — they are causally
    // equivalent at the point of fork, even though their ID intervals differ.
    assert!(
        left.leq(&right),
        "left fork should be <= right fork immediately after fork (same event tree)"
    );
    assert!(
        right.leq(&left),
        "right fork should be <= left fork immediately after fork (same event tree)"
    );
    // IDs are distinct — each half owns a different partition
    assert_ne!(left.id, right.id, "forked stamps must have distinct IDs");
    assert!(!left.id.is_zero(), "left fork must own some interval");
    assert!(!right.id.is_zero(), "right fork must own some interval");
}

#[test]
fn fork_becomes_non_comparable_after_independent_events() {
    let seed = Stamp::seed();
    let (mut left, mut right) = seed.fork();
    // Once both halves record independent events, they become concurrent
    left.event();
    right.event();
    assert!(
        !left.leq(&right),
        "after independent events, left should not be <= right"
    );
    assert!(
        !right.leq(&left),
        "after independent events, right should not be <= left"
    );
}

#[test]
fn double_fork_join_roundtrip() {
    let seed = Stamp::seed();
    let (mut a, b_seed) = seed.fork();
    let (mut b, mut c) = b_seed.fork();

    a.event();
    b.event();
    b.event();
    c.event();

    // Join b and c first, then with a
    let bc = Stamp::join(&b, &c);
    let all = Stamp::join(&a, &bc);

    assert!(a.leq(&all));
    assert!(b.leq(&all));
    assert!(c.leq(&all));
    assert_eq!(all.id, Id::one());
}

// ===========================================================================
// Monotonicity
// ===========================================================================

#[test]
fn event_monotonically_increases_single_stamp() {
    let mut stamp = Stamp::seed();
    let mut history = vec![stamp.event.max_value()];

    for _ in 0..10 {
        stamp.event();
        history.push(stamp.event.max_value());
    }

    // Each step must be strictly greater than the previous
    for window in history.windows(2) {
        assert!(
            window[1] > window[0],
            "event must strictly increase: {} -> {}",
            window[0],
            window[1]
        );
    }
}

#[test]
fn chain_of_10_events_maintains_causal_leq() {
    let mut stamp = Stamp::seed();
    let mut history = vec![stamp.clone()];

    for _ in 0..10 {
        stamp.event();
        history.push(stamp.clone());
    }

    // Every earlier stamp must be leq every later stamp
    for i in 0..history.len() {
        for j in i..history.len() {
            assert!(
                history[i].leq(&history[j]),
                "stamp[{i}] should be <= stamp[{j}]"
            );
        }
    }
}

#[test]
fn event_on_half_interval_still_monotonic() {
    let seed = Stamp::seed();
    let (mut a, _b) = seed.fork();
    let before = a.event.max_value();
    a.event();
    assert!(
        a.event.max_value() > before,
        "event on half-interval must increase"
    );
}

// ===========================================================================
// Transitivity
// ===========================================================================

#[test]
fn leq_transitivity_chain() {
    let mut s = Stamp::seed();
    let s0 = s.clone();
    s.event();
    let s1 = s.clone();
    s.event();
    let s2 = s.clone();
    s.event();
    let s3 = s.clone();

    assert!(s0.leq(&s1), "s0 <= s1");
    assert!(s1.leq(&s2), "s1 <= s2");
    assert!(s2.leq(&s3), "s2 <= s3");
    // Transitivity
    assert!(s0.leq(&s2), "s0 <= s2 (transitive)");
    assert!(s0.leq(&s3), "s0 <= s3 (transitive)");
    assert!(s1.leq(&s3), "s1 <= s3 (transitive)");
}

#[test]
fn leq_reflexive() {
    let mut s = Stamp::seed();
    s.event();
    s.event();
    assert!(s.leq(&s), "stamp must be leq itself");
}

#[test]
fn leq_antisymmetric_after_events() {
    let mut s = Stamp::seed();
    let before = s.clone();
    s.event();
    // before < s (not equal)
    assert!(before.leq(&s), "before <= after");
    assert!(!s.leq(&before), "after should NOT be <= before");
}

// ===========================================================================
// Concurrency Detection
// ===========================================================================

#[test]
fn independently_forked_stamps_are_concurrent() {
    let seed = Stamp::seed();
    let (mut a, mut b) = seed.fork();
    a.event();
    b.event();
    // Neither should be leq the other
    assert!(!a.leq(&b), "a should not be <= b");
    assert!(!b.leq(&a), "b should not be <= a");
}

#[test]
fn concurrent_stamps_detected_correctly() {
    let seed = Stamp::seed();
    let (mut a, mut b) = seed.fork();
    a.event();
    b.event();
    assert!(a.concurrent(&b), "a and b should be concurrent");
    assert!(b.concurrent(&a), "concurrent is symmetric");
}

#[test]
fn non_concurrent_when_one_dominates() {
    let seed = Stamp::seed();
    let (mut a, b) = seed.fork();
    a.event();
    let joined = Stamp::join(&a, &b);
    // joined dominates a; they are not concurrent
    assert!(
        !a.concurrent(&joined),
        "dominated stamp is not concurrent with dominator"
    );
    assert!(
        !joined.concurrent(&a),
        "dominator is not concurrent with dominated"
    );
}

#[test]
fn seed_stamps_not_concurrent_with_themselves() {
    let s = Stamp::seed();
    assert!(
        !s.concurrent(&s),
        "stamp should not be concurrent with itself"
    );
}

// ===========================================================================
// Multi-Agent Scenarios
// ===========================================================================

#[test]
fn two_agent_fork_work_retire() {
    let seed = Stamp::seed();
    let (mut a, mut b) = seed.fork();

    a.event();
    a.event();
    b.event();

    assert!(
        a.concurrent(&b),
        "after independent work, agents must be concurrent"
    );

    let merged = Stamp::join(&a, &b);
    assert!(a.leq(&merged), "A <= merged");
    assert!(b.leq(&merged), "B <= merged");
    assert_eq!(merged.id, Id::one(), "merged ID must be full interval");
}

#[test]
fn four_agent_scenario() {
    let seed = Stamp::seed();
    let (ab, cd) = seed.fork();
    let (mut a, mut b) = ab.fork();
    let (mut c, mut d) = cd.fork();

    // Each agent does independent work
    a.event();
    b.event();
    b.event();
    c.event();
    c.event();
    c.event();
    d.event();

    // All pairs should be concurrent
    let agents = [&a, &b, &c, &d];
    for i in 0..agents.len() {
        for j in (i + 1)..agents.len() {
            assert!(
                agents[i].concurrent(agents[j]),
                "agents {i} and {j} should be concurrent"
            );
        }
    }

    // Merge all and verify dominance
    let ab_joined = Stamp::join(&a, &b);
    let cd_joined = Stamp::join(&c, &d);
    let all = Stamp::join(&ab_joined, &cd_joined);

    for (i, agent) in agents.iter().enumerate() {
        assert!(agent.leq(&all), "agent {i} should be <= merged");
    }
    assert_eq!(all.id, Id::one());
}

#[test]
fn eight_agent_scenario() {
    let seed = Stamp::seed();

    let (h1, h2) = seed.fork();
    let (q1, q2) = h1.fork();
    let (q3, q4) = h2.fork();
    let (mut a1, mut a2) = q1.fork();
    let (mut a3, mut a4) = q2.fork();
    let (mut a5, mut a6) = q3.fork();
    let (mut a7, mut a8) = q4.fork();

    // Each agent does (index+1) events
    for (i, agent) in [
        &mut a1, &mut a2, &mut a3, &mut a4, &mut a5, &mut a6, &mut a7, &mut a8,
    ]
    .iter_mut()
    .enumerate()
    {
        for _ in 0..=(i as u32) {
            agent.event();
        }
    }

    let snapshots = [&a1, &a2, &a3, &a4, &a5, &a6, &a7, &a8];

    // All pairs concurrent
    for i in 0..8 {
        for j in (i + 1)..8 {
            assert!(
                snapshots[i].concurrent(snapshots[j]),
                "agents {i} and {j} should be concurrent"
            );
        }
    }

    // Merge all agents into one
    let mut merged = snapshots[0].clone();
    for s in &snapshots[1..] {
        merged = Stamp::join(&merged, s);
    }

    for (i, s) in snapshots.iter().enumerate() {
        assert!(s.leq(&merged), "agent {i} should be <= merged");
    }
    assert_eq!(merged.id, Id::one());
}

fn fork_n(stamp: Stamp, depth: u32) -> Vec<Stamp> {
    if depth == 0 {
        return vec![stamp];
    }
    let (l, r) = stamp.fork();
    let mut result = fork_n(l, depth - 1);
    result.extend(fork_n(r, depth - 1));
    result
}

#[test]
fn sixteen_agent_scenario() {
    let seed = Stamp::seed();
    let mut agents = fork_n(seed, 4); // 2^4 = 16 agents
    assert_eq!(agents.len(), 16);

    // Each agent does (i % 5) + 1 events
    for (i, agent) in agents.iter_mut().enumerate() {
        for _ in 0..((i % 5) + 1) {
            agent.event();
        }
    }

    // All pairs should be concurrent
    for i in 0..16 {
        for j in (i + 1)..16 {
            assert!(
                agents[i].concurrent(&agents[j]),
                "agents {i} and {j} should be concurrent"
            );
        }
    }

    // Merge all back and verify dominance
    let mut merged = agents[0].clone();
    for a in &agents[1..] {
        merged = Stamp::join(&merged, a);
    }

    for (i, a) in agents.iter().enumerate() {
        assert!(a.leq(&merged), "agent {i} should be <= merged");
    }
    assert_eq!(merged.id, Id::one());
}

// ===========================================================================
// Serialization Roundtrip (compact binary codec)
// ===========================================================================

#[test]
fn serialization_roundtrip_seed_stamp() {
    let stamp = Stamp::seed();
    let bytes = stamp.serialize_compact();
    let deserialized = Stamp::deserialize_compact(&bytes).expect("deserialize seed");
    assert_eq!(stamp, deserialized);
}

#[test]
fn serialization_roundtrip_after_events() {
    let mut stamp = Stamp::seed();
    stamp.event();
    stamp.event();
    let bytes = stamp.serialize_compact();
    let deserialized = Stamp::deserialize_compact(&bytes).expect("deserialize after events");
    assert_eq!(stamp, deserialized);
}

#[test]
fn serialization_roundtrip_forked_stamp() {
    let seed = Stamp::seed();
    let (mut a, _b) = seed.fork();
    a.event();
    a.event();
    let bytes = a.serialize_compact();
    let deserialized = Stamp::deserialize_compact(&bytes).expect("deserialize forked stamp");
    assert_eq!(a, deserialized);
}

#[test]
fn serialization_roundtrip_deep_fork_tree() {
    // Fork down 4 levels to get a deep ID tree
    let seed = Stamp::seed();
    let agents = fork_n(seed, 4);
    let mut agent = agents[7].clone();
    agent.event();
    agent.event();
    agent.event();

    let bytes = agent.serialize_compact();
    let deserialized = Stamp::deserialize_compact(&bytes).expect("deserialize deep fork");
    assert_eq!(agent, deserialized);
}

#[test]
fn serialization_roundtrip_merged_stamp() {
    let seed = Stamp::seed();
    let (mut a, mut b) = seed.fork();
    a.event();
    a.event();
    b.event();
    let merged = Stamp::join(&a, &b);
    let bytes = merged.serialize_compact();
    let deserialized = Stamp::deserialize_compact(&bytes).expect("deserialize merged");
    assert_eq!(merged, deserialized);
}

#[test]
fn serialization_roundtrip_anonymous_stamp() {
    let anon = Stamp::anonymous();
    let bytes = anon.serialize_compact();
    let deserialized = Stamp::deserialize_compact(&bytes).expect("deserialize anonymous");
    assert_eq!(anon, deserialized);
}

// ===========================================================================
// Edge Cases
// ===========================================================================

#[test]
fn single_agent_full_interval() {
    let mut s = Stamp::seed();
    // The seed owns the full interval — Id::one()
    assert_eq!(s.id, Id::one());
    assert!(!s.is_anonymous());

    // Events should increment cleanly on a full-interval stamp
    for _ in 0..5 {
        s.event();
    }
    assert!(s.event.max_value() >= 5);
}

#[test]
fn maximum_fork_depth_then_rejoin() {
    // Fork 8 levels deep (256 agents), rejoin, verify coverage
    let seed = Stamp::seed();
    let agents = fork_n(seed, 8); // 2^8 = 256 agents
    assert_eq!(agents.len(), 256);

    // Merge all back
    let mut merged = agents[0].clone();
    for a in &agents[1..] {
        merged = Stamp::join(&merged, a);
    }

    assert_eq!(
        merged.id,
        Id::one(),
        "after rejoining 256 agents, ID must be full interval"
    );
    assert_eq!(
        merged.event,
        Event::zero(),
        "no events were recorded, so event tree must be zero"
    );
}

#[test]
fn join_of_already_joined_stamp_is_idempotent() {
    let seed = Stamp::seed();
    let (mut a, mut b) = seed.fork();
    a.event();
    b.event();

    let joined_once = Stamp::join(&a, &b);
    let joined_twice = Stamp::join(&joined_once, &joined_once);
    assert_eq!(
        joined_once, joined_twice,
        "joining a stamp with itself should be idempotent"
    );
}

#[test]
fn join_anonymous_is_identity() {
    let seed = Stamp::seed();
    let anon = Stamp::anonymous();
    let joined = Stamp::join(&seed, &anon);
    assert_eq!(joined.id, Id::one());
    assert_eq!(joined.event, Event::zero());
}

#[test]
fn join_commutativity() {
    let seed = Stamp::seed();
    let (mut a, mut b) = seed.fork();
    a.event();
    a.event();
    b.event();

    let ab = Stamp::join(&a, &b);
    let ba = Stamp::join(&b, &a);
    assert_eq!(ab, ba, "join must be commutative");
}

#[test]
fn join_associativity() {
    let seed = Stamp::seed();
    let (abc, mut d) = seed.fork();
    let (ab, mut c) = abc.fork();
    let (mut a, mut b) = ab.fork();

    a.event();
    b.event();
    b.event();
    c.event();
    c.event();
    c.event();
    d.event();

    // (a ∪ b) ∪ (c ∪ d) == a ∪ (b ∪ (c ∪ d))
    let left = Stamp::join(&Stamp::join(&a, &b), &Stamp::join(&c, &d));
    let right = Stamp::join(&a, &Stamp::join(&b, &Stamp::join(&c, &d)));
    assert_eq!(left.event, right.event, "join must be associative");
}

// ===========================================================================
// Size Regression: serialized stamp <= 50 bytes for <= 8 active agents
// ===========================================================================

#[test]
fn compact_size_single_agent_seed() {
    let stamp = Stamp::seed();
    let bytes = stamp.serialize_compact();
    assert!(
        bytes.len() <= 20,
        "single-agent seed stamp too large: {} bytes (limit 20)",
        bytes.len()
    );
}

#[test]
fn compact_size_single_agent_with_events() {
    let mut stamp = Stamp::seed();
    for _ in 0..10 {
        stamp.event();
    }
    let bytes = stamp.serialize_compact();
    assert!(
        bytes.len() <= 30,
        "single-agent stamp with 10 events too large: {} bytes (limit 30)",
        bytes.len()
    );
}

#[test]
fn compact_size_eight_agents_under_budget() {
    let seed = Stamp::seed();
    let mut agents = fork_n(seed, 3); // 2^3 = 8 agents
    assert_eq!(agents.len(), 8);

    for (i, agent) in agents.iter_mut().enumerate() {
        for _ in 0..=(i as u32) {
            agent.event();
        }
    }

    // Size budget: <= 50 bytes for any active agent stamp
    for (i, agent) in agents.iter().enumerate() {
        let bytes = agent.serialize_compact();
        assert!(
            bytes.len() <= 50,
            "agent {i} compact stamp too large: {} bytes (limit 50)",
            bytes.len()
        );
    }
}

#[test]
fn compact_size_merged_eight_agents_under_budget() {
    let seed = Stamp::seed();
    let mut agents = fork_n(seed, 3); // 8 agents

    for (i, agent) in agents.iter_mut().enumerate() {
        for _ in 0..=i {
            agent.event();
        }
    }

    let mut merged = agents[0].clone();
    for a in &agents[1..] {
        merged = Stamp::join(&merged, a);
    }

    let bytes = merged.serialize_compact();
    assert!(
        bytes.len() <= 50,
        "merged 8-agent stamp too large: {} bytes (limit 50)",
        bytes.len()
    );
}

// ===========================================================================
// Property Tests
// ===========================================================================

proptest! {
    #[test]
    fn prop_fork_join_roundtrip(n_events in 0u32..10) {
        let mut s = Stamp::seed();
        for _ in 0..n_events {
            s.event();
        }
        let original_event_max = s.event.max_value();
        let (a, b) = s.fork();
        let joined = Stamp::join(&a, &b);

        // ID must be fully recovered
        prop_assert_eq!(&joined.id, &Id::one());
        // Event must be at least as advanced
        prop_assert!(joined.event.max_value() >= original_event_max);
    }

    #[test]
    fn prop_event_strictly_monotonic(n in 1u32..20) {
        let mut s = Stamp::seed();
        let mut prev = s.event.max_value();
        for _ in 0..n {
            s.event();
            let cur = s.event.max_value();
            prop_assert!(cur > prev, "event must be strictly monotonic: {} -> {}", prev, cur);
            prev = cur;
        }
    }

    #[test]
    fn prop_leq_reflexive(n in 0u32..10) {
        let mut s = Stamp::seed();
        for _ in 0..n {
            s.event();
        }
        prop_assert!(s.leq(&s), "stamp must be leq itself");
    }

    #[test]
    fn prop_leq_transitivity(n1 in 0u32..4, n2 in 1u32..4, n3 in 1u32..4) {
        let mut s = Stamp::seed();
        for _ in 0..n1 { s.event(); }
        let s0 = s.clone();
        for _ in 0..n2 { s.event(); }
        let s1 = s.clone();
        for _ in 0..n3 { s.event(); }
        let s2 = s;

        prop_assert!(s0.leq(&s1));
        prop_assert!(s1.leq(&s2));
        prop_assert!(s0.leq(&s2), "transitivity: s0 <= s1 <= s2 => s0 <= s2");
    }

    #[test]
    fn prop_fork_produces_concurrent_stamps(n_events_a in 1u32..5, n_events_b in 1u32..5) {
        let seed = Stamp::seed();
        let (mut a, mut b) = seed.fork();
        for _ in 0..n_events_a { a.event(); }
        for _ in 0..n_events_b { b.event(); }
        prop_assert!(a.concurrent(&b), "independently forked/evented stamps must be concurrent");
    }

    #[test]
    fn prop_join_dominates_both(n_a in 1u32..5, n_b in 1u32..5) {
        let seed = Stamp::seed();
        let (mut a, mut b) = seed.fork();
        for _ in 0..n_a { a.event(); }
        for _ in 0..n_b { b.event(); }
        let joined = Stamp::join(&a, &b);
        prop_assert!(a.leq(&joined), "a <= join(a,b)");
        prop_assert!(b.leq(&joined), "b <= join(a,b)");
    }

    #[test]
    fn prop_serialization_roundtrip(n_events in 0u32..5, fork_depth in 0u32..3) {
        let seed = Stamp::seed();
        let mut agents = fork_n(seed, fork_depth);

        // Pick the first agent, do events, then roundtrip
        let agent = &mut agents[0];
        for _ in 0..n_events { agent.event(); }
        let bytes = agent.serialize_compact();
        let decoded = Stamp::deserialize_compact(&bytes);
        prop_assert_eq!(decoded, Ok(agent.clone()));
    }

    #[test]
    fn prop_concurrent_symmetric(n_a in 1u32..5, n_b in 1u32..5) {
        let seed = Stamp::seed();
        let (mut a, mut b) = seed.fork();
        for _ in 0..n_a { a.event(); }
        for _ in 0..n_b { b.event(); }
        prop_assert_eq!(a.concurrent(&b), b.concurrent(&a), "concurrent must be symmetric");
    }
}
