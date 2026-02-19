use bones_core::clock::itc::Stamp;
use bones_core::crdt::item_state::WorkItemState;
use bones_core::crdt::lww::LwwRegister;
use bones_core::crdt::state::{EpochPhaseState, Phase as LifecyclePhase};
use bones_core::crdt::*;
use bones_core::model::item::{Kind, Size, Urgency};
use chrono::{TimeZone, Utc};
use proptest::prelude::*;
use std::hash::Hash;

pub fn arb_timestamp() -> impl Strategy<Value = Timestamp> + Clone {
    (
        0i64..2_000_000_000,
        0u32..1_000_000_000,
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

fn stamp_from_token(token: u8) -> Stamp {
    // Keep ID constant and vary only causal event count so `leq` comparisons
    // align with LWW assumptions (same stamp => same write).
    let mut stamp = Stamp::seed();

    // token=0 still produces a non-zero event clock.
    for _ in 0..=token {
        stamp.event();
    }

    stamp
}

fn lww_from_token<T>(token: u8, value: T) -> LwwRegister<T> {
    let token_u64 = u64::from(token);
    LwwRegister::new(
        value,
        stamp_from_token(token),
        token_u64,
        format!("agent-{}", token % 11),
        format!("blake3:{token:02x}"),
    )
}

fn arb_lww_register_string(prefix: &'static str) -> impl Strategy<Value = LwwRegister<String>> + Clone {
    any::<u8>().prop_map(move |token| lww_from_token(token, format!("{prefix}-{token:02x}")))
}

fn arb_lww_register_kind() -> impl Strategy<Value = LwwRegister<Kind>> + Clone {
    any::<u8>().prop_map(|token| {
        let value = match token % 3 {
            0 => Kind::Task,
            1 => Kind::Goal,
            _ => Kind::Bug,
        };
        lww_from_token(token, value)
    })
}

fn arb_lww_register_size() -> impl Strategy<Value = LwwRegister<Option<Size>>> + Clone {
    any::<u8>().prop_map(|token| {
        let value = match token % 8 {
            0 => None,
            1 => Some(Size::Xxs),
            2 => Some(Size::Xs),
            3 => Some(Size::S),
            4 => Some(Size::M),
            5 => Some(Size::L),
            6 => Some(Size::Xl),
            _ => Some(Size::Xxl),
        };
        lww_from_token(token, value)
    })
}

fn arb_lww_register_urgency() -> impl Strategy<Value = LwwRegister<Urgency>> + Clone {
    any::<u8>().prop_map(|token| {
        let value = match token % 3 {
            0 => Urgency::Urgent,
            1 => Urgency::Default,
            _ => Urgency::Punt,
        };
        lww_from_token(token, value)
    })
}

fn arb_lww_register_parent() -> impl Strategy<Value = LwwRegister<String>> + Clone {
    any::<u8>().prop_map(|token| {
        let value = if token % 4 == 0 {
            String::new()
        } else {
            format!("bn-p{token:02x}")
        };
        lww_from_token(token, value)
    })
}

fn arb_lww_register_bool() -> impl Strategy<Value = LwwRegister<bool>> + Clone {
    any::<u8>().prop_map(|token| lww_from_token(token, token % 2 == 0))
}

fn arb_orset_string() -> impl Strategy<Value = OrSet<String>> + Clone {
    arb_orset::<u16>().prop_map(|set| OrSet {
        elements: set
            .elements
            .into_iter()
            .map(|(value, ts)| (format!("v{value}"), ts))
            .collect(),
        tombstone: set
            .tombstone
            .into_iter()
            .map(|(value, ts)| (format!("v{value}"), ts))
            .collect(),
    })
}

fn arb_gset_string() -> impl Strategy<Value = GSet<String>> + Clone {
    arb_gset::<u16>().prop_map(|set| GSet {
        elements: set
            .elements
            .into_iter()
            .map(|value| format!("c{value}"))
            .collect(),
    })
}

pub fn arb_epoch_phase_state() -> impl Strategy<Value = EpochPhaseState> + Clone {
    (
        0u64..32,
        prop_oneof![
            Just(LifecyclePhase::Open),
            Just(LifecyclePhase::Doing),
            Just(LifecyclePhase::Done),
            Just(LifecyclePhase::Archived)
        ],
    )
        .prop_map(|(epoch, phase)| EpochPhaseState::with(epoch, phase))
}

pub fn arb_work_item_state() -> impl Strategy<Value = WorkItemState> + Clone {
    (
        (
            arb_lww_register_string("title"),
            arb_lww_register_string("description"),
            arb_lww_register_kind(),
            arb_epoch_phase_state(),
            arb_lww_register_size(),
            arb_lww_register_urgency(),
            arb_lww_register_parent(),
        ),
        (
            arb_orset_string(),
            arb_orset_string(),
            arb_orset_string(),
            arb_orset_string(),
            arb_gset_string(),
            arb_lww_register_bool(),
        ),
        0u64..100_000,
        0u64..10_000,
    )
        .prop_map(
            |(
                (title, description, kind, state, size, urgency, parent),
                (assignees, labels, blocked_by, related_to, comments, deleted),
                created_at,
                delta,
            )| WorkItemState {
                title,
                description,
                kind,
                state,
                size,
                urgency,
                parent,
                assignees,
                labels,
                blocked_by,
                related_to,
                comments,
                deleted,
                created_at,
                updated_at: created_at.saturating_add(delta),
            },
        )
}
