//! `bn assign` / `bn unassign` — manage assignees for work items.
//!
//! - `bn assign <id> <agent>` emits `item.assign` with `action=assign`
//! - `bn unassign <id>` emits `item.assign` with `action=unassign` for the
//!   currently resolved command agent.

use crate::agent;
use crate::cmd::show::resolve_item_id;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;
use bones_core::db;
use bones_core::db::project;
use bones_core::event::Event;
use bones_core::event::data::{AssignAction, AssignData, EventData};
use bones_core::event::types::EventType;
use bones_core::event::writer;
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;
use clap::Args;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct AssignArgs {
    /// Item ID to assign (supports partial IDs).
    pub id: String,

    /// Agent to assign to this item.
    #[arg(value_name = "ASSIGNEE")]
    pub assignee: String,

    /// Additional item IDs to assign the same agent to.
    #[arg(long = "ids", value_name = "ID", num_args = 1..)]
    pub additional_ids: Vec<String>,
}

#[derive(Args, Debug)]
pub struct UnassignArgs {
    /// Item ID to unassign from the current agent (supports partial IDs).
    pub id: String,

    /// Additional item IDs to unassign.
    #[arg(value_name = "ID")]
    pub ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AssignOutput {
    ok: bool,
    item_id: String,
    agent: String,
    action: String,
    event_hash: String,
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

fn resolve_existing_item_id(
    conn: &rusqlite::Connection,
    raw_id: &str,
    output: OutputMode,
) -> anyhow::Result<String> {
    match resolve_item_id(conn, raw_id)? {
        Some(id) => Ok(id),
        None => {
            let msg = format!("item '{raw_id}' not found");
            render_error(
                output,
                &CliError::with_details(
                    &msg,
                    "Check the item ID with 'bn list' or 'bn show'",
                    "item_not_found",
                ),
            )?;
            anyhow::bail!(msg);
        }
    }
}

fn emit_assign_event(
    bones_dir: &Path,
    actor: &str,
    item_id: &str,
    assignee: &str,
    action: AssignAction,
) -> anyhow::Result<Event> {
    let shard_mgr = ShardManager::new(bones_dir);
    let ts = shard_mgr
        .next_timestamp()
        .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

    let mut event = Event {
        wall_ts_us: ts,
        agent: actor.to_string(),
        itc: "itc:AQ".to_string(),
        parents: vec![],
        event_type: EventType::Assign,
        item_id: ItemId::new_unchecked(item_id),
        data: EventData::Assign(AssignData {
            agent: assignee.to_string(),
            action,
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };

    let line = writer::write_event(&mut event)
        .map_err(|e| anyhow::anyhow!("failed to serialize event: {e}"))?;

    shard_mgr
        .append(&line, false, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("failed to write event: {e}"))?;

    Ok(event)
}

fn run_assign_action(
    raw_item_id: &str,
    assignee: &str,
    action: AssignAction,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let actor = match agent::require_agent(agent_flag) {
        Ok(a) => a,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(
                    &e.message,
                    "Set --agent, BONES_AGENT, AGENT, or USER (interactive only)",
                    e.code,
                ),
            )?;
            anyhow::bail!(e.message);
        }
    };

    if let Err(e) = validate::validate_agent(&actor) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!(e.reason);
    }
    if let Err(e) = validate::validate_agent(assignee) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!(e.reason);
    }
    if let Err(e) = validate::validate_item_id(raw_item_id) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!(e.reason);
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
        anyhow::anyhow!(msg)
    })?;

    let db_path = bones_dir.join("bones.db");
    let conn = db::open_projection(&db_path)?;
    let _ = project::ensure_tracking_table(&conn);

    let item_id = resolve_existing_item_id(&conn, raw_item_id, output)?;

    let event = emit_assign_event(&bones_dir, &actor, &item_id, assignee, action)?;

    let projector = project::Projector::new(&conn);
    if let Err(e) = projector.project_event(&event) {
        tracing::warn!("projection failed (will be fixed on rebuild): {e}");
    }

    let result = AssignOutput {
        ok: true,
        item_id: item_id.clone(),
        agent: assignee.to_string(),
        action: action.to_string(),
        event_hash: event.event_hash,
    };

    render(output, &result, |r, w| match r.action.as_str() {
        "assign" => writeln!(w, "✓ {}: assigned {}", r.item_id, r.agent),
        "unassign" => writeln!(w, "✓ {}: unassigned {}", r.item_id, r.agent),
        _ => writeln!(w, "✓ {}: {} {}", r.item_id, r.action, r.agent),
    })?;

    Ok(())
}

