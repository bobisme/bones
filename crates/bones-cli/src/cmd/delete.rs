//! `bn delete` — soft-delete an item via tombstone event.
//!
//! Emits `item.delete` with optional reason. Deleted items remain in the
//! append-only event log but are excluded from active views.

use crate::agent;
use crate::cmd::show::resolve_item_id;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;
use bones_core::db;
use bones_core::db::project;
use bones_core::db::query;
use bones_core::event::Event;
use bones_core::event::data::{DeleteData, EventData};
use bones_core::event::types::EventType;
use bones_core::event::writer;
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;
use clap::Args;
use rusqlite::params;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct DeleteArgs {
    /// Item ID to delete (supports partial IDs).
    pub id: String,

    /// Optional reason for deletion.
    #[arg(long)]
    pub reason: Option<String>,

    /// Skip interactive confirmation prompt.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Serialize)]
struct DeleteOutput {
    id: String,
    deleted: bool,
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

fn resolve_any_item_id(conn: &rusqlite::Connection, input: &str) -> anyhow::Result<Option<String>> {
    let input = input.trim();

    let exact: Option<String> = conn
        .query_row(
            "SELECT item_id FROM items WHERE item_id = ?1 LIMIT 1",
            params![input],
            |row| row.get(0),
        )
        .ok();
    if exact.is_some() {
        return Ok(exact);
    }

    if !input.starts_with("bn-") {
        let with_prefix = format!("bn-{input}");
        let exact2: Option<String> = conn
            .query_row(
                "SELECT item_id FROM items WHERE item_id = ?1 LIMIT 1",
                params![with_prefix],
                |row| row.get(0),
            )
            .ok();
        if exact2.is_some() {
            return Ok(exact2);
        }

        let like_pattern = format!("bn-{input}%");
        let prefix: Option<String> = conn
            .query_row(
                "SELECT item_id FROM items WHERE item_id LIKE ?1 ORDER BY item_id LIMIT 1",
                params![like_pattern],
                |row| row.get(0),
            )
            .ok();
        if prefix.is_some() {
            return Ok(prefix);
        }
    } else {
        let like_pattern = format!("{input}%");
        let prefix: Option<String> = conn
            .query_row(
                "SELECT item_id FROM items WHERE item_id LIKE ?1 ORDER BY item_id LIMIT 1",
                params![like_pattern],
                |row| row.get(0),
            )
            .ok();
        if prefix.is_some() {
            return Ok(prefix);
        }
    }

    Ok(None)
}

fn confirm_delete(id: &str, title: &str) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(true);
    }

    eprint!("Delete {} '{}'? [y/N] ", id, title);
    std::io::stderr().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

pub fn run_delete(
    args: &DeleteArgs,
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

    if let Err(e) = validate::validate_item_id(&args.id) {
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

    let resolved_id = match resolve_item_id(&conn, &args.id)? {
        Some(id) => id,
        None => {
            if let Some(any_id) = resolve_any_item_id(&conn, &args.id)?
                && let Some(item) = query::get_item(&conn, &any_id, true)?
                && item.is_deleted
            {
                let msg = format!("item '{}' is already deleted", item.item_id);
                render_error(
                    output,
                    &CliError::with_details(
                        &msg,
                        "Deleted items can still be inspected with 'bn show <id>'",
                        "already_deleted",
                    ),
                )?;
                anyhow::bail!("{}", msg);
            }

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

    if !args.force && !confirm_delete(&resolved_id, &item.title)? {
        let msg = "deletion cancelled";
        render_error(
            output,
            &CliError::with_details(msg, "Use --force to skip confirmation", "cancelled"),
        )?;
        anyhow::bail!("{}", msg);
    }

    let shard_mgr = ShardManager::new(&bones_dir);
    let ts = shard_mgr
        .next_timestamp()
        .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

    let mut event = Event {
        wall_ts_us: ts,
        agent,
        itc: "itc:AQ".to_string(),
        parents: vec![],
        event_type: EventType::Delete,
        item_id: ItemId::new_unchecked(&resolved_id),
        data: EventData::Delete(DeleteData {
            reason: args.reason.clone(),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };

    let line = writer::write_event(&mut event)
        .map_err(|e| anyhow::anyhow!("failed to serialize event: {e}"))?;

    shard_mgr
        .append(&line, false, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("failed to write event: {e}"))?;

    let projector = project::Projector::new(&conn);
    if let Err(e) = projector.project_event(&event) {
        tracing::warn!("projection failed (will be fixed on next rebuild): {e}");
    }

    render(
        output,
        &DeleteOutput {
            id: resolved_id,
            deleted: true,
        },
        |r, w| {
            writeln!(w, "✓ deleted {}", r.id)?;
            Ok(())
        },
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db;
    use bones_core::db::project;
    use bones_core::db::query;
    use bones_core::event::Event;
    use bones_core::event::data::{CreateData, EventData};
    use bones_core::event::types::EventType;
    use bones_core::event::writer;
    use bones_core::model::item::{Kind, Urgency};
    use bones_core::model::item_id::ItemId;
    use clap::Parser;
    use tempfile::TempDir;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: DeleteArgs,
    }

    fn setup_project() -> (TempDir, String) {
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

        let item_id = "bn-del1";
        let ts = shard_mgr.next_timestamp().unwrap();

        let mut create_event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: "Delete me".to_string(),
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

        (dir, item_id.to_string())
    }

    #[test]
    fn delete_args_parse() {
        let w = Wrapper::parse_from(["bn", "bn-123", "--reason", "duplicate", "--force"]);
        assert_eq!(w.args.id, "bn-123");
        assert_eq!(w.args.reason.as_deref(), Some("duplicate"));
        assert!(w.args.force);
    }

    #[test]
    fn delete_marks_item_deleted() {
        let (dir, item_id) = setup_project();
        let args = DeleteArgs {
            id: item_id.clone(),
            reason: Some("duplicate".to_string()),
            force: true,
        };

        let result = run_delete(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "delete failed: {:?}", result.err());

        let conn = db::open_projection(&dir.path().join(".bones/bones.db")).unwrap();
        let item = query::get_item(&conn, &item_id, true).unwrap().unwrap();
        assert!(item.is_deleted);
    }

    #[test]
    fn delete_rejects_already_deleted_item() {
        let (dir, item_id) = setup_project();
        let args = DeleteArgs {
            id: item_id.clone(),
            reason: None,
            force: true,
        };
        run_delete(&args, Some("test-agent"), OutputMode::Json, dir.path()).unwrap();

        let second = run_delete(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(second.is_err());
        assert!(second.unwrap_err().to_string().contains("already deleted"));
    }
}
