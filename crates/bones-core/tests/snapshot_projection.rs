//! Snapshot / projection agreement (bn-t37g).
//!
//! Spec: `bn compact` must write a snapshot of the same item that the
//! `SQLite` projection shows (`bn show`). The snapshot comes from
//! `WorkItemState::from_events`, so for any event set, the state from
//! `from_events` and the state rebuilt from the compacted snapshot must both
//! match the projection of the same events, field by field.
//!
//! The generator writes events of a few items with overlapping clocks (a
//! create can sort after an update), times at or before 0, creates without
//! optional fields, malformed update values, legacy label arrays, every link
//! type, compact events and duplicate lines.
//!
//! Malformed update values are no write in both (bn-18fs, see
//! `model::field_value`).
//!
//! The projection of the snapshot alone, as a compacted log holds it, must
//! also show the same item (bn-18fs), except for comments (a snapshot
//! keeps their hashes, not their bodies) and `updated_at` (the snapshot
//! sorts after its sources).
//!
//! Known representation gaps, normalized here: the state stores "no
//! description" and "no parent" as `""` where the projection stores NULL.
//! Redactions are left out: compaction refuses items with redacted events.

use bones_core::compact::{compact_item, extract_snapshot_payload};
use bones_core::crdt::item_state::{WorkItemState, is_blocking_link_type, is_related_link_type};
use bones_core::crdt::state::Phase;
use bones_core::db::migrations;
use bones_core::db::project::{Projector, ensure_tracking_table};
use bones_core::event::Event;
use bones_core::event::data::{
    AssignAction, AssignData, CommentData, CompactData, CreateData, DeleteData, EventData,
    LinkData, MoveData, UnlinkData, UpdateData,
};
use bones_core::event::types::EventType;
use bones_core::event::writer::write_event;
use bones_core::model::item::{Kind, Size, State, Urgency};
use bones_core::model::item_id::ItemId;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use rusqlite::{Connection, params};
use std::collections::{BTreeMap, BTreeSet, HashSet};

const ITEMS: [&str; 2] = ["bn-a1", "bn-b2"];
/// Link targets and parents. The log creates them first: the projection's
/// foreign keys need them.
const TARGETS: [&str; 2] = ["bn-t0", "bn-t1"];
const PARENTS: [&str; 2] = ["bn-p0", "bn-p1"];
const AGENTS: [&str; 3] = ["agent-a", "agent-b", "agent-c"];
const LINK_TYPES: [&str; 6] = [
    "blocks",
    "blocked_by",
    "related_to",
    "related",
    "relates",
    "duplicates",
];

fn proptest_config() -> Config {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(256);
    let mut config = Config::with_cases(cases);
    config.failure_persistence = None;
    // Deterministic by default. PROPTEST_SEED picks another seed.
    let seed = std::env::var("PROPTEST_SEED")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0x7337_600d);
    config.rng_seed = RngSeed::Fixed(seed);
    config
}

/// One mutation of an item, without its metadata.
#[derive(Debug, Clone)]
enum Op {
    Create {
        title: u8,
        kind: Kind,
        size: Option<Size>,
        urgency: Urgency,
        description: Option<u8>,
        parent: bool,
        labels: u8,
    },
    /// Update of `field` to a JSON value.
    Update(&'static str, serde_json::Value),
    Label {
        add: bool,
        label: u8,
    },
    /// Legacy whole-set label replacement: the labels whose bits are set.
    LabelsReplace(u8),
    Assign {
        assign: bool,
        agent: u8,
    },
    Move(State),
    Link {
        target: u8,
        link_type: u8,
    },
    /// Remove one link type, or every link to the target.
    Unlink {
        target: u8,
        link_type: Option<u8>,
    },
    Comment(u8),
    Compact(u8),
    Delete,
    /// Write an earlier event again (a duplicate line from a union merge).
    Duplicate(u8),
}

fn arb_kind() -> impl Strategy<Value = Kind> {
    prop_oneof![Just(Kind::Task), Just(Kind::Goal), Just(Kind::Bug)]
}

fn arb_size() -> impl Strategy<Value = Size> {
    prop_oneof![Just(Size::S), Just(Size::M), Just(Size::L)]
}

fn arb_urgency() -> impl Strategy<Value = Urgency> {
    prop_oneof![
        Just(Urgency::Urgent),
        Just(Urgency::Default),
        Just(Urgency::Punt)
    ]
}

/// Malformed update values (see `model::field_value`): values that are not
/// strings, and strings that do not parse.
fn arb_non_string() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        Just(serde_json::Value::Null),
        Just(serde_json::json!(7)),
        Just(serde_json::json!(true)),
        Just(serde_json::json!({"x": 1})),
    ]
}

