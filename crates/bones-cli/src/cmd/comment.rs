//! `bn comment` and `bn comments` — append and inspect item comment timelines.

use crate::agent;
use crate::cmd::show::resolve_item_id;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;
use bones_core::db::project;
use bones_core::db::query;
use bones_core::event::data::{CommentData, EventData};
use bones_core::event::writer::write_event;
use bones_core::event::{Event, EventType};
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

const MAX_COMMENT_BODY_CHARS: usize = 8_192;

#[derive(Args, Debug)]
pub struct CommentArgs {
    #[command(subcommand)]
    pub command: CommentCommand,
}

#[derive(Subcommand, Debug)]
pub enum CommentCommand {
    #[command(
        about = "Add a comment to a work item",
        after_help = "EXAMPLES:\n    # Add a progress note\n    bn comment add bn-abc \"Investigating timeout path\"\n\n    # Add with explicit agent\n    bn --agent alice comment add bn-abc \"Root cause found\""
    )]
    Add(CommentAddArgs),
}

#[derive(Args, Debug)]
pub struct CommentAddArgs {
    /// Item ID to comment on (supports partial IDs).
    pub id: String,

    /// Comment body.
    pub body: String,
}

#[derive(Args, Debug)]
pub struct CommentsArgs {
    /// Item ID to show comments for (supports partial IDs).
    pub id: String,
}