fn assign_item_ids(args: &AssignArgs) -> impl Iterator<Item = &str> {
    std::iter::once(args.id.as_str()).chain(args.additional_ids.iter().map(String::as_str))
}

pub fn run_assign(
    args: &AssignArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let mut failures = Vec::new();
    for raw_id in assign_item_ids(args) {
        if let Err(e) = run_assign_action(
            raw_id,
            &args.assignee,
            AssignAction::Assign,
            agent_flag,
            output,
            project_root,
        ) {
            failures.push(format!("{raw_id}: {e}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else if failures.len() == 1 {
        anyhow::bail!("{}", failures[0]);
    } else {
        anyhow::bail!("{} item(s) failed", failures.len());
    }
}

fn unassign_item_ids(args: &UnassignArgs) -> impl Iterator<Item = &str> {
    std::iter::once(args.id.as_str()).chain(args.ids.iter().map(String::as_str))
}

pub fn run_unassign(
    args: &UnassignArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let resolved = match agent::require_agent(agent_flag) {
        Ok(a) => a,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(
                    &e.message,
                    "Set --agent, BONES_AGENT, AGENT, or USER (interactive only)",
                    e.code,
                ),
            )?;
            anyhow::bail!(e.message);
        }
    };

    let mut failures = Vec::new();
    for raw_id in unassign_item_ids(args) {
        if let Err(e) = run_assign_action(
            raw_id,
            &resolved,
            AssignAction::Unassign,
            Some(resolved.as_str()),
            output,
            project_root,
        ) {
            failures.push(format!("{raw_id}: {e}"));
        }
    }
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
    use bones_core::db::query;
    use bones_core::event::data::{CreateData, EventData};
    use bones_core::event::types::EventType;
    use bones_core::event::writer::write_event;
    use bones_core::model::item::Kind;
    use bones_core::model::item::Urgency;
    use std::time::Duration;
    use tempfile::TempDir;

    fn setup_project() -> (TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let bones_dir = root.join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init shard");

        let db_path = bones_dir.join("bones.db");
        let conn = db::open_projection(&db_path).expect("open projection");
        let _ = project::ensure_tracking_table(&conn);
        let projector = project::Projector::new(&conn);

        let item_id = "bn-asg1";
        let ts = shard_mgr.next_timestamp().expect("timestamp");
        let mut event = Event {
            wall_ts_us: ts,
            agent: "seed-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: "Assignment test".to_string(),
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

        let line = write_event(&mut event).expect("serialize create");
        shard_mgr
            .append(&line, false, Duration::from_secs(5))
            .expect("append create");
        projector.project_event(&event).expect("project create");

        (dir, item_id.to_string())
    }

    #[test]
    fn assign_args_parse() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: AssignArgs,
        }

        let w = Wrapper::parse_from(["test", "bn-abc", "alice"]);
        assert_eq!(w.args.id, "bn-abc");
        assert_eq!(w.args.assignee, "alice");
    }

    #[test]
    fn unassign_args_parse() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: UnassignArgs,
        }

        let w = Wrapper::parse_from(["test", "bn-abc"]);
        assert_eq!(w.args.id, "bn-abc");
    }

    #[test]
    fn assign_and_unassign_roundtrip() {
        let (dir, item_id) = setup_project();

        run_assign(
            &AssignArgs {
                id: item_id.clone(),
                assignee: "alice".to_string(),
                additional_ids: vec![],
            },
            Some("operator"),
            OutputMode::Json,
            dir.path(),
        )
        .expect("assign should succeed");

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).expect("open db");
        let assignees = query::get_assignees(&conn, &item_id).expect("query assignees");
        assert_eq!(assignees.len(), 1);
        assert_eq!(assignees[0].agent, "alice");

        run_unassign(
            &UnassignArgs {
                id: item_id.clone(),
                ids: vec![],
            },
            Some("alice"),
            OutputMode::Json,
            dir.path(),
        )
        .expect("unassign should succeed");

        let assignees_after = query::get_assignees(&conn, &item_id).expect("query assignees");
        assert!(assignees_after.is_empty());
    }

    #[test]
    fn assign_supports_partial_item_id() {
        let (dir, _item_id) = setup_project();

        run_assign(
            &AssignArgs {
                id: "asg1".to_string(),
                assignee: "alice".to_string(),
                additional_ids: vec![],
            },
            Some("operator"),
            OutputMode::Json,
            dir.path(),
        )
        .expect("assign should resolve partial id");

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).expect("open db");
        let assignees = query::get_assignees(&conn, "bn-asg1").expect("query assignees");
        assert_eq!(assignees.len(), 1);
        assert_eq!(assignees[0].agent, "alice");
    }
}
