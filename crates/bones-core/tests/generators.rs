use bones_core::crdt::*;
use chrono::{TimeZone, Utc};
use proptest::prelude::*;
use std::hash::Hash;

pub fn arb_timestamp() -> impl Strategy<Value = Timestamp> + Clone {
    (
        0i64..2000000000,
        0u32..1000000000,
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(wall_secs, wall_nsecs, actor, event_hash, itc)| Timestamp {
                wall: Utc.timestamp_opt(wall_secs, wall_nsecs).unwrap(),
                actor,
                event_hash,
                itc,
            },
        )
}

pub fn arb_lww<T: Arbitrary + Clone + 'static>() -> impl Strategy<Value = Lww<T>> + Clone
where
    <T as Arbitrary>::Strategy: Clone,
{
    (any::<T>(), arb_timestamp()).prop_map(|(value, timestamp)| Lww { value, timestamp })
}

pub fn arb_gset<T: Arbitrary + Clone + Hash + Eq + 'static>()
-> impl Strategy<Value = GSet<T>> + Clone
where
    <T as Arbitrary>::Strategy: Clone,
{
    prop::collection::hash_set(any::<T>(), 0..50).prop_map(|elements| GSet { elements })
}

pub fn arb_orset<T: Arbitrary + Clone + Hash + Eq + 'static>()
-> impl Strategy<Value = OrSet<T>> + Clone
where
    <T as Arbitrary>::Strategy: Clone,
{
    let element_strategy = (any::<T>(), arb_timestamp());
    (
        prop::collection::hash_set(element_strategy.clone(), 0..20),
        prop::collection::hash_set(element_strategy, 0..20),
    )
        .prop_map(|(elements, tombstone)| OrSet {
            elements,
            tombstone,
        })
}

pub fn arb_epoch_phase() -> impl Strategy<Value = EpochPhase> + Clone {
    (
        any::<u64>(),
        prop_oneof![Just(Phase::Init), Just(Phase::Propose), Just(Phase::Commit),],
    )
        .prop_map(|(epoch, phase)| EpochPhase { epoch, phase })
}
