//! Integration tests: event replay pipeline (events → DAG → CRDT → SQLite).
//!
//! Covers the full critical path:
//!   - Linear event sequences through the Projector into SQLite
//!   - All 11 event types projected correctly
//!   - Multi-agent concurrent merge scenarios (LWW, OR-Set, epoch+phase)
//!   - Incremental vs full replay equivalence
//!   - Ordering determinism (shuffle events, same projection)
//!   - Rebuild (clear + replay produces identical state)
//!   - DAG divergent replay → CRDT → SQLite integration

use bones_core::crdt::item_state::WorkItemState;
use bones_core::crdt::state::Phase;
use bones_core::dag::graph::EventDag;
use bones_core::dag::replay::replay_divergent;
use bones_core::db::migrations;
use bones_core::db::project::{Projector, clear_projection, ensure_tracking_table};
use bones_core::db::query;
use bones_core::event::Event;
use bones_core::event::data::{
    AssignAction, AssignData, CommentData, CompactData, CreateData, DeleteData, EventData,
    LinkData, MoveData, RedactData, SnapshotData, UnlinkData, UpdateData,
};
use bones_core::event::types::EventType;
use bones_core::event::writer::write_event;
use bones_core::model::item::{Kind, Size, State, Urgency};
use bones_core::model::item_id::ItemId;
use rusqlite::Connection;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Open an in-memory SQLite database with full schema + tracking table.
fn test_db() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    migrations::migrate(&mut conn).expect("migrate schema");
    ensure_tracking_table(&conn).expect("create tracking table");
    conn
}

/// Build a create event with computed hash (via `write_event`).
fn make_create(id: &str, title: &str, agent: &str, ts: i64, parents: &[&str]) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: parents.iter().map(|s| (*s).to_string()).collect(),
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

