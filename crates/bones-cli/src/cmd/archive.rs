//! `bn archive` — transition done items to archived state.
//!
//! Supports two modes:
//! - `bn archive <id>`: archive one item (done -> archived)
//! - `bn archive --auto [--days N]`: archive done items older than N days

use crate::agent;
use crate::cmd::show::resolve_item_id;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;
use clap::Args;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
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
pub struct ArchiveArgs {
    /// Item ID to archive. Omit when using --auto.
    pub id: Option<String>,

    /// Bulk-archive done items older than N days.
    #[arg(long)]
    pub auto: bool,

    /// Days threshold for --auto. Defaults to [archive].auto_days in
    /// .bones/config.toml, or 30 when not configured.
    #[arg(long)]
    pub days: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ArchiveOutput {
    id: String,
    previous_state: String,
    new_state: String,
    agent: String,
    event_hash: String,
}

#[derive(Debug, Serialize)]
struct ArchiveAutoOutput {
    archived_count: usize,
    days: u32,
    archived_ids: Vec<String>,
    agent: String,
}

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

fn read_auto_days_from_config(bones_dir: &Path) -> Option<u32> {
    let config_path = bones_dir.join("config.toml");
    let content = fs::read_to_string(config_path).ok()?;

    let mut in_archive = false;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            in_archive = line == "[archive]";
            continue;
        }

        if !in_archive {
            continue;
        }

        if let Some((key, value_raw)) = line.split_once('=') {
            if key.trim() != "auto_days" {
                continue;
            }

            let value = value_raw
                .split('#')
                .next()
                .unwrap_or_default()
                .trim()
                .replace('_', "");
            if let Ok(days) = value.parse::<u32>() {
                return Some(days);
            }
        }
    }

    None
}

fn resolve_days(args: &ArchiveArgs, bones_dir: &Path) -> u32 {
    args.days
        .or_else(|| read_auto_days_from_config(bones_dir))
        .unwrap_or(30)
}

