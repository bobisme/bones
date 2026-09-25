//! Projection order-independence (bn-1ugh).
//!
//! Spec: the `SQLite` projection of an event set must not depend on the order
//! of lines in the event log. Git union merges give two replicas the same
//! events in different orders (`ours` first, then `theirs`), so any
//! order-dependence makes replicas diverge permanently.
//!
//! The reference is the projection of the same events written in canonical
//! `(wall_ts_us, agent, event_hash)` order, the order replay and the event
//! merge driver already use. Two replicas that each append a branch of
//! concurrent events to a shared base must both match it, through a full
//! rebuild and through an incremental apply of the late-arriving branch.

use bones_core::db::incremental::incremental_apply;
use bones_core::db::rebuild::rebuild;
use bones_core::event::Event;
use bones_core::event::data::{
    AssignAction, AssignData, CommentData, CreateData, DeleteData, EventData, LinkData, MoveData,
    UnlinkData, UpdateData,
};
use bones_core::event::types::EventType;
use bones_core::event::writer::write_event;
use bones_core::model::item::{Kind, Size, State, Urgency};
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

const ITEMS: [&str; 2] = ["bn-a1", "bn-b2"];

fn proptest_config() -> Config {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(64);
    let mut config = Config::with_cases(cases);
    config.failure_persistence = None;
    if let Some(seed) = std::env::var("PROPTEST_SEED")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        config.rng_seed = RngSeed::Fixed(seed);
    }
    config
}

/// One mutation of an item, without its metadata.
#[derive(Debug, Clone)]
enum Op {
    Title(u8),
    Description(u8),
    Kind(Kind),
    Size(Size),
    Urgency(Urgency),
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
    /// Link or remove a link. A link always has a type. A removal without
    /// one removes every link type to the target.
    Link {
        link: bool,
        link_type: Option<u8>,
    },
    Comment(u8),
    Delete,
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0u8..4).prop_map(Op::Title),
        (0u8..4).prop_map(Op::Description),
        prop_oneof![Just(Kind::Task), Just(Kind::Goal), Just(Kind::Bug)].prop_map(Op::Kind),
        prop_oneof![Just(Size::S), Just(Size::M), Just(Size::L)].prop_map(Op::Size),
        prop_oneof![
            Just(Urgency::Urgent),
            Just(Urgency::Default),
            Just(Urgency::Punt)
        ]
        .prop_map(Op::Urgency),
        (any::<bool>(), 0u8..3).prop_map(|(add, label)| Op::Label { add, label }),
        (0u8..8).prop_map(Op::LabelsReplace),
        (any::<bool>(), 0u8..2).prop_map(|(assign, agent)| Op::Assign { assign, agent }),
        prop_oneof![Just(State::Open), Just(State::Doing), Just(State::Done)].prop_map(Op::Move),
        (any::<bool>(), prop::option::of(0u8..2)).prop_map(|(link, link_type)| Op::Link {
            link_type: if link {
                Some(link_type.unwrap_or(0))
            } else {
                link_type
            },
            link,
        }),
        (0u8..4).prop_map(Op::Comment),
        Just(Op::Delete),
    ]
}

/// A generated write: which item, what op, and its wall clock.
#[derive(Debug, Clone)]
struct Write {
    item: usize,
    op: Op,
    wall_ts: i64,
}

