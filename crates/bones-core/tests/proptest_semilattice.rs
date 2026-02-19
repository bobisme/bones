use bones_core::crdt::item_state::WorkItemState;
use bones_core::crdt::merge::Merge;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

// Import generators module
// Since generators.rs is a sibling file in tests/, we use #[path] to include it as a module.
#[path = "generators.rs"]
mod generators;
use generators::*;

fn proptest_config() -> Config {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(10_000);

    let mut config = Config::with_cases(cases);

    // Avoid noisy SourceParallel warnings for integration tests in this workspace.
    config.failure_persistence = None;

    // Allow deterministic replay with a project-level env var.
    if let Some(seed) = std::env::var("PROPTEST_SEED")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        config.rng_seed = RngSeed::Fixed(seed);
    }

    config
}

fn work_item_states_equal(a: &WorkItemState, b: &WorkItemState) -> bool {
    a.title == b.title
        && a.description == b.description
        && a.kind == b.kind
        && a.state == b.state
        && a.size == b.size
        && a.urgency == b.urgency
        && a.parent == b.parent
        && a.assignees == b.assignees
        && a.labels == b.labels
        && a.blocked_by == b.blocked_by
        && a.related_to == b.related_to
        && a.comments == b.comments
        && a.deleted == b.deleted
        && a.created_at == b.created_at
        && a.updated_at == b.updated_at
}

proptest! {
    #![proptest_config(proptest_config())]

    // LWW Tests
    #[test]
    fn lww_commutative(a in arb_lww::<u32>(), b in arb_lww::<u32>()) {
        let mut ma = a.clone();
        ma.merge(b.clone());
        let mut mb = b.clone();
        mb.merge(a.clone());
        prop_assert_eq!(ma, mb);
    }

    #[test]
    fn lww_associative(a in arb_lww::<u32>(), b in arb_lww::<u32>(), c in arb_lww::<u32>()) {
        let mut m1 = a.clone();
        m1.merge(b.clone());
        m1.merge(c.clone());

        let mut m2 = b.clone();
        m2.merge(c.clone());
        let mut m3 = a.clone();
        m3.merge(m2);

        prop_assert_eq!(m1, m3);
    }

    #[test]
    fn lww_idempotent(a in arb_lww::<u32>()) {
        let mut ma = a.clone();
        ma.merge(a.clone());
        prop_assert_eq!(ma, a);
    }

    // GSet Tests
    #[test]
    fn gset_commutative(a in arb_gset::<u32>(), b in arb_gset::<u32>()) {
        let mut ma = a.clone();
        ma.merge(b.clone());
        let mut mb = b.clone();
        mb.merge(a.clone());
        prop_assert_eq!(ma, mb);
    }

    #[test]
    fn gset_associative(a in arb_gset::<u32>(), b in arb_gset::<u32>(), c in arb_gset::<u32>()) {
        let mut m1 = a.clone();
        m1.merge(b.clone());
        m1.merge(c.clone());

        let mut m2 = b.clone();
        m2.merge(c.clone());
        let mut m3 = a.clone();
        m3.merge(m2);

        prop_assert_eq!(m1, m3);
    }

    #[test]
    fn gset_idempotent(a in arb_gset::<u32>()) {
        let mut ma = a.clone();
        ma.merge(a.clone());
        prop_assert_eq!(ma, a);
    }

    // OrSet Tests
    #[test]
    fn orset_commutative(a in arb_orset::<u32>(), b in arb_orset::<u32>()) {
        let mut ma = a.clone();
        ma.merge(b.clone());
        let mut mb = b.clone();
        mb.merge(a.clone());
        prop_assert_eq!(ma, mb);
    }

    #[test]
    fn orset_associative(a in arb_orset::<u32>(), b in arb_orset::<u32>(), c in arb_orset::<u32>()) {
        let mut m1 = a.clone();
        m1.merge(b.clone());
        m1.merge(c.clone());

        let mut m2 = b.clone();
        m2.merge(c.clone());
        let mut m3 = a.clone();
        m3.merge(m2);

        prop_assert_eq!(m1, m3);
    }

    #[test]
    fn orset_idempotent(a in arb_orset::<u32>()) {
        let mut ma = a.clone();
        ma.merge(a.clone());
        prop_assert_eq!(ma, a);
    }

    // EpochPhase Tests
    #[test]
    fn epoch_phase_commutative(a in arb_epoch_phase(), b in arb_epoch_phase()) {
        let mut ma = a.clone();
        ma.merge(b.clone());
        let mut mb = b.clone();
        mb.merge(a.clone());
        prop_assert_eq!(ma, mb);
    }

    #[test]
    fn epoch_phase_associative(a in arb_epoch_phase(), b in arb_epoch_phase(), c in arb_epoch_phase()) {
        let mut m1 = a.clone();
        m1.merge(b.clone());
        m1.merge(c.clone());

        let mut m2 = b.clone();
        m2.merge(c.clone());
        let mut m3 = a.clone();
        m3.merge(m2);

        prop_assert_eq!(m1, m3);
    }

    #[test]
    fn epoch_phase_idempotent(a in arb_epoch_phase()) {
        let mut ma = a.clone();
        ma.merge(a.clone());
        prop_assert_eq!(ma, a);
    }

    // WorkItemState aggregate tests
    #[test]
    fn work_item_state_commutative(a in arb_work_item_state(), b in arb_work_item_state()) {
        let mut ab = a.clone();
        ab.merge(&b);

        let mut ba = b.clone();
        ba.merge(&a);

        prop_assert!(work_item_states_equal(&ab, &ba));
    }

    #[test]
    fn work_item_state_associative(
        a in arb_work_item_state(),
        b in arb_work_item_state(),
        c in arb_work_item_state()
    ) {
        let mut ab_c = a.clone();
        ab_c.merge(&b);
        ab_c.merge(&c);

        let mut bc = b.clone();
        bc.merge(&c);

        let mut a_bc = a.clone();
        a_bc.merge(&bc);

        prop_assert!(work_item_states_equal(&ab_c, &a_bc));
    }

    #[test]
    fn work_item_state_idempotent(a in arb_work_item_state()) {
        let before = a.clone();
        let mut merged = a.clone();
        merged.merge(&a);

        prop_assert!(work_item_states_equal(&merged, &before));
    }
}
