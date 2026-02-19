//! `bn done` — transition an item to "done" state.
//!
//! Validates the item exists and is in a valid source state (open→done or
//! doing→done), emits an `item.move` event with `{state: "done"}`, projects
//! the state change into SQLite, and outputs the result.
//!
//! Supports `--reason` flag for recording why the item is done. When a goal's
//! last open/doing child is completed, goal auto-complete emits an additional
//! move event for the parent goal.

use crate::agent;
use crate::cmd::show::resolve_item_id;
use crate::output::{CliError, OutputMode, render, render_error};
use clap::Args;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use bones_core::db;
use bones_core::db::project;
use bones_core::db::query;
use bones_core::event::data::{EventData, MoveData};
use bones_core::event::types::EventType;
use bones_core::event::writer;
use bones_core::event::Event;
use bones_core::model::item::State;
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;

#[derive(Args, Debug)]
pub struct DoneArgs {
    /// Item ID to mark as done (supports partial IDs).
    pub id: String,

    /// Optional reason for completing this item.
    #[arg(long)]
    pub reason: Option<String>,
}

/// JSON output for a `bn done` transition.
#[derive(Debug, Serialize)]
struct DoneOutput {
    id: String,
    previous_state: String,
    new_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    agent: String,
    event_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_completed_parent: Option<String>,
}

/// Find the `.bones` directory by walking up from `start`.
fn find_bones_dir(start: &Path) -> Option<std::path::PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let candidate = current.join(".bones");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Check if completing this item should auto-complete a parent goal.
///
/// Returns the parent item ID if all siblings (including this item, now done)
/// are in "done" or "archived" state and the parent is a "goal" kind.
fn check_goal_auto_complete(
    conn: &rusqlite::Connection,
    item_id: &str,
) -> anyhow::Result<Option<String>> {
    // Get the item's parent_id
    let item = match query::get_item(conn, item_id, false)? {
        Some(i) => i,
        None => return Ok(None),
    };

    let parent_id = match item.parent_id {
        Some(ref pid) => pid.clone(),
        None => return Ok(None),
    };

    // Check parent is a goal
    let parent = match query::get_item(conn, &parent_id, false)? {
        Some(p) => p,
        None => return Ok(None),
    };

    if parent.kind != "goal" {
        return Ok(None);
    }

    // Parent is already done/archived — nothing to do
    if parent.state == "done" || parent.state == "archived" {
        return Ok(None);
    }

    // Get all children of the parent
    let children = query::get_children(conn, &parent_id)?;
    if children.is_empty() {
        return Ok(None);
    }

    // Check if all children are done or archived.
    // Note: the current item may still show old state in DB if projection
    // hasn't been applied yet, so we treat it as "done" regardless.
    let all_complete = children.iter().all(|child| {
        if child.item_id == item_id {
            true // this item is being completed
        } else {
            child.state == "done" || child.state == "archived"
        }
    });

    if all_complete {
        // Check parent can transition to done
        let parent_state: State = parent.state.parse().unwrap_or(State::Open);
        if parent_state.can_transition_to(State::Done).is_ok() {
            return Ok(Some(parent_id));
        }
    }

    Ok(None)
}

