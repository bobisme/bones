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
//!
//! The generator covers every event type: creates that sort after updates of
//! their item (and items that only a branch creates), parent changes, link
//! targets without a row, odd update values, compacts, and snapshots built
//! from a subset of the earlier events of the item (bn-18fs). A snapshot's
//! `parents` are its sources, as `bn compact` writes them, so a redaction
//! of a source also redacts the snapshot (bn-1npc). Links to the item
//! itself or to a target that is no item ID, and labels with a NUL, are
//! in too (bn-1npc). Every event
//! must project without error, and the comparison covers every column that a
//! user can see.

use bones_core::crdt::item_state::WorkItemState;
use bones_core::db::incremental::incremental_apply;
use bones_core::db::rebuild::rebuild;
use bones_core::event::Event;
use bones_core::event::data::{
    AssignAction, AssignData, CommentData, CompactData, CreateData, DeleteData, EventData,
    LinkData, MoveData, RedactData, SnapshotData, UnlinkData, UpdateData,
};
use bones_core::event::types::EventType;
use bones_core::event::writer::write_event;
use bones_core::model::item::{Kind, Size, State, Urgency};
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestCaseError};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

/// Items the generator writes to. The base creates only the first
/// `BASE_ITEMS`: the others exist only through branch events, so their
/// creates (if any) can arrive after, and sort after, their updates.
const ITEMS: [&str; 3] = ["bn-a1", "bn-b2", "bn-c3"];
const BASE_ITEMS: usize = 2;

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
    /// Set the parent: an item (index into ITEMS, possibly the item
    /// itself), the empty string (3) or JSON null (4).
    Parent(u8),
    /// Link or remove a link. A link always has a type. A removal without
    /// one removes every link type to the target.
    Link {
        link: bool,
        link_type: Option<u8>,
        /// The target: one of the two other items (0, 1), the item itself
        /// (2) or a target that is no item ID (3).
        target: u8,
    },
    Comment(u8),
    /// A create of an item that may already exist, with varied fields.
    Create {
        title: u8,
        kind: Kind,
        /// Parent as for `Parent`, except that 3 and 4 both mean none.
        parent: u8,
        labels: u8,
        description: Option<u8>,
    },
    Compact(u8),
    /// An update with an odd value: a field of `RAW_FIELDS` and a value of
    /// `raw_value`. Replicas may write values the CLI never would.
    Raw {
        field: u8,
        value: u8,
    },
    /// Snapshot of the item built from the subset of earlier events of the
    /// item (base and own branch) whose index bit (mod 8) is set.
    Snapshot(u8),
    Delete,
    /// Redact an earlier event of the same branch or the base (index mod
    /// the number of earlier events).
    Redact(u8),
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        1 => (0u8..4).prop_map(Op::Title),
        1 => (0u8..4).prop_map(Op::Description),
        1 => prop_oneof![Just(Kind::Task), Just(Kind::Goal), Just(Kind::Bug)].prop_map(Op::Kind),
        1 => prop_oneof![Just(Size::S), Just(Size::M), Just(Size::L)].prop_map(Op::Size),
        1 => prop_oneof![
            Just(Urgency::Urgent),
            Just(Urgency::Default),
            Just(Urgency::Punt)
        ]
        .prop_map(Op::Urgency),
        1 => (any::<bool>(), 0u8..4).prop_map(|(add, label)| Op::Label { add, label }),
        1 => (0u8..8).prop_map(Op::LabelsReplace),
        1 => (any::<bool>(), 0u8..2).prop_map(|(assign, agent)| Op::Assign { assign, agent }),
        1 => prop_oneof![Just(State::Open), Just(State::Doing), Just(State::Done)].prop_map(Op::Move),
        1 => (0u8..5).prop_map(Op::Parent),
        1 => (any::<bool>(), prop::option::of(0u8..2), 0u8..4).prop_map(
            |(link, link_type, target)| Op::Link {
                link_type: if link {
                    Some(link_type.unwrap_or(0))
                } else {
                    link_type
                },
                link,
                target,
            }
        ),
        1 => (0u8..4).prop_map(Op::Comment),
        1 => (
            0u8..4,
            prop_oneof![Just(Kind::Task), Just(Kind::Goal), Just(Kind::Bug)],
            0u8..5,
            0u8..8,
            prop::option::of(0u8..3),
        )
            .prop_map(|(title, kind, parent, labels, description)| Op::Create {
                title,
                kind,
                parent,
                labels,
                description,
            }),
        1 => (0u8..3).prop_map(Op::Compact),
        // Weighted up: a snapshot's claims only tie with the claims of its
        // source events when both sit in the log.
        3 => (0u8..6, 0u8..4).prop_map(|(field, value)| Op::Raw { field, value }),
        3 => any::<u8>().prop_map(Op::Snapshot),
        1 => Just(Op::Delete),
        1 => (0u8..8).prop_map(Op::Redact),
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
        1..8,
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