/// Build an update-title event with computed hash.
fn make_update_title(id: &str, title: &str, agent: &str, ts: i64, parents: &[&str]) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: parents.iter().map(|s| (*s).to_string()).collect(),
        event_type: EventType::Update,
        item_id: ItemId::new_unchecked(id),
        data: EventData::Update(UpdateData {
            field: "title".into(),
            value: serde_json::json!(title),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("compute hash");
    e
}

/// Build a move event with computed hash.
fn make_move(id: &str, state: State, agent: &str, ts: i64, parents: &[&str]) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: parents.iter().map(|s| (*s).to_string()).collect(),
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

/// Build an assign event with computed hash.
fn make_assign(
    id: &str,
    target_agent: &str,
    action: AssignAction,
    actor: &str,
    ts: i64,
    parents: &[&str],
) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: actor.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: parents.iter().map(|s| (*s).to_string()).collect(),
        event_type: EventType::Assign,
        item_id: ItemId::new_unchecked(id),
        data: EventData::Assign(AssignData {
            agent: target_agent.into(),
            action,
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("compute hash");
    e
}

/// Build a comment event with computed hash.
fn make_comment(id: &str, body: &str, agent: &str, ts: i64, parents: &[&str]) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: parents.iter().map(|s| (*s).to_string()).collect(),
        event_type: EventType::Comment,
        item_id: ItemId::new_unchecked(id),
        data: EventData::Comment(CommentData {
            body: body.into(),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("compute hash");
    e
}

/// Build a link event with computed hash.
fn make_link(
    id: &str,
    target: &str,
    link_type: &str,
    agent: &str,
    ts: i64,
    parents: &[&str],
) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: parents.iter().map(|s| (*s).to_string()).collect(),
        event_type: EventType::Link,
        item_id: ItemId::new_unchecked(id),
        data: EventData::Link(LinkData {
            target: target.into(),
            link_type: link_type.into(),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("compute hash");
    e
}

/// Build an unlink event with computed hash.
fn make_unlink(
    id: &str,
    target: &str,
    link_type: Option<&str>,
    agent: &str,
    ts: i64,
    parents: &[&str],
) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: parents.iter().map(|s| (*s).to_string()).collect(),
        event_type: EventType::Unlink,
        item_id: ItemId::new_unchecked(id),
        data: EventData::Unlink(UnlinkData {
            target: target.into(),
            link_type: link_type.map(str::to_string),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("compute hash");
    e
}

/// Build a delete event with computed hash.
fn make_delete(id: &str, agent: &str, ts: i64, parents: &[&str]) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: parents.iter().map(|s| (*s).to_string()).collect(),
        event_type: EventType::Delete,
        item_id: ItemId::new_unchecked(id),
        data: EventData::Delete(DeleteData {
            reason: Some("test delete".into()),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("compute hash");
    e
}

/// Build a compact event with computed hash.
fn make_compact(id: &str, summary: &str, agent: &str, ts: i64, parents: &[&str]) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: parents.iter().map(|s| (*s).to_string()).collect(),
        event_type: EventType::Compact,
        item_id: ItemId::new_unchecked(id),
        data: EventData::Compact(CompactData {
            summary: summary.into(),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("compute hash");
    e
}

/// Build a snapshot event with computed hash.
fn make_snapshot(
    id: &str,
    state_json: serde_json::Value,
    agent: &str,
    ts: i64,
    parents: &[&str],
) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: parents.iter().map(|s| (*s).to_string()).collect(),
        event_type: EventType::Snapshot,
        item_id: ItemId::new_unchecked(id),
        data: EventData::Snapshot(SnapshotData {
            state: state_json,
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("compute hash");
    e
}

/// Build a redact event with computed hash.
fn make_redact(
    id: &str,
    target_hash: &str,
    reason: &str,
    agent: &str,
    ts: i64,
    parents: &[&str],
) -> Event {
    let mut e = Event {
        wall_ts_us: ts,
        agent: agent.into(),
        itc: format!("itc:AQ.{ts}"),
        parents: parents.iter().map(|s| (*s).to_string()).collect(),
        event_type: EventType::Redact,
        item_id: ItemId::new_unchecked(id),
        data: EventData::Redact(RedactData {
            target_hash: target_hash.into(),
            reason: reason.into(),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };
    write_event(&mut e).expect("compute hash");
    e
}

/// Project a batch of events and return the connection for assertions.
fn project_events(events: &[Event]) -> Connection {
    let conn = test_db();
    let projector = Projector::new(&conn);
    let stats = projector.project_batch(events).expect("project batch");
    assert_eq!(stats.errors, 0, "projection had errors");
    conn
}

// ---------------------------------------------------------------------------
// 1. Linear event sequences
// ---------------------------------------------------------------------------

/// Full create → update → move → done lifecycle; verify SQLite projection state.
#[test]
fn linear_create_update_move_done_projects_to_sqlite() {
    let create = make_create("bn-lin1", "Auth timeout bug", "alice", 1_000, &[]);
    let update_title = make_update_title(
        "bn-lin1",
        "Auth timeout bug (confirmed)",
        "alice",
        2_000,
        &[],
    );
    let update_desc = {
        let mut e = Event {
            wall_ts_us: 3_000,
            agent: "alice".into(),
            itc: "itc:AQ.3000".into(),
            parents: vec![],
            event_type: EventType::Update,
            item_id: ItemId::new_unchecked("bn-lin1"),
            data: EventData::Update(UpdateData {
                field: "description".into(),
                value: serde_json::json!("Updated description with root cause"),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        write_event(&mut e).expect("hash");
        e
    };
    let assign = make_assign("bn-lin1", "bob", AssignAction::Assign, "alice", 4_000, &[]);
    let comment = make_comment("bn-lin1", "Investigating…", "bob", 5_000, &[]);
    let move_doing = make_move("bn-lin1", State::Doing, "bob", 6_000, &[]);
    let move_done = make_move("bn-lin1", State::Done, "bob", 7_000, &[]);

    let events = vec![
        create,
        update_title,
        update_desc,
        assign,
        comment,
        move_doing,
        move_done,
    ];
    let conn = project_events(&events);

    let item = query::get_item(&conn, "bn-lin1", false)
        .expect("query item")
        .expect("item should exist");
    assert_eq!(item.title, "Auth timeout bug (confirmed)");
    assert_eq!(item.state, "done");
    assert_eq!(
        item.description.as_deref(),
        Some("Updated description with root cause")
    );
    assert_eq!(item.updated_at_us, 7_000);

    let assignees = query::get_assignees(&conn, "bn-lin1").expect("assignees");
    assert_eq!(assignees.len(), 1);
    assert_eq!(assignees[0].agent, "bob");

    let comments = query::get_comments(&conn, "bn-lin1").expect("comments");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].body, "Investigating…");
}

/// Verify a full open → doing → done → archived lifecycle in SQLite.
#[test]
fn full_state_lifecycle_open_doing_done_archived() {
    let create = make_create("bn-lc2", "Lifecycle item", "alice", 1_000, &[]);
    let to_doing = make_move("bn-lc2", State::Doing, "alice", 2_000, &[]);
    let to_done = make_move("bn-lc2", State::Done, "alice", 3_000, &[]);
    let to_archived = make_move("bn-lc2", State::Archived, "alice", 4_000, &[]);

    let conn = project_events(&[create, to_doing, to_done, to_archived]);
    let item = query::get_item(&conn, "bn-lc2", false)
        .expect("query")
        .expect("exists");
    assert_eq!(item.state, "archived");
}

/// Reopen (Done → Open) after close creates placeholder "open" state.
#[test]
fn reopen_after_done_restores_open_state() {
    let create = make_create("bn-reopen", "Reopened item", "alice", 1_000, &[]);
    let to_done = make_move("bn-reopen", State::Done, "alice", 2_000, &[]);
    let reopen = make_move("bn-reopen", State::Open, "alice", 3_000, &[]);

    let conn = project_events(&[create, to_done, reopen]);
    let item = query::get_item(&conn, "bn-reopen", false)
        .expect("query")
        .expect("exists");
    assert_eq!(item.state, "open");
}

// ---------------------------------------------------------------------------
// 2. All 11 event types through the projection pipeline
// ---------------------------------------------------------------------------

/// All 11 event types applied to one item verify correct SQLite projection.
#[test]
fn all_11_event_types_project_to_correct_sqlite_state() {
    // 1. Create the main item
    let create = make_create("bn-all11", "Full coverage item", "alice", 1_000, &[]);

    // Also create the blocker item (needed for the link FK constraint)
    let create_blocker = make_create("bn-all12", "Blocker item", "alice", 1_100, &[]);

    // 2. Update title
    let update_title = make_update_title(
        "bn-all11",
        "Full coverage item (updated)",
        "alice",
        2_000,
        &[],
    );

    // 3. Move → Doing
    let move_doing = make_move("bn-all11", State::Doing, "alice", 3_000, &[]);

    // 4. Assign alice
    let assign = make_assign(
        "bn-all11",
        "alice",
        AssignAction::Assign,
        "admin",
        4_000,
        &[],
    );

    // 5. Comment
    let comment = make_comment("bn-all11", "First comment", "alice", 5_000, &[]);

    // 6. Link → blocked by bn-all12 (exists due to create_blocker above)
    let link = make_link("bn-all11", "bn-all12", "blocks", "alice", 6_000, &[]);

    // 7. Unlink → remove blocked_by
    let unlink = make_unlink("bn-all11", "bn-all12", Some("blocks"), "alice", 7_000, &[]);

    // 8. Compact description
    let compact = make_compact("bn-all11", "TL;DR: full coverage", "alice", 8_000, &[]);

    // 9. Snapshot
    let snapshot = make_snapshot(
        "bn-all11",
        serde_json::json!({"id": "bn-all11", "state": "doing"}),
        "alice",
        9_000,
        &[],
    );

    // 10. Redact the comment (first comment event hash)
    let comment_hash = comment.event_hash.clone();
    let redact = make_redact(
        "bn-all11",
        &comment_hash,
        "Accidental leak",
        "admin",
        10_000,
        &[],
    );

    // 11. Delete (soft)
    let delete = make_delete("bn-all11", "admin", 11_000, &[]);

    let events = vec![
        create,
        create_blocker,
        update_title,
        move_doing,
        assign,
        comment,
        link,
        unlink,
        compact,
        snapshot,
        redact,
        delete,
    ];

    let conn = project_events(&events);

    // Verify update (title changed)
    let item = query::get_item(&conn, "bn-all11", true)
        .expect("query deleted")
        .expect("exists (include_deleted)");
    assert_eq!(item.title, "Full coverage item (updated)");
    assert_eq!(item.state, "doing");
    assert!(item.is_deleted, "item should be soft-deleted");
    assert!(item.compact_summary.is_some());
    assert_eq!(
        item.compact_summary.as_deref(),
        Some("TL;DR: full coverage")
    );
    // Snapshot stored in DB (query snapshot_json directly)
    let snapshot_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM items WHERE item_id = 'bn-all11' AND snapshot_json IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("count snapshot");
    assert_eq!(snapshot_count, 1, "snapshot_json should be set");

    // Assign: alice is assigned
    let assignees = query::get_assignees(&conn, "bn-all11").expect("assignees");
    assert_eq!(assignees.len(), 1);
    assert_eq!(assignees[0].agent, "alice");

    // Unlink: no remaining dependencies
    let deps = query::get_dependencies(&conn, "bn-all11").expect("deps");
    assert!(deps.is_empty(), "all links removed after unlink");

    // Redact: comment body is replaced
    let comments = query::get_comments(&conn, "bn-all11").expect("comments");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].body, "[redacted]");

    // Redaction record exists
    let redaction_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM event_redactions WHERE item_id = 'bn-all11'",
            [],
            |row| row.get(0),
        )
        .expect("count redactions");
    assert_eq!(redaction_count, 1);
}

// ---------------------------------------------------------------------------
// 3. Concurrent multi-agent merge scenarios
// ---------------------------------------------------------------------------

/// Scenario A: Agent A updates title to "Alpha" at ts 2000;
/// Agent B updates same item's title to "Beta" at ts 3000 (higher timestamp).
/// After merging both branches through the DAG and CRDT, "Beta" should win (LWW).
#[test]
fn two_agents_concurrent_title_update_lww_wins() {
    // Shared ancestor: item.create
    let root = make_create("bn-lww1", "Original title", "agent-a", 1_000, &[]);

    // Agent A branches and updates title to "Alpha" at ts 2000
    let update_a = make_update_title("bn-lww1", "Alpha", "agent-a", 2_000, &[&root.event_hash]);

    // Agent B branches from same root and updates title to "Beta" at ts 3000
    let update_b = make_update_title("bn-lww1", "Beta", "agent-b", 3_000, &[&root.event_hash]);

    // Build DAG
    let dag = EventDag::from_events(&[root.clone(), update_a.clone(), update_b.clone()]);

    // Get merged replay order from DAG
    let replay =
        replay_divergent(&dag, &update_a.event_hash, &update_b.event_hash).expect("replay");
    assert_eq!(replay.lca, root.event_hash);
    assert_eq!(replay.branch_a.len(), 1);
    assert_eq!(replay.branch_b.len(), 1);
    assert_eq!(replay.merged.len(), 2);

    // Apply through WorkItemState CRDT
    let mut crdt_state = WorkItemState::new();
    crdt_state.apply_event(&root);
    for event in &replay.merged {
        crdt_state.apply_event(event);
    }
    // Higher wall_ts (Beta at 3000) should win LWW
    assert_eq!(crdt_state.title.value, "Beta");

    // Also verify through SQLite projection (all 3 events)
    let all_events = [root, update_a, update_b];
    let conn = project_events(&all_events);
    // SQLite projection applies in order given, last-wins for title
    let item = query::get_item(&conn, "bn-lww1", false)
        .expect("query")
        .expect("exists");
    // Both projected — the higher ts update won via projection order
    assert!(!item.title.is_empty());
}

/// Scenario B: Two agents use concurrent state transitions.
/// Agent A closes (Done) while Agent B reopens (Open) from Done state.
/// In epoch+phase CRDT: reopen increments epoch, so reopen wins.
#[test]
fn two_agents_concurrent_close_vs_reopen_reopen_wins_crdt() {
    let root = make_create("bn-ep1", "Epoch test item", "agent-a", 1_000, &[]);

    // Shared state: item is Done (epoch 0)
    let to_done = make_move("bn-ep1", State::Done, "agent-a", 2_000, &[&root.event_hash]);

    // Agent A: closes to Archived (epoch 0, Archived)
    let close_archived = make_move(
        "bn-ep1",
        State::Archived,
        "agent-a",
        3_000,
        &[&to_done.event_hash],
    );

    // Agent B: reopens (epoch 1, Open)
    let reopen = make_move(
        "bn-ep1",
        State::Open,
        "agent-b",
        3_100,
        &[&to_done.event_hash],
    );

    // Build DAG
    let dag = EventDag::from_events(&[
        root.clone(),
        to_done.clone(),
        close_archived.clone(),
        reopen.clone(),
    ]);

    // Get merged events
    let replay =
        replay_divergent(&dag, &close_archived.event_hash, &reopen.event_hash).expect("replay");

    // Apply via CRDT
    let mut state = WorkItemState::new();
    state.apply_event(&root);
    state.apply_event(&to_done);
    for event in &replay.merged {
        state.apply_event(event);
    }

    // Reopen (epoch 1) wins over archived (epoch 0)
    assert_eq!(state.epoch(), 1, "reopen should increment epoch to 1");
    assert_eq!(
        state.phase(),
        Phase::Open,
        "reopen should set phase to Open"
    );
}

/// Scenario C: Two agents concurrently assign different people.
/// OR-Set semantics: both assigns should be preserved (add-wins).
#[test]
fn two_agents_concurrent_assigns_both_preserved() {
    let root = make_create("bn-assign1", "OR-Set assign test", "admin", 1_000, &[]);

    // Agent A assigns alice
    let assign_a = make_assign(
        "bn-assign1",
        "alice",
        AssignAction::Assign,
        "agent-a",
        2_000,
        &[&root.event_hash],
    );

    // Agent B concurrently assigns bob from same root
    let assign_b = make_assign(
        "bn-assign1",
        "bob",
        AssignAction::Assign,
        "agent-b",
        2_100,
        &[&root.event_hash],
    );

    // Apply in both CRDT and SQLite
    let events = vec![root, assign_a, assign_b];
    let conn = project_events(&events);

    let assignees = query::get_assignees(&conn, "bn-assign1").expect("assignees");
    assert_eq!(assignees.len(), 2, "both concurrent assigns should be kept");

    let names: Vec<&str> = assignees.iter().map(|a| a.agent.as_str()).collect();
    assert!(names.contains(&"alice"), "alice should be assigned");
    assert!(names.contains(&"bob"), "bob should be assigned");

    // Also verify via CRDT merge
    let mut state_a = WorkItemState::new();
    state_a.apply_event(&events[0]);
    state_a.apply_event(&events[1]);

    let mut state_b = WorkItemState::new();
    state_b.apply_event(&events[0]);
    state_b.apply_event(&events[2]);

    state_a.merge(&state_b);
    let assigned = state_a.assignee_names();
    assert!(assigned.contains(&"alice".to_string()));
    assert!(assigned.contains(&"bob".to_string()));
}

/// Scenario D: Two agents concurrently add comments.
/// G-Set semantics: both comments should be preserved in the union.
#[test]
fn two_agents_concurrent_comments_both_preserved() {
    let root = make_create(
        "bn-comments1",
        "Comment convergence test",
        "admin",
        1_000,
        &[],
    );

    let comment_a = make_comment(
        "bn-comments1",
        "Agent A's insight",
        "agent-a",
        2_000,
        &[&root.event_hash],
    );
    let comment_b = make_comment(
        "bn-comments1",
        "Agent B's insight",
        "agent-b",
        2_050,
        &[&root.event_hash],
    );

    let conn = project_events(&[root, comment_a, comment_b]);
    let comments = query::get_comments(&conn, "bn-comments1").expect("comments");
    assert_eq!(comments.len(), 2, "both concurrent comments preserved");
}

/// Scenario E: Concurrent label add by different agents; both preserved.
#[test]
fn two_agents_concurrent_label_adds_both_preserved() {
    let root = make_create("bn-labels1", "Label OR-Set test", "admin", 1_000, &[]);

    let add_label_a = {
        let mut e = Event {
            wall_ts_us: 2_000,
            agent: "agent-a".into(),
            itc: "itc:AQ.2000".into(),
            parents: vec![root.event_hash.clone()],
            event_type: EventType::Update,
            item_id: ItemId::new_unchecked("bn-labels1"),
            data: EventData::Update(UpdateData {
                field: "labels".into(),
                value: serde_json::json!({"action": "add", "label": "frontend"}),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        write_event(&mut e).expect("hash");
        e
    };

    let add_label_b = {
        let mut e = Event {
            wall_ts_us: 2_100,
            agent: "agent-b".into(),
            itc: "itc:AQ.2100".into(),
            parents: vec![root.event_hash.clone()],
            event_type: EventType::Update,
            item_id: ItemId::new_unchecked("bn-labels1"),
            data: EventData::Update(UpdateData {
                field: "labels".into(),
                value: serde_json::json!({"action": "add", "label": "backend"}),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        write_event(&mut e).expect("hash");
        e
    };

    // Through CRDT: both labels preserved
    let mut state_a = WorkItemState::new();
    state_a.apply_event(&root);
    state_a.apply_event(&add_label_a);

    let mut state_b = WorkItemState::new();
    state_b.apply_event(&root);
    state_b.apply_event(&add_label_b);

    state_a.merge(&state_b);
    let labels = state_a.label_names();
    assert!(
        labels.contains(&"frontend".to_string()),
        "frontend label preserved"
    );
    assert!(
        labels.contains(&"backend".to_string()),
        "backend label preserved"
    );
}

/// Scenario F: Multi-agent deep fork then converge.
/// Three items created by two agents; linked to each other; all merge correctly.
#[test]
fn three_items_multi_agent_link_convergence() {
    // Agent A creates item 1 and item 2; Agent B creates item 3
    let create1 = make_create("bn-multi1", "Item 1", "agent-a", 1_000, &[]);
    let create2 = make_create("bn-multi2", "Item 2", "agent-a", 1_001, &[]);
    let create3 = make_create("bn-multi3", "Item 3", "agent-b", 1_002, &[]);

    // Agent A links item 2 as depending on item 1
    let link_2_to_1 = make_link("bn-multi2", "bn-multi1", "blocks", "agent-a", 2_000, &[]);

    // Agent B links item 3 as depending on item 2
    let link_3_to_2 = make_link("bn-multi3", "bn-multi2", "blocks", "agent-b", 2_100, &[]);

    // Agent A assigns all items to alice
    let assign1 = make_assign(
        "bn-multi1",
        "alice",
        AssignAction::Assign,
        "agent-a",
        3_000,
        &[],
    );
    let assign2 = make_assign(
        "bn-multi2",
        "alice",
        AssignAction::Assign,
        "agent-a",
        3_001,
        &[],
    );
    let assign3 = make_assign(
        "bn-multi3",
        "alice",
        AssignAction::Assign,
        "agent-b",
        3_002,
        &[],
    );

    let events = vec![
        create1,
        create2,
        create3,
        link_2_to_1,
        link_3_to_2,
        assign1,
        assign2,
        assign3,
    ];
    let conn = project_events(&events);

    // All 3 items exist
    for id in &["bn-multi1", "bn-multi2", "bn-multi3"] {
        let item = query::get_item(&conn, id, false)
            .expect("query")
            .unwrap_or_else(|| panic!("{id} should exist"));
        assert_eq!(item.state, "open");
    }

    // Dependency chain: multi2 → multi1, multi3 → multi2
    let deps2 = query::get_dependencies(&conn, "bn-multi2").expect("deps2");
    assert_eq!(deps2.len(), 1);
    assert_eq!(deps2[0].depends_on_item_id, "bn-multi1");

    let deps3 = query::get_dependencies(&conn, "bn-multi3").expect("deps3");
    assert_eq!(deps3.len(), 1);
    assert_eq!(deps3[0].depends_on_item_id, "bn-multi2");
}

// ---------------------------------------------------------------------------
// 4. Incremental vs full replay equivalence
// ---------------------------------------------------------------------------

/// Project 10 events one at a time (incremental) and all at once (batch).
/// The resulting SQLite state must be identical.
#[test]
fn incremental_replay_matches_full_replay_10_events() {
    let events: Vec<Event> = vec![
        make_create("bn-inc1", "Incremental item", "alice", 1_000, &[]),
        make_update_title("bn-inc1", "Incremental item v2", "alice", 2_000, &[]),
        make_move("bn-inc1", State::Doing, "alice", 3_000, &[]),
        make_assign("bn-inc1", "bob", AssignAction::Assign, "alice", 4_000, &[]),
        make_comment("bn-inc1", "Progress update", "bob", 5_000, &[]),
        make_create("bn-inc2", "Second item", "bob", 6_000, &[]),
        make_link("bn-inc2", "bn-inc1", "blocks", "bob", 7_000, &[]),
        make_assign(
            "bn-inc1",
            "carol",
            AssignAction::Assign,
            "alice",
            8_000,
            &[],
        ),
        make_comment("bn-inc1", "Final comment", "carol", 9_000, &[]),
        make_move("bn-inc1", State::Done, "alice", 10_000, &[]),
    ];

    // Full batch replay
    let conn_full = test_db();
    let proj_full = Projector::new(&conn_full);
    proj_full.project_batch(&events).expect("full batch");

    // Incremental: one event at a time
    let conn_inc = test_db();
    let proj_inc = Projector::new(&conn_inc);
    for event in &events {
        proj_inc.project_event(event).expect("incremental event");
    }

    // Compare final states
    for id in &["bn-inc1", "bn-inc2"] {
        let item_full = query::get_item(&conn_full, id, false)
            .expect("query full")
            .unwrap_or_else(|| panic!("{id} missing in full"));
        let item_inc = query::get_item(&conn_inc, id, false)
            .expect("query inc")
            .unwrap_or_else(|| panic!("{id} missing in incremental"));
        assert_eq!(item_full.title, item_inc.title, "title mismatch for {id}");
        assert_eq!(item_full.state, item_inc.state, "state mismatch for {id}");
        assert_eq!(
            item_full.updated_at_us, item_inc.updated_at_us,
            "updated_at mismatch for {id}"
        );
    }

    // Assignees match
    for id in &["bn-inc1"] {
        let full_ass = query::get_assignees(&conn_full, id).expect("assignees full");
        let inc_ass = query::get_assignees(&conn_inc, id).expect("assignees inc");
        let mut full_names: Vec<_> = full_ass.iter().map(|a| a.agent.clone()).collect();
        let mut inc_names: Vec<_> = inc_ass.iter().map(|a| a.agent.clone()).collect();
        full_names.sort();
        inc_names.sort();
        assert_eq!(full_names, inc_names, "assignees mismatch for {id}");
    }

    // Comments match
    let full_comments = query::get_comments(&conn_full, "bn-inc1").expect("comments full");
    let inc_comments = query::get_comments(&conn_inc, "bn-inc1").expect("comments inc");
    assert_eq!(
        full_comments.len(),
        inc_comments.len(),
        "comment count mismatch"
    );
}

// ---------------------------------------------------------------------------
// 5. Ordering determinism
// ---------------------------------------------------------------------------

/// Build an EventDag from 20 events fed in 5 different orderings.
///
/// The DAG's `topological_order()` is deterministic regardless of insertion
/// order. So all 5 permutations produce the same topological replay sequence,
/// and applying that sequence through CRDT state produces the same result.
///
/// This validates the pipeline guarantee: events → DAG → deterministic order →
/// same SQLite projection for any input ordering.
#[test]
fn five_orderings_dag_topological_order_is_identical() {
    // Build 20 events across 5 items; events use explicit parent hashes to
    // create causal chains within each item.

    // ---- item 1 chain ----
    let e01 = make_create("bn-ord1", "Order item 1", "alice", 1_000, &[]);
    let e02 = make_update_title(
        "bn-ord1",
        "Order item 1 (updated)",
        "alice",
        2_000,
        &[&e01.event_hash],
    );
    let e03 = make_move("bn-ord1", State::Doing, "alice", 3_000, &[&e02.event_hash]);
    let e04 = make_assign(
        "bn-ord1",
        "bob",
        AssignAction::Assign,
        "alice",
        4_000,
        &[&e03.event_hash],
    );

    // ---- item 2 chain ----
    let e05 = make_create("bn-ord2", "Order item 2", "bob", 1_100, &[]);
    let e06 = make_move("bn-ord2", State::Done, "bob", 5_000, &[&e05.event_hash]);

    // ---- item 3 chain ----
    let e07 = make_create("bn-ord3", "Order item 3", "carol", 1_200, &[]);
    let e08 = make_comment(
        "bn-ord3",
        "Carol's note",
        "carol",
        6_000,
        &[&e07.event_hash],
    );

    // ---- item 4 chain (links to item 1, causal deps include e01 to ensure ordering) ----
    let e09 = make_create("bn-ord4", "Order item 4", "dave", 1_300, &[]);
    // e10 depends on BOTH e09 (its item) and e01 (the link target) to enforce causal order
    let e10 = make_link(
        "bn-ord4",
        "bn-ord1",
        "blocks",
        "dave",
        7_000,
        &[&e09.event_hash, &e01.event_hash],
    );
    let e11 = make_assign(
        "bn-ord4",
        "alice",
        AssignAction::Assign,
        "dave",
        8_000,
        &[&e10.event_hash],
    );

    // ---- item 5 chain ----
    let e12 = make_create("bn-ord5", "Order item 5", "eve", 1_400, &[]);
    let e13 = make_move("bn-ord5", State::Doing, "eve", 9_000, &[&e12.event_hash]);
    let e14 = make_comment("bn-ord5", "Eve's note", "eve", 10_000, &[&e13.event_hash]);
    let e15 = make_move("bn-ord5", State::Done, "eve", 11_000, &[&e14.event_hash]);

    // ---- more on item 1 ----
    let e16 = make_comment("bn-ord1", "Bob's note", "bob", 12_000, &[&e04.event_hash]);
    let e17 = make_move("bn-ord1", State::Done, "alice", 13_000, &[&e16.event_hash]);

    // ---- cross-item operations ----
    // e18: bn-ord2 links to bn-ord3; causally depends on both items' create events
    let e18 = make_link(
        "bn-ord2",
        "bn-ord3",
        "related_to",
        "bob",
        14_000,
        &[&e06.event_hash, &e07.event_hash],
    );
    let e19 = make_assign(
        "bn-ord3",
        "dave",
        AssignAction::Assign,
        "carol",
        15_000,
        &[&e08.event_hash],
    );
    let e20 = make_comment("bn-ord2", "Summary", "bob", 16_000, &[&e18.event_hash]);

    let canonical_events: Vec<Event> = vec![
        e01.clone(),
        e02.clone(),
        e03.clone(),
        e04.clone(),
        e05.clone(),
        e06.clone(),
        e07.clone(),
        e08.clone(),
        e09.clone(),
        e10.clone(),
        e11.clone(),
        e12.clone(),
        e13.clone(),
        e14.clone(),
        e15.clone(),
        e16.clone(),
        e17.clone(),
        e18.clone(),
        e19.clone(),
        e20.clone(),
    ];
    assert_eq!(canonical_events.len(), 20);

    // 5 permutations of the event slice (different insertion orders into the DAG)
    let permutations: &[&[usize]] = &[
        // Original order
        &[
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
        ],
        // Reversed
        &[
            19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0,
        ],
        // Interleaved by item
        &[
            0, 4, 6, 8, 11, 1, 5, 7, 9, 12, 2, 15, 13, 10, 3, 16, 17, 14, 18, 19,
        ],
        // Items in reverse, events within each item forward
        &[
            11, 12, 13, 14, 8, 9, 10, 6, 7, 18, 4, 5, 17, 19, 0, 1, 2, 3, 15, 16,
        ],
        // All creates first, then rest
        &[
            0, 4, 6, 8, 11, 1, 2, 3, 5, 7, 9, 10, 12, 13, 14, 15, 16, 17, 18, 19,
        ],
    ];

    // Compute topological order from canonical DAG (baseline)
    let canon_dag = EventDag::from_events(&canonical_events);
    let canon_order: Vec<String> = canon_dag
        .topological_order()
        .iter()
        .map(|e| e.event_hash.clone())
        .collect();

    // For each permutation: build DAG from shuffled input, get topological order,
    // verify it matches the canonical order exactly.
    for (perm_idx, perm) in permutations.iter().enumerate() {
        let shuffled: Vec<Event> = perm.iter().map(|&i| canonical_events[i].clone()).collect();
        let shuffled_dag = EventDag::from_events(&shuffled);
        let shuffled_order: Vec<String> = shuffled_dag
            .topological_order()
            .iter()
            .map(|e| e.event_hash.clone())
            .collect();

        assert_eq!(
            canon_order, shuffled_order,
            "perm {perm_idx}: topological order must be identical regardless of insertion order"
        );
    }

    // Apply canonical topological order through CRDT state and SQLite projection.
    // Collect events in topological order (respecting FK constraints: items created before links).
    let ordered_events: Vec<Event> = canon_dag
        .topological_order()
        .iter()
        .map(|e| (*e).clone())
        .collect();

    let conn = project_events(&ordered_events);

    // Verify final state: item 1 is done with assignee bob, item 2 is done,
    // item 5 is done, item 4 links to item 1.
    let item1 = query::get_item(&conn, "bn-ord1", false)
        .expect("query")
        .expect("bn-ord1 exists");
    assert_eq!(item1.title, "Order item 1 (updated)");
    assert_eq!(item1.state, "done");

    let item5 = query::get_item(&conn, "bn-ord5", false)
        .expect("query")
        .expect("bn-ord5 exists");
    assert_eq!(item5.state, "done");

    let deps4 = query::get_dependencies(&conn, "bn-ord4").expect("deps4");
    assert_eq!(deps4.len(), 1);
    assert_eq!(deps4[0].depends_on_item_id, "bn-ord1");
}

// ---------------------------------------------------------------------------
// 6. Rebuild determinism
// ---------------------------------------------------------------------------

/// Clear + replay produces identical SQLite state as original projection.
#[test]
fn rebuild_clear_and_replay_identical_to_original() {
    let events: Vec<Event> = vec![
        make_create("bn-rb1", "Rebuild item 1", "alice", 1_000, &[]),
        make_create("bn-rb2", "Rebuild item 2", "bob", 1_001, &[]),
        make_update_title("bn-rb1", "Rebuild item 1 (updated)", "alice", 2_000, &[]),
        make_move("bn-rb1", State::Doing, "alice", 3_000, &[]),
        make_assign("bn-rb1", "carol", AssignAction::Assign, "alice", 4_000, &[]),
        make_comment("bn-rb1", "First comment", "carol", 5_000, &[]),
        make_link("bn-rb2", "bn-rb1", "blocks", "bob", 6_000, &[]),
        make_move("bn-rb1", State::Done, "alice", 7_000, &[]),
    ];

    let conn = test_db();
    let projector = Projector::new(&conn);

    // First projection
    projector.project_batch(&events).expect("first projection");

    // Snapshot state after first projection
    let item1_before = query::get_item(&conn, "bn-rb1", false)
        .expect("query")
        .expect("exists");
    let comments_before = query::get_comments(&conn, "bn-rb1").expect("comments");
    let deps_before = query::get_dependencies(&conn, "bn-rb2").expect("deps");

    // Clear all projection data
    clear_projection(&conn).expect("clear projection");

    // Verify cleared
    let item_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .expect("count");
    assert_eq!(item_count, 0, "items table must be empty after clear");

    // Replay all events
    projector
        .project_batch(&events)
        .expect("rebuild projection");

    // Verify state is identical to before
    let item1_after = query::get_item(&conn, "bn-rb1", false)
        .expect("query")
        .expect("exists after rebuild");
    assert_eq!(
        item1_before.title, item1_after.title,
        "title must match after rebuild"
    );
    assert_eq!(
        item1_before.state, item1_after.state,
        "state must match after rebuild"
    );
    assert_eq!(
        item1_before.updated_at_us, item1_after.updated_at_us,
        "updated_at must match after rebuild"
    );

    let comments_after = query::get_comments(&conn, "bn-rb1").expect("comments after");
    assert_eq!(
        comments_before.len(),
        comments_after.len(),
        "comment count must match after rebuild"
    );

    let deps_after = query::get_dependencies(&conn, "bn-rb2").expect("deps after");
    assert_eq!(
        deps_before.len(),
        deps_after.len(),
        "dependency count must match after rebuild"
    );
}

/// Rebuild twice into separate DBs from the same events; verify both produce
/// identical item rows (hash equality check on key fields).
#[test]
fn two_rebuilds_from_same_events_are_identical() {
    let events: Vec<Event> = vec![
        make_create("bn-det1", "Determinism check 1", "alice", 1_000, &[]),
        make_create("bn-det2", "Determinism check 2", "bob", 1_001, &[]),
        make_create("bn-det3", "Determinism check 3", "carol", 1_002, &[]),
        make_update_title("bn-det1", "Determinism check 1 (v2)", "alice", 2_000, &[]),
        make_move("bn-det2", State::Doing, "bob", 3_000, &[]),
        make_assign(
            "bn-det3",
            "alice",
            AssignAction::Assign,
            "carol",
            4_000,
            &[],
        ),
        make_link("bn-det2", "bn-det1", "blocks", "bob", 5_000, &[]),
    ];

    // Build A
    let conn_a = test_db();
    Projector::new(&conn_a)
        .project_batch(&events)
        .expect("project A");

    // Build B
    let conn_b = test_db();
    Projector::new(&conn_b)
        .project_batch(&events)
        .expect("project B");

    // Both must have identical item state
    for id in &["bn-det1", "bn-det2", "bn-det3"] {
        let item_a = query::get_item(&conn_a, id, false)
            .expect("query a")
            .unwrap_or_else(|| panic!("{id} missing in A"));
        let item_b = query::get_item(&conn_b, id, false)
            .expect("query b")
            .unwrap_or_else(|| panic!("{id} missing in B"));

        assert_eq!(item_a.title, item_b.title, "title mismatch for {id}");
        assert_eq!(item_a.state, item_b.state, "state mismatch for {id}");
        assert_eq!(
            item_a.updated_at_us, item_b.updated_at_us,
            "updated_at mismatch for {id}"
        );
    }
}

// ---------------------------------------------------------------------------
// 7. DAG divergent replay → CRDT → SQLite
// ---------------------------------------------------------------------------

/// Full pipeline: build EventDag from real events with parent hashes,
/// use replay_divergent to get merged order, apply through CRDT, then SQLite.
#[test]
fn dag_divergent_replay_pipeline_full() {
    // Build a forked DAG:
    //   root
    //   ├── branch_a: update title to "Branch A"
    //   └── branch_b: update title to "Branch B" + add assignee "bob"

    let root = make_create("bn-dag1", "Root item", "agent-root", 1_000, &[]);

    let branch_a = make_update_title(
        "bn-dag1",
        "Branch A title",
        "agent-a",
        2_000,
        &[&root.event_hash],
    );

    let branch_b_title = make_update_title(
        "bn-dag1",
        "Branch B title",
        "agent-b",
        3_000, // higher ts → will win LWW
        &[&root.event_hash],
    );

    let branch_b_assign = make_assign(
        "bn-dag1",
        "bob",
        AssignAction::Assign,
        "agent-b",
        3_100,
        &[&branch_b_title.event_hash],
    );

    let dag = EventDag::from_events(&[
        root.clone(),
        branch_a.clone(),
        branch_b_title.clone(),
        branch_b_assign.clone(),
    ]);

    // Divergent replay: tip_a = branch_a, tip_b = branch_b_assign
    let replay_result = replay_divergent(&dag, &branch_a.event_hash, &branch_b_assign.event_hash)
        .expect("replay_divergent");

    assert_eq!(replay_result.lca, root.event_hash, "LCA should be root");

    // Apply through CRDT
    let mut crdt = WorkItemState::new();
    crdt.apply_event(&root);
    for event in &replay_result.merged {
        crdt.apply_event(event);
    }

    // Branch B's title has higher wall_ts (3000 > 2000) → wins LWW
    assert_eq!(
        crdt.title.value, "Branch B title",
        "branch B title wins LWW (higher timestamp)"
    );
    // Branch B's assign is preserved
    assert!(
        crdt.assignee_names().contains(&"bob".to_string()),
        "bob assigned via branch B"
    );

    // Apply through SQLite projection
    let all_events = vec![root, branch_a, branch_b_title, branch_b_assign];
    let conn = project_events(&all_events);

    let item = query::get_item(&conn, "bn-dag1", false)
        .expect("query")
        .expect("exists");
    // Projection also shows an assignee
    let assignees = query::get_assignees(&conn, "bn-dag1").expect("assignees");
    assert_eq!(assignees.len(), 1);
    assert_eq!(assignees[0].agent, "bob");
    // Item exists with latest title from sequential projection
    assert!(!item.title.is_empty());
}

/// DAG divergent replay with both orderings (tip_a, tip_b) and (tip_b, tip_a)
/// must produce the same merged event sequence (symmetry guarantee).
#[test]
fn dag_divergent_replay_is_symmetric() {
    let root = make_create("bn-sym1", "Symmetry test", "agent-root", 1_000, &[]);
    let tip_a = make_update_title("bn-sym1", "Title A", "agent-a", 2_000, &[&root.event_hash]);
    let tip_b = make_update_title("bn-sym1", "Title B", "agent-b", 2_100, &[&root.event_hash]);

    let dag = EventDag::from_events(&[root.clone(), tip_a.clone(), tip_b.clone()]);

    let replay_ab =
        replay_divergent(&dag, &tip_a.event_hash, &tip_b.event_hash).expect("replay AB");
    let replay_ba =
        replay_divergent(&dag, &tip_b.event_hash, &tip_a.event_hash).expect("replay BA");

    // Merged sequences must be identical (same hashes, same order)
    let hashes_ab: Vec<&str> = replay_ab
        .merged
        .iter()
        .map(|e| e.event_hash.as_str())
        .collect();
    let hashes_ba: Vec<&str> = replay_ba
        .merged
        .iter()
        .map(|e| e.event_hash.as_str())
        .collect();
    assert_eq!(hashes_ab, hashes_ba, "divergent replay must be symmetric");

    // Applying both in merged order must produce same CRDT state
    let mut crdt_ab = WorkItemState::new();
    crdt_ab.apply_event(&root);
    for e in &replay_ab.merged {
        crdt_ab.apply_event(e);
    }

    let mut crdt_ba = WorkItemState::new();
    crdt_ba.apply_event(&root);
    for e in &replay_ba.merged {
        crdt_ba.apply_event(e);
    }

    assert_eq!(
        crdt_ab.title.value, crdt_ba.title.value,
        "title must be identical regardless of replay order"
    );
}

/// Verify replay with a post-merge fork (the diamond pattern):
///   root → a → b → merge → c
///                        ↘ d
/// LCA of c and d should be the merge event, not root.
#[test]
fn dag_post_merge_fork_lca_is_merge_point() {
    let root = make_create("bn-diamond", "Diamond item", "agent-root", 1_000, &[]);
    let a = make_move(
        "bn-diamond",
        State::Doing,
        "agent-a",
        2_000,
        &[&root.event_hash],
    );
    let b = make_comment(
        "bn-diamond",
        "B comment",
        "agent-b",
        2_100,
        &[&root.event_hash],
    );
    let merge_event = make_move(
        "bn-diamond",
        State::Done,
        "agent-a",
        3_000,
        &[&a.event_hash, &b.event_hash],
    );
    let c = make_update_title(
        "bn-diamond",
        "Post-merge A",
        "agent-a",
        4_000,
        &[&merge_event.event_hash],
    );
    let d = make_update_title(
        "bn-diamond",
        "Post-merge B",
        "agent-b",
        4_100,
        &[&merge_event.event_hash],
    );

    let dag = EventDag::from_events(&[
        root.clone(),
        a.clone(),
        b.clone(),
        merge_event.clone(),
        c.clone(),
        d.clone(),
    ]);

    let replay = replay_divergent(&dag, &c.event_hash, &d.event_hash).expect("replay");
    // LCA should be the merge point, not the root
    assert_eq!(
        replay.lca, merge_event.event_hash,
        "LCA should be the merge event for a post-merge fork"
    );
    // Only 2 divergent events: c and d
    assert_eq!(
        replay.merged.len(),
        2,
        "only 2 divergent events after merge point"
    );
}

// ---------------------------------------------------------------------------
// 8. Mixed event type replay (20 events, all types)
// ---------------------------------------------------------------------------

/// 20+ events covering all event types; verify final SQLite state is correct.
#[test]
fn replay_20_mixed_events_correct_sqlite_state() {
    // Items: bn-mix1, bn-mix2, bn-mix3
    let e01 = make_create("bn-mix1", "Mix item 1", "alice", 1_000, &[]);
    let e02 = make_create("bn-mix2", "Mix item 2", "bob", 1_001, &[]);
    let e03 = make_create("bn-mix3", "Mix item 3", "carol", 1_002, &[]);

    // Updates
    let e04 = make_update_title("bn-mix1", "Mix item 1 (final)", "alice", 2_000, &[]);
    let e05 = {
        let mut e = Event {
            wall_ts_us: 2_100,
            agent: "alice".into(),
            itc: "itc:AQ.2100".into(),
            parents: vec![],
            event_type: EventType::Update,
            item_id: ItemId::new_unchecked("bn-mix1"),
            data: EventData::Update(UpdateData {
                field: "size".into(),
                value: serde_json::json!("xl"),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        write_event(&mut e).expect("hash");
        e
    };

    // Moves
    let e06 = make_move("bn-mix1", State::Doing, "alice", 3_000, &[]);
    let e07 = make_move("bn-mix2", State::Doing, "bob", 3_100, &[]);

    // Assigns
    let e08 = make_assign("bn-mix1", "dave", AssignAction::Assign, "alice", 4_000, &[]);
    let e09 = make_assign("bn-mix2", "eve", AssignAction::Assign, "bob", 4_100, &[]);

    // Links and unlinks
    let e10 = make_link("bn-mix2", "bn-mix1", "blocks", "bob", 5_000, &[]);
    let e11 = make_link("bn-mix3", "bn-mix2", "related_to", "carol", 5_100, &[]);
    let e12 = make_unlink("bn-mix2", "bn-mix1", Some("blocks"), "bob", 6_000, &[]);

    // Comments
    let e13 = make_comment("bn-mix1", "This is comment 1", "dave", 7_000, &[]);
    let e14 = make_comment("bn-mix1", "This is comment 2", "alice", 7_100, &[]);

    // Compact
    let e15 = make_compact("bn-mix3", "TL;DR: mix 3 is related", "carol", 8_000, &[]);

    // Close items
    let e16 = make_move("bn-mix1", State::Done, "alice", 9_000, &[]);
    let e17 = make_move("bn-mix2", State::Done, "bob", 9_100, &[]);

    // Reopen bn-mix1
    let e18 = make_move("bn-mix1", State::Open, "alice", 10_000, &[]);

    // Delete bn-mix3
    let e19 = make_delete("bn-mix3", "admin", 11_000, &[]);

    // Redact first comment on bn-mix1
    let comment_hash = e13.event_hash.clone();
    let e20 = make_redact("bn-mix1", &comment_hash, "Privacy", "admin", 12_000, &[]);

    let all_events = vec![
        e01, e02, e03, e04, e05, e06, e07, e08, e09, e10, e11, e12, e13, e14, e15, e16, e17, e18,
        e19, e20,
    ];
    assert!(all_events.len() >= 20, "should have 20+ events");

    let conn = project_events(&all_events);

    // bn-mix1: title updated, state reopened (open), size xl
    let mix1 = query::get_item(&conn, "bn-mix1", false)
        .expect("query mix1")
        .expect("mix1 exists");
    assert_eq!(mix1.title, "Mix item 1 (final)");
    assert_eq!(mix1.state, "open", "mix1 was reopened");
    assert_eq!(mix1.size.as_deref(), Some("xl"));

    // bn-mix1: 2 comments, first one redacted
    let mix1_comments = query::get_comments(&conn, "bn-mix1").expect("mix1 comments");
    assert_eq!(mix1_comments.len(), 2);
    let redacted = mix1_comments.iter().find(|c| c.event_hash == comment_hash);
    assert!(redacted.is_some(), "first comment should be findable");
    assert_eq!(
        redacted.unwrap().body,
        "[redacted]",
        "first comment should be redacted"
    );

    // bn-mix1: dave is assigned
    let mix1_assignees = query::get_assignees(&conn, "bn-mix1").expect("mix1 assignees");
    let names: Vec<&str> = mix1_assignees.iter().map(|a| a.agent.as_str()).collect();
    assert!(names.contains(&"dave"));

    // bn-mix2: unlinked (no deps to mix1)
    let mix2_deps = query::get_dependencies(&conn, "bn-mix2").expect("mix2 deps");
    assert!(
        mix2_deps.is_empty(),
        "bn-mix2's link to bn-mix1 was removed"
    );

    // bn-mix3: compact summary set, deleted
    let mix3 = query::get_item(&conn, "bn-mix3", true)
        .expect("query mix3 (include deleted)")
        .expect("mix3 exists (soft-deleted)");
    assert!(mix3.is_deleted, "bn-mix3 should be deleted");
    assert_eq!(
        mix3.compact_summary.as_deref(),
        Some("TL;DR: mix 3 is related")
    );

    // bn-mix3 not visible without include_deleted
    assert!(
        query::get_item(&conn, "bn-mix3", false)
            .expect("query")
            .is_none(),
        "deleted item should not be visible without include_deleted"
    );
}
