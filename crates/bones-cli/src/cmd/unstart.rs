//! `bn unstart` — revert a bone from `doing` back to `open`.
//!
//! Emits an `item.move` event transitioning the bone to the Open state.
//! Valid source states: doing→open. Other states are rejected.
//!
//! Use this when an agent abandons or is reassigned mid-flight: the only
//! alternative — `bn done` followed by `bn reopen` — round-trips through
//! `done` and pollutes the event log with a spurious closure.
//!
//! The event carries `{"unstart": true}` in its extra fields so downstream
//! filters (audit, history, projection) can distinguish a true reversion
//! from a fresh `bn do` after a manual edit.

use crate::agent;
use crate::cmd::open_projection_for_mutation;
use crate::cmd::show::resolve_item_id;
use crate::itc_state::assign_next_itc;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;
use clap::Args;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use bones_core::db::project;
use bones_core::db::query;
use bones_core::event::Event;
use bones_core::event::data::{EventData, MoveData};
use bones_core::event::types::EventType;
use bones_core::event::writer;
use bones_core::model::item::State;
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;

/// Arguments for `bn unstart`.
#[derive(Args, Debug)]
pub struct UnstartArgs {
    /// Bone ID to unstart (supports partial IDs).
    pub id: String,

    /// Additional bone IDs to unstart in the same command.
    #[arg(value_name = "ID")]
    pub ids: Vec<String>,
}

/// JSON output for a successful `bn unstart` transition.
#[derive(Debug, Serialize)]
struct UnstartOutput {
    id: String,
    previous_state: String,
    new_state: String,
    agent: String,
    event_hash: String,
}

#[derive(Debug, Serialize)]
struct UnstartResult {
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
struct UnstartBatchOutput {
    results: Vec<UnstartResult>,
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

fn run_unstart_single(
    project_root: &Path,
    conn: &rusqlite::Connection,
    shard_mgr: &ShardManager,
    agent: &str,
    raw_id: &str,
) -> anyhow::Result<UnstartOutput> {
    validate::validate_item_id(raw_id)
        .map_err(|e| anyhow::anyhow!("invalid item_id '{}': {}", e.value, e.reason))?;

    let resolved_id = resolve_item_id(conn, raw_id)?
        .ok_or_else(|| anyhow::anyhow!("item '{raw_id}' not found"))?;

    let item = query::get_item(conn, &resolved_id, false)?
        .ok_or_else(|| anyhow::anyhow!("item '{resolved_id}' not found"))?;

    let current_state: State = item.state.parse().map_err(|_| {
        anyhow::anyhow!("item '{}' has invalid state '{}'", resolved_id, item.state)
    })?;

    match current_state {
        State::Doing => {}
        State::Open => anyhow::bail!("cannot unstart '{resolved_id}': item is already open"),
        State::Done => {
            anyhow::bail!("cannot unstart '{resolved_id}': item is done (use `bn reopen`)")
        }
        State::Archived => {
            anyhow::bail!("cannot unstart '{resolved_id}': item is archived (use `bn reopen`)")
        }
    }

    let mut extra = BTreeMap::new();
    extra.insert("unstart".to_string(), serde_json::Value::Bool(true));

    let move_data = MoveData {
        state: State::Open,
        reason: None,
        extra,
    };

    let mut event = Event {
        wall_ts_us: 0,
        agent: agent.to_string(),
        itc: String::new(),
        parents: vec![],
        event_type: EventType::Move,
        item_id: ItemId::new_unchecked(&resolved_id),
        data: EventData::Move(move_data),
        event_hash: String::new(),
    };

    {
        use bones_core::lock::ShardLock;
        let lock_path = shard_mgr.lock_path();
        let _lock = ShardLock::acquire(&lock_path, Duration::from_secs(5))
            .map_err(|e| anyhow::anyhow!("failed to acquire lock: {e}"))?;

        let (year, month) = shard_mgr
            .rotate_if_needed()
            .map_err(|e| anyhow::anyhow!("failed to rotate shards: {e}"))?;

        event.wall_ts_us = shard_mgr
            .next_timestamp()
            .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

        assign_next_itc(project_root, &mut event)?;

        let line = writer::write_event(&mut event)
            .map_err(|e| anyhow::anyhow!("failed to serialize event: {e}"))?;

        shard_mgr
            .append_raw(year, month, &line)
            .map_err(|e| anyhow::anyhow!("failed to write event: {e}"))?;
    }

    let projector = project::Projector::new(conn);
    if let Err(e) = projector.project_event(&event) {
        tracing::warn!("projection failed (will be fixed on next rebuild): {e}");
    }

    Ok(UnstartOutput {
        id: resolved_id,
        previous_state: current_state.to_string(),
        new_state: State::Open.to_string(),
        agent: agent.to_string(),
        event_hash: event.event_hash,
    })
}

fn item_ids(args: &UnstartArgs) -> impl Iterator<Item = &str> {
    std::iter::once(args.id.as_str()).chain(args.ids.iter().map(String::as_str))
}

pub fn run_unstart(
    args: &UnstartArgs,
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
        anyhow::anyhow!("{msg}")
    })?;