pub fn run_done(
    args: &DoneArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    // 1. Require agent identity
    let agent = match agent::require_agent(agent_flag) {
        Ok(a) => a,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(&e.message, "Set --agent, BONES_AGENT, or AGENT", e.code),
            )?;
            anyhow::bail!("{}", e.message);
        }
    };

    // 2. Find .bones directory
    let bones_dir = find_bones_dir(project_root).ok_or_else(|| {
        let msg = "Not a bones project: .bones directory not found";
        render_error(
            output,
            &CliError::with_details(msg, "Run 'bn init' to create a new bones project", "not_a_project"),
        )
        .ok();
        anyhow::anyhow!("{}", msg)
    })?;

    // 3. Open projection DB
    let db_path = bones_dir.join("bones.db");
    let conn = db::open_projection(&db_path)?;
    let _ = project::ensure_tracking_table(&conn);

    // 4. Resolve item ID
    let resolved_id = match resolve_item_id(&conn, &args.id)? {
        Some(id) => id,
        None => {
            let msg = format!("item '{}' not found", args.id);
            render_error(
                output,
                &CliError::with_details(
                    &msg,
                    "Check the item ID with 'bn list' or 'bn show'",
                    "item_not_found",
                ),
            )?;
            anyhow::bail!("{}", msg);
        }
    };

    // 5. Get current item and validate state transition
    let item = match query::get_item(&conn, &resolved_id, false)? {
        Some(item) => item,
        None => {
            let msg = format!("item '{}' not found", resolved_id);
            render_error(
                output,
                &CliError::with_details(&msg, "The item may have been deleted", "item_not_found"),
            )?;
            anyhow::bail!("{}", msg);
        }
    };

    let current_state: State = item.state.parse().map_err(|_| {
        anyhow::anyhow!(
            "item '{}' has invalid state '{}'",
            resolved_id,
            item.state
        )
    })?;

    let target_state = State::Done;

    if let Err(e) = current_state.can_transition_to(target_state) {
        let msg = format!(
            "cannot transition '{}' from {} to done: {}",
            resolved_id, e.from, e.reason
        );
        let suggestion = match current_state {
            State::Done => "Item is already done".to_string(),
            State::Archived => {
                "Item is archived. Use 'bn move --state open' to reopen it first".to_string()
            }
            _ => format!("Current state is '{}', which cannot transition to 'done'", current_state),
        };
        render_error(
            output,
            &CliError::with_details(&msg, &suggestion, "invalid_transition"),
        )?;
        anyhow::bail!("{}", msg);
    }

    // 6. Set up shard manager and get timestamp
    let shard_mgr = ShardManager::new(&bones_dir);
    let ts = shard_mgr
        .next_timestamp()
        .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

    // 7. Build item.move event
    let move_data = MoveData {
        state: target_state,
        reason: args.reason.clone(),
        extra: BTreeMap::new(),
    };

    let mut event = Event {
        wall_ts_us: ts,
        agent: agent.clone(),
        itc: "itc:AQ".to_string(),
        parents: vec![],
        event_type: EventType::Move,
        item_id: ItemId::new_unchecked(&resolved_id),
        data: EventData::Move(move_data),
        event_hash: String::new(),
    };

    // 8. Serialize and write
    let line = writer::write_event(&mut event)
        .map_err(|e| anyhow::anyhow!("failed to serialize event: {e}"))?;

    shard_mgr
        .append(&line, false, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("failed to write event: {e}"))?;

    // 9. Project into SQLite
    let projector = project::Projector::new(&conn);
    if let Err(e) = projector.project_event(&event) {
        tracing::warn!("projection failed (will be fixed on next rebuild): {e}");
    }

    // 10. Check goal auto-complete
    let auto_completed_parent = match check_goal_auto_complete(&conn, &resolved_id)? {
        Some(parent_id) => {
            let ts2 = shard_mgr
                .next_timestamp()
                .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

            let mut parent_event = Event {
                wall_ts_us: ts2,
                agent: agent.clone(),
                itc: "itc:AQ".to_string(),
                parents: vec![],
                event_type: EventType::Move,
                item_id: ItemId::new_unchecked(&parent_id),
                data: EventData::Move(MoveData {
                    state: State::Done,
                    reason: Some(format!("auto-completed: all children of {} are done", parent_id)),
                    extra: BTreeMap::new(),
                }),
                event_hash: String::new(),
            };

            let parent_line = writer::write_event(&mut parent_event)
                .map_err(|e| anyhow::anyhow!("failed to serialize parent event: {e}"))?;

            shard_mgr
                .append(&parent_line, false, Duration::from_secs(5))
                .map_err(|e| anyhow::anyhow!("failed to write parent event: {e}"))?;

            if let Err(e) = projector.project_event(&parent_event) {
                tracing::warn!("parent projection failed: {e}");
            }

            Some(parent_id)
        }
        None => None,
    };

    // 11. Output
    let result = DoneOutput {
        id: resolved_id.clone(),
        previous_state: current_state.to_string(),
        new_state: target_state.to_string(),
        reason: args.reason.clone(),
        agent,
        event_hash: event.event_hash.clone(),
        auto_completed_parent: auto_completed_parent.clone(),
    };

    render(output, &result, |r, w| {
        use std::io::Write;
        writeln!(w, "✓ {} → done (was {})", r.id, r.previous_state)?;
        if let Some(ref reason) = r.reason {
            writeln!(w, "  reason: {reason}")?;
        }
        if let Some(ref parent) = r.auto_completed_parent {
            writeln!(w, "  ✓ auto-completed parent goal {parent}")?;
        }
        Ok(())
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db;
    use bones_core::db::project;
    use bones_core::event::data::{CreateData, EventData, MoveData};
    use bones_core::event::types::EventType;
    use bones_core::event::writer;
    use bones_core::model::item::{Kind, State, Urgency};
    use bones_core::model::item_id::ItemId;
    use bones_core::shard::ShardManager;
    use clap::Parser;
    use tempfile::TempDir;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: DoneArgs,
    }

    /// Set up a bones project with one item at the given state.
    fn setup_project(state: &str) -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();

        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let db_path = bones_dir.join("bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let _ = project::ensure_tracking_table(&conn);
        let projector = project::Projector::new(&conn);

        let item_id = "bn-test1";
        let ts = shard_mgr.next_timestamp().unwrap();

        let mut create_event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: "Test item".to_string(),
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

        let line = writer::write_event(&mut create_event).unwrap();
        shard_mgr.append(&line, false, Duration::from_secs(5)).unwrap();
        projector.project_event(&create_event).unwrap();

        if state != "open" {
            let steps = match state {
                "doing" => vec![State::Doing],
                "done" => vec![State::Doing, State::Done],
                "archived" => vec![State::Doing, State::Done, State::Archived],
                _ => vec![],
            };

            for step_state in steps {
                let ts2 = shard_mgr.next_timestamp().unwrap();
                let mut move_event = Event {
                    wall_ts_us: ts2,
                    agent: "test-agent".to_string(),
                    itc: "itc:AQ".to_string(),
                    parents: vec![],
                    event_type: EventType::Move,
                    item_id: ItemId::new_unchecked(item_id),
                    data: EventData::Move(MoveData {
                        state: step_state,
                        reason: None,
                        extra: BTreeMap::new(),
                    }),
                    event_hash: String::new(),
                };
                let line = writer::write_event(&mut move_event).unwrap();
                shard_mgr.append(&line, false, Duration::from_secs(5)).unwrap();
                projector.project_event(&move_event).unwrap();
            }
        }

        (dir, item_id.to_string())
    }

    /// Set up a goal with N children, where count_done of them are already done.
    fn setup_goal_with_children(
        num_children: usize,
        count_done: usize,
    ) -> (TempDir, String, Vec<String>) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();

        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let db_path = bones_dir.join("bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let _ = project::ensure_tracking_table(&conn);
        let projector = project::Projector::new(&conn);

        // Create the parent goal
        let goal_id = "bn-goal1";
        let ts = shard_mgr.next_timestamp().unwrap();
        let mut goal_event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(goal_id),
            data: EventData::Create(CreateData {
                title: "Parent goal".to_string(),
                kind: Kind::Goal,
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
        let line = writer::write_event(&mut goal_event).unwrap();
        shard_mgr.append(&line, false, Duration::from_secs(5)).unwrap();
        projector.project_event(&goal_event).unwrap();

        // Create children
        let mut child_ids = Vec::new();
        for i in 0..num_children {
            let child_id = format!("bn-child{}", i + 1);
            let ts = shard_mgr.next_timestamp().unwrap();
            let mut child_event = Event {
                wall_ts_us: ts,
                agent: "test-agent".to_string(),
                itc: "itc:AQ".to_string(),
                parents: vec![],
                event_type: EventType::Create,
                item_id: ItemId::new_unchecked(&child_id),
                data: EventData::Create(CreateData {
                    title: format!("Child {}", i + 1),
                    kind: Kind::Task,
                    size: None,
                    urgency: Urgency::Default,
                    labels: vec![],
                    parent: Some(goal_id.to_string()),
                    causation: None,
                    description: None,
                    extra: BTreeMap::new(),
                }),
                event_hash: String::new(),
            };
            let line = writer::write_event(&mut child_event).unwrap();
            shard_mgr.append(&line, false, Duration::from_secs(5)).unwrap();
            projector.project_event(&child_event).unwrap();

            // Mark some children as done
            if i < count_done {
                for step in [State::Doing, State::Done] {
                    let ts = shard_mgr.next_timestamp().unwrap();
                    let mut move_event = Event {
                        wall_ts_us: ts,
                        agent: "test-agent".to_string(),
                        itc: "itc:AQ".to_string(),
                        parents: vec![],
                        event_type: EventType::Move,
                        item_id: ItemId::new_unchecked(&child_id),
                        data: EventData::Move(MoveData {
                            state: step,
                            reason: None,
                            extra: BTreeMap::new(),
                        }),
                        event_hash: String::new(),
                    };
                    let line = writer::write_event(&mut move_event).unwrap();
                    shard_mgr.append(&line, false, Duration::from_secs(5)).unwrap();
                    projector.project_event(&move_event).unwrap();
                }
            }

            child_ids.push(child_id);
        }

        (dir, goal_id.to_string(), child_ids)
    }

    #[test]
    fn done_args_parses_id() {
        let w = Wrapper::parse_from(["test", "item-789"]);
        assert_eq!(w.args.id, "item-789");
        assert!(w.args.reason.is_none());
    }

    #[test]
    fn done_args_parses_reason() {
        let w = Wrapper::parse_from(["test", "item-789", "--reason", "Shipped it"]);
        assert_eq!(w.args.id, "item-789");
        assert_eq!(w.args.reason.as_deref(), Some("Shipped it"));
    }

    #[test]
    fn done_from_doing() {
        let (dir, item_id) = setup_project("doing");
        let args = DoneArgs { id: item_id.clone(), reason: None };
        let result = run_done(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "done failed: {:?}", result.err());

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.state, "done");
    }

    #[test]
    fn done_from_open() {
        let (dir, item_id) = setup_project("open");
        let args = DoneArgs { id: item_id.clone(), reason: None };
        let result = run_done(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "done from open failed: {:?}", result.err());

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.state, "done");
    }

    #[test]
    fn done_with_reason() {
        let (dir, item_id) = setup_project("doing");
        let args = DoneArgs {
            id: item_id.clone(),
            reason: Some("Shipped in commit abc123".to_string()),
        };
        let result = run_done(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "done with reason failed: {:?}", result.err());

        // Verify the event has the reason
        let bones_dir = dir.path().join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        let replay = shard_mgr.replay().unwrap();
        let lines: Vec<&str> = replay
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();

        let last_line = lines.last().unwrap();
        let fields: Vec<&str> = last_line.split('\t').collect();
        assert_eq!(fields[4], "item.move");
        assert!(fields[6].contains("abc123"), "reason not in event data");
    }

    #[test]
    fn done_rejects_already_done() {
        let (dir, item_id) = setup_project("done");
        let args = DoneArgs { id: item_id, reason: None };
        let result = run_done(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("cannot transition"), "unexpected error: {err_msg}");
    }

    #[test]
    fn done_rejects_archived() {
        let (dir, item_id) = setup_project("archived");
        let args = DoneArgs { id: item_id, reason: None };
        let result = run_done(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("cannot transition"), "unexpected error: {err_msg}");
    }

    #[test]
    fn done_rejects_nonexistent_item() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();
        let db_path = bones_dir.join("bones.db");
        let _conn = db::open_projection(&db_path).unwrap();

        let args = DoneArgs { id: "bn-nonexistent".to_string(), reason: None };
        let result = run_done(&args, Some("test-agent"), OutputMode::Json, root);
        assert!(result.is_err());
    }

    #[test]
    fn done_partial_id_resolution() {
        let (dir, _item_id) = setup_project("doing");
        let args = DoneArgs { id: "test1".to_string(), reason: None };
        let result = run_done(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "partial ID resolution failed: {:?}", result.err());

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, "bn-test1", false).unwrap().unwrap();
        assert_eq!(item.state, "done");
    }

    #[test]
    fn done_goal_auto_complete_triggers() {
        // Goal with 2 children: child1 already done, child2 is doing.
        // done-ing child2 should auto-complete the goal.
        let (dir, goal_id, child_ids) = setup_goal_with_children(2, 1);
        let last_child = &child_ids[1]; // child2, still open

        // Move child2 to doing first
        let args_do = super::super::do_cmd::DoArgs { id: last_child.clone() };
        super::super::do_cmd::run_do(
            &args_do,
            Some("test-agent"),
            OutputMode::Json,
            dir.path(),
        )
        .unwrap();

        // Now done child2
        let args = DoneArgs { id: last_child.clone(), reason: None };
        let result = run_done(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "done failed: {:?}", result.err());

        // Verify goal is now done
        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let goal = query::get_item(&conn, &goal_id, false).unwrap().unwrap();
        assert_eq!(goal.state, "done", "goal should be auto-completed");
    }

    #[test]
    fn done_goal_no_auto_complete_when_siblings_open() {
        // Goal with 3 children: child1 done, child2 being completed, child3 still open.
        let (dir, goal_id, child_ids) = setup_goal_with_children(3, 1);
        let second_child = &child_ids[1]; // child2, still open

        // Move child2 to doing then done
        let args_do = super::super::do_cmd::DoArgs { id: second_child.clone() };
        super::super::do_cmd::run_do(
            &args_do,
            Some("test-agent"),
            OutputMode::Json,
            dir.path(),
        )
        .unwrap();

        let args = DoneArgs { id: second_child.clone(), reason: None };
        run_done(&args, Some("test-agent"), OutputMode::Json, dir.path()).unwrap();

        // Goal should still be open (child3 is not done)
        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let goal = query::get_item(&conn, &goal_id, false).unwrap().unwrap();
        assert_eq!(goal.state, "open", "goal should NOT be auto-completed");
    }

    #[test]
    fn done_no_auto_complete_for_task_parent() {
        // Regular task as parent — no auto-complete even if all children done
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();
        let db_path = bones_dir.join("bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let _ = project::ensure_tracking_table(&conn);
        let projector = project::Projector::new(&conn);

        // Create parent task (NOT a goal)
        let ts = shard_mgr.next_timestamp().unwrap();
        let mut parent_event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-parent"),
            data: EventData::Create(CreateData {
                title: "Parent task".to_string(),
                kind: Kind::Task, // NOT a goal
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
        let line = writer::write_event(&mut parent_event).unwrap();
        shard_mgr.append(&line, false, Duration::from_secs(5)).unwrap();
        projector.project_event(&parent_event).unwrap();

        // Create child
        let ts = shard_mgr.next_timestamp().unwrap();
        let mut child_event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-child1"),
            data: EventData::Create(CreateData {
                title: "Child task".to_string(),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: Some("bn-parent".to_string()),
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        let line = writer::write_event(&mut child_event).unwrap();
        shard_mgr.append(&line, false, Duration::from_secs(5)).unwrap();
        projector.project_event(&child_event).unwrap();

        // Done the child
        let args = DoneArgs { id: "bn-child1".to_string(), reason: None };
        run_done(&args, Some("test-agent"), OutputMode::Json, root).unwrap();

        // Parent should still be open (not a goal)
        let parent = query::get_item(&conn, "bn-parent", false).unwrap().unwrap();
        assert_eq!(parent.state, "open", "task parent should NOT auto-complete");
    }

    #[test]
    fn done_writes_event_to_shard() {
        let (dir, item_id) = setup_project("doing");
        let args = DoneArgs { id: item_id.clone(), reason: Some("All done".to_string()) };
        run_done(&args, Some("test-agent"), OutputMode::Json, dir.path()).unwrap();

        let bones_dir = dir.path().join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        let replay = shard_mgr.replay().unwrap();
        let lines: Vec<&str> = replay
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();

        // create + do + done = 3 events
        assert!(lines.len() >= 3, "expected at least 3 events, got {}", lines.len());

        let last_line = lines.last().unwrap();
        let fields: Vec<&str> = last_line.split('\t').collect();
        assert_eq!(fields[4], "item.move");
        assert!(fields[6].contains("\"done\""), "should contain done state");
        assert!(fields[6].contains("All done"), "should contain reason");
    }

    #[test]
    fn done_not_bones_project() {
        let dir = TempDir::new().unwrap();
        let args = DoneArgs { id: "bn-test".to_string(), reason: None };
        let result = run_done(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
    }
}