fn append_archive_event(
    shard_mgr: &ShardManager,
    conn: &rusqlite::Connection,
    agent: &str,
    item_id: &str,
) -> anyhow::Result<String> {
    let ts = shard_mgr
        .next_timestamp()
        .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

    let move_data = MoveData {
        state: State::Archived,
        reason: None,
        extra: BTreeMap::new(),
    };

    let mut event = Event {
        wall_ts_us: ts,
        agent: agent.to_string(),
        itc: "itc:AQ".to_string(),
        parents: vec![],
        event_type: EventType::Move,
        item_id: ItemId::new_unchecked(item_id),
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

    Ok(event.event_hash)
}

fn run_archive_single(
    id: &str,
    agent: &str,
    output: OutputMode,
    conn: &rusqlite::Connection,
    shard_mgr: &ShardManager,
) -> anyhow::Result<()> {
    if let Err(e) = validate::validate_item_id(id) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }

    let resolved_id = match resolve_item_id(conn, id)? {
        Some(id) => id,
        None => {
            let msg = format!("item '{}' not found", id);
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

    let item = match query::get_item(conn, &resolved_id, false)? {
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
        anyhow::anyhow!("item '{}' has invalid state '{}'", resolved_id, item.state)
    })?;

    if let Err(e) = current_state.can_transition_to(State::Archived) {
        let msg = format!(
            "cannot transition '{}' from {} to archived: {}",
            resolved_id, e.from, e.reason
        );
        let suggestion = match current_state {
            State::Open | State::Doing => {
                "Archive is only valid for done items. Use 'bn done <id>' first".to_string()
            }
            State::Archived => "Item is already archived".to_string(),
            State::Done => "Item can be archived".to_string(),
        };
        render_error(
            output,
            &CliError::with_details(&msg, &suggestion, "invalid_transition"),
        )?;
        anyhow::bail!("{}", msg);
    }

    let event_hash = append_archive_event(shard_mgr, conn, agent, &resolved_id)?;

    let result = ArchiveOutput {
        id: resolved_id,
        previous_state: current_state.to_string(),
        new_state: State::Archived.to_string(),
        agent: agent.to_string(),
        event_hash,
    };

    render(output, &result, |r, w| {
        use std::io::Write;
        writeln!(w, "✓ {} → archived (was {})", r.id, r.previous_state)?;
        Ok(())
    })?;

    Ok(())
}

fn run_archive_auto(
    days: u32,
    agent: &str,
    output: OutputMode,
    conn: &rusqlite::Connection,
    shard_mgr: &ShardManager,
) -> anyhow::Result<()> {
    let now_us = chrono::Utc::now().timestamp_micros();
    let threshold_us = now_us.saturating_sub(days as i64 * 24 * 60 * 60 * 1_000_000);

    let done_items = query::list_items(
        conn,
        &query::ItemFilter {
            state: Some(State::Done.to_string()),
            limit: None,
            ..Default::default()
        },
    )?;

    let mut archived_ids = Vec::new();

    for item in done_items {
        if item.updated_at_us > threshold_us {
            continue;
        }

        append_archive_event(shard_mgr, conn, agent, &item.item_id)?;
        archived_ids.push(item.item_id);
    }

    let result = ArchiveAutoOutput {
        archived_count: archived_ids.len(),
        days,
        archived_ids,
        agent: agent.to_string(),
    };

    render(output, &result, |r, w| {
        use std::io::Write;
        if r.archived_count == 0 {
            writeln!(w, "No done items older than {} day(s) to archive", r.days)?;
        } else {
            writeln!(
                w,
                "✓ Archived {} item(s) older than {} day(s)",
                r.archived_count, r.days
            )?;
        }
        Ok(())
    })?;

    Ok(())
}

pub fn run_archive(
    args: &ArchiveArgs,
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

    if args.auto && args.id.is_some() {
        let msg = "cannot use item ID together with --auto";
        render_error(
            output,
            &CliError::with_details(
                msg,
                "Use either 'bn archive <id>' or 'bn archive --auto [--days N]'",
                "invalid_arguments",
            ),
        )?;
        anyhow::bail!("{msg}");
    }

    if !args.auto && args.id.is_none() {
        let msg = "missing required item ID (or use --auto)";
        render_error(
            output,
            &CliError::with_details(
                msg,
                "Usage: 'bn archive <id>' or 'bn archive --auto [--days N]'",
                "missing_argument",
            ),
        )?;
        anyhow::bail!("{msg}");
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

    if args.auto {
        let days = resolve_days(args, &bones_dir);
        return run_archive_auto(days, &agent, output, &conn, &shard_mgr);
    }

    run_archive_single(
        args.id.as_deref().expect("checked id exists"),
        &agent,
        output,
        &conn,
        &shard_mgr,
    )
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
        args: ArchiveArgs,
    }

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

        let item_id = "bn-archive1";

        // item.create
        let ts1 = shard_mgr.next_timestamp().unwrap();
        let mut create_event = Event {
            wall_ts_us: ts1,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: "Archive Test".to_string(),
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
        let line1 = writer::write_event(&mut create_event).unwrap();
        shard_mgr
            .append(&line1, false, Duration::from_secs(5))
            .unwrap();
        projector.project_event(&create_event).unwrap();

        // Optional state transition path
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
                let line2 = writer::write_event(&mut move_event).unwrap();
                shard_mgr
                    .append(&line2, false, Duration::from_secs(5))
                    .unwrap();
                projector.project_event(&move_event).unwrap();
            }
        }

        (dir, item_id.to_string())
    }

    #[test]
    fn archive_args_parse_manual_mode() {
        let w = Wrapper::parse_from(["bn", "bn-123"]);
        assert_eq!(w.args.id.as_deref(), Some("bn-123"));
        assert!(!w.args.auto);
        assert_eq!(w.args.days, None);
    }

    #[test]
    fn archive_args_parse_auto_mode() {
        let w = Wrapper::parse_from(["bn", "--auto", "--days", "14"]);
        assert!(w.args.auto);
        assert_eq!(w.args.days, Some(14));
        assert!(w.args.id.is_none());
    }

    #[test]
    fn archive_from_done() {
        let (dir, item_id) = setup_project("done");
        let args = ArchiveArgs {
            id: Some(item_id.clone()),
            auto: false,
            days: None,
        };

        let result = run_archive(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "archive failed: {:?}", result.err());

        let conn = db::open_projection(&dir.path().join(".bones/bones.db")).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.state, "archived");
    }

    #[test]
    fn archive_rejects_open() {
        let (dir, item_id) = setup_project("open");
        let args = ArchiveArgs {
            id: Some(item_id),
            auto: false,
            days: None,
        };

        let result = run_archive(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn archive_rejects_archived() {
        let (dir, item_id) = setup_project("archived");
        let args = ArchiveArgs {
            id: Some(item_id),
            auto: false,
            days: None,
        };

        let result = run_archive(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn archive_auto_archives_old_done_items() {
        let (dir, item_id) = setup_project("done");

        let conn = db::open_projection(&dir.path().join(".bones/bones.db")).unwrap();
        conn.execute(
            "UPDATE items SET updated_at_us = ?1 WHERE item_id = ?2",
            rusqlite::params![1_i64, item_id.clone()],
        )
        .unwrap();

        let args = ArchiveArgs {
            id: None,
            auto: true,
            days: Some(30),
        };

        let result = run_archive(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "auto archive failed: {:?}", result.err());

        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.state, "archived");
    }

    #[test]
    fn archive_auto_rejects_with_id() {
        let (dir, item_id) = setup_project("done");
        let args = ArchiveArgs {
            id: Some(item_id),
            auto: true,
            days: Some(30),
        };

        let result = run_archive(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn archive_manual_requires_id() {
        let (dir, _item_id) = setup_project("done");
        let args = ArchiveArgs {
            id: None,
            auto: false,
            days: None,
        };

        let result = run_archive(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn read_days_from_config_archive_section() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".bones")).unwrap();
        std::fs::write(
            dir.path().join(".bones/config.toml"),
            "[archive]\nauto_days = 45\n",
        )
        .unwrap();

        assert_eq!(
            read_auto_days_from_config(&dir.path().join(".bones")),
            Some(45)
        );
    }
}