/// Build a branch's events in order, assigning each hash as it goes so a
/// redaction can name an earlier event of the branch or the base.
fn build_branch(writes: &[Write], agent: &str, base: &[Event]) -> Vec<Event> {
    let mut prior: Vec<Event> = base.to_vec();
    let mut out = Vec::with_capacity(writes.len());
    for write in writes {
        let mut e = build_event(write, agent, &prior);
        write_event(&mut e).expect("serialize event");
        prior.push(e.clone());
        out.push(e);
    }
    out
}

fn build_event(write: &Write, agent: &str, prior: &[Event]) -> Event {
    if let Op::Redact(n) = write.op {
        let target = &prior[usize::from(n) % prior.len()];
        return event(
            EventType::Redact,
            // Any item: a redaction may sit on another item than its target.
            ITEMS[write.item],
            EventData::Redact(RedactData {
                target_hash: target.event_hash.clone(),
                reason: "secret".to_string(),
                extra: BTreeMap::new(),
            }),
            write.wall_ts,
            agent,
        );
    }
    let item = ITEMS[write.item];
    if let Op::Snapshot(keep) = write.op {
        return snapshot_event(item, keep, write.wall_ts, agent, prior);
    }
    let parent_of = |n: u8| ITEMS.get(usize::from(n)).map(|p| (*p).to_string());
    let (event_type, data) = match &write.op {
        Op::Title(n) => update("title", serde_json::json!(format!("title {n}"))),
        // 0 clears the description.
        Op::Description(0) => update("description", serde_json::json!("")),
        Op::Description(n) => update("description", serde_json::json!(format!("desc {n}"))),
        Op::Kind(k) => update("kind", serde_json::json!(k.to_string())),
        Op::Size(s) => update("size", serde_json::json!(s.to_string())),
        Op::Urgency(u) => update("urgency", serde_json::json!(u.to_string())),
        Op::Label { add, label } => update(
            "labels",
            serde_json::json!({
                "action": if *add { "add" } else { "remove" },
                // 3: a NUL inside, which SQLite's length() stops at.
                "label": if *label == 3 {
                    "x\u{0}y".to_string()
                } else {
                    format!("label-{label}")
                },
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
        Op::Parent(n) => update(
            "parent",
            match n {
                3 => serde_json::json!(""),
                4 => serde_json::Value::Null,
                _ => serde_json::json!(parent_of(*n)),
            },
        ),
        Op::Link {
            link: true,
            link_type,
            target,
        } => (
            EventType::Link,
            EventData::Link(LinkData {
                target: link_target(write.item, *target),
                link_type: link_type_name(link_type.unwrap_or(0)),
                extra: BTreeMap::new(),
            }),
        ),
        Op::Link {
            link: false,
            link_type,
            target,
        } => (
            EventType::Unlink,
            EventData::Unlink(UnlinkData {
                target: link_target(write.item, *target),
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
        Op::Create {
            title,
            kind,
            parent,
            labels,
            description,
        } => (
            EventType::Create,
            EventData::Create(CreateData {
                title: format!("created {title}"),
                kind: *kind,
                size: None,
                urgency: Urgency::Default,
                labels: (0u8..3)
                    .filter(|i| labels & (1 << i) != 0)
                    .map(|i| format!("label-{i}"))
                    .collect(),
                parent: parent_of(*parent),
                causation: None,
                description: description.map(|d| {
                    if d == 0 {
                        String::new()
                    } else {
                        format!("created desc {d}")
                    }
                }),
                extra: BTreeMap::new(),
            }),
        ),
        Op::Raw { field, value } => update(
            RAW_FIELDS[usize::from(*field) % RAW_FIELDS.len()],
            raw_value(*value),
        ),
        Op::Compact(n) => (
            EventType::Compact,
            EventData::Compact(CompactData {
                summary: format!("summary {n} by {agent}"),
                extra: BTreeMap::new(),
            }),
        ),
        Op::Redact(_) | Op::Snapshot(_) => unreachable!("handled above"),
    };
    event(event_type, item, data, write.wall_ts, agent)
}

const RAW_FIELDS: [&str; 6] = ["title", "description", "kind", "size", "urgency", "parent"];

fn raw_value(n: u8) -> serde_json::Value {
    match n % 4 {
        0 => serde_json::Value::Null,
        1 => serde_json::json!(""),
        2 => serde_json::json!("bogus"),
        _ => serde_json::json!(5),
    }
}

/// Link target `n` of `item`: see `Op::Link`.
fn link_target(item: usize, n: u8) -> String {
    match n {
        2 => ITEMS[item].to_string(),
        3 => "not an id".to_string(),
        _ => ITEMS[(item + 1 + usize::from(n % 2)) % ITEMS.len()].to_string(),
    }
}

/// A plausible `item.snapshot`: the lattice state of a subset of the
/// earlier events of the item, as `compact_item` builds it. The subset
/// models a replica that saw only some of the events.
fn snapshot_event(item: &str, keep: u8, wall_ts: i64, agent: &str, prior: &[Event]) -> Event {
    let sources: Vec<&Event> = prior
        .iter()
        .filter(|e| e.item_id.as_str() == item)
        .enumerate()
        .filter(|(i, _)| keep & (1 << (i % 8)) != 0)
        .map(|(_, e)| e)
        .collect();
    let earliest = sources.iter().map(|e| e.order_ts()).min().unwrap_or(0);
    let latest = sources.iter().map(|e| e.order_ts()).max().unwrap_or(0);
    let payload = WorkItemState::from_events(sources.iter().copied()).to_snapshot_payload(
        item,
        sources.len(),
        earliest,
        latest,
    );
    let mut snapshot = event(
        EventType::Snapshot,
        item,
        EventData::Snapshot(SnapshotData {
            state: serde_json::to_value(&payload).expect("serialize snapshot payload"),
            extra: BTreeMap::new(),
        }),
        wall_ts,
        agent,
    );
    // The sources, sorted and once each, as `compact_item` writes them.
    let mut parents: Vec<String> = sources.iter().map(|e| e.event_hash.clone()).collect();
    parents.sort();
    parents.dedup();
    snapshot.parents = parents;
    snapshot
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

/// The base creates. Their timestamps overlap the branches' range, so the
/// canonical order can put a branch's update or redaction before the create
/// of the item it touches.
fn base_events(create_ts: [i64; BASE_ITEMS]) -> Vec<Event> {
    ITEMS[..BASE_ITEMS]
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
                create_ts[i],
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

    /// Every generated event must project without error: a failed event
    /// is skipped, which hides what its handler would do.
    fn rebuild(&self) -> Snapshot {
        let report = rebuild(&self.events_dir(), &self.db_path()).expect("rebuild");
        assert_eq!(report.projection_errors, 0, "rebuild projection errors");
        snapshot(&self.db_path())
    }

    fn incremental(&self) -> Snapshot {
        let report = incremental_apply(&self.events_dir(), &self.db_path(), false)
            .expect("incremental apply");
        assert_eq!(report.projection_errors, 0, "incremental projection errors");
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
            "SELECT item_id, title, description, kind, state, urgency, size, parent_id, \
             compact_summary, snapshot_json, is_deleted, deleted_at_us, search_labels, \
             created_at_us, updated_at_us FROM items ORDER BY item_id",
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
        (
            "redactions",
            "SELECT target_event_hash, item_id, reason, redacted_by, redacted_at_us \
             FROM event_redactions ORDER BY target_event_hash",
        ),
    ] {
        out.extend(rows(&conn, sql).into_iter().map(|r| format!("{name}: {r}")));
    }
    Snapshot(out)
}

fn canonical_key(e: &Event) -> (i64, String, String) {
    (e.wall_ts_us, e.agent.clone(), e.event_hash.clone())
}

/// Two replicas holding the same events in union-merge orders agree with
/// each other and with the canonical-order projection, through a full
/// rebuild and through an incremental apply of the other branch.
fn check_orders(
    a: &[Write],
    b: &[Write],
    create_ts: [i64; BASE_ITEMS],
) -> Result<(), TestCaseError> {
    let mut base = base_events(create_ts);
    let base_lines = lines(&mut base);
    let mut branch_a = build_branch(a, "agent-a", &base);
    let mut branch_b = build_branch(b, "agent-b", &base);
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
    prop_assert_eq!(
        &replica_a.incremental(),
        &reference,
        "replica A incremental"
    );
    prop_assert_eq!(&replica_a.rebuild(), &reference, "replica A rebuild");

    // Replica B: base, own branch, then pulls A.
    let replica_b = Replica::new();
    replica_b.append(&base_lines);
    replica_b.append(&b_lines);
    replica_b.rebuild();
    replica_b.append(&a_lines);
    prop_assert_eq!(
        &replica_b.incremental(),
        &reference,
        "replica B incremental"
    );
    prop_assert_eq!(&replica_b.rebuild(), &reference, "replica B rebuild");
    Ok(())
}

proptest! {
    #![proptest_config(proptest_config())]

    #[test]
    fn projection_is_independent_of_log_order(
        a in arb_branch(),
        b in arb_branch(),
        create_ts in [95i64..110, 95i64..110],
    ) {
        check_orders(&a, &b, create_ts)?;
    }
}

// ---------------------------------------------------------------------------
// Regressions: minimal cases the property test found (bn-18fs)
// ---------------------------------------------------------------------------

fn w(item: usize, op: Op, wall_ts: i64) -> Write {
    Write { item, op, wall_ts }
}

fn check_fixed(a: &[Write], b: &[Write], create_ts: [i64; BASE_ITEMS]) {
    if let Err(err) = check_orders(a, b, create_ts) {
        panic!("{err}");
    }
}

/// A parent that has no row hit the `parent_id` foreign key. The rebuild
/// kept the value (its journal is off, so the failed statement is not
/// undone), the incremental apply dropped it.
#[test]
fn parent_without_a_row_projects_the_same_in_every_order() {
    check_fixed(
        &[w(0, Op::Title(0), 100)],
        &[w(
            0,
            Op::Create {
                title: 0,
                kind: Kind::Task,
                parent: 2,
                labels: 0,
                description: None,
            },
            102,
        )],
        [95, 95],
    );
}

/// An update to an invalid kind claimed the field, then failed the CHECK
/// constraint, and left different partial effects in each path.
#[test]
fn invalid_kind_update_projects_the_same_in_every_order() {
    check_fixed(
        &[w(0, Op::Title(0), 100)],
        &[w(1, Op::Raw { field: 2, value: 1 }, 100)],
        [95, 101],
    );
}

/// A link target's reference placeholder must exist whether or not the
/// link wins against a concurrent unlink.
#[test]
fn link_target_placeholder_does_not_depend_on_the_winner() {
    check_fixed(
        &[w(
            1,
            Op::Link {
                link: true,
                link_type: Some(0),
                target: 0,
            },
            100,
        )],
        &[w(
            1,
            Op::Link {
                link: false,
                link_type: None,
                target: 0,
            },
            100,
        )],
        [95, 95],
    );
}

/// A snapshot claims the description with the key of the update it
/// carries. The update wrote "" and the snapshot wrote NULL, so the first
/// of the two tied claims decided the value.
#[test]
fn cleared_description_and_its_snapshot_agree() {
    check_fixed(
        &[
            w(0, Op::Raw { field: 1, value: 1 }, 105),
            w(0, Op::Snapshot(2), 100),
        ],
        &[w(0, Op::Title(0), 100)],
        [95, 95],
    );
}

/// A redaction of a snapshot's source redacts the snapshot too, whether it
/// sorts before or after the snapshot (bn-1npc).
#[test]
fn redacted_snapshot_source_projects_the_same_in_every_order() {
    for redact_ts in [100, 109] {
        check_fixed(
            &[
                w(
                    0,
                    Op::Label {
                        add: true,
                        label: 1,
                    },
                    105,
                ),
                w(0, Op::Snapshot(0xff), 106),
                // prior: two base creates, the label, the snapshot.
                w(0, Op::Redact(2), redact_ts),
            ],
            &[w(0, Op::Title(0), 104)],
            [95, 95],
        );
    }
}
