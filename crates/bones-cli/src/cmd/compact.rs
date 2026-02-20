//! `bn compact` — lattice-based log compaction for completed items.
//!
//! Replaces event sequences for old done/archived items with a single
//! `item.snapshot` event. Compaction is coordination-free: each replica
//! can compact independently and converge.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use bones_core::compact;
use bones_core::db::query;
use bones_core::event::parser;
use bones_core::event::writer;
use bones_core::shard::ShardManager;
use clap::Args;
use serde::Serialize;

use crate::output::{OutputMode, render, render_error, CliError};

/// Arguments for `bn compact`.
#[derive(Args, Debug)]
pub struct CompactArgs {
    /// Minimum days in done/archived state before an item is compacted.
    #[arg(long, default_value = "30")]
    pub min_age_days: u32,

    /// Perform a dry run: report what would be compacted without writing.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip verification step (not recommended).
    #[arg(long)]
    pub no_verify: bool,

    /// Agent identity for the snapshot events.
    #[arg(long, default_value = "compactor")]
    pub agent: String,
}

/// Output payload for `bn compact`.
#[derive(Debug, Serialize)]
pub struct CompactOutput {
    pub items_compacted: usize,
    pub events_replaced: usize,
    pub snapshots_created: usize,
    pub items_skipped: usize,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
}

/// Execute `bn compact`.
pub fn run_compact(
    args: &CompactArgs,
    output: OutputMode,
    project_root: &Path,
) -> Result<()> {
    let bones_dir = project_root.join(".bones");
    let events_dir = bones_dir.join("events");

    if !events_dir.exists() {
        render_error(
            output,
            &CliError::with_details(
                "no .bones/events directory found".to_string(),
                "run `bn init` first, then create some items",
                "no_events_dir",
            ),
        )?;
        anyhow::bail!("no .bones/events directory");
    }

    // Read all events from all shards.
    let shard_mgr = ShardManager::new(&bones_dir);
    let all_text = shard_mgr
        .replay()
        .context("read event shards")?;

    // Parse events and group by item_id.
    let mut events_by_item: BTreeMap<String, Vec<bones_core::event::Event>> = BTreeMap::new();
    let events = parser::parse_lines(&all_text)
        .map_err(|(line_no, err)| anyhow::anyhow!("parse error at line {line_no}: {err}"))?;

    for event in &events {
        events_by_item
            .entry(event.item_id.as_str().to_string())
            .or_default()
            .push(event.clone());
    }

    // Collect redacted event hashes from the projection DB (if available).
    let redacted_hashes = collect_redacted_hashes(&bones_dir);

    // Current time in microseconds.
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64;

    // Run compaction.
    let (snapshots, report) = compact::compact_items(
        &events_by_item,
        &args.agent,
        args.min_age_days,
        now_us,
        &redacted_hashes,
    );

    // Verify compaction if requested and not a dry run.
    let verification = if !args.dry_run && !args.no_verify && !snapshots.is_empty() {
        let mut all_ok = true;
        for snapshot in &snapshots {
            let item_id = snapshot.item_id.as_str();
            if let Some(original_events) = events_by_item.get(item_id) {
                match compact::verify_compaction(item_id, original_events, snapshot) {
                    Ok(true) => {}
                    Ok(false) => {
                        all_ok = false;
                        eprintln!(
                            "WARN: verification failed for {item_id}: state mismatch"
                        );
                    }
                    Err(e) => {
                        all_ok = false;
                        eprintln!("WARN: verification error for {item_id}: {e}");
                    }
                }
            }
        }
        if all_ok {
            Some("all_passed".to_string())
        } else {
            Some("some_failed".to_string())
        }
    } else if args.dry_run {
        Some("skipped_dry_run".to_string())
    } else if args.no_verify {
        Some("skipped_by_flag".to_string())
    } else {
        None
    };

    // Write snapshot events to the active shard (unless dry run).
    if !args.dry_run && !snapshots.is_empty() {
        // Only write if verification passed (or was skipped).
        let should_write = verification
            .as_ref()
            .is_none_or(|v| v != "some_failed");

        if should_write {
            let (year, month) = shard_mgr.rotate_if_needed()?;
            for snapshot in &snapshots {
                let line = writer::write_line(snapshot)
                    .context("serialize snapshot event")?;
                shard_mgr
                    .append_raw(year, month, &line)
                    .context("append snapshot to shard")?;
            }
        } else {
            eprintln!("Skipping write: verification failures detected");
        }
    }

    let out = CompactOutput {
        items_compacted: report.items_compacted,
        events_replaced: report.events_replaced,
        snapshots_created: report.snapshots_created,
        items_skipped: report.items_skipped,
        dry_run: args.dry_run,
        verification,
    };

    render(output, &out, |out, w| {
        if out.dry_run {
            writeln!(w, "DRY RUN — no events written")?;
        }
        writeln!(w, "Items compacted:  {}", out.items_compacted)?;
        writeln!(w, "Events replaced:  {}", out.events_replaced)?;
        writeln!(w, "Snapshots created: {}", out.snapshots_created)?;
        writeln!(w, "Items skipped:    {}", out.items_skipped)?;
        if let Some(ref verify) = out.verification {
            writeln!(w, "Verification:     {verify}")?;
        }
        Ok(())
    })?;

    Ok(())
}

/// Collect redacted event hashes from the projection database.
///
/// If the DB doesn't exist or can't be read, returns an empty set
/// (conservative: no redactions assumed).
fn collect_redacted_hashes(bones_dir: &Path) -> HashSet<String> {
    let db_path = bones_dir.join("bones.db");
    let mut hashes = HashSet::new();

    if let Ok(Some(conn)) = query::try_open_projection(&db_path) {
        if let Ok(mut stmt) = conn.prepare("SELECT target_event_hash FROM event_redactions") {
            if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                for hash in rows.flatten() {
                    hashes.insert(hash);
                }
            }
        }
    }

    hashes
}