    let conn = open_projection_for_mutation(&bones_dir)?;
    let shard_mgr = ShardManager::new(&bones_dir);

    let mut results = Vec::new();
    let mut failures = Vec::new();

    for raw_id in item_ids(args) {
        match run_unstart_single(project_root, &conn, &shard_mgr, &agent, raw_id) {
            Ok(ok) => results.push(UnstartResult {
                id: ok.id,
                ok: true,
                previous_state: Some(ok.previous_state),
                new_state: Some(ok.new_state),
                event_hash: Some(ok.event_hash),
                error: None,
            }),
            Err(err) => {
                failures.push(err.to_string());
                results.push(UnstartResult {
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

    let payload = UnstartBatchOutput { results };

    render(output, &payload, |r, w| {
        writeln!(w, "{:<4}  {:<16}  TRANSITION", "OK", "ID")?;
        for result in &r.results {
            if result.ok {
                writeln!(
                    w,
                    "ok    {:<16}  {} -> open",
                    result.id,
                    result.previous_state.as_deref().unwrap_or("unknown")
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
        args: UnstartArgs,
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

        let item_id = "bn-unstart1";
        let ts = shard_mgr.next_timestamp().unwrap();

        let mut create_event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: "Unstart test".to_string(),
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
    fn unstart_args_parses_id() {
        let w = Wrapper::parse_from(["test", "item-999"]);
        assert_eq!(w.args.id, "item-999");
    }

    #[test]
    fn unstart_from_doing() {
        let (dir, item_id) = setup_project("doing");
        let args = UnstartArgs {
            id: item_id.clone(),
            ids: vec![],
        };
        let result = run_unstart(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "unstart failed: {:?}", result.err());

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.state, "open");
    }

    #[test]
    fn unstart_rejects_already_open() {
        let (dir, item_id) = setup_project("open");
        let args = UnstartArgs {
            id: item_id,
            ids: vec![],
        };
        let result = run_unstart(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("already open"), "unexpected error: {err}");
    }

    #[test]
    fn unstart_rejects_done() {
        let (dir, item_id) = setup_project("done");
        let args = UnstartArgs {
            id: item_id,
            ids: vec![],
        };
        let result = run_unstart(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("done") && err.contains("bn reopen"),
            "expected hint to bn reopen: {err}"
        );
    }

    #[test]
    fn unstart_rejects_archived() {
        let (dir, item_id) = setup_project("archived");
        let args = UnstartArgs {
            id: item_id,
            ids: vec![],
        };
        let result = run_unstart(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("archived") && err.contains("bn reopen"),
            "expected hint to bn reopen: {err}"
        );
    }

    #[test]
    fn unstart_partial_id() {
        let (dir, _) = setup_project("doing");
        let args = UnstartArgs {
            id: "unstart1".to_string(),
            ids: vec![],
        };
        let result = run_unstart(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(
            result.is_ok(),
            "unstart via partial ID failed: {:?}",
            result.err()
        );

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, "bn-unstart1", false)
            .unwrap()
            .unwrap();
        assert_eq!(item.state, "open");
    }

    #[test]
    fn unstart_event_carries_unstart_flag() {
        let (dir, item_id) = setup_project("doing");
        let args = UnstartArgs {
            id: item_id,
            ids: vec![],
        };
        run_unstart(&args, Some("test-agent"), OutputMode::Json, dir.path()).unwrap();

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
        assert!(
            fields[6].contains("\"open\""),
            "should contain open state: {}",
            fields[6]
        );
        assert!(
            fields[6].contains("unstart"),
            "should carry unstart flag: {}",
            fields[6]
        );
    }

    #[test]
    fn unstart_rejects_nonexistent_item() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();
        let db_path = bones_dir.join("bones.db");
        let _conn = db::open_projection(&db_path).unwrap();

        let args = UnstartArgs {
            id: "bn-nonexistent".to_string(),
            ids: vec![],
        };
        let result = run_unstart(&args, Some("test-agent"), OutputMode::Json, root);
        assert!(result.is_err());
    }

    #[test]
    fn unstart_not_bones_project() {
        let dir = TempDir::new().unwrap();
        let args = UnstartArgs {
            id: "bn-test".to_string(),
            ids: vec![],
        };
        let result = run_unstart(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn unstart_then_redo_lands_back_on_doing() {
        // Verify the doing -> open -> doing cycle works cleanly.
        let (dir, item_id) = setup_project("doing");
        let root = dir.path();

        let unstart_args = UnstartArgs {
            id: item_id.clone(),
            ids: vec![],
        };
        run_unstart(&unstart_args, Some("test-agent"), OutputMode::Json, root).unwrap();

        let do_args = super::super::do_cmd::DoArgs {
            id: item_id.clone(),
            ids: vec![],
        };
        super::super::do_cmd::run_do(&do_args, Some("test-agent"), OutputMode::Json, root).unwrap();

        let db_path = root.join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.state, "doing", "should be doing after redo");
    }
}
