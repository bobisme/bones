use std::path::Path;

use anyhow::Result;

use crate::output::{OutputMode, pretty_kv, pretty_section};

/// Run `bn admin rebuild` and refresh both projection DB and binary cache.
///
/// # Errors
///
/// Returns an error if projection rebuild or cache rebuild fails.
pub fn run_rebuild(project_root: &Path, _incremental: bool, output: OutputMode) -> Result<()> {
    let bones_dir = project_root.join(".bones");
    let events_dir = bones_dir.join("events");
    let db_path = bones_dir.join("bones.db");
    let cache_path = bones_dir.join("cache/events.bin");

    let db_report = bones_core::db::rebuild::rebuild(&events_dir, &db_path)?;
    let cache_stats = bones_core::cache::rebuild_cache(&events_dir, &cache_path)?;

    match output {
        OutputMode::Json => {
            let val = serde_json::json!({
                "projection_events": db_report.event_count,
                "projection_items": db_report.item_count,
                "shards": db_report.shard_count,
                "cache_events": cache_stats.total_events,
                "cache_bytes": cache_stats.file_size_bytes,
            });
            println!("{}", serde_json::to_string_pretty(&val)?);
        }
        OutputMode::Text => {
            println!(
                "rebuild projection_events={} items={} shards={} cache_events={} cache_bytes={}",
                db_report.event_count,
                db_report.item_count,
                db_report.shard_count,
                cache_stats.total_events,
                cache_stats.file_size_bytes
            );
        }
        OutputMode::Pretty => {
            let stdout = std::io::stdout();
            let mut w = stdout.lock();
            pretty_section(&mut w, "Rebuild Complete")?;
            pretty_kv(
                &mut w,
                "Projection events",
                db_report.event_count.to_string(),
            )?;
            pretty_kv(&mut w, "Items", db_report.item_count.to_string())?;
            pretty_kv(&mut w, "Shards", db_report.shard_count.to_string())?;
            pretty_kv(&mut w, "Cache events", cache_stats.total_events.to_string())?;
            pretty_kv(
                &mut w,
                "Cache bytes",
                cache_stats.file_size_bytes.to_string(),
            )?;
        }
    }

    Ok(())
}