/// Strings that no kind, urgency or size parses, and that are no item ID.
fn arb_bad_string() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        Just(serde_json::json!("")),
        Just(serde_json::json!("bogus")),
    ]
}

fn arb_update() -> impl Strategy<Value = Op> {
    prop_oneof![
        arb_bad_string().prop_map(|v| Op::Update("kind", v)),
        arb_bad_string().prop_map(|v| Op::Update("urgency", v)),
        arb_bad_string().prop_map(|v| Op::Update("size", v)),
        arb_bad_string().prop_map(|v| Op::Update("parent", v)),
        Just(Op::Update("description", serde_json::json!(""))),
        // Blank members are no write (bn-18fs).
        Just(Op::Update(
            "labels",
            serde_json::json!({"action": "add", "label": "  "})
        )),
        Just(Op::Update("labels", serde_json::json!(["", "label-1"]))),
        (0u8..3).prop_map(|n| Op::Update("title", serde_json::json!(format!("title {n}")))),
        arb_non_string().prop_map(|v| Op::Update("title", v)),
        (0u8..3).prop_map(|n| Op::Update("description", serde_json::json!(format!("desc {n}")))),
        arb_non_string().prop_map(|v| Op::Update("description", v)),
        arb_kind().prop_map(|k| Op::Update("kind", serde_json::json!(k.to_string()))),
        arb_non_string().prop_map(|v| Op::Update("kind", v)),
        arb_size().prop_map(|s| Op::Update("size", serde_json::json!(s.to_string()))),
        arb_non_string().prop_map(|v| Op::Update("size", v)),
        arb_urgency().prop_map(|u| Op::Update("urgency", serde_json::json!(u.to_string()))),
        arb_non_string().prop_map(|v| Op::Update("urgency", v)),
        Just(Op::Update("parent", serde_json::json!(PARENTS[1]))),
        arb_non_string().prop_map(|v| Op::Update("parent", v)),
        Just(Op::Update("unknown_field", serde_json::json!("x"))),
    ]
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (
            0u8..3,
            arb_kind(),
            prop::option::of(arb_size()),
            arb_urgency(),
            prop::option::of(0u8..3),
            any::<bool>(),
            0u8..8,
        )
            .prop_map(|(title, kind, size, urgency, description, parent, labels)| {
                Op::Create {
                    title,
                    kind,
                    size,
                    urgency,
                    description,
                    parent,
                    labels,
                }
            }),
        6 => arb_update(),
        2 => (any::<bool>(), 0u8..3).prop_map(|(add, label)| Op::Label { add, label }),
        1 => (0u8..8).prop_map(Op::LabelsReplace),
        1 => (any::<bool>(), 0u8..2).prop_map(|(assign, agent)| Op::Assign { assign, agent }),
        2 => prop_oneof![
            Just(State::Open),
            Just(State::Doing),
            Just(State::Done),
            Just(State::Archived)
        ]
        .prop_map(Op::Move),
        3 => (0u8..2, 0..LINK_TYPES.len() as u8)
            .prop_map(|(target, link_type)| Op::Link { target, link_type }),
        2 => (0u8..2, prop::option::of(0..LINK_TYPES.len() as u8))
            .prop_map(|(target, link_type)| Op::Unlink { target, link_type }),
        1 => (0u8..3).prop_map(Op::Comment),
        1 => (0u8..3).prop_map(Op::Compact),
        1 => Just(Op::Delete),
        1 => (0u8..16).prop_map(Op::Duplicate),
    ]
}

/// A generated write: which item, what op, its wall clock and agent.
#[derive(Debug, Clone)]
struct Write {
    item: usize,
    op: Op,
    wall_ts: i64,
    agent: usize,
}

fn arb_writes() -> impl Strategy<Value = Vec<Write>> {
    prop::collection::vec(
        (0..ITEMS.len(), arb_op(), -2i64..8, 0..AGENTS.len()).prop_map(
            |(item, op, wall_ts, agent)| Write {
                item,
                op,
                wall_ts,
                agent,
            },
        ),
        1..14,
    )
}

