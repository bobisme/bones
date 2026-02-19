use std::path::Path;

use anyhow::Result;

/// Run `bn rebuild` and refresh both projection DB and binary cache.
///
/// # Errors
///
/// Returns an error if projection rebuild or cache rebuild fails.
pub fn run_rebuild(project_root: &Path, _incremental: bool) -> Result<()> {
    let bones_dir = project_root.join(".bones");
    let events_dir = bones_dir.join("events");
    let db_path = bones_dir.join("bones.db");
    let cache_path = bones_dir.join("cache/events.bin");

    let db_report = bones_core::db::rebuild::rebuild(&events_dir, &db_path)?;
    let cache_stats = bones_core::cache::rebuild_cache(&events_dir, &cache_path)?;

    println!(
        "rebuild: projection={} items={} shards={} cache_events={} cache_bytes={}",
        db_report.event_count,
        db_report.item_count,
        db_report.shard_count,
        cache_stats.total_events,
        cache_stats.file_size_bytes
    );

    Ok(())
}
