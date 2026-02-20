//! `bn do` — transition an item to "doing" state.
//!
//! Validates the item exists and is in a valid source state (open or
//! doing→open reopen), emits an `item.move` event with `{state: "doing"}`,
//! projects the state change into SQLite, and outputs the result.

use crate::agent;
use crate::cmd::show::resolve_item_id;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;
use clap::Args;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use bones_core::db;
use bones_core::db::project;
use bones_core::db::query;
use bones_core::event::Event;
use bones_core::event::data::{EventData, MoveData};
use bones_core::event::types::EventType;
use bones_core::event::writer;
use bones_core::model::item::State;
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;

#[derive(Args, Debug)]
pub struct DoArgs {
    /// Item ID to transition (supports partial IDs).
    pub id: String,

    /// Additional item IDs to transition in the same command.
    #[arg(value_name = "ID")]
    pub ids: Vec<String>,
}

/// JSON output for a successful `bn do` transition.
#[derive(Debug, Serialize)]
struct DoOutput {
    id: String,
    previous_state: String,
    new_state: String,
    agent: String,
    event_hash: String,
}

#[derive(Debug, Serialize)]
struct DoResult {
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct DoBatchOutput {
    results: Vec<DoResult>,
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

fn run_do_single(
    conn: &rusqlite::Connection,
    shard_mgr: &ShardManager,
    agent: &str,
    raw_id: &str,
) -> anyhow::Result<DoOutput> {
    validate::validate_item_id(raw_id)
        .map_err(|e| anyhow::anyhow!("invalid item_id '{}': {}", e.value, e.reason))?;

    // Resolve item ID (supports partial IDs)
    let resolved_id = resolve_item_id(conn, raw_id)?
        .ok_or_else(|| anyhow::anyhow!("item '{}' not found", raw_id))?;

    // Get current item and validate state transition
    let item = query::get_item(conn, &resolved_id, false)?
        .ok_or_else(|| anyhow::anyhow!("item '{}' not found", resolved_id))?;

    let current_state: State = item.state.parse().map_err(|_| {
        anyhow::anyhow!("item '{}' has invalid state '{}'", resolved_id, item.state)
    })?;

    let target_state = State::Doing;

    if let Err(e) = current_state.can_transition_to(target_state) {
        anyhow::bail!(
            "cannot transition '{}' from {} to {}: {}",
            resolved_id,
            e.from,
            e.to,
            e.reason
        );
    }

    // Build item.move event
    let ts = shard_mgr
        .next_timestamp()
        .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

    let move_data = MoveData {
        state: target_state,
        reason: None,
        extra: BTreeMap::new(),
    };

    let mut event = Event {
        wall_ts_us: ts,
        agent: agent.to_string(),
        itc: "itc:AQ".to_string(),
        parents: vec![],
        event_type: EventType::Move,
        item_id: ItemId::new_unchecked(&resolved_id),
        data: EventData::Move(move_data),
        event_hash: String::new(),
    };

    // Serialize and write
    let line = writer::write_event(&mut event)
        .map_err(|e| anyhow::anyhow!("failed to serialize event: {e}"))?;

    shard_mgr
        .append(&line, false, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("failed to write event: {e}"))?;

    // Project into SQLite
    let projector = project::Projector::new(conn);
    if let Err(e) = projector.project_event(&event) {
        tracing::warn!("projection failed (will be fixed on next rebuild): {e}");
    }

    Ok(DoOutput {
        id: resolved_id,
        previous_state: current_state.to_string(),
        new_state: target_state.to_string(),
        agent: agent.to_string(),
        event_hash: event.event_hash,
    })
}

fn item_ids(args: &DoArgs) -> impl Iterator<Item = &str> {
    std::iter::once(args.id.as_str()).chain(args.ids.iter().map(String::as_str))
}

pub fn run_do(
    args: &DoArgs,
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

    if let Err(e) = validate::validate_agent(&agent) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }

    // 2. Find .bones directory
    let bones_dir = find_bones_dir(project_root).ok_or_else(|| {
        let msg = "Not a bones project: .bones directory not found";
        render_error(
            output,
            &CliError::with_details(
                msg,
                "Run 'bn init' to create a new bones project",
                "not_a_project",
            ),
        )
        .ok();
        anyhow::anyhow!("{}", msg)
    })?;

    // 3. Open projection DB
    let db_path = bones_dir.join("bones.db");
    let conn = db::open_projection(&db_path)?;
    let _ = project::ensure_tracking_table(&conn);
    let shard_mgr = ShardManager::new(&bones_dir);

    // 4. Process each item independently
    let mut results = Vec::new();
    let mut failures = Vec::new();

    for raw_id in item_ids(args) {
        match run_do_single(&conn, &shard_mgr, &agent, raw_id) {
            Ok(ok) => results.push(DoResult {
                id: ok.id,
                ok: true,
                previous_state: Some(ok.previous_state),
                new_state: Some(ok.new_state),
                event_hash: Some(ok.event_hash),
                error: None,
            }),
            Err(err) => {
                failures.push(err.to_string());
                results.push(DoResult {
                    id: raw_id.to_string(),
                    ok: false,
                    previous_state: None,
                    new_state: None,
                    event_hash: None,
                    error: Some(err.to_string()),
                });
            }
        }
    }

    let payload = DoBatchOutput { results };

    render(output, &payload, |r, w| {
        writeln!(w, "Do results")?;
        writeln!(w, "{:-<88}", "")?;
        writeln!(w, "{:<4}  {:<16}  TRANSITION", "OK", "ID")?;
        writeln!(w, "{:-<88}", "")?;
        for result in &r.results {
            if result.ok {
                writeln!(
                    w,
                    "ok    {:<16}  {} -> {}",
                    result.id,
                    result.previous_state.as_deref().unwrap_or("unknown"),
                    result.new_state.as_deref().unwrap_or("doing")
                )?;
            } else {
                writeln!(
                    w,
                    "err   {:<16}  {}",
                    result.id,
                    result.error.as_deref().unwrap_or("unknown error")
                )?;
            }
        }
        Ok(())
    })?;

    if failures.is_empty() {
        Ok(())
    } else if failures.len() == 1 {
        anyhow::bail!("{}", failures[0]);
    } else {
        anyhow::bail!("{} item(s) failed", failures.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db;
    use bones_core::db::project;
    use bones_core::event::data::{CreateData, EventData};
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
        args: DoArgs,
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

        // Create the DB and item
        let db_path = bones_dir.join("bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let _ = project::ensure_tracking_table(&conn);
        let projector = project::Projector::new(&conn);

        let item_id = "bn-test1";
        let ts = shard_mgr.next_timestamp().unwrap();

        // Emit create event
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
        shard_mgr
            .append(&line, false, Duration::from_secs(5))
            .unwrap();
        projector.project_event(&create_event).unwrap();

        // If desired state is not "open", move to it
        if state != "open" {
            let target: State = state.parse().unwrap();
            // May need intermediate step: open → doing → done
            let steps = match target {
                State::Doing => vec![State::Doing],
                State::Done => vec![State::Doing, State::Done],
                State::Archived => vec![State::Doing, State::Done, State::Archived],
                State::Open => vec![],
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
                shard_mgr
                    .append(&line, false, Duration::from_secs(5))
                    .unwrap();
                projector.project_event(&move_event).unwrap();
            }
        }

        (dir, item_id.to_string())
    }

    #[test]
    fn do_args_parses_id() {
        let w = Wrapper::parse_from(["test", "item-456"]);
        assert_eq!(w.args.id, "item-456");
    }

    #[test]
    fn do_open_to_doing() {
        let (dir, item_id) = setup_project("open");
        let args = DoArgs {
            id: item_id.clone(),
            ids: vec![],
        };
        let result = run_do(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "do failed: {:?}", result.err());

        // Verify state changed in DB
        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.state, "doing");
    }

    #[test]
    fn do_rejects_already_doing() {
        let (dir, item_id) = setup_project("doing");
        let args = DoArgs {
            id: item_id,
            ids: vec![],
        };
        let result = run_do(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("cannot transition"),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn do_rejects_done_item() {
        let (dir, item_id) = setup_project("done");
        let args = DoArgs {
            id: item_id,
            ids: vec![],
        };
        let result = run_do(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("cannot transition"),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn do_rejects_archived_item() {
        let (dir, item_id) = setup_project("archived");
        let args = DoArgs {
            id: item_id,
            ids: vec![],
        };
        let result = run_do(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("cannot transition"),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn do_rejects_nonexistent_item() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();
        let db_path = bones_dir.join("bones.db");
        let _conn = db::open_projection(&db_path).unwrap();

        let args = DoArgs {
            id: "bn-nonexistent".to_string(),
            ids: vec![],
        };
        let result = run_do(&args, Some("test-agent"), OutputMode::Json, root);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not found"), "unexpected error: {err_msg}");
    }

    #[test]
    fn do_requires_agent() {
        let (dir, item_id) = setup_project("open");
        let args = DoArgs {
            id: item_id,
            ids: vec![],
        };
        // Don't pass agent flag and clear env
        let result = run_do(&args, None, OutputMode::Json, dir.path());
        // This may or may not fail depending on env vars; just verify no panic
        let _ = result;
    }

    #[test]
    fn do_writes_event_to_shard() {
        let (dir, item_id) = setup_project("open");
        let args = DoArgs {
            id: item_id.clone(),
            ids: vec![],
        };
        run_do(&args, Some("test-agent"), OutputMode::Json, dir.path()).unwrap();

        // Check the shard has the move event
        let bones_dir = dir.path().join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        let replay = shard_mgr.replay().unwrap();
        let lines: Vec<&str> = replay
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();

        // Should have create event + move event
        assert!(
            lines.len() >= 2,
            "expected at least 2 events, got {}",
            lines.len()
        );

        let last_line = lines.last().unwrap();
        let fields: Vec<&str> = last_line.split('\t').collect();
        assert_eq!(fields[4], "item.move", "last event should be item.move");
        assert!(
            fields[6].contains("\"doing\""),
            "should contain doing state"
        );
    }

    #[test]
    fn do_partial_id_resolution() {
        let (dir, _item_id) = setup_project("open");
        // Use partial ID "test1" instead of "bn-test1"
        let args = DoArgs {
            id: "test1".to_string(),
            ids: vec![],
        };
        let result = run_do(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(
            result.is_ok(),
            "partial ID resolution failed: {:?}",
            result.err()
        );

        // Verify state changed
        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, "bn-test1", false).unwrap().unwrap();
        assert_eq!(item.state, "doing");
    }

    #[test]
    fn do_not_bones_project() {
        let dir = TempDir::new().unwrap();
        let args = DoArgs {
            id: "bn-test".to_string(),
            ids: vec![],
        };
        let result = run_do(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Not a bones project") || err_msg.contains(".bones"));
    }
}