/// Only link and link-removal writes on one item and one target, with the
/// four types that feed the views, so both views see many conflicts.
fn arb_link_writes() -> impl Strategy<Value = Vec<Write>> {
    let op = prop_oneof![
        (0u8..4).prop_map(|link_type| Op::Link {
            target: 0,
            link_type,
        }),
        prop::option::of(0u8..4).prop_map(|link_type| Op::Unlink {
            target: 0,
            link_type,
        }),
    ];
    prop::collection::vec(
        (op, 0i64..6, 0..AGENTS.len()).prop_map(|(op, wall_ts, agent)| Write {
            item: 0,
            op,
            wall_ts,
            agent,
        }),
        1..12,
    )
}

fn update(field: &str, value: serde_json::Value) -> (EventType, EventData) {
    (
        EventType::Update,
        EventData::Update(UpdateData {
            field: field.to_string(),
            value,
            extra: BTreeMap::new(),
        }),
    )
}

fn label_names(bits: u8) -> Vec<String> {
    (0u8..3)
        .filter(|i| bits & (1 << i) != 0)
        .map(|i| format!("label-{i}"))
        .collect()
}

fn build_data(op: &Op, n: usize) -> (EventType, EventData) {
    match op {
        Op::Create {
            title,
            kind,
            size,
            urgency,
            description,
            parent,
            labels,
        } => (
            EventType::Create,
            EventData::Create(CreateData {
                title: format!("item {title}"),
                kind: *kind,
                size: *size,
                urgency: *urgency,
                labels: label_names(*labels),
                parent: parent.then(|| PARENTS[0].to_string()),
                causation: None,
                description: description.map(|d| format!("created {d}")),
                extra: BTreeMap::new(),
            }),
        ),
        Op::Update(field, value) => update(field, value.clone()),
        Op::Label { add, label } => update(
            "labels",
            serde_json::json!({
                "action": if *add { "add" } else { "remove" },
                "label": format!("label-{label}"),
            }),
        ),
        Op::LabelsReplace(bits) => update("labels", serde_json::json!(label_names(*bits))),
        Op::Assign { assign, agent } => (
            EventType::Assign,
            EventData::Assign(AssignData {
                agent: format!("person-{agent}"),
                action: if *assign {
                    AssignAction::Assign
                } else {
                    AssignAction::Unassign
                },
                extra: BTreeMap::new(),
            }),
        ),
        Op::Move(state) => (
            EventType::Move,
            EventData::Move(MoveData {
                state: *state,
                reason: None,
                extra: BTreeMap::new(),
            }),
        ),
        Op::Link { target, link_type } => (
            EventType::Link,
            EventData::Link(LinkData {
                target: TARGETS[usize::from(*target)].to_string(),
                link_type: LINK_TYPES[usize::from(*link_type)].to_string(),
                extra: BTreeMap::new(),
            }),
        ),
        Op::Unlink { target, link_type } => (
            EventType::Unlink,
            EventData::Unlink(UnlinkData {
                target: TARGETS[usize::from(*target)].to_string(),
                link_type: link_type.map(|t| LINK_TYPES[usize::from(t)].to_string()),
                extra: BTreeMap::new(),
            }),
        ),
        Op::Comment(c) => (
            EventType::Comment,
            EventData::Comment(CommentData {
                // The index keeps equal comments of one agent distinct.
                body: format!("comment {c} #{n}"),
                extra: BTreeMap::new(),
            }),
        ),
        Op::Compact(c) => (
            EventType::Compact,
            EventData::Compact(CompactData {
                summary: format!("summary {c}"),
                extra: BTreeMap::new(),
            }),
        ),
        Op::Delete => (
            EventType::Delete,
            EventData::Delete(DeleteData {
                reason: None,
                extra: BTreeMap::new(),
            }),
        ),
        Op::Duplicate(_) => unreachable!("handled by build_events"),
    }
}

