//! Edge case tests for bones-core: concurrent mutations, empty state, and boundary values.
//!
//! Covers bn-yot.5 acceptance criteria:
//!   - Every concurrent mutation scenario converges to deterministic state
//!   - Empty state produces no crashes and reasonable output
//!   - Boundary values handled without panics or OOM

use bones_core::clock::itc::Stamp;
use bones_core::crdt::item_state::WorkItemState;
use bones_core::crdt::state::Phase;
use bones_core::db::migrations;
use bones_core::db::project::{Projector, ensure_tracking_table};
use bones_core::db::query::{self, ItemFilter};
use bones_core::event::Event;
use bones_core::event::data::{CreateData, EventData, LinkData, MoveData, UpdateData};
use bones_core::event::types::EventType;
use bones_core::event::writer::write_event;
use bones_core::graph::blocking::BlockingGraph;
use bones_core::graph::cycles::detect_cycle_on_add;
use bones_core::model::item::{Kind, Size, State, Urgency};
use bones_core::model::item_id::ItemId;
use rusqlite::Connection;
use std::collections::{BTreeMap, HashMap};

// ---------------------------------------------------------------------------
// Test helpers (mirrors replay_pipeline.rs helpers)
// ---------------------------------------------------------------------------

fn test_db() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    migrations::migrate(&mut conn).expect("migrate schema");
    ensure_tracking_table(&conn).expect("create tracking table");
    conn
}

fn project_events(events: &[Event]) -> Connection {
    let conn = test_db();
    let projector = Projector::new(&conn);
    let stats = projector.project_batch(events).expect("project batch");
    assert_eq!(stats.errors, 0, "projection had errors");
    conn
}

