//! `bn reopen` — reopen a closed or archived work item.
//!
//! Emits an `item.move` event transitioning the item to the Open state.
//! Valid source states: done→open, archived→open.
//!
//! Reopening uses the EpochPhase CRDT semantics: the new epoch ensures
//! the reopen wins against concurrent operations in the prior epoch.
//! The event carries `{"reopen": true}` in its extra fields to signal
//! epoch-increment intent to CRDT-aware projectors.

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

/// Arguments for `bn reopen`.
#[derive(Args, Debug)]
pub struct ReopenArgs {
    /// Item ID to reopen (supports partial IDs).
    pub id: String,

    /// Additional item IDs to reopen in the same command.
    #[arg(value_name = "ID")]
    pub ids: Vec<String>,
}

/// JSON output for a successful `bn reopen` transition.
#[derive(Debug, Serialize)]
struct ReopenOutput {
    id: String,
    previous_state: String,
    new_state: String,
    agent: String,
    event_hash: String,
}

#[derive(Debug, Serialize)]
struct ReopenResult {
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
struct ReopenBatchOutput {
    results: Vec<ReopenResult>,
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

fn run_reopen_single(
    conn: &rusqlite::Connection,
    shard_mgr: &ShardManager,
    agent: &str,
    raw_id: &str,
) -> anyhow::Result<ReopenOutput> {
    validate::validate_item_id(raw_id)
        .map_err(|e| anyhow::anyhow!("invalid item_id '{}': {}", e.value, e.reason))?;

    let resolved_id = resolve_item_id(conn, raw_id)?
        .ok_or_else(|| anyhow::anyhow!("item '{}' not found", raw_id))?;

    let item = query::get_item(conn, &resolved_id, false)?
        .ok_or_else(|| anyhow::anyhow!("item '{}' not found", resolved_id))?;

    let current_state: State = item.state.parse().map_err(|_| {
        anyhow::anyhow!("item '{}' has invalid state '{}'", resolved_id, item.state)
    })?;

    match current_state {
        State::Open => anyhow::bail!("cannot reopen '{}': item is already open", resolved_id),
        State::Doing => anyhow::bail!(
            "cannot reopen '{}': item is in progress (doing)",
            resolved_id
        ),
        State::Done | State::Archived => {}
    }

    let ts = shard_mgr
        .next_timestamp()
        .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

    let mut extra = BTreeMap::new();
    extra.insert("reopen".to_string(), serde_json::Value::Bool(true));

    let move_data = MoveData {
        state: State::Open,
        reason: None,
        extra,
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

    let line = writer::write_event(&mut event)
        .map_err(|e| anyhow::anyhow!("failed to serialize event: {e}"))?;

    shard_mgr
        .append(&line, false, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("failed to write event: {e}"))?;

    let projector = project::Projector::new(conn);
    if let Err(e) = projector.project_event(&event) {
        tracing::warn!("projection failed (will be fixed on next rebuild): {e}");
    }

    Ok(ReopenOutput {
        id: resolved_id,
        previous_state: current_state.to_string(),
        new_state: State::Open.to_string(),
        agent: agent.to_string(),
        event_hash: event.event_hash,
    })
}

fn item_ids(args: &ReopenArgs) -> impl Iterator<Item = &str> {
    std::iter::once(args.id.as_str()).chain(args.ids.iter().map(String::as_str))
}

pub fn run_reopen(
    args: &ReopenArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
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

    let db_path = bones_dir.join("bones.db");
    let conn = db::open_projection(&db_path)?;
    let _ = project::ensure_tracking_table(&conn);
    let shard_mgr = ShardManager::new(&bones_dir);

    let mut results = Vec::new();
    let mut failures = Vec::new();

    for raw_id in item_ids(args) {
        match run_reopen_single(&conn, &shard_mgr, &agent, raw_id) {
            Ok(ok) => results.push(ReopenResult {
                id: ok.id,
                ok: true,
                previous_state: Some(ok.previous_state),
                new_state: Some(ok.new_state),
                event_hash: Some(ok.event_hash),
                error: None,
            }),
            Err(err) => {
                failures.push(err.to_string());
                results.push(ReopenResult {
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

    let payload = ReopenBatchOutput { results };

    render(output, &payload, |r, w| {
        use std::io::Write;
        for result in &r.results {
            if result.ok {
                writeln!(
                    w,
                    "✓ {} → open (was {})",
                    result.id,
                    result.previous_state.as_deref().unwrap_or("unknown")
                )?;
            } else {
                writeln!(
                    w,
                    "✗ {}: {}",
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
    use bones_core::db::query;
    use bones_core::event::Event;
    use bones_core::event::data::{CreateData, EventData, MoveData};
    use bones_core::event::types::EventType;
    use bones_core::event::writer;
    use bones_core::model::item::{Kind, State, Urgency};
    use bones_core::model::item_id::ItemId;
    use bones_core::shard::ShardManager;
    use clap::Parser;
    use std::collections::BTreeMap;
    use std::time::Duration;
    use tempfile::TempDir;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: ReopenArgs,
    }

    /// Create a bones project with one item at the given state.
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

        let item_id = "bn-reopen1";
        let ts = shard_mgr.next_timestamp().unwrap();

        let mut create_event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: "Reopen test".to_string(),
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

        if state != "open" {
            let steps: Vec<State> = match state {
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
                shard_mgr
                    .append(&line, false, Duration::from_secs(5))
                    .unwrap();
                projector.project_event(&move_event).unwrap();
            }
        }

        (dir, item_id.to_string())
    }

    #[test]
    fn reopen_args_parses_id() {
        let w = Wrapper::parse_from(["test", "item-999"]);
        assert_eq!(w.args.id, "item-999");
    }

    #[test]
    fn reopen_from_done() {
        let (dir, item_id) = setup_project("done");
        let args = ReopenArgs {
            id: item_id.clone(),
            ids: vec![],
        };
        let result = run_reopen(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "reopen failed: {:?}", result.err());

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.state, "open");
    }

    #[test]
    fn reopen_from_archived() {
        let (dir, item_id) = setup_project("archived");
        let args = ReopenArgs {
            id: item_id.clone(),
            ids: vec![],
        };
        let result = run_reopen(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(
            result.is_ok(),
            "reopen from archived failed: {:?}",
            result.err()
        );

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.state, "open");
    }

    #[test]
    fn reopen_rejects_already_open() {
        let (dir, item_id) = setup_project("open");
        let args = ReopenArgs {
            id: item_id,
            ids: vec![],
        };
        let result = run_reopen(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("already open") || err.contains("cannot reopen"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn reopen_rejects_doing() {
        let (dir, item_id) = setup_project("doing");
        let args = ReopenArgs {
            id: item_id,
            ids: vec![],
        };
        let result = run_reopen(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("in progress") || err.contains("cannot reopen"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn reopen_partial_id() {
        let (dir, _) = setup_project("done");
        let args = ReopenArgs {
            id: "reopen1".to_string(),
            ids: vec![],
        };
        let result = run_reopen(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(
            result.is_ok(),
            "reopen via partial ID failed: {:?}",
            result.err()
        );

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, "bn-reopen1", false)
            .unwrap()
            .unwrap();
        assert_eq!(item.state, "open");
    }

    #[test]
    fn reopen_event_carries_reopen_flag() {
        let (dir, item_id) = setup_project("done");
        let args = ReopenArgs {
            id: item_id.clone(),
            ids: vec![],
        };
        run_reopen(&args, Some("test-agent"), OutputMode::Json, dir.path()).unwrap();

        // Check the shard event has reopen=true in extra
        let bones_dir = dir.path().join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        let replay = shard_mgr.replay().unwrap();
        let lines: Vec<&str> = replay
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();

        // Last line should be the reopen move event
        let last_line = lines.last().unwrap();
        let fields: Vec<&str> = last_line.split('\t').collect();
        assert_eq!(fields[4], "item.move");
        assert!(
            fields[6].contains("\"open\""),
            "should contain open state: {}",
            fields[6]
        );
        assert!(
            fields[6].contains("reopen"),
            "should carry reopen flag: {}",
            fields[6]
        );
    }

    #[test]
    fn reopen_writes_event_to_shard() {
        let (dir, item_id) = setup_project("done");
        let args = ReopenArgs {
            id: item_id.clone(),
            ids: vec![],
        };
        run_reopen(&args, Some("test-agent"), OutputMode::Json, dir.path()).unwrap();

        let bones_dir = dir.path().join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        let replay = shard_mgr.replay().unwrap();
        let event_lines: Vec<&str> = replay
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();

        // create + doing + done + reopen = 4 events
        assert!(
            event_lines.len() >= 4,
            "expected at least 4 events, got {}",
            event_lines.len()
        );
    }

    #[test]
    fn reopen_rejects_nonexistent_item() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();
        let db_path = bones_dir.join("bones.db");
        let _conn = db::open_projection(&db_path).unwrap();

        let args = ReopenArgs {
            id: "bn-nonexistent".to_string(),
            ids: vec![],
        };
        let result = run_reopen(&args, Some("test-agent"), OutputMode::Json, root);
        assert!(result.is_err());
    }

    #[test]
    fn reopen_not_bones_project() {
        let dir = TempDir::new().unwrap();
        let args = ReopenArgs {
            id: "bn-test".to_string(),
            ids: vec![],
        };
        let result = run_reopen(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn reopen_cycle_done_reopen_done_reopen() {
        // Test that an item can be closed and reopened multiple times
        let (dir, item_id) = setup_project("done");
        let root = dir.path();

        // Reopen
        let reopen_args = ReopenArgs {
            id: item_id.clone(),
            ids: vec![],
        };
        run_reopen(&reopen_args, Some("test-agent"), OutputMode::Json, root).unwrap();

        // Close again
        let close_args = super::super::done::DoneArgs {
            id: item_id.clone(),
            ids: vec![],
            reason: None,
        };
        super::super::done::run_done(&close_args, Some("test-agent"), OutputMode::Json, root)
            .unwrap();

        // Reopen again
        run_reopen(&reopen_args, Some("test-agent"), OutputMode::Json, root).unwrap();

        let db_path = root.join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(
            item.state, "open",
            "item should be open after second reopen"
        );
    }
}
