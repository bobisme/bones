use bones_core::clock::itc::{Id, Stamp};
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

/// Operation in a simulated multi-replica causal history.
#[derive(Clone, Debug)]
enum HistoryOp {
    /// `replica` records a write stamped with its clock and `wall_ts`.
    Write { replica: usize, wall_ts: u64 },
    /// `to` learns the causal history of `from` (ITC receive).
    Sync { from: usize, to: usize },
}

/// One write from a simulated causal history.
#[derive(Clone, Debug)]
pub struct HistoryWrite {
    pub stamp: Stamp,
    pub wall_ts: u64,
    pub agent_id: String,
    pub event_hash: String,
}

const HISTORY_REPLICAS: usize = 3;

fn arb_history_op() -> impl Strategy<Value = HistoryOp> + Clone {
    prop_oneof![
        3 => (0..HISTORY_REPLICAS, 0u64..6)
            .prop_map(|(replica, wall_ts)| HistoryOp::Write { replica, wall_ts }),
        1 => (0..HISTORY_REPLICAS, 0..HISTORY_REPLICAS)
            .prop_map(|(from, to)| HistoryOp::Sync { from, to }),
    ]
}

fn replay_history(ops: &[HistoryOp]) -> Vec<HistoryWrite> {
    let (left, c) = Stamp::seed().fork();
    let (a, b) = left.fork();
    let mut replicas = [a, b, c];
    let mut writes = Vec::new();
    for op in ops {
        match *op {
            HistoryOp::Write { replica, wall_ts } => {
                replicas[replica].event();
                let index = writes.len();
                writes.push(HistoryWrite {
                    stamp: replicas[replica].clone(),
                    wall_ts,
                    agent_id: format!("agent-{replica}"),
                    event_hash: format!("blake3:{index:04x}"),
                });
            }
            HistoryOp::Sync { from, to } => {
                let known = Stamp::new(Id::zero(), replicas[from].event.clone());
                replicas[to] = Stamp::join(&replicas[to], &known);
            }
        }
    }
    writes
}

/// A causal history of writes from three replicas that sync at random.
///
/// Stamps come from real ITC fork/event/join operations, and wall clocks are
/// drawn independently of causality from a small range. A causally later
/// write can therefore carry a lower wall clock (clock skew), and ties are
/// common. Each write has a unique event hash.
pub fn arb_causal_history() -> impl Strategy<Value = Vec<HistoryWrite>> + Clone {
    (
        (0..HISTORY_REPLICAS, 0u64..6),
        prop::collection::vec(arb_history_op(), 2..16),
    )
        .prop_map(|((replica, wall_ts), rest)| {
            let mut ops = vec![HistoryOp::Write { replica, wall_ts }];
            ops.extend(rest);
            replay_history(&ops)
        })
}

/// Build the register for write `index` of `history`, with a value derived
/// from the index so that one write always carries one value.
pub fn history_register<T>(
    history: &[HistoryWrite],
    index: usize,
    value: impl Fn(usize) -> T,
) -> LwwRegister<T> {
    let index = index % history.len();
    let write = &history[index];
    LwwRegister::new(
        value(index),
        write.stamp.clone(),
        write.wall_ts,
        write.agent_id.clone(),
        write.event_hash.clone(),
    )
}

/// Three registers picked from one causal history.
pub fn arb_lww_register_triple() -> impl Strategy<Value = [LwwRegister<String>; 3]> + Clone {
    (arb_causal_history(), any::<[usize; 3]>()).prop_map(|(history, picks)| {
        picks.map(|index| history_register(&history, index, |i| format!("w{i}")))
    })
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

fn kind_from_index(index: usize) -> Kind {
    match index % 3 {
        0 => Kind::Task,
        1 => Kind::Goal,
        _ => Kind::Bug,
    }
}

fn size_from_index(index: usize) -> Option<Size> {
    match index % 6 {
        0 => None,
        1 => Some(Size::Xs),
        2 => Some(Size::S),
        3 => Some(Size::M),
        4 => Some(Size::L),
        _ => Some(Size::Xl),
    }
}

fn urgency_from_index(index: usize) -> Urgency {
    match index % 3 {
        0 => Urgency::Urgent,
        1 => Urgency::Default,
        _ => Urgency::Punt,
    }
}

fn parent_from_index(index: usize) -> String {
    if index % 4 == 0 {
        String::new()
    } else {
        format!("bn-p{index:02x}")
    }
}

fn arb_work_item_state_from(history: Vec<HistoryWrite>) -> impl Strategy<Value = WorkItemState> {
    (
        any::<[usize; 7]>(),
        arb_epoch_phase_state(),
        (
            arb_orset_string(),
            arb_orset_string(),
            arb_orset_string(),
            arb_orset_string(),
            arb_gset_string(),
        ),
        0u64..100_000,
        0u64..10_000,
    )
        .prop_map(
            move |(
                picks,
                state,
                (assignees, labels, blocked_by, related_to, comments),
                created_at,
                delta,
            )| WorkItemState {
                title: history_register(&history, picks[0], |i| format!("title-{i}")),
                description: history_register(&history, picks[1], |i| format!("desc-{i}")),
                kind: history_register(&history, picks[2], kind_from_index),
                state,
                size: history_register(&history, picks[3], size_from_index),
                urgency: history_register(&history, picks[4], urgency_from_index),
                parent: history_register(&history, picks[5], parent_from_index),
                assignees,
                labels,
                blocked_by,
                related_to,
                comments,
                deleted: history_register(&history, picks[6], |i| i % 2 == 0),
                created_at,
                updated_at: created_at.saturating_add(delta),
            },
        )
}

/// Three work item states whose LWW fields hold writes from one causal
/// history, so merges compare stamps that are causally meaningful.
pub fn arb_work_item_state_triple()
-> impl Strategy<Value = (WorkItemState, WorkItemState, WorkItemState)> {
    arb_causal_history().prop_flat_map(|history| {
        (
            arb_work_item_state_from(history.clone()),
            arb_work_item_state_from(history.clone()),
            arb_work_item_state_from(history),
        )
    })
}