#[derive(Debug, Serialize)]
struct CommentAddOutput {
    ok: bool,
    item_id: String,
    agent: String,
    body: String,
    ts: i64,
    event_hash: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct CommentRow {
    hash: String,
    agent: String,
    body: String,
    ts: i64,
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

fn micros_to_rfc3339(us: i64) -> String {
    DateTime::<Utc>::from_timestamp_micros(us)
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_else(|| us.to_string())
}

fn validate_comment_body(body: &str) -> anyhow::Result<()> {
    if body.trim().is_empty() {
        anyhow::bail!("comment body must not be empty");
    }

    if body.chars().count() > MAX_COMMENT_BODY_CHARS {
        anyhow::bail!(
            "comment body must be <= {MAX_COMMENT_BODY_CHARS} characters (got {})",
            body.chars().count()
        );
    }

    if body
        .chars()
        .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
    {
        anyhow::bail!("comment body must not contain control characters");
    }

    Ok(())
}

fn timeline_rows(mut comments: Vec<query::QueryComment>) -> Vec<CommentRow> {
    // Query returns newest-first; timeline command should show oldest-first.
    comments.sort_by(|a, b| {
        a.created_at_us
            .cmp(&b.created_at_us)
            .then_with(|| a.comment_id.cmp(&b.comment_id))
    });

    comments
        .into_iter()
        .map(|c| CommentRow {
            hash: c.event_hash,
            agent: c.author,
            body: c.body,
            ts: c.created_at_us,
        })
        .collect()
}

pub fn run_comment(
    args: &CommentArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    match &args.command {
        CommentCommand::Add(add) => run_comment_add(add, agent_flag, output, project_root),
    }
}

fn run_comment_add(
    args: &CommentAddArgs,
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
    if let Err(e) = validate_comment_body(&args.body) {
        let msg = e.to_string();
        render_error(
            output,
            &CliError::with_details(
                &msg,
                "Use plain UTF-8 text and keep within size limit",
                "invalid_comment",
            ),
        )?;
        anyhow::bail!("{}", msg);
    }

    let bones_dir = find_bones_dir(project_root).ok_or_else(|| {
        let msg = "Not a bones project: .bones directory not found";
        let _ = render_error(
            output,
            &CliError::with_details(
                msg,
                "Run 'bn init' to create a new project",
                "not_a_project",
            ),
        );
        anyhow::anyhow!(msg)
    })?;

    let db_path = bones_dir.join("bones.db");
    let conn = match query::try_open_projection(&db_path)? {
        Some(conn) => conn,
        None => {
            let msg = format!(
                "projection database not found or corrupt at {}",
                db_path.display()
            );
            render_error(
                output,
                &CliError::with_details(
                    &msg,
                    "Run `bn rebuild` to initialize the projection",
                    "projection_missing",
                ),
            )?;
            anyhow::bail!("{}", msg);
        }
    };

    let resolved_id = match resolve_item_id(&conn, &args.id)? {
        Some(id) => id,
        None => {
            let msg = format!("item '{}' not found", args.id);
            render_error(
                output,
                &CliError::with_details(&msg, "Check the item ID with `bn list`", "item_not_found"),
            )?;
            anyhow::bail!("{}", msg);
        }
    };

    let item_id = ItemId::new_unchecked(&resolved_id);

    let shard_mgr = ShardManager::new(&bones_dir);
    let ts = shard_mgr
        .next_timestamp()
        .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

    let mut event = Event {
        wall_ts_us: ts,
        agent: agent.clone(),
        itc: "itc:AQ".to_string(),
        parents: vec![],
        event_type: EventType::Comment,
        item_id,
        data: EventData::Comment(CommentData {
            body: args.body.clone(),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };

    let line =
        write_event(&mut event).map_err(|e| anyhow::anyhow!("failed to serialize event: {e}"))?;

    shard_mgr
        .append(&line, false, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("failed to write event: {e}"))?;

    let _ = project::ensure_tracking_table(&conn);
    let projector = project::Projector::new(&conn);
    if let Err(e) = projector.project_event(&event) {
        tracing::warn!("projection failed (will be fixed on next rebuild): {e}");
    }

    let result = CommentAddOutput {
        ok: true,
        item_id: resolved_id,
        agent,
        body: args.body.clone(),
        ts,
        event_hash: event.event_hash,
    };

    render(output, &result, |r, w| {
        writeln!(w, "✓ {}: comment added", r.item_id)
    })
}

pub fn run_comments(
    args: &CommentsArgs,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    if let Err(e) = validate::validate_item_id(&args.id) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }

    let bones_dir = find_bones_dir(project_root).ok_or_else(|| {
        let msg = "Not a bones project: .bones directory not found";
        let _ = render_error(
            output,
            &CliError::with_details(
                msg,
                "Run 'bn init' to create a new project",
                "not_a_project",
            ),
        );
        anyhow::anyhow!(msg)
    })?;

    let db_path = bones_dir.join("bones.db");
    let conn = match query::try_open_projection(&db_path)? {
        Some(conn) => conn,
        None => {
            let msg = format!(
                "projection database not found or corrupt at {}",
                db_path.display()
            );
            render_error(
                output,
                &CliError::with_details(
                    &msg,
                    "Run `bn rebuild` to initialize the projection",
                    "projection_missing",
                ),
            )?;
            anyhow::bail!("{}", msg);
        }
    };

    let resolved_id = match resolve_item_id(&conn, &args.id)? {
        Some(id) => id,
        None => {
            let msg = format!("item '{}' not found", args.id);
            render_error(
                output,
                &CliError::with_details(&msg, "Check the item ID with `bn list`", "item_not_found"),
            )?;
            anyhow::bail!("{}", msg);
        }
    };

    let comments = timeline_rows(query::get_comments(&conn, &resolved_id)?);

    render(output, &comments, |rows, w| {
        if rows.is_empty() {
            writeln!(w, "(no comments for {})", resolved_id)?;
            return Ok(());
        }

        writeln!(w, "Comments for {}:", resolved_id)?;
        for row in rows {
            writeln!(
                w,
                "- [{}] {}: {}",
                micros_to_rfc3339(row.ts),
                row.agent,
                row.body
            )?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputMode;
    use bones_core::db::rebuild;
    use bones_core::event::data::CreateData;
    use bones_core::event::writer::write_event;
    use bones_core::event::{Event, EventData, EventType};
    use bones_core::model::item::{Kind, Urgency};
    use bones_core::shard::ShardManager;
    use clap::Parser;

    #[derive(Parser)]
    struct CommentWrapper {
        #[command(subcommand)]
        cmd: CommentCommand,
    }

    #[derive(Parser)]
    struct CommentsWrapper {
        #[command(flatten)]
        args: CommentsArgs,
    }

    fn setup_test_project() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().to_path_buf();
        let bones_dir = root.join(".bones");

        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init shard");

        let item_id = "bn-cmt1".to_string();
        let ts = shard_mgr.next_timestamp().expect("timestamp");

        let mut event = Event {
            wall_ts_us: ts,
            agent: "seed-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(&item_id),
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

        let line = write_event(&mut event).expect("serialize create event");
        shard_mgr
            .append(&line, false, Duration::from_secs(5))
            .expect("append create event");

        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild projection");

        (dir, root, item_id)
    }

    #[test]
    fn comment_add_args_parse() {
        let parsed =
            CommentWrapper::parse_from(["test", "add", "bn-abc", "Investigating auth timeout"]);

        match parsed.cmd {
            CommentCommand::Add(args) => {
                assert_eq!(args.id, "bn-abc");
                assert_eq!(args.body, "Investigating auth timeout");
            }
        }
    }

    #[test]
    fn comments_args_parse() {
        let parsed = CommentsWrapper::parse_from(["test", "bn-abc"]);
        assert_eq!(parsed.args.id, "bn-abc");
    }

    #[test]
    fn validate_comment_body_rejects_control_chars() {
        let body = "bad\u{0007}comment";
        let err = validate_comment_body(body).expect_err("control chars should fail");
        assert!(err.to_string().contains("control characters"));
    }

    #[test]
    fn timeline_rows_sorted_oldest_first() {
        let input = vec![
            query::QueryComment {
                comment_id: 2,
                item_id: "bn-1".into(),
                event_hash: "blake3:b".into(),
                author: "bob".into(),
                body: "second".into(),
                created_at_us: 2_000,
            },
            query::QueryComment {
                comment_id: 1,
                item_id: "bn-1".into(),
                event_hash: "blake3:a".into(),
                author: "alice".into(),
                body: "first".into(),
                created_at_us: 1_000,
            },
        ];

        let out = timeline_rows(input);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].agent, "alice");
        assert_eq!(out[1].agent, "bob");
    }

    #[test]
    fn run_comment_add_projects_comment() {
        let (_dir, root, item_id) = setup_test_project();

        let args = CommentAddArgs {
            id: item_id.clone(),
            body: "Root cause found".to_string(),
        };

        run_comment_add(&args, Some("alice"), OutputMode::Json, &root)
            .expect("comment add should succeed");

        let db_path = root.join(".bones/bones.db");
        let conn = query::try_open_projection(&db_path)
            .expect("open projection")
            .expect("projection exists");

        let comments = query::get_comments(&conn, &item_id).expect("load comments");
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "alice");
        assert_eq!(comments[0].body, "Root cause found");
    }

    #[test]
    fn run_comment_add_accepts_partial_id() {
        let (_dir, root, _item_id) = setup_test_project();

        let args = CommentAddArgs {
            id: "cmt1".to_string(),
            body: "Using partial id".to_string(),
        };

        run_comment_add(&args, Some("alice"), OutputMode::Json, &root)
            .expect("comment add with partial id should succeed");

        let db_path = root.join(".bones/bones.db");
        let conn = query::try_open_projection(&db_path)
            .expect("open projection")
            .expect("projection exists");
        let comments = query::get_comments(&conn, "bn-cmt1").expect("load comments");
        assert_eq!(comments.len(), 1);
    }

    #[test]
    fn run_comments_succeeds() {
        let (_dir, root, item_id) = setup_test_project();

        let add = CommentAddArgs {
            id: item_id.clone(),
            body: "first".to_string(),
        };
        run_comment_add(&add, Some("alice"), OutputMode::Json, &root).expect("add comment");

        let args = CommentsArgs { id: item_id };
        run_comments(&args, OutputMode::Json, &root).expect("comments listing should succeed");
    }
}
