//! `bn stats` — project reporting dashboard.

use std::io::Write;
use std::path::Path;

use bones_core::db::query;
use clap::Args;
use serde::Serialize;

use crate::output::{CliError, OutputMode, render, render_error};

/// Arguments for `bn stats`.
#[derive(Args, Debug, Default)]
pub struct StatsArgs {}

#[derive(Debug, Serialize)]
struct Velocity {
    opened_7d: usize,
    closed_7d: usize,
    opened_30d: usize,
    closed_30d: usize,
}

#[derive(Debug, Serialize)]
struct Aging {
    avg_open_age_days: f64,
    stale_count_30d: usize,
}

/// Report payload for `bn stats`.
#[derive(Debug, Serialize)]
pub struct ProjectStats {
    pub by_state: std::collections::HashMap<String, usize>,
    pub by_kind: std::collections::HashMap<String, usize>,
    pub by_urgency: std::collections::HashMap<String, usize>,
    pub events_by_type: std::collections::HashMap<String, usize>,
    pub events_by_agent: std::collections::HashMap<String, usize>,
    pub shard_bytes: u64,
    pub velocity: Velocity,
    pub aging: Aging,
}

/// Execute `bn stats`.
pub fn run_stats(
    _args: &StatsArgs,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let db_path = project_root.join(".bones/bones.db");
    let conn = match query::try_open_projection(&db_path)? {
        Some(conn) => conn,
        None => {
            render_error(
                output,
                &CliError::with_details(
                    "projection database not found",
                    "run `bn rebuild` to initialize the projection",
                    "projection_missing",
                ),
            )?;
            anyhow::bail!("projection not found");
        }
    };

    let by_state = query::item_counts_by_state(&conn)?;
    let by_kind = query::item_counts_by_kind(&conn)?;
    let by_urgency = query::item_counts_by_urgency(&conn)?;
    let events_by_type = query::event_counts_by_type(&conn)?;
    let events_by_agent = query::event_counts_by_agent(&conn)?;
    let shard_bytes = shard_directory_bytes(&project_root.join(".bones/events"));

    let now_us = chrono::Utc::now().timestamp_micros();
    let velocity = compute_velocity(&conn, now_us)?;
    let aging = compute_aging(&conn, now_us)?;

    let payload = ProjectStats {
        by_state,
        by_kind,
        by_urgency,
        events_by_type,
        events_by_agent,
        shard_bytes,
        velocity,
        aging,
    };

    render(output, &payload, |payload, w| render_stats_human(payload, w))
}

fn compute_velocity(conn: &rusqlite::Connection, now_us: i64) -> anyhow::Result<Velocity> {
    const DAY_US: i64 = 86_400_000_000;
    let opened_7d = count_items_created_between(conn, now_us - 7 * DAY_US, now_us)?;
    let opened_30d = count_items_created_between(conn, now_us - 30 * DAY_US, now_us)?;
    let closed_7d = count_items_closed_between(conn, now_us - 7 * DAY_US, now_us)?;
    let closed_30d = count_items_closed_between(conn, now_us - 30 * DAY_US, now_us)?;

    Ok(Velocity {
        opened_7d,
        closed_7d,
        opened_30d,
        closed_30d,
    })
}

fn compute_aging(conn: &rusqlite::Connection, now_us: i64) -> anyhow::Result<Aging> {
    const DAY_US: i64 = 86_400_000_000;
    let stale_count_30d = count_open_items_older_than(conn, now_us - 30 * DAY_US)?;

    let (sum_age_us, open_count): (i64, usize) = {
        let mut stmt = conn.prepare(
            "SELECT COALESCE(SUM(?1 - created_at_us), 0), COUNT(*) \
             FROM items \
             WHERE is_deleted = 0 \
               AND state IN ('open', 'doing')",
        )?;

        let (sum, count): (i64, i64) = stmt.query_row([now_us], |row| Ok((row.get(0)?, row.get(1)?)))?;
        (sum, usize::try_from(count).unwrap_or(usize::MAX))
    };

    let avg_open_age_days = if open_count == 0 {
        0.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        let open_count_f64 = open_count as f64;
        #[allow(clippy::cast_precision_loss)]
        let sum_f64 = sum_age_us as f64;
        sum_f64 / open_count_f64 / 86_400_000_000.0
    };

    Ok(Aging {
        avg_open_age_days,
        stale_count_30d,
    })
}

