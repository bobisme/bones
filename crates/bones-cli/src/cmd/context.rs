//! `bn context` — bundled provider snapshot for chief and other orchestrators.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use bones_core::db::query::{self, ItemFilter, QueryItem, SortOrder};
use bones_core::shard::ShardManager;
use clap::Args;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::cmd::triage_support::{RankedItem, build_triage_snapshot};
use crate::output::{CliError, OutputMode, render_error, render_mode};

const CHIEF_CONTEXT_SCHEMA_VERSION: u32 = 1;
const CONTEXT_COMMAND: &str = "bn context --format json";

/// Arguments for `bn context`.
#[derive(Args, Debug, Default)]
pub struct ContextArgs {}

#[derive(Debug, Serialize)]
struct ContextPayload {
    schema_version: u32,
    generated_at: String,
    provider: &'static str,
    command: &'static str,
    summary: ContextSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    recommended_next: Option<RecommendedNext>,
    blocked: Vec<BlockedItem>,
    active_goals: Vec<GoalItem>,
    provenance: ContextProvenance,
}

#[derive(Debug, Serialize)]
struct ContextSummary {
    open_count: u64,
    doing_count: u64,
    blocked_count: u64,
    stale_count: u64,
}

#[derive(Debug, Serialize)]
struct RecommendedNext {
    id: String,
    title: String,
    kind: String,
    state: String,
    urgency: String,
    score: f64,
    why: Vec<String>,
}

#[derive(Debug, Serialize)]
struct BlockedItem {
    id: String,
    title: String,
    kind: String,
    state: String,
    urgency: String,
    blocked_by: Vec<String>,
}

#[derive(Debug, Serialize)]
struct GoalItem {
    id: String,
    title: String,
    state: String,
    urgency: String,
}

#[derive(Debug, Serialize)]
struct ContextProvenance {
    provider: &'static str,
    command: &'static str,
    generated_at: String,
    projection_schema_version: Option<u32>,
    projection_last_event_offset: Option<i64>,
    projection_last_event_hash: Option<String>,
    projection_last_rebuild_at_us: Option<i64>,
    event_log_bytes: Option<u64>,
}

#[derive(Debug)]
struct ProjectionFreshness {
    schema_version: Option<u32>,
    last_event_offset: Option<i64>,
    last_event_hash: Option<String>,
    last_rebuild_at_us: Option<i64>,
    event_log_bytes: Option<u64>,
}

/// Execute `bn context`.
pub fn run_context(
    _args: &ContextArgs,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let db_path = project_root.join(".bones/bones.db");
    let conn = if let Some(conn) = query::try_open_projection(&db_path)? {
        conn
    } else {
        render_error(
            output,
            &CliError::with_details(
                "projection database not found",
                "run `bn admin rebuild` to initialize the projection",
                "projection_missing",
            ),
        )?;
        anyhow::bail!("projection not found");
    };

    let generated_at = chrono::Utc::now().to_rfc3339();
    let now_us = chrono::Utc::now().timestamp_micros();
    let snapshot = build_triage_snapshot(&conn, now_us)?;

    let open_count = count_state(&conn, "open")?;
    let doing_count = count_state(&conn, "doing")?;
    let blocked_items = blocked_items(&conn, &snapshot.ranked)?;
    let active_goals = active_goals(&conn)?;
    let freshness = projection_freshness(&conn, project_root)?;

    let payload = ContextPayload {
        schema_version: CHIEF_CONTEXT_SCHEMA_VERSION,
        generated_at: generated_at.clone(),
        provider: "bones",
        command: CONTEXT_COMMAND,
        summary: ContextSummary {
            open_count,
            doing_count,
            blocked_count: u64::try_from(blocked_items.len()).unwrap_or(u64::MAX),
            stale_count: u64::try_from(snapshot.stale_in_progress.len()).unwrap_or(u64::MAX),
        },
        recommended_next: recommended_next(
            &snapshot.unblocked_ranked,
            &snapshot.needs_decomposition,
        ),
        blocked: blocked_items,
        active_goals,
        provenance: ContextProvenance {
            provider: "bones",
            command: CONTEXT_COMMAND,
            generated_at,
            projection_schema_version: freshness.schema_version,
            projection_last_event_offset: freshness.last_event_offset,
            projection_last_event_hash: freshness.last_event_hash,
            projection_last_rebuild_at_us: freshness.last_rebuild_at_us,
            event_log_bytes: freshness.event_log_bytes,
        },
    };

    render_mode(
        output,
        &payload,
        |payload, w| render_context_text(payload, w),
        |payload, w| render_context_human(payload, w),
    )
}

fn count_state(conn: &rusqlite::Connection, state: &str) -> anyhow::Result<u64> {
    let filter = ItemFilter {
        state: Some(state.to_string()),
        include_deleted: false,
        ..Default::default()
    };
    query::count_items(conn, &filter)
}