/// Build the log: events with hashes, duplicates included, in write order.
fn build_events(writes: &[Write]) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::with_capacity(writes.len());
    for id in TARGETS.iter().chain(&PARENTS) {
        let mut event = Event {
            wall_ts_us: 1,
            agent: "agent-base".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(*id),
            data: EventData::Create(CreateData {
                title: (*id).to_string(),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        write_event(&mut event).expect("serialize event");
        out.push(event);
    }
    for (n, write) in writes.iter().enumerate() {
        if let Op::Duplicate(k) = write.op {
            if !out.is_empty() {
                let copy = out[usize::from(k) % out.len()].clone();
                out.push(copy);
            }
            continue;
        }
        let (event_type, data) = build_data(&write.op, n);
        let mut event = Event {
            wall_ts_us: write.wall_ts,
            agent: AGENTS[write.agent].to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type,
            item_id: ItemId::new_unchecked(ITEMS[write.item]),
            data,
            event_hash: String::new(),
        };
        write_event(&mut event).expect("serialize event");
        out.push(event);
    }
    out
}

/// Every field a snapshot carries, as the user sees it.
#[derive(Debug, PartialEq, Eq)]
struct View {
    title: String,
    description: String,
    kind: String,
    state: String,
    size: Option<String>,
    urgency: String,
    parent: String,
    compact_summary: String,
    deleted: bool,
    labels: BTreeSet<String>,
    assignees: BTreeSet<String>,
    links: BTreeSet<(String, String)>,
    comments: BTreeSet<String>,
    created_at: i64,
    updated_at: i64,
}

fn strings(conn: &Connection, sql: &str, item: &str) -> BTreeSet<String> {
    let mut stmt = conn.prepare(sql).expect("prepare");
    stmt.query_map(params![item], |row| row.get::<_, String>(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("read rows")
}

fn projection_view(conn: &Connection, item: &str) -> Option<View> {
    let mut stmt = conn
        .prepare(
            "SELECT title, description, kind, state, size, urgency, parent_id, \
             compact_summary, is_deleted, created_at_us, updated_at_us \
             FROM items WHERE item_id = ?1",
        )
        .expect("prepare item");
    let mut rows = stmt.query(params![item]).expect("query item");
    let row = rows.next().expect("read item")?;
    let text = |i: usize| -> String {
        row.get::<_, Option<String>>(i)
            .expect("text column")
            .unwrap_or_default()
    };
    let links = {
        let mut stmt = conn
            .prepare(
                "SELECT depends_on_item_id, link_type FROM item_dependencies WHERE item_id = ?1",
            )
            .expect("prepare links");
        stmt.query_map(params![item], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query links")
            .collect::<Result<_, _>>()
            .expect("read links")
    };
    Some(View {
        title: text(0),
        description: text(1),
        kind: text(2),
        state: text(3),
        size: row.get(4).expect("size"),
        urgency: text(5),
        parent: text(6),
        compact_summary: text(7),
        deleted: row.get::<_, i64>(8).expect("is_deleted") != 0,
        labels: strings(
            conn,
            "SELECT label FROM item_labels WHERE item_id = ?1",
            item,
        ),
        assignees: strings(
            conn,
            "SELECT agent FROM item_assignees WHERE item_id = ?1",
            item,
        ),
        links,
        comments: strings(
            conn,
            "SELECT event_hash FROM item_comments WHERE item_id = ?1",
            item,
        ),
        created_at: row.get(9).expect("created_at_us"),
        updated_at: row.get(10).expect("updated_at_us"),
    })
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Open => "open",
        Phase::Doing => "doing",
        Phase::Done => "done",
        Phase::Archived => "archived",
    }
}

fn state_view(state: &WorkItemState) -> View {
    View {
        title: state.title.value.clone(),
        description: state.description.value.clone(),
        kind: state.kind.value.to_string(),
        state: phase_name(state.phase()).to_string(),
        size: state.size.value.map(|s| s.to_string()),
        urgency: state.urgency.value.to_string(),
        parent: state.parent.value.clone(),
        compact_summary: state.compact_summary.value.clone(),
        deleted: state.is_deleted(),
        labels: state.label_names().into_iter().cloned().collect(),
        assignees: state.assignee_names().into_iter().cloned().collect(),
        links: state
            .link_keys()
            .into_iter()
            .map(|k| (k.target.clone(), k.link_type.clone()))
            .collect(),
        comments: state.comment_hashes().iter().cloned().collect(),
        created_at: state.created_at.cast_signed(),
        updated_at: state.updated_at.cast_signed(),
    }
}

/// `blocked_by` and `related_to` must be the views of `links` they claim.
fn link_views_agree(state: &WorkItemState) -> Result<(), String> {
    let targets = |pred: fn(&str) -> bool| -> HashSet<String> {
        state
            .link_keys()
            .into_iter()
            .filter(|k| pred(&k.link_type))
            .map(|k| k.target.clone())
            .collect()
    };
    let blocked: HashSet<String> = state.blocked_by_ids().into_iter().cloned().collect();
    let related: HashSet<String> = state.related_to_ids().into_iter().cloned().collect();
    if blocked != targets(is_blocking_link_type) {
        return Err(format!("blocked_by view {blocked:?} != links"));
    }
    if related != targets(is_related_link_type) {
        return Err(format!("related_to view {related:?} != links"));
    }
    Ok(())
}

fn project(events: &[Event]) -> Connection {
    let mut conn = Connection::open_in_memory().expect("open db");
    migrations::migrate(&mut conn).expect("migrate");
    ensure_tracking_table(&conn).expect("tracking table");
    let stats = Projector::new(&conn)
        .project_batch(events)
        .expect("project events");
    assert_eq!(stats.errors, 0, "projection errors");
    conn
}

proptest! {
    #![proptest_config(proptest_config())]

    /// `from_events`, and the state rebuilt from the compacted snapshot, show
    /// each item as the projection does.
    #[test]
    fn snapshot_matches_projection(writes in arb_writes()) {
        let events = build_events(&writes);
        let conn = project(&events);

        for item in ITEMS {
            let item_events: Vec<Event> = events
                .iter()
                .filter(|e| e.item_id.as_str() == item)
                .cloned()
                .collect();
            let Some(projected) = projection_view(&conn, item) else {
                prop_assert!(item_events.is_empty(), "{} has events but no row", item);
                continue;
            };

            let state = WorkItemState::from_events(&item_events);
            prop_assert_eq!(&state_view(&state), &projected, "from_events of {}", item);
            prop_assert_eq!(link_views_agree(&state), Ok(()), "link views of {}", item);

            let snapshot = compact_item(item, &item_events, "compactor", &HashSet::<String>::new())
                .expect("compact item");
            let payload = extract_snapshot_payload(&snapshot).expect("snapshot payload");
            let restored = WorkItemState::from_snapshot_payload(&payload);
            prop_assert_eq!(&state_view(&restored), &projected, "snapshot of {}", item);
            prop_assert_eq!(link_views_agree(&restored), Ok(()), "snapshot link views of {}", item);

            // The compacted log: the other items' events, and this item's
            // snapshot in place of its events.
            let compacted: Vec<Event> = events
                .iter()
                .filter(|e| e.item_id.as_str() != item)
                .cloned()
                .chain(std::iter::once(snapshot.clone()))
                .collect();
            let compacted_conn = project(&compacted);
            let mut from_snapshot =
                projection_view(&compacted_conn, item).expect("snapshot projects its item");
            let mut expected = projected;
            for view in [&mut from_snapshot, &mut expected] {
                view.comments.clear();
                view.updated_at = 0;
            }
            prop_assert_eq!(&from_snapshot, &expected, "projected snapshot of {}", item);
        }
    }
}

proptest! {
    #![proptest_config(proptest_config())]

    /// After merge of states built from any subsets of an event set, and
    /// after a snapshot round trip of the merge, `blocked_by` and
    /// `related_to` are still the views of `links` (bn-t37g).
    #[test]
    fn merged_link_views_match_links(
        writes in prop_oneof![arb_writes(), arb_link_writes()],
        masks in any::<[u32; 3]>(),
    ) {
        let events = build_events(&writes);
        for item in ITEMS {
            let item_events: Vec<&Event> =
                events.iter().filter(|e| e.item_id.as_str() == item).collect();
            let subset = |mask: u32| {
                WorkItemState::from_events(
                    item_events
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| mask & (1 << (i % 32)) != 0)
                        .map(|(_, e)| *e),
                )
            };
            let mut merged = subset(masks[0]);
            merged.merge(&subset(masks[1]));
            merged.merge(&subset(masks[2]));
            prop_assert_eq!(link_views_agree(&merged), Ok(()), "merge of {}", item);

            let payload = merged.to_snapshot_payload(item, 0, 0, 0);
            let json = serde_json::to_value(&payload).expect("serialize payload");
            let payload = serde_json::from_value(json).expect("deserialize payload");
            let restored = WorkItemState::from_snapshot_payload(&payload);
            prop_assert_eq!(link_views_agree(&restored), Ok(()), "snapshot of {}", item);
        }
    }
}