fn arb_branch() -> impl Strategy<Value = Vec<Write>> {
    prop::collection::vec(
        (0..ITEMS.len(), arb_op(), 100i64..110).prop_map(|(item, op, wall_ts)| Write {
            item,
            op,
            wall_ts,
        }),
        1..6,
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

fn build_event(write: &Write, agent: &str) -> Event {
    let item = ITEMS[write.item];
    let other = ITEMS[1 - write.item];
    let (event_type, data) = match &write.op {
        Op::Title(n) => update("title", serde_json::json!(format!("title {n}"))),
        Op::Description(n) => update("description", serde_json::json!(format!("desc {n}"))),
        Op::Kind(k) => update("kind", serde_json::json!(k.to_string())),
        Op::Size(s) => update("size", serde_json::json!(s.to_string())),
        Op::Urgency(u) => update("urgency", serde_json::json!(u.to_string())),
        Op::Label { add, label } => update(
            "labels",
            serde_json::json!({
                "action": if *add { "add" } else { "remove" },
                "label": format!("label-{label}"),
            }),
        ),
        Op::LabelsReplace(bits) => update(
            "labels",
            serde_json::json!(
                (0u8..3)
                    .filter(|i| bits & (1 << i) != 0)
                    .map(|i| format!("label-{i}"))
                    .collect::<Vec<_>>()
            ),
        ),
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
        Op::Link {
            link: true,
            link_type,
        } => (
            EventType::Link,
            EventData::Link(LinkData {
                target: other.to_string(),
                link_type: link_type_name(link_type.unwrap_or(0)),
                extra: BTreeMap::new(),
            }),
        ),
        Op::Link {
            link: false,
            link_type,
        } => (
            EventType::Unlink,
            EventData::Unlink(UnlinkData {
                target: other.to_string(),
                link_type: link_type.map(link_type_name),
                extra: BTreeMap::new(),
            }),
        ),
        Op::Comment(n) => (
            EventType::Comment,
            EventData::Comment(CommentData {
                body: format!("comment {n} by {agent}"),
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
    };
    event(event_type, item, data, write.wall_ts, agent)
}

fn link_type_name(n: u8) -> String {
    if n == 0 { "blocks" } else { "related_to" }.to_string()
}

fn event(event_type: EventType, item: &str, data: EventData, wall_ts: i64, agent: &str) -> Event {
    Event {
        wall_ts_us: wall_ts,
        agent: agent.to_string(),
        itc: "itc:AQ".to_string(),
        parents: vec![],
        event_type,
        item_id: ItemId::new_unchecked(item),
        data,
        event_hash: String::new(),
    }
}

fn base_events() -> Vec<Event> {
    ITEMS
        .iter()
        .enumerate()
        .map(|(i, item)| {
            event(
                EventType::Create,
                item,
                EventData::Create(CreateData {
                    title: format!("item {i}"),
                    kind: Kind::Task,
                    size: Some(Size::M),
                    urgency: Urgency::Default,
                    labels: vec!["label-0".to_string()],
                    parent: None,
                    causation: None,
                    description: Some("base".to_string()),
                    extra: BTreeMap::new(),
                }),
                10 + i64::try_from(i).expect("small index"),
                "agent-base",
            )
        })
        .collect()
}

/// Serialize events (assigning their hashes) into log lines.
fn lines(events: &mut [Event]) -> Vec<String> {
    events
        .iter_mut()
        .map(|e| write_event(e).expect("serialize event"))
        .collect()
}

struct Replica {
    _dir: TempDir,
    bones_dir: std::path::PathBuf,
}

impl Replica {
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(&bones_dir).expect("create .bones");
        ShardManager::new(&bones_dir).init().expect("init shards");
        Self {
            _dir: dir,
            bones_dir,
        }
    }

    fn append(&self, lines: &[String]) {
        let shards = ShardManager::new(&self.bones_dir);
        for line in lines {
            shards
                .append(line, false, Duration::from_secs(5))
                .expect("append line");
        }
    }

    fn events_dir(&self) -> std::path::PathBuf {
        self.bones_dir.join("events")
    }

    fn db_path(&self) -> std::path::PathBuf {
        self.bones_dir.join("bones.db")
    }

    fn rebuild(&self) -> Snapshot {
        rebuild(&self.events_dir(), &self.db_path()).expect("rebuild");
        snapshot(&self.db_path())
    }

    fn incremental(&self) -> Snapshot {
        incremental_apply(&self.events_dir(), &self.db_path(), false).expect("incremental apply");
        snapshot(&self.db_path())
    }
}

/// Everything a user can observe in the projection, in a stable order.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot(Vec<String>);

fn rows(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).expect("prepare snapshot query");
    let columns = stmt.column_count();
    stmt.query_map([], |row| {
        let mut parts = Vec::with_capacity(columns);
        for i in 0..columns {
            let value: rusqlite::types::Value = row.get(i)?;
            parts.push(format!("{value:?}"));
        }
        Ok(parts.join(" | "))
    })
    .expect("run snapshot query")
    .collect::<Result<_, _>>()
    .expect("read snapshot rows")
}

fn snapshot(db_path: &Path) -> Snapshot {
    let conn = Connection::open(db_path).expect("open projection");
    let mut out = Vec::new();
    for (name, sql) in [
        (
            "items",
            "SELECT item_id, title, description, kind, state, urgency, size, is_deleted, \
             search_labels, created_at_us, updated_at_us FROM items ORDER BY item_id",
        ),
        (
            "labels",
            "SELECT item_id, label, created_at_us FROM item_labels ORDER BY item_id, label",
        ),
        (
            "assignees",
            "SELECT item_id, agent, created_at_us FROM item_assignees ORDER BY item_id, agent",
        ),
        (
            "deps",
            "SELECT item_id, depends_on_item_id, link_type, created_at_us FROM item_dependencies \
             ORDER BY item_id, depends_on_item_id, link_type",
        ),
        (
            "comments",
            "SELECT item_id, event_hash, author, body, created_at_us FROM item_comments \
             ORDER BY event_hash",
        ),
    ] {
        out.extend(rows(&conn, sql).into_iter().map(|r| format!("{name}: {r}")));
    }
    Snapshot(out)
}

fn canonical_key(e: &Event) -> (i64, String, String) {
    (e.wall_ts_us, e.agent.clone(), e.event_hash.clone())
}

proptest! {
    #![proptest_config(proptest_config())]

    /// Two replicas holding the same events in union-merge orders agree with
    /// each other and with the canonical-order projection, through a full
    /// rebuild and through an incremental apply of the other branch.
    #[test]
    fn projection_is_independent_of_log_order(a in arb_branch(), b in arb_branch()) {
        let mut base = base_events();
        let mut branch_a: Vec<Event> = a.iter().map(|w| build_event(w, "agent-a")).collect();
        let mut branch_b: Vec<Event> = b.iter().map(|w| build_event(w, "agent-b")).collect();
        let base_lines = lines(&mut base);
        let a_lines = lines(&mut branch_a);
        let b_lines = lines(&mut branch_b);

        // Reference: every event in canonical order.
        let mut all: Vec<(Event, String)> = base
            .iter()
            .cloned()
            .zip(base_lines.iter().cloned())
            .chain(branch_a.iter().cloned().zip(a_lines.iter().cloned()))
            .chain(branch_b.iter().cloned().zip(b_lines.iter().cloned()))
            .collect();
        all.sort_by_key(|(e, _)| canonical_key(e));
        let reference_replica = Replica::new();
        reference_replica.append(&all.into_iter().map(|(_, l)| l).collect::<Vec<_>>());
        let reference = reference_replica.rebuild();

        // Replica A: base, own branch, then pulls B (union: ours first).
        let replica_a = Replica::new();
        replica_a.append(&base_lines);
        replica_a.append(&a_lines);
        replica_a.rebuild();
        replica_a.append(&b_lines);
        prop_assert_eq!(&replica_a.incremental(), &reference, "replica A incremental");
        prop_assert_eq!(&replica_a.rebuild(), &reference, "replica A rebuild");

        // Replica B: base, own branch, then pulls A.
        let replica_b = Replica::new();
        replica_b.append(&base_lines);
        replica_b.append(&b_lines);
        replica_b.rebuild();
        replica_b.append(&a_lines);
        prop_assert_eq!(&replica_b.incremental(), &reference, "replica B incremental");
        prop_assert_eq!(&replica_b.rebuild(), &reference, "replica B rebuild");
    }
}