fn recommended_next(
    ranked: &[RankedItem],
    needs_decomposition: &[RankedItem],
) -> Option<RecommendedNext> {
    let needs_decomp: HashSet<&str> = needs_decomposition
        .iter()
        .map(|item| item.id.as_str())
        .collect();

    ranked
        .iter()
        .find(|item| !needs_decomp.contains(item.id.as_str()))
        .map(|item| RecommendedNext {
            id: item.id.clone(),
            title: item.title.clone(),
            kind: item.kind.clone(),
            state: item.state.clone(),
            urgency: item.urgency.to_string(),
            score: item.score,
            why: vec![item.explanation.clone()],
        })
}

fn blocked_items(
    conn: &rusqlite::Connection,
    ranked: &[RankedItem],
) -> anyhow::Result<Vec<BlockedItem>> {
    let mut out = Vec::new();
    for item in ranked
        .iter()
        .filter(|item| item.blocked_by_active > 0)
        .take(25)
    {
        out.push(BlockedItem {
            id: item.id.clone(),
            title: item.title.clone(),
            kind: item.kind.clone(),
            state: item.state.clone(),
            urgency: item.urgency.to_string(),
            blocked_by: active_blockers(conn, &item.id)?,
        });
    }
    Ok(out)
}

fn active_blockers(conn: &rusqlite::Connection, item_id: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT d.depends_on_item_id
         FROM item_dependencies d
         JOIN items blocker ON blocker.item_id = d.depends_on_item_id
         WHERE d.item_id = ?1
           AND d.link_type IN ('blocks', 'blocked_by')
           AND blocker.state NOT IN ('done', 'archived')
           AND blocker.is_deleted = 0
         ORDER BY d.depends_on_item_id",
    )?;
    let rows = stmt.query_map(params![item_id], |row| row.get::<_, String>(0))?;

    let mut blockers = Vec::new();
    for row in rows {
        blockers.push(row?);
    }
    Ok(blockers)
}

fn active_goals(conn: &rusqlite::Connection) -> anyhow::Result<Vec<GoalItem>> {
    let goals = query::list_items(
        conn,
        &ItemFilter {
            kind: Some("goal".to_string()),
            include_deleted: false,
            sort: SortOrder::UpdatedDesc,
            limit: Some(25),
            ..Default::default()
        },
    )?;

    Ok(goals
        .into_iter()
        .filter(is_active_goal)
        .map(|item| GoalItem {
            id: item.item_id,
            title: item.title,
            state: item.state,
            urgency: item.urgency,
        })
        .collect())
}

fn is_active_goal(item: &QueryItem) -> bool {
    matches!(item.state.as_str(), "open" | "doing")
}

fn projection_freshness(
    conn: &rusqlite::Connection,
    project_root: &Path,
) -> anyhow::Result<ProjectionFreshness> {
    let meta = conn
        .query_row(
            "SELECT schema_version, last_event_offset, last_event_hash, last_rebuild_at_us
             FROM projection_meta WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;

    let event_log_bytes = ShardManager::new(project_root.join(".bones"))
        .total_content_len()
        .ok()
        .and_then(|value| u64::try_from(value).ok());

    let Some((schema_version, last_event_offset, last_event_hash, last_rebuild_at_us)) = meta
    else {
        return Ok(ProjectionFreshness {
            schema_version: None,
            last_event_offset: None,
            last_event_hash: None,
            last_rebuild_at_us: None,
            event_log_bytes,
        });
    };

    Ok(ProjectionFreshness {
        schema_version: Some(schema_version),
        last_event_offset: Some(last_event_offset),
        last_event_hash,
        last_rebuild_at_us: Some(last_rebuild_at_us),
        event_log_bytes,
    })
}

fn render_context_human(payload: &ContextPayload, w: &mut dyn Write) -> std::io::Result<()> {
    writeln!(w, "Context")?;
    writeln!(w, "{:-<72}", "")?;
    writeln!(
        w,
        "{} open, {} doing, {} blocked, {} stale",
        payload.summary.open_count,
        payload.summary.doing_count,
        payload.summary.blocked_count,
        payload.summary.stale_count
    )?;
    if let Some(next) = &payload.recommended_next {
        writeln!(w)?;
        writeln!(w, "Next: {} ({})", next.id, next.title)?;
        if let Some(reason) = next.why.first() {
            writeln!(w, "Why: {reason}")?;
        }
    }
    Ok(())
}

fn render_context_text(payload: &ContextPayload, w: &mut dyn Write) -> std::io::Result<()> {
    writeln!(
        w,
        "summary\topen={}\tdoing={}\tblocked={}\tstale={}",
        payload.summary.open_count,
        payload.summary.doing_count,
        payload.summary.blocked_count,
        payload.summary.stale_count
    )?;
    if let Some(next) = &payload.recommended_next {
        writeln!(w, "next\t{}\t{}\t{}", next.id, next.urgency, next.title)?;
    }
    for item in &payload.blocked {
        writeln!(
            w,
            "blocked\t{}\tblocked_by={}\t{}",
            item.id,
            item.blocked_by.join(","),
            item.title
        )?;
    }
    Ok(())
}