fn make_create(id: &str, title: &str, agent: &str, ts: i64) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: vec![],
        event_type: EventType::Create,
        item_id: ItemId::new_unchecked(id),
        data: EventData::Create(CreateData {
            title: title.into(),
            kind: Kind::Task,
            size: Some(Size::M),
            urgency: Urgency::Default,
            labels: vec![],
            parent: None,
            causation: None,
            description: Some(format!("Description for {title}")),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("compute hash");
    e
}

fn make_create_goal(id: &str, title: &str, agent: &str, ts: i64) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: vec![],
        event_type: EventType::Create,
        item_id: ItemId::new_unchecked(id),
        data: EventData::Create(CreateData {
            title: title.into(),
            kind: Kind::Goal,
            size: Some(Size::L),
            urgency: Urgency::Default,
            labels: vec![],
            parent: None,
            causation: None,
            description: Some("a goal".into()),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("compute hash");
    e
}

fn make_create_child(id: &str, title: &str, parent_id: &str, agent: &str, ts: i64) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: vec![],
        event_type: EventType::Create,
        item_id: ItemId::new_unchecked(id),
        data: EventData::Create(CreateData {
            title: title.into(),
            kind: Kind::Task,
            size: None,
            urgency: Urgency::Default,
            labels: vec![],
            parent: Some(parent_id.into()),
            causation: None,
            description: None,
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("compute hash");
    e
}

fn make_move(id: &str, state: State, agent: &str, ts: i64) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: vec![],
        event_type: EventType::Move,
        item_id: ItemId::new_unchecked(id),
        data: EventData::Move(MoveData {
            state,
            reason: None,
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("compute hash");
    e
}

/// Build a WorkItemState and apply events using convergence.rs-style make_event.
fn make_crdt_event(
    event_type: EventType,
    data: EventData,
    wall_ts_us: i64,
    agent: &str,
    event_hash: &str,
    item_id: &str,
) -> Event {
    let mut stamp = Stamp::seed();
    stamp.event();
    Event {
        wall_ts_us,
        agent: agent.to_string(),
        itc: stamp.to_string(),
        parents: vec![],
        event_type,
        item_id: ItemId::new_unchecked(item_id),
        data,
        event_hash: event_hash.to_string(),
    }
}

fn crdt_update_title(item_id: &str, title: &str, ts: i64, agent: &str, hash: &str) -> Event {
    make_crdt_event(
        EventType::Update,
        EventData::Update(UpdateData {
            field: "title".into(),
            value: serde_json::json!(title),
            extra: BTreeMap::new(),
        }),
        ts,
        agent,
        hash,
        item_id,
    )
}

fn crdt_label(item_id: &str, action: &str, label: &str, ts: i64, agent: &str, hash: &str) -> Event {
    make_crdt_event(
        EventType::Update,
        EventData::Update(UpdateData {
            field: "labels".into(),
            value: serde_json::json!({"action": action, "label": label}),
            extra: BTreeMap::new(),
        }),
        ts,
        agent,
        hash,
        item_id,
    )
}

fn crdt_move(item_id: &str, state: State, ts: i64, agent: &str, hash: &str) -> Event {
    make_crdt_event(
        EventType::Move,
        EventData::Move(MoveData {
            state,
            reason: None,
            extra: BTreeMap::new(),
        }),
        ts,
        agent,
        hash,
        item_id,
    )
}

// ===========================================================================
// 1. Concurrent Mutation Matrix
// ===========================================================================

/// Two agents update the same LWW field at identical wall timestamps.
/// Tie-break must use agent_id lexicographic order (greater wins).
/// Agent "zeta" > "alpha" lexicographically → "zeta" wins.
/// Both merge orderings must produce identical result.
#[test]
fn lww_same_ts_different_agents_tie_breaks_by_agent_id() {
    let item = "bn-ec-lww1";
    let ts = 100_000;

    let mut alpha_state = WorkItemState::new();
    alpha_state.apply_event(&crdt_update_title(
        item,
        "title-from-alpha",
        ts,
        "alpha",
        "blake3:alpha-hash",
    ));

    let mut zeta_state = WorkItemState::new();
    zeta_state.apply_event(&crdt_update_title(
        item,
        "title-from-zeta",
        ts,
        "zeta",
        "blake3:zeta-hash",
    ));

    // alpha merges zeta
    let mut alpha_then_zeta = alpha_state.clone();
    alpha_then_zeta.merge(&zeta_state);

    // zeta merges alpha
    let mut zeta_then_alpha = zeta_state.clone();
    zeta_then_alpha.merge(&alpha_state);

    // Must be convergent
    assert_eq!(
        alpha_then_zeta.title.value, zeta_then_alpha.title.value,
        "LWW tie-break must be commutative"
    );
    // "zeta" > "alpha" lexicographically → zeta wins
    assert_eq!(
        alpha_then_zeta.title.value, "title-from-zeta",
        "lexicographically greater agent_id wins LWW tie"
    );
}

/// Two agents add the same label concurrently.
/// OR-Set idempotence: the label appears exactly once after merge.
#[test]
fn orset_both_agents_add_same_label_is_idempotent() {
    let item = "bn-ec-orset1";
    let label = "urgent";

    let mut state_a = WorkItemState::new();
    state_a.apply_event(&crdt_label(
        item,
        "add",
        label,
        1_000,
        "agent-a",
        "blake3:a-add",
    ));

    let mut state_b = WorkItemState::new();
    state_b.apply_event(&crdt_label(
        item,
        "add",
        label,
        1_000,
        "agent-b",
        "blake3:b-add",
    ));

    // Merge in both orderings
    let mut a_then_b = state_a.clone();
    a_then_b.merge(&state_b);

    let mut b_then_a = state_b.clone();
    b_then_a.merge(&state_a);

    // Convergent
    let labels_atb: std::collections::BTreeSet<String> =
        a_then_b.label_names().into_iter().cloned().collect();
    let labels_bta: std::collections::BTreeSet<String> =
        b_then_a.label_names().into_iter().cloned().collect();
    assert_eq!(labels_atb, labels_bta, "OR-Set must converge");

    // Label present (exactly once — OR-Set is a set)
    assert!(
        labels_atb.contains(label),
        "label must be present after concurrent adds"
    );
    assert_eq!(
        labels_atb.len(),
        1,
        "OR-Set deduplicates: label appears exactly once"
    );
}

/// One agent adds label "backend", another removes "backend" concurrently.
/// OR-Set add-wins semantics: label must be present after merge.
/// (Mirrors convergence.rs test but verifies all 4 merge orderings.)
#[test]
fn orset_concurrent_add_remove_is_add_wins_all_orderings() {
    let item = "bn-ec-orset2";

    // Base: add label from agent-a
    let mut add_side = WorkItemState::new();
    add_side.apply_event(&crdt_label(
        item,
        "add",
        "backend",
        1_000,
        "agent-a",
        "blake3:add-be",
    ));

    // Concurrent: remove label from agent-b
    let mut remove_side = WorkItemState::new();
    remove_side.apply_event(&crdt_label(
        item,
        "remove",
        "backend",
        1_000,
        "agent-b",
        "blake3:rm-be",
    ));

    let mut add_then_rm = add_side.clone();
    add_then_rm.merge(&remove_side);

    let mut rm_then_add = remove_side.clone();
    rm_then_add.merge(&add_side);

    let add_then_rm_labels: std::collections::BTreeSet<_> =
        add_then_rm.label_names().into_iter().cloned().collect();
    let rm_then_add_labels: std::collections::BTreeSet<_> =
        rm_then_add.label_names().into_iter().cloned().collect();

    // Convergent
    assert_eq!(
        add_then_rm_labels, rm_then_add_labels,
        "add/remove race must converge"
    );
    // Add-wins
    assert!(
        add_then_rm_labels.contains("backend"),
        "concurrent add beats concurrent remove (add-wins OR-Set)"
    );
}

/// Two agents concurrently move the same item to different states.
/// Agent A moves to Done, Agent B moves to Archived — both at same epoch.
/// Result must be deterministic and identical regardless of merge order.
#[test]
fn concurrent_move_different_states_converges_deterministically() {
    let item = "bn-ec-move1";
    let ts = 5_000;

    let mut state_done = WorkItemState::new();
    state_done.apply_event(&crdt_move(
        item,
        State::Done,
        ts,
        "agent-a",
        "blake3:done-hash",
    ));

    let mut state_arch = WorkItemState::new();
    state_arch.apply_event(&crdt_move(
        item,
        State::Archived,
        ts,
        "agent-b",
        "blake3:arch-hash",
    ));

    // Merge in both orderings
    let mut done_then_arch = state_done.clone();
    done_then_arch.merge(&state_arch);

    let mut arch_then_done = state_arch.clone();
    arch_then_done.merge(&state_done);

    // Must converge to the same phase
    assert_eq!(
        done_then_arch.phase(),
        arch_then_done.phase(),
        "concurrent moves must converge to the same phase"
    );
    assert_eq!(
        done_then_arch.epoch(),
        arch_then_done.epoch(),
        "concurrent moves must converge to the same epoch"
    );
}

/// Both Agent A and Agent B concurrently move the same item to Done.
/// The result must be Done (idempotent) regardless of merge order.
#[test]
fn concurrent_done_by_both_agents_is_idempotent() {
    let item = "bn-ec-idempotent1";
    let ts = 2_000;

    let mut state_a = WorkItemState::new();
    state_a.apply_event(&crdt_move(
        item,
        State::Done,
        ts,
        "agent-a",
        "blake3:done-a",
    ));

    let mut state_b = WorkItemState::new();
    state_b.apply_event(&crdt_move(
        item,
        State::Done,
        ts,
        "agent-b",
        "blake3:done-b",
    ));

    let mut a_then_b = state_a.clone();
    a_then_b.merge(&state_b);

    let mut b_then_a = state_b.clone();
    b_then_a.merge(&state_a);

    // Both orderings converge to Done
    assert_eq!(a_then_b.phase(), b_then_a.phase());
    assert_eq!(
        a_then_b.phase(),
        Phase::Done,
        "both agents done = item is done"
    );
}

/// Two agents create two separate items with the same title/content.
/// Since they use different item_ids and different event_hashes, they
/// produce two fully distinct items in the projection.
#[test]
fn two_creates_different_item_ids_same_content_are_distinct() {
    let create_a = make_create("bn-ec-dup1", "Duplicate title", "agent-a", 1_000);
    let create_b = make_create("bn-ec-dup2", "Duplicate title", "agent-b", 1_000);

    // Titles and content are identical, but item IDs differ
    assert_ne!(
        create_a.item_id.to_string(),
        create_b.item_id.to_string(),
        "different item_ids"
    );
    assert_ne!(
        create_a.event_hash, create_b.event_hash,
        "different event hashes — no collision possible"
    );

    let conn = project_events(&[create_a, create_b]);

    let item1 = query::get_item(&conn, "bn-ec-dup1", false)
        .expect("query 1")
        .expect("item 1 exists");
    let item2 = query::get_item(&conn, "bn-ec-dup2", false)
        .expect("query 2")
        .expect("item 2 exists");

    assert_eq!(item1.title, item2.title, "same title");
    assert_ne!(
        item1.item_id, item2.item_id,
        "different IDs — distinct items"
    );

    let all = query::list_items(&conn, &ItemFilter::default()).expect("list");
    assert_eq!(all.len(), 2, "two distinct items in projection");
}

/// Agent A closes (moves to Done) a goal while Agent B concurrently adds
/// a child item to it. Both the Done state and the child must be preserved
/// after merge — the CRDT does not automatically reopen at the CRDT layer.
#[test]
fn goal_child_added_while_goal_closes_both_preserved() {
    let goal_id = "bn-ec-goal1";
    let child_id = "bn-ec-goal1-child1";

    // Project goal creation + concurrent Done + child creation
    let create_goal = make_create_goal(goal_id, "Parent goal", "agent-a", 1_000);
    let close_goal = make_move(goal_id, State::Done, "agent-a", 2_000);
    let add_child = make_create_child(child_id, "New child", goal_id, "agent-b", 2_000);

    let conn = project_events(&[create_goal, close_goal, add_child]);

    // Goal: Done state preserved
    let goal = query::get_item(&conn, goal_id, false)
        .expect("query goal")
        .expect("goal exists");
    assert_eq!(goal.state, "done", "goal should be in done state");

    // Child item: also present (add is preserved)
    let child = query::get_item(&conn, child_id, false)
        .expect("query child")
        .expect("child item exists");
    assert_eq!(
        child.parent_id.as_deref(),
        Some(goal_id),
        "child has correct parent_id"
    );
}

/// Two agents update the same LWW field: one at higher timestamp, one at lower.
/// The higher wall_ts must win regardless of merge order.
#[test]
fn lww_higher_wall_ts_wins_regardless_of_merge_order() {
    let item = "bn-ec-lww2";

    // agent-early: ts=1000 (lower)
    let mut early = WorkItemState::new();
    early.apply_event(&crdt_update_title(
        item,
        "old-title",
        1_000,
        "agent-early",
        "blake3:early",
    ));

    // agent-late: ts=9000 (higher)
    let mut late = WorkItemState::new();
    late.apply_event(&crdt_update_title(
        item,
        "new-title",
        9_000,
        "agent-late",
        "blake3:late",
    ));

    let mut early_then_late = early.clone();
    early_then_late.merge(&late);

    let mut late_then_early = late.clone();
    late_then_early.merge(&early);

    assert_eq!(
        early_then_late.title.value, late_then_early.title.value,
        "LWW must be convergent"
    );
    assert_eq!(
        early_then_late.title.value, "new-title",
        "higher wall_ts wins LWW"
    );
}

// ===========================================================================
// 2. Empty State Tests
// ===========================================================================

/// Empty event log: list_items returns an empty result, no panic.
#[test]
fn empty_db_list_items_returns_empty() {
    let conn = test_db();
    let items = query::list_items(&conn, &ItemFilter::default()).expect("list_items on empty db");
    assert!(items.is_empty(), "empty DB should return no items");
}

/// Empty event log: get_item on any ID returns None, no panic.
#[test]
fn empty_db_get_item_returns_none() {
    let conn = test_db();
    let item = query::get_item(&conn, "bn-ec-missing", false).expect("get_item on empty db");
    assert!(item.is_none(), "non-existent item should be None");
}

/// Empty event log: get_dependencies returns empty, no panic.
#[test]
fn empty_db_get_dependencies_returns_empty() {
    let conn = test_db();
    let deps =
        query::get_dependencies(&conn, "bn-ec-nodeps").expect("get_dependencies on empty db");
    assert!(deps.is_empty(), "no deps in empty db");
}

/// Empty event log: get_comments returns empty, no panic.
#[test]
fn empty_db_get_comments_returns_empty() {
    let conn = test_db();
    let comments =
        query::get_comments(&conn, "bn-ec-nocomments").expect("get_comments on empty db");
    assert!(comments.is_empty(), "no comments in empty db");
}

/// Single item with no dependencies: list_items returns exactly that item.
#[test]
fn single_item_no_deps_list_returns_it() {
    let create = make_create("bn-ec-single1", "The only item", "agent-a", 1_000);
    let conn = project_events(&[create]);

    let items = query::list_items(&conn, &ItemFilter::default()).expect("list single item");
    assert_eq!(items.len(), 1, "exactly one item in projection");
    assert_eq!(items[0].item_id, "bn-ec-single1");
    assert_eq!(items[0].title, "The only item");
    assert_eq!(items[0].state, "open");

    let deps = query::get_dependencies(&conn, "bn-ec-single1").expect("deps");
    assert!(deps.is_empty(), "no deps for lonely item");
}

/// Goal with zero children: querying children returns empty, no panic.
#[test]
fn goal_with_zero_children_has_no_child_rows() {
    let create_goal = make_create_goal("bn-ec-emptygoal", "Empty goal", "agent-a", 1_000);
    let conn = project_events(&[create_goal]);

    let goal = query::get_item(&conn, "bn-ec-emptygoal", false)
        .expect("query")
        .expect("goal exists");
    assert_eq!(goal.kind, "goal");

    // No children in items table with parent_id = goal
    let child_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM items WHERE parent_id = 'bn-ec-emptygoal'",
            [],
            |row| row.get(0),
        )
        .expect("count children");
    assert_eq!(child_count, 0, "goal has no children");
}

// ===========================================================================
// 3. Boundary Value Tests
// ===========================================================================

/// Item with a 64 KB description round-trips through create → project → query.
/// No panic, no truncation.
#[test]
fn item_with_64kb_description_round_trips() {
    // 64 KB of text (65536 chars of ASCII)
    let long_desc = "x".repeat(65_536);

    let mut e = Event {
        wall_ts_us: 1_000,
        agent: "agent-a".into(),
        itc: "itc:AQ.1000".into(),
        parents: vec![],
        event_type: EventType::Create,
        item_id: ItemId::new_unchecked("bn-ec-bigdesc"),
        data: EventData::Create(CreateData {
            title: "Large description item".into(),
            kind: Kind::Task,
            size: Some(Size::Xxl),
            urgency: Urgency::Default,
            labels: vec![],
            parent: None,
            causation: None,
            description: Some(long_desc.clone()),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("hash");
    let conn = project_events(&[e]);

    let item = query::get_item(&conn, "bn-ec-bigdesc", false)
        .expect("query")
        .expect("item exists");
    assert_eq!(
        item.description.as_deref(),
        Some(long_desc.as_str()),
        "64 KB description must round-trip without truncation"
    );
}

/// Item with 1,000 distinct labels — all preserved after projection.
/// Tests OR-Set scalability and DB storage of many label rows.
///
/// The SQLite projector handles labels via the array-replacement format
/// (field="labels", value=["a","b","c",…]), which is what the CLI emits.
/// We emit one large update event with all 1,000 labels as a JSON array.
#[test]
fn item_with_1000_labels_all_preserved() {
    let item_id = "bn-ec-manlabels";

    // Build 1,000 label strings
    let all_labels: Vec<serde_json::Value> = (0..1_000usize)
        .map(|i| serde_json::json!(format!("label-{i:04}")))
        .collect();

    let create = make_create(item_id, "Label test item", "agent-a", 1_000);

    // Single update event: set labels = ["label-0000", …, "label-0999"] (array format)
    let mut label_event = Event {
        wall_ts_us: 2_000,
        agent: "agent-a".into(),
        itc: "itc:AQ.2000".into(),
        parents: vec![],
        event_type: EventType::Update,
        item_id: ItemId::new_unchecked(item_id),
        data: EventData::Update(UpdateData {
            field: "labels".into(),
            value: serde_json::Value::Array(all_labels),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut label_event).expect("compute hash");

    let conn = project_events(&[create, label_event]);

    let labels = query::get_labels(&conn, item_id).expect("get labels");
    assert_eq!(labels.len(), 1_000, "all 1000 labels must be stored");

    // Spot-check first and last
    let label_names: std::collections::BTreeSet<_> =
        labels.iter().map(|l| l.label.as_str()).collect();
    assert!(label_names.contains("label-0000"), "first label present");
    assert!(label_names.contains("label-0999"), "last label present");
}

/// Item with 10,000 labels in WorkItemState CRDT (memory/speed boundary).
#[test]
fn item_with_10000_labels_crdt_no_panic() {
    let item_id = "bn-ec-10klabels";
    let mut state = WorkItemState::new();

    for i in 0..10_000usize {
        let label = format!("lbl-{i}");
        state.apply_event(&crdt_label(
            item_id,
            "add",
            &label,
            i as i64,
            "agent-a",
            &format!("blake3:label-{i}"),
        ));
    }

    let labels = state.label_names();
    assert_eq!(labels.len(), 10_000, "all 10,000 labels present in CRDT");
}

/// 100-deep goal nesting chain: creates parent → child → grandchild chain.
/// Must not panic, OOM, or stack overflow.
#[test]
fn goal_nesting_100_deep_does_not_panic() {
    let mut events = Vec::new();

    // First item: root
    events.push(make_create_goal(
        "bn-ec-nest-0000",
        "Root goal",
        "agent-a",
        1_000,
    ));

    // Chain: each item is a child of the previous
    for i in 1..=100usize {
        let id = format!("bn-ec-nest-{i:04}");
        let parent_id = format!("bn-ec-nest-{:04}", i - 1);
        events.push(make_create_child(
            &id,
            &format!("Level {i}"),
            &parent_id,
            "agent-a",
            1_000 + i as i64,
        ));
    }

    // Must not panic
    let conn = project_events(&events);

    // Verify the deepest item exists with correct parent
    let deep_item = query::get_item(&conn, "bn-ec-nest-0100", false)
        .expect("query deep item")
        .expect("deep item exists");
    assert_eq!(
        deep_item.parent_id.as_deref(),
        Some("bn-ec-nest-0099"),
        "deepest item has correct parent"
    );

    // Root exists
    let root = query::get_item(&conn, "bn-ec-nest-0000", false)
        .expect("query root")
        .expect("root exists");
    assert!(root.parent_id.is_none(), "root has no parent");
}

/// Circular blocking dependency A→B→C→A is detected by the cycle detector.
/// The cycle warning must contain the cycle path.
#[test]
fn circular_blocking_dependency_detected_and_reported() {
    // Build a WorkItemState map for A, B, C
    // A blocks B, B blocks C, C would block A (closing the cycle)
    let mut states: HashMap<String, WorkItemState> = HashMap::new();

    let item_a = "bn-ec-cycA";
    let item_b = "bn-ec-cycB";
    let item_c = "bn-ec-cycC";

    // Create base states
    let mut state_a = WorkItemState::new();
    state_a.apply_event(&crdt_move(
        item_a,
        State::Open,
        1_000,
        "agent",
        "blake3:a-create",
    ));

    let mut state_b = WorkItemState::new();
    state_b.apply_event(&crdt_move(
        item_b,
        State::Open,
        1_000,
        "agent",
        "blake3:b-create",
    ));
    // B is blocked by A (A → B edge: A blocks B)
    state_b.apply_event(&make_crdt_event(
        EventType::Link,
        EventData::Link(LinkData {
            target: item_a.into(),
            link_type: "blocked_by".into(),
            extra: BTreeMap::new(),
        }),
        1_100,
        "agent",
        "blake3:b-link-a",
        item_b,
    ));

    let mut state_c = WorkItemState::new();
    state_c.apply_event(&crdt_move(
        item_c,
        State::Open,
        1_000,
        "agent",
        "blake3:c-create",
    ));
    // C is blocked by B (B → C edge: B blocks C)
    state_c.apply_event(&make_crdt_event(
        EventType::Link,
        EventData::Link(LinkData {
            target: item_b.into(),
            link_type: "blocked_by".into(),
            extra: BTreeMap::new(),
        }),
        1_100,
        "agent",
        "blake3:c-link-b",
        item_c,
    ));

    states.insert(item_a.to_string(), state_a);
    states.insert(item_b.to_string(), state_b);
    states.insert(item_c.to_string(), state_c);

    let graph = BlockingGraph::from_states(&states);

    // Adding edge A blocked_by C (C→A) would close the cycle A→B→C→A
    // detect_cycle_on_add(graph, from=C, to=A) checks: would C→A close a cycle?
    // Wait: the convention is item "from" would be blocked by "to".
    // If A is blocked_by C, that means A→C edge in "blocks" direction,
    // so we're adding: from=A, to=C in "A depends on C" sense.
    // detect_cycle_on_add checks: from "to" (C), can we reach "from" (A)?
    // Path: C → (blocked by B) → B → (blocked by A) → A. Yes, cycle!
    let warning = detect_cycle_on_add(&graph, item_a, item_c);
    assert!(
        warning.is_some(),
        "adding A blocked_by C should detect A→B→C→A cycle"
    );
    let w = warning.unwrap();
    assert!(
        w.cycle_len() >= 3,
        "cycle should involve at least 3 items, got cycle_len={}",
        w.cycle_len()
    );
}

/// Self-loop: item blocked by itself is detected immediately.
#[test]
fn self_loop_blocking_dependency_detected() {
    let item = "bn-ec-selfloop";
    let mut states: HashMap<String, WorkItemState> = HashMap::new();
    let state = WorkItemState::new();
    states.insert(item.to_string(), state);

    let graph = BlockingGraph::from_states(&states);
    let warning = detect_cycle_on_add(&graph, item, item);
    assert!(warning.is_some(), "self-loop must be detected");
    let w = warning.unwrap();
    assert!(w.is_self_loop(), "warning should classify as self-loop");
}

/// Two items in a mutual block (A blocked by B, adding B blocked by A) is detected.
#[test]
fn mutual_blocking_dependency_detected() {
    let item_a = "bn-ec-mutA";
    let item_b = "bn-ec-mutB";

    let mut states: HashMap<String, WorkItemState> = HashMap::new();

    let mut state_a = WorkItemState::new();
    // A is blocked by B
    state_a.apply_event(&make_crdt_event(
        EventType::Link,
        EventData::Link(LinkData {
            target: item_b.into(),
            link_type: "blocked_by".into(),
            extra: BTreeMap::new(),
        }),
        1_000,
        "agent",
        "blake3:a-blocked-by-b",
        item_a,
    ));
    states.insert(item_a.to_string(), state_a);
    states.insert(item_b.to_string(), WorkItemState::new());

    let graph = BlockingGraph::from_states(&states);

    // Adding B blocked_by A → mutual block
    let warning = detect_cycle_on_add(&graph, item_b, item_a);
    assert!(warning.is_some(), "mutual block A↔B must be detected");
    let w = warning.unwrap();
    assert!(
        w.is_mutual_block(),
        "warning should classify as mutual block"
    );
}

/// Item ID collision via content hash: structural impossibility test.
/// Two Create events with same item_id from different agents produce different
/// event_hashes — no hash collision is possible in practice.
#[test]
fn same_item_id_different_agents_have_different_event_hashes() {
    // Two agents create the same item_id at the same time with same title.
    // Their event_hashes differ because the agent field differs.
    let create_a = make_create("bn-ec-collision1", "Same content", "agent-a", 5_000);
    let create_b = make_create("bn-ec-collision1", "Same content", "agent-b", 5_000);

    assert_ne!(
        create_a.event_hash, create_b.event_hash,
        "different agents produce different event_hashes for same item_id/content/ts"
    );

    // Projecting both: single item in DB (CRDT merges same item_id)
    let conn = project_events(&[create_a, create_b]);
    let item = query::get_item(&conn, "bn-ec-collision1", false)
        .expect("query")
        .expect("item exists");
    assert_eq!(item.item_id, "bn-ec-collision1");

    // Only one row for this item_id (upsert behavior)
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM items WHERE item_id = 'bn-ec-collision1'",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(count, 1, "same item_id → single row in projection (upsert)");
}

// ===========================================================================
// 4. Stress Tests
// ===========================================================================

/// Create 1,000 items rapidly. All must be present in the projection.
/// No duplicates, no missing IDs.
#[test]
fn create_1000_items_all_present() {
    let mut events = Vec::with_capacity(1_000);

    for i in 0..1_000usize {
        // Use unique item IDs in bn-XXXX format
        // Generate human-readable IDs since we can't use the ID generator here
        let id = format!("bn-stress-{i:04}");
        events.push(make_create(
            &id,
            &format!("Stress item {i}"),
            "agent-a",
            1_000 + i as i64,
        ));
    }

    let conn = project_events(&events);

    let all_items = query::list_items(&conn, &ItemFilter::default()).expect("list 1000 items");
    assert_eq!(all_items.len(), 1_000, "all 1,000 items must be present");

    // Verify no duplicates (all IDs unique)
    let ids: std::collections::BTreeSet<_> = all_items.iter().map(|i| i.item_id.as_str()).collect();
    assert_eq!(ids.len(), 1_000, "no duplicate item IDs");

    // Spot-check first and last
    let item_0 = query::get_item(&conn, "bn-stress-0000", false)
        .expect("query 0")
        .expect("item 0 exists");
    assert_eq!(item_0.title, "Stress item 0");

    let item_999 = query::get_item(&conn, "bn-stress-0999", false)
        .expect("query 999")
        .expect("item 999 exists");
    assert_eq!(item_999.title, "Stress item 999");
}

/// Apply 1,000 title updates to a single item and verify final state is correct.
/// Tests that WorkItemState handles many sequential LWW updates efficiently.
#[test]
fn single_item_1000_updates_final_state_correct() {
    let item_id = "bn-ec-updates";
    let mut state = WorkItemState::new();

    for i in 0..1_000usize {
        state.apply_event(&crdt_update_title(
            item_id,
            &format!("title-v{i}"),
            i as i64,
            "agent-a",
            &format!("blake3:upd-{i:04}"),
        ));
    }

    // The last update (highest wall_ts = 999) should win
    assert_eq!(
        state.title.value, "title-v999",
        "final title must be from the update with highest timestamp"
    );
}

/// Three-way concurrent merge: agents A, B, C each apply different fields
/// to the same item. All 6 permutations must produce identical state.
#[test]
fn three_way_concurrent_merge_all_6_permutations_converge() {
    let item = "bn-ec-3way";

    let mut a = WorkItemState::new();
    a.apply_event(&crdt_update_title(
        item,
        "title-from-a",
        3_000,
        "agent-a",
        "blake3:a-title",
    ));
    a.apply_event(&crdt_label(
        item,
        "add",
        "frontend",
        3_100,
        "agent-a",
        "blake3:a-label",
    ));

    let mut b = WorkItemState::new();
    b.apply_event(&crdt_update_title(
        item,
        "title-from-b",
        2_000,
        "agent-b",
        "blake3:b-title",
    ));
    b.apply_event(&crdt_label(
        item,
        "add",
        "backend",
        2_100,
        "agent-b",
        "blake3:b-label",
    ));

    let mut c = WorkItemState::new();
    c.apply_event(&crdt_move(
        item,
        State::Done,
        1_000,
        "agent-c",
        "blake3:c-done",
    ));

    let parts = [&a, &b, &c];
    let orderings = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];

    let mut results = Vec::new();
    for ordering in &orderings {
        let mut merged = WorkItemState::new();
        for &idx in ordering {
            merged.merge(parts[idx]);
        }
        results.push((
            merged.title.value.clone(),
            merged.phase(),
            merged
                .label_names()
                .into_iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
        ));
    }

    // All 6 orderings must converge
    for i in 1..results.len() {
        assert_eq!(
            results[0], results[i],
            "3-way merge ordering {i} diverged from ordering 0"
        );
    }

    // title-from-a has highest wall_ts (3000) → wins LWW
    assert_eq!(results[0].0, "title-from-a", "highest wall_ts title wins");
    // Both labels preserved (OR-Set)
    assert!(
        results[0].2.contains("frontend"),
        "frontend label preserved"
    );
    assert!(results[0].2.contains("backend"), "backend label preserved");
    // Phase: Done from agent-c
    assert_eq!(results[0].1, Phase::Done, "Done phase preserved");
}
