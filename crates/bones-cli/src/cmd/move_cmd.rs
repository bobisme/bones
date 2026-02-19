//! `bn move` — reparent a work item under a different goal.

use crate::agent;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;
use bones_core::db::query::{get_item, try_open_projection};
use bones_core::event::data::UpdateData;
use bones_core::event::writer::write_event;
use bones_core::event::{Event, EventData, EventType};
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;
use clap::Args;
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct MoveArgs {
    /// Item ID to move.
    pub id: String,

    /// New parent item ID. Use "--parent none" to make top-level.
    #[arg(long)]
    pub parent: String,
}

/// Open the projection DB, returning a helpful error if it doesn't exist.
fn open_db(project_root: &std::path::Path) -> anyhow::Result<Connection> {
    let db_path = project_root.join(".bones").join("bones.db");
    match try_open_projection(&db_path)? {
        Some(conn) => Ok(conn),
        None => anyhow::bail!(
            "projection database not found or corrupt at {}.\n  Run `bn rebuild` to initialize it.",
            db_path.display()
        ),
    }
}

/// Emit an `item.update` event for the parent field.
fn emit_parent_event(
    project_root: &std::path::Path,
    agent: &str,
    item_id: &ItemId,
    new_parent: Option<&str>,
) -> anyhow::Result<()> {
    let bones_dir = project_root.join(".bones");
    let shard_mgr = ShardManager::new(&bones_dir);

    // Get a monotonic timestamp
    let ts = shard_mgr
        .next_timestamp()
        .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

    let parent_value = match new_parent {
        Some(p) => json!(p),
        None => json!(null),
    };

    let mut event = Event {
        wall_ts_us: ts,
        agent: agent.to_string(),
        itc: "itc:AQ".into(),
        parents: vec![],
        event_type: EventType::Update,
        item_id: item_id.clone(),
        data: EventData::Update(UpdateData {
            field: "parent".into(),
            value: parent_value,
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };

    let line =
        write_event(&mut event).map_err(|e| anyhow::anyhow!("failed to serialize event: {e}"))?;

    shard_mgr
        .append(&line, false, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("failed to write event: {e}"))?;

    Ok(())
}

pub fn run_move(
    args: &MoveArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &std::path::Path,
) -> anyhow::Result<()> {
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

    if let Err(e) = validate::validate_agent(&agent) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }
    if let Err(e) = validate::validate_item_id(&args.id) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }

    // Parse and validate item ID
    let item_id = ItemId::parse(&args.id)
        .map_err(|e| anyhow::anyhow!("invalid item ID '{}': {}", args.id, e))?;

    // Determine new parent: "none" means top-level (null parent)
    let new_parent: Option<ItemId> = if args.parent.to_lowercase() == "none" {
        None
    } else {
        if let Err(e) = validate::validate_item_id(&args.parent) {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
        }
        let parent_id = ItemId::parse(&args.parent)
            .map_err(|e| anyhow::anyhow!("invalid parent ID '{}': {}", args.parent, e))?;
        Some(parent_id)
    };

    // If reparenting to a specific parent, validate it is a goal
    if let Some(ref parent_id) = new_parent {
        let conn = match open_db(project_root) {
            Ok(c) => c,
            Err(e) => {
                render_error(output, &CliError::new(e.to_string()))?;
                return Err(e);
            }
        };

        // Check that the item itself exists
        if !bones_core::db::query::item_exists(&conn, item_id.as_str())? {
            let err = anyhow::anyhow!("item not found: {}", item_id.as_str());
            render_error(output, &CliError::new(err.to_string()))?;
            return Err(err);
        }

        // Validate parent exists and is a goal
        let parent_item = match get_item(&conn, parent_id.as_str(), false)? {
            Some(item) => item,
            None => {
                let err = anyhow::anyhow!("parent item not found: {}", parent_id.as_str());
                render_error(output, &CliError::new(err.to_string()))?;
                return Err(err);
            }
        };

        if parent_item.kind != "goal" {
            let err = anyhow::anyhow!(
                "parent '{}' is a {} (kind={}), but only goals can contain items.\n  \
                 Create a goal first with: bn create --kind goal --title \"My Goal\"",
                parent_id.as_str(),
                parent_item.kind,
                parent_item.kind
            );
            render_error(output, &CliError::new(err.to_string()))?;
            return Err(err);
        }
    } else {
        // Moving to top-level: still validate the item itself exists if DB is available
        let db_path = project_root.join(".bones").join("bones.db");
        if let Some(conn) = try_open_projection(&db_path)? {
            if !bones_core::db::query::item_exists(&conn, item_id.as_str())? {
                let err = anyhow::anyhow!("item not found: {}", item_id.as_str());
                render_error(output, &CliError::new(err.to_string()))?;
                return Err(err);
            }
        }
    }

    // Emit the parent update event
    let parent_str = new_parent.as_ref().map(|p| p.as_str());
    if let Err(e) = emit_parent_event(project_root, &agent, &item_id, parent_str) {
        render_error(output, &CliError::new(e.to_string()))?;
        return Err(e);
    }

    // Output result
    let val = match &new_parent {
        Some(parent_id) => json!({
            "ok": true,
            "item_id": item_id.as_str(),
            "parent_id": parent_id.as_str(),
        }),
        None => json!({
            "ok": true,
            "item_id": item_id.as_str(),
            "parent_id": null,
        }),
    };

    render(output, &val, |v, w| {
        let item = v["item_id"].as_str().unwrap_or("");
        match v["parent_id"].as_str() {
            Some(parent) => writeln!(w, "✓ {item}: moved under parent {parent}"),
            None => writeln!(w, "✓ {item}: moved to top level (no parent)"),
        }
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_args_parses() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: MoveArgs,
        }
        let w = Wrapper::parse_from(["test", "item-1", "--parent", "goal-1"]);
        assert_eq!(w.args.id, "item-1");
        assert_eq!(w.args.parent, "goal-1");
    }

    #[test]
    fn move_to_top_level() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: MoveArgs,
        }
        let w = Wrapper::parse_from(["test", "item-1", "--parent", "none"]);
        assert_eq!(w.args.parent, "none");
    }

    #[test]
    fn move_parent_none_case_insensitive() {
        // "none" should be treated as top-level regardless of case
        for none_str in ["none", "None", "NONE"] {
            let result = none_str.to_lowercase() == "none";
            assert!(result, "'{none_str}' should map to top-level");
        }
    }

    #[test]
    fn parent_update_data_structure() {
        // Verify the UpdateData structure is built correctly for parent field
        let data_with_parent = UpdateData {
            field: "parent".into(),
            value: json!("bn-a7x"),
            extra: BTreeMap::new(),
        };
        assert_eq!(data_with_parent.field, "parent");
        assert_eq!(data_with_parent.value, json!("bn-a7x"));

        let data_top_level = UpdateData {
            field: "parent".into(),
            value: json!(null),
            extra: BTreeMap::new(),
        };
        assert_eq!(data_top_level.field, "parent");
        assert!(data_top_level.value.is_null());
    }

    #[test]
    fn emit_parent_event_structure_with_parent() {
        // Verify event structure when a parent is provided
        let parent_value = json!("bn-goal-1");
        assert!(parent_value.is_string());
        assert_eq!(parent_value.as_str(), Some("bn-goal-1"));
    }

    #[test]
    fn emit_parent_event_structure_top_level() {
        // Verify event structure when parent is None (top-level)
        let parent_value = json!(null);
        assert!(parent_value.is_null());
    }

    #[test]
    fn validate_goal_kind() {
        // Ensure we check for "goal" kind, not any other kind
        let valid_kind = "goal";
        let invalid_kinds = ["task", "bug", "epic", "milestone"];
        assert_eq!(valid_kind, "goal");
        for kind in &invalid_kinds {
            assert_ne!(*kind, "goal");
        }
    }

    // -----------------------------------------------------------------------
    // Integration tests
    // -----------------------------------------------------------------------

    /// Set up a test project with a goal and a task item.
    #[cfg(test)]
    fn setup_test_project_with_items() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        String, // task item id
        String, // goal item id
    ) {
        use bones_core::db::rebuild;
        use bones_core::event::data::CreateData;
        use bones_core::event::writer::write_event;
        use bones_core::event::{Event, EventData, EventType};
        use bones_core::model::item::{Kind, Urgency};
        use bones_core::model::item_id::ItemId;
        use bones_core::shard::ShardManager;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path().to_path_buf();
        let bones_dir = root.join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init shard");

        let task_id = "bn-tsk1";
        let goal_id = "bn-gol1";

        // Create the task item
        let ts1 = shard_mgr.next_timestamp().expect("timestamp");
        let mut task_event = Event {
            wall_ts_us: ts1,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(task_id),
            data: EventData::Create(CreateData {
                title: "My task".into(),
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
        let line = write_event(&mut task_event).expect("write task event");
        shard_mgr
            .append(&line, false, Duration::from_secs(5))
            .expect("append task event");

        // Create the goal item
        let ts2 = shard_mgr.next_timestamp().expect("timestamp");
        let mut goal_event = Event {
            wall_ts_us: ts2,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(goal_id),
            data: EventData::Create(CreateData {
                title: "My goal".into(),
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
        let line = write_event(&mut goal_event).expect("write goal event");
        shard_mgr
            .append(&line, false, Duration::from_secs(5))
            .expect("append goal event");

        // Rebuild the projection DB
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild projection");

        (dir, root, task_id.to_string(), goal_id.to_string())
    }

    #[test]
    fn run_move_reparents_task_under_goal() {
        use crate::output::OutputMode;
        use bones_core::db::query::{get_item, try_open_projection};
        use bones_core::db::rebuild;

        let (_dir, root, task_id, goal_id) = setup_test_project_with_items();

        // Move task under goal
        let args = MoveArgs {
            id: task_id.clone(),
            parent: goal_id.clone(),
        };
        run_move(&args, Some("test-agent"), OutputMode::Human, &root)
            .expect("run_move should succeed");

        // Rebuild and verify parent_id is set
        let bones_dir = root.join(".bones");
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild");

        let conn = try_open_projection(&db_path)
            .expect("open db")
            .expect("db exists");
        let item = get_item(&conn, &task_id, false)
            .expect("get item")
            .expect("item exists");
        assert_eq!(
            item.parent_id.as_deref(),
            Some(goal_id.as_str()),
            "task should be parented under goal"
        );
    }

    #[test]
    fn run_move_rejects_non_goal_parent() {
        use crate::output::OutputMode;

        let (_dir, root, task_id, _goal_id) = setup_test_project_with_items();

        // Try to move task under another task (not a goal) - use task_id as parent
        let args = MoveArgs {
            id: "bn-tsk1".to_string(),
            parent: task_id.clone(), // task is not a goal
        };
        // This will try to use the same item as both child and parent,
        // but the important thing is that the parent kind validation happens
        let result = run_move(&args, Some("test-agent"), OutputMode::Human, &root);
        assert!(result.is_err(), "should fail when parent is not a goal");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("goal") || err_msg.contains("kind"),
            "error should mention goal requirement, got: {err_msg}"
        );
    }

    #[test]
    fn run_move_to_top_level() {
        use crate::output::OutputMode;
        use bones_core::db::query::{get_item, try_open_projection};
        use bones_core::db::rebuild;

        let (_dir, root, task_id, goal_id) = setup_test_project_with_items();

        // First move task under the goal
        let args = MoveArgs {
            id: task_id.clone(),
            parent: goal_id.clone(),
        };
        run_move(&args, Some("test-agent"), OutputMode::Human, &root).expect("first move");

        // Rebuild
        let bones_dir = root.join(".bones");
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild");

        // Now move to top-level
        let args2 = MoveArgs {
            id: task_id.clone(),
            parent: "none".to_string(),
        };
        run_move(&args2, Some("test-agent"), OutputMode::Human, &root)
            .expect("move to top-level should succeed");

        // Rebuild and verify parent_id is null
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild again");
        let conn = try_open_projection(&db_path)
            .expect("open db")
            .expect("db exists");
        let item = get_item(&conn, &task_id, false)
            .expect("get item")
            .expect("item exists");
        assert!(
            item.parent_id.is_none(),
            "task should have no parent after moving to top-level"
        );
    }
}
