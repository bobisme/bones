use proptest::prelude::*;
use bones_core::crdt::merge::Merge;

// Import generators module
// Since generators.rs is a sibling file in tests/, we use #[path] to include it as a module.
#[path = "generators.rs"]
mod generators;
use generators::*;

proptest! {
    // Configure 10,000 cases for local dev (CI should override this via env vars or config)
    #![proptest_config(proptest::test_runner::Config::with_cases(10000))]

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
}