fn count_items_created_between(
    conn: &rusqlite::Connection,
    start_us: i64,
    end_us: i64,
) -> anyhow::Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM items \
         WHERE is_deleted = 0 \
           AND created_at_us >= ?1 \
           AND created_at_us <= ?2",
        [start_us, end_us],
        |row| row.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

fn count_items_closed_between(
    conn: &rusqlite::Connection,
    start_us: i64,
    end_us: i64,
) -> anyhow::Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM items \
         WHERE is_deleted = 0 \
           AND state IN ('done', 'archived') \
           AND updated_at_us >= ?1 \
           AND updated_at_us <= ?2",
        [start_us, end_us],
        |row| row.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

fn count_open_items_older_than(
    conn: &rusqlite::Connection,
    threshold_us: i64,
) -> anyhow::Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM items \
         WHERE is_deleted = 0 \
           AND state IN ('open', 'doing') \
           AND created_at_us < ?1",
        [threshold_us],
        |row| row.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

fn shard_directory_bytes(shards_dir: &Path) -> u64 {
    let mut total = 0_u64;

    let entries = match std::fs::read_dir(shards_dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("events") {
            continue;
        }
        if let Ok(meta) = path.metadata() {
            total = total.saturating_add(meta.len());
        }
    }

    total
}

fn render_sorted_map(map: &std::collections::HashMap<String, usize>) -> Vec<(&str, usize)> {
    let mut entries: Vec<_> = map.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    entries.sort_unstable_by(|(ka, va), (kb, vb)| vb.cmp(va).then_with(|| ka.cmp(kb)));
    entries
}

fn render_stats_human(stats: &ProjectStats, w: &mut dyn Write) -> std::io::Result<()> {
    writeln!(w, "Project reporting")?;

    writeln!(w, "\nItems by state:")?;
    for (state, count) in render_sorted_map(&stats.by_state) {
        writeln!(w, "  {state}: {count}")?;
    }

    writeln!(w, "\nItems by kind:")?;
    for (kind, count) in render_sorted_map(&stats.by_kind) {
        writeln!(w, "  {kind}: {count}")?;
    }

    writeln!(w, "\nItems by urgency:")?;
    for (urgency, count) in render_sorted_map(&stats.by_urgency) {
        writeln!(w, "  {urgency}: {count}")?;
    }

    writeln!(w, "\nVelocity (last 7 / 30 days):")?;
    writeln!(w, "  opened:  {} / {}", stats.velocity.opened_7d, stats.velocity.opened_30d)?;
    writeln!(w, "  closed:  {} / {}", stats.velocity.closed_7d, stats.velocity.closed_30d)?;

    writeln!(w, "\nAging:")?;
    writeln!(w, "  avg open age (days): {:.1}", stats.aging.avg_open_age_days)?;
    writeln!(w, "  stale (>30 days):    {}", stats.aging.stale_count_30d)?;

    writeln!(w, "\nEvents by type:")?;
    for (event_type, count) in render_sorted_map(&stats.events_by_type) {
        writeln!(w, "  {event_type}: {count}")?;
    }

    writeln!(w, "\nEvents by agent:")?;
    for (agent, count) in render_sorted_map(&stats.events_by_agent) {
        writeln!(w, "  {agent}: {count}")?;
    }

    writeln!(w, "\nShard storage: {} bytes", stats.shard_bytes)?;

    Ok(())
}
