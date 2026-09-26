//! Verus-verified merge kernels for bones CRDTs (bn-226p).
//!
//! Each function here is the decision `bones-core` makes when it merges two
//! CRDT values. Verus proves, for all inputs and with no size bound:
//!
//! - `bytes_cmp` computes byte-lexicographic order, the order of Rust's
//!   `Ord for str`, and that order is a strict total order.
//! - `lww_compare` computes the LWW key order `(wall_ts, agent, event_hash)`,
//!   a strict total order, so "keep the winner" is commutative, associative
//!   and idempotent: a join semilattice.
//! - `epoch_phase_join` is commutative, associative, idempotent and the least
//!   upper bound of its inputs.
//! - `min_nonzero` and `max` (created/updated timestamps) are semilattice
//!   joins.
//!
//! Run the proofs with `just verus`. A plain `cargo build` erases the specs
//! and proofs and compiles the executable code only.
use vstd::prelude::*;

verus! {

// ===========================================================================
// Byte-lexicographic order
// ===========================================================================

/// `a` sorts strictly before `b` in byte-lexicographic order: the order of
/// `Ord for [u8]` and `Ord for str`.
pub open spec fn bytes_lt(a: Seq<u8>, b: Seq<u8>) -> bool
    decreases a.len(),
{
    if a.len() == 0 {
        b.len() > 0
    } else if b.len() == 0 {
        false
    } else if a[0] != b[0] {
        a[0] < b[0]
    } else {
        bytes_lt(a.drop_first(), b.drop_first())
    }
}

pub proof fn lemma_bytes_lt_irreflexive(a: Seq<u8>)
    ensures
        !bytes_lt(a, a),
    decreases a.len(),
{
    if a.len() > 0 {
        lemma_bytes_lt_irreflexive(a.drop_first());
    }
}

pub proof fn lemma_bytes_lt_transitive(a: Seq<u8>, b: Seq<u8>, c: Seq<u8>)
    requires
        bytes_lt(a, b),
        bytes_lt(b, c),
    ensures
        bytes_lt(a, c),
    decreases a.len(),
{
    if a.len() > 0 && b.len() > 0 && c.len() > 0 && a[0] == b[0] && b[0] == c[0] {
        lemma_bytes_lt_transitive(a.drop_first(), b.drop_first(), c.drop_first());
    }
}

/// Totality: two byte strings are equal or one sorts before the other.
pub proof fn lemma_bytes_lt_total(a: Seq<u8>, b: Seq<u8>)
    ensures
        a == b || bytes_lt(a, b) || bytes_lt(b, a),
    decreases a.len(),
{
    if a.len() > 0 && b.len() > 0 && a[0] == b[0] {
        lemma_bytes_lt_total(a.drop_first(), b.drop_first());
        if a.drop_first() == b.drop_first() {
            assert(a =~= b) by {
                assert forall|i: int| 0 <= i < a.len() implies a[i] == b[i] by {
                    if i > 0 {
                        assert(a[i] == a.drop_first()[i - 1]);
                        assert(b[i] == b.drop_first()[i - 1]);
                    }
                }
            }
        }
    } else if a.len() == 0 && b.len() == 0 {
        assert(a =~= b);
    }
}

pub proof fn lemma_bytes_lt_asymmetric(a: Seq<u8>, b: Seq<u8>)
    requires
        bytes_lt(a, b),
    ensures
        !bytes_lt(b, a),
{
    if bytes_lt(b, a) {
        lemma_bytes_lt_transitive(a, b, a);
        lemma_bytes_lt_irreflexive(a);
    }
}

/// Comparing from index `i` on decides the whole comparison when the first
/// `i` bytes are equal.
proof fn lemma_bytes_lt_skip(a: Seq<u8>, b: Seq<u8>, i: int)
    requires
        0 <= i <= a.len(),
        i <= b.len(),
        a.subrange(0, i) == b.subrange(0, i),
    ensures
        bytes_lt(a, b) == bytes_lt(a.skip(i), b.skip(i)),
    decreases i,
{
    if i > 0 {
        assert(a[0] == a.subrange(0, i)[0]);
        assert(b[0] == b.subrange(0, i)[0]);
        assert(a.drop_first().subrange(0, i - 1) =~= a.subrange(0, i).drop_first());
        assert(b.drop_first().subrange(0, i - 1) =~= b.subrange(0, i).drop_first());
        lemma_bytes_lt_skip(a.drop_first(), b.drop_first(), i - 1);
        assert(a.drop_first().skip(i - 1) =~= a.skip(i));
        assert(b.drop_first().skip(i - 1) =~= b.skip(i));
    } else {
        assert(a.skip(0) =~= a);
        assert(b.skip(0) =~= b);
    }
}

/// Byte-lexicographic comparison: negative, zero or positive as `a` sorts
/// before, equal to, or after `b`.
pub fn bytes_cmp(a: &[u8], b: &[u8]) -> (r: i8)
    ensures
        (r == 0) == (a@ == b@),
        (r < 0) == bytes_lt(a@, b@),
        (r > 0) == bytes_lt(b@, a@),
{
    let mut i: usize = 0;
    while i < a.len() && i < b.len()
        invariant
            i <= a.len(),
            i <= b.len(),
            a@.subrange(0, i as int) == b@.subrange(0, i as int),
        decreases a.len() - i,
    {
        if a[i] != b[i] {
            proof {
                lemma_bytes_lt_skip(a@, b@, i as int);
                lemma_bytes_lt_skip(b@, a@, i as int);
                assert(a@.skip(i as int)[0] == a@[i as int]);
                assert(b@.skip(i as int)[0] == b@[i as int]);
                assert(a@[i as int] != b@[i as int]);
            }
            return if a[i] < b[i] {
                -1
            } else {
                1
            };
        }
        proof {
            assert(a@.subrange(0, i as int + 1) =~= a@.subrange(0, i as int).push(a@[i as int]));
            assert(b@.subrange(0, i as int + 1) =~= b@.subrange(0, i as int).push(b@[i as int]));
        }
        i += 1;
    }
    proof {
        lemma_bytes_lt_skip(a@, b@, i as int);
        lemma_bytes_lt_skip(b@, a@, i as int);
    }
    if a.len() == b.len() {
        proof {
            assert(a@ =~= a@.subrange(0, i as int));
            assert(b@ =~= b@.subrange(0, i as int));
            lemma_bytes_lt_irreflexive(a@);
        }
        0
    } else if a.len() < b.len() {
        proof {
            assert(a@.skip(i as int).len() == 0);
        }
        -1
    } else {
        proof {
            assert(b@.skip(i as int).len() == 0);
        }
        1
    }
}

// ===========================================================================
// LWW key order
// ===========================================================================

/// The key an LWW register write is ordered by.
pub struct LwwKeyView {
    pub wall_ts: u64,
    pub agent: Seq<u8>,
    pub event_hash: Seq<u8>,
}

/// `a` loses to `b`: `(wall_ts, agent, event_hash)` compared
/// lexicographically.
pub open spec fn key_lt(a: LwwKeyView, b: LwwKeyView) -> bool {
    a.wall_ts < b.wall_ts || (a.wall_ts == b.wall_ts && (bytes_lt(a.agent, b.agent) || (
    a.agent == b.agent && bytes_lt(a.event_hash, b.event_hash))))
}

pub proof fn lemma_key_lt_irreflexive(a: LwwKeyView)
    ensures
        !key_lt(a, a),
{
    lemma_bytes_lt_irreflexive(a.agent);
    lemma_bytes_lt_irreflexive(a.event_hash);
}

pub proof fn lemma_key_lt_transitive(a: LwwKeyView, b: LwwKeyView, c: LwwKeyView)
    requires
        key_lt(a, b),
        key_lt(b, c),
    ensures
        key_lt(a, c),
{
    if a.wall_ts == b.wall_ts && b.wall_ts == c.wall_ts {
        if bytes_lt(a.agent, b.agent) && bytes_lt(b.agent, c.agent) {
            lemma_bytes_lt_transitive(a.agent, b.agent, c.agent);
        }
        if a.agent == b.agent && b.agent == c.agent && bytes_lt(a.event_hash, b.event_hash)
            && bytes_lt(b.event_hash, c.event_hash) {
            lemma_bytes_lt_transitive(a.event_hash, b.event_hash, c.event_hash);
        }
    }
}

/// Totality: two keys are equal or one loses to the other.
pub proof fn lemma_key_lt_total(a: LwwKeyView, b: LwwKeyView)
    ensures
        a == b || key_lt(a, b) || key_lt(b, a),
{
    lemma_bytes_lt_total(a.agent, b.agent);
    lemma_bytes_lt_total(a.event_hash, b.event_hash);
}

pub proof fn lemma_key_lt_asymmetric(a: LwwKeyView, b: LwwKeyView)
    requires
        key_lt(a, b),
    ensures
        !key_lt(b, a),
{
    if key_lt(b, a) {
        lemma_key_lt_transitive(a, b, a);
        lemma_key_lt_irreflexive(a);
    }
}

/// LWW merge on keys: keep `a` unless it loses to `b`. Equal keys carry equal
/// values (an event hash identifies one write), so merging keys is merging
/// registers.
pub open spec fn key_merge(a: LwwKeyView, b: LwwKeyView) -> LwwKeyView {
    if key_lt(a, b) {
        b
    } else {
        a
    }
}

pub proof fn lemma_key_merge_semilattice(a: LwwKeyView, b: LwwKeyView, c: LwwKeyView)
    ensures
        key_merge(a, a) == a,
        key_merge(a, b) == key_merge(b, a),
        key_merge(key_merge(a, b), c) == key_merge(a, key_merge(b, c)),
{
    lemma_key_lt_irreflexive(a);
    lemma_key_lt_total(a, b);
    lemma_key_lt_total(b, c);
    lemma_key_lt_total(a, c);
    if key_lt(a, b) {
        lemma_key_lt_asymmetric(a, b);
    }
    if key_lt(b, a) {
        lemma_key_lt_asymmetric(b, a);
    }
    if key_lt(a, b) && key_lt(b, c) {
        lemma_key_lt_transitive(a, b, c);
    }
    if key_lt(b, a) && key_lt(c, b) {
        lemma_key_lt_transitive(c, b, a);
    }
    if key_lt(a, c) {
        lemma_key_lt_asymmetric(a, c);
    }
    if key_lt(b, c) {
        lemma_key_lt_asymmetric(b, c);
    }
    if key_lt(c, a) && key_lt(a, b) {
        lemma_key_lt_transitive(c, a, b);
    }
    if key_lt(a, c) && key_lt(c, b) {
        lemma_key_lt_transitive(a, c, b);
    }
    if key_lt(b, a) && key_lt(a, c) {
        lemma_key_lt_transitive(b, a, c);
    }
    if key_lt(c, a) && key_lt(b, c) {
        lemma_key_lt_transitive(b, c, a);
    }
}

/// Which key component decided a comparison.
pub open spec fn decisive_step(a: LwwKeyView, b: LwwKeyView) -> u8 {
    if a.wall_ts != b.wall_ts {
        0
    } else if a.agent != b.agent {
        1
    } else {
        2
    }
}

/// Compare two LWW writes. Returns whether `a` wins (does not lose to `b`)
/// and the deciding step: 0 wall clock, 1 agent, 2 event hash.
pub fn lww_compare(
    wall_ts_a: u64,
    agent_a: &[u8],
    hash_a: &[u8],
    wall_ts_b: u64,
    agent_b: &[u8],
    hash_b: &[u8],
) -> (r: (bool, u8))
    ensures
        ({
            let a = LwwKeyView { wall_ts: wall_ts_a, agent: agent_a@, event_hash: hash_a@ };
            let b = LwwKeyView { wall_ts: wall_ts_b, agent: agent_b@, event_hash: hash_b@ };
            r.0 == !key_lt(a, b) && r.1 == decisive_step(a, b)
        }),
{
    if wall_ts_a != wall_ts_b {
        return (wall_ts_a > wall_ts_b, 0);
    }
    let agent = bytes_cmp(agent_a, agent_b);
    if agent != 0 {
        proof {
            if agent > 0 {
                lemma_bytes_lt_asymmetric(agent_b@, agent_a@);
            }
        }
        return (agent > 0, 1);
    }
    let hash = bytes_cmp(hash_a, hash_b);
    (hash >= 0, 2)
}

// ===========================================================================
// Epoch/phase join
// ===========================================================================

/// Lifecycle state `(epoch, phase rank)`: a higher epoch wins outright; in
/// one epoch the higher phase wins.
pub open spec fn ep_le(a: (u64, u8), b: (u64, u8)) -> bool {
    a.0 < b.0 || (a.0 == b.0 && a.1 <= b.1)
}

pub open spec fn ep_join(a: (u64, u8), b: (u64, u8)) -> (u64, u8) {
    if a.0 < b.0 {
        b
    } else if b.0 < a.0 {
        a
    } else if a.1 < b.1 {
        b
    } else {
        a
    }
}

pub proof fn lemma_ep_join_semilattice(a: (u64, u8), b: (u64, u8), c: (u64, u8))
    ensures
        ep_join(a, a) == a,
        ep_join(a, b) == ep_join(b, a),
        ep_join(ep_join(a, b), c) == ep_join(a, ep_join(b, c)),
        ep_le(a, ep_join(a, b)),
        ep_le(b, ep_join(a, b)),
        ep_le(a, c) && ep_le(b, c) ==> ep_le(ep_join(a, b), c),
{
}

/// Merge two lifecycle states `(epoch, phase rank)`.
pub fn epoch_phase_join(a: (u64, u8), b: (u64, u8)) -> (r: (u64, u8))
    ensures
        r == ep_join(a, b),
{
    if a.0 < b.0 {
        b
    } else if b.0 < a.0 {
        a
    } else if a.1 < b.1 {
        b
    } else {
        a
    }
}

// ===========================================================================
// Item timestamps
// ===========================================================================

/// Earliest timestamp, where 0 means "unknown" and loses to any value.
pub open spec fn spec_min_nonzero(a: u64, b: u64) -> u64 {
    if b != 0 && (a == 0 || b < a) {
        b
    } else {
        a
    }
}

pub proof fn lemma_min_nonzero_semilattice(a: u64, b: u64, c: u64)
    ensures
        spec_min_nonzero(a, a) == a,
        spec_min_nonzero(a, b) == spec_min_nonzero(b, a),
        spec_min_nonzero(spec_min_nonzero(a, b), c) == spec_min_nonzero(
            a,
            spec_min_nonzero(b, c),
        ),
{
}

/// Merge two `created_at` values.
pub fn min_nonzero(a: u64, b: u64) -> (r: u64)
    ensures
        r == spec_min_nonzero(a, b),
{
    if b != 0 && (a == 0 || b < a) {
        b
    } else {
        a
    }
}

/// Merge two `updated_at` values. `max` is commutative, associative and
/// idempotent without further proof.
pub fn max(a: u64, b: u64) -> (r: u64)
    ensures
        r == if a >= b {
            a
        } else {
            b
        },
{
    if a >= b {
        a
    } else {
        b
    }
}

} // verus!
