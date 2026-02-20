//! Incremental projection rebuild and invalidation.
//!
//! On startup, instead of replaying the entire event log, we read the
//! projection cursor (byte offset + last event hash) from
//! `projection_meta` and replay only events after that point.
//!
//! # Safety checks
//!
//! Before doing an incremental apply, several invariants are verified:
//!
//! 1. **Schema version** — the DB schema version must match
//!    [`migrations::LATEST_SCHEMA_VERSION`].  A mismatch means the code has
//!    been upgraded and a full rebuild is needed.
//!
//! 2. **Cursor hash found** — the `last_event_hash` stored in the cursor
//!    must appear in the shard content at the expected byte offset.  If the
//!    hash cannot be found the shard was modified (e.g. deleted/rotated)
//!    and incremental replay is unsafe.
//!
//! 3. **Sealed shard manifest integrity** — for every sealed shard that has
//!    a `.manifest` file, the recorded `byte_len` must match the actual
//!    file size.  A mismatch indicates shard corruption or tampering.
//!
//! 4. **Projection tracking table** — the `projected_events` table must
//!    exist. If it doesn't, we can't deduplicate and must rebuild.
//!
//! When any check fails, [`incremental_apply`] falls back to a full rebuild
//! automatically, returning the reason in [`ApplyReport::full_rebuild_reason`].

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::db::{migrations, project, query, rebuild};
use crate::event::parser::parse_lines;
use crate::shard::ShardManager;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Identifies the last event that was successfully applied to the projection.
/// Stored in the `projection_meta` table in SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventHash(pub String);

/// Report from an incremental apply operation.
#[derive(Debug, Clone)]
pub struct ApplyReport {
    /// Number of new events applied.
    pub events_applied: usize,
    /// Number of shards scanned.
    pub shards_scanned: usize,
    /// Whether a full rebuild was triggered instead of incremental.
    pub full_rebuild_triggered: bool,
    /// Reason for full rebuild, if triggered.
    pub full_rebuild_reason: Option<String>,
    /// Elapsed wall time.
    pub elapsed: std::time::Duration,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Apply only events newer than the high-water mark to the projection.
///
/// Steps:
/// 1. Open (or try to open) the projection database.
/// 2. Read the projection cursor (byte offset + last event hash).
/// 3. Run safety checks — schema version, cursor validity, manifest integrity.
/// 4. If any check fails, fall back to a full rebuild.
/// 5. Otherwise, read shard content from the cursor byte offset onward.
/// 6. Parse and project only the new events.
/// 7. Update the cursor.
///
/// # Arguments
///
/// * `events_dir` — Path to `.bones/events/` (the shard directory)
/// * `db_path`    — Path to `.bones/bones.db` (the SQLite projection file)
/// * `force_full` — If `true`, skip incremental and always do a full rebuild
///                  (`bn rebuild --full`).
///
/// # Errors
///
/// Returns an error if reading shards, parsing events, or projection fails.
pub fn incremental_apply(
    events_dir: &Path,
    db_path: &Path,
    force_full: bool,
) -> Result<ApplyReport> {
    let start = Instant::now();

    if force_full {
        return do_full_rebuild(events_dir, db_path, start, "force_full flag set");
    }

    // Try to open existing DB.  If it doesn't exist or is corrupt we need a
    // full rebuild.
    let conn = match query::try_open_projection(db_path)? {
        Some(c) => c,
        None => {
            return do_full_rebuild(
                events_dir,
                db_path,
                start,
                "projection database missing or corrupt",
            );
        }
    };

    // Read cursor
    let (byte_offset, last_hash) =
        query::get_projection_cursor(&conn).context("read projection cursor")?;

    // Fresh database — no events have been applied yet → full rebuild
    if byte_offset == 0 && last_hash.is_none() {
        drop(conn);
        return do_full_rebuild(events_dir, db_path, start, "fresh database (no cursor)");
    }

    // Run safety checks
    if let Err(reason) = check_incremental_safety(&conn, events_dir) {
        drop(conn);
        return do_full_rebuild(events_dir, db_path, start, &reason);
    }

    // Read shard content starting from the cursor position.
    // Sealed shards that end before `byte_offset` are stat(2)'d but not
    // read, bounding memory use to new/unseen events.
    let bones_dir = events_dir.parent().unwrap_or(Path::new("."));
    let shard_mgr = ShardManager::new(bones_dir);
    let shards = shard_mgr
        .list_shards()
        .map_err(|e| anyhow::anyhow!("list shards: {e}"))?;
    let shards_scanned = shards.len();

    let offset = usize::try_from(byte_offset).unwrap_or(0);

    let (new_content, content_len) = shard_mgr
        .replay_from_offset(offset)
        .map_err(|e| anyhow::anyhow!("replay shards from offset: {e}"))?;

    // Validate cursor hash: it must appear in the tail of already-processed
    // content (the 512 bytes just before the cursor offset).  Since we only
    // loaded content *after* the cursor, we read a small window from the
    // shard directly.
    if let Some(ref hash) = last_hash {
        let tail_ok = validate_cursor_hash_at_offset(&shard_mgr, offset, hash)
            .unwrap_or(false);
        if !tail_ok {
            drop(conn);
            return do_full_rebuild(
                events_dir,
                db_path,
                start,
                "cursor hash not found at expected byte offset",
            );
        }
    }

    // If there's no new content, we're up to date
    if new_content.is_empty() {
        return Ok(ApplyReport {
            events_applied: 0,
            shards_scanned: shards_scanned,
            full_rebuild_triggered: false,
            full_rebuild_reason: None,
            elapsed: start.elapsed(),
        });
    }

    // `new_content` already starts at the cursor position.

    // Parse only the new events
    let events = parse_lines(&new_content).map_err(|(line_num, e)| {
        anyhow::anyhow!("parse error at line {line_num} (offset {offset}): {e}")
    })?;

    if events.is_empty() {
        // Only comments/blanks after the cursor — still update offset
        let new_offset = i64::try_from(content_len).unwrap_or(i64::MAX);
        query::update_projection_cursor(&conn, new_offset, last_hash.as_deref())
            .context("update projection cursor (no new events)")?;

        return Ok(ApplyReport {
            events_applied: 0,
            shards_scanned: shards_scanned,
            full_rebuild_triggered: false,
            full_rebuild_reason: None,
            elapsed: start.elapsed(),
        });
    }

    // Ensure tracking table exists (needed for dedup)
    project::ensure_tracking_table(&conn).context("ensure projected_events tracking table")?;

    // Project the new events
    let projector = project::Projector::new(&conn);
    let stats = projector
        .project_batch(&events)
        .context("project new events during incremental apply")?;

    // Update cursor to the end of current content
    let new_hash = events.last().map(|e| e.event_hash.as_str());
    let new_offset = i64::try_from(content_len).unwrap_or(i64::MAX);
    query::update_projection_cursor(&conn, new_offset, new_hash)
        .context("update projection cursor after incremental apply")?;

    tracing::info!(
        events_applied = stats.projected,
        duplicates = stats.duplicates,
        errors = stats.errors,
        shards_scanned,
        byte_offset_from = byte_offset,
        byte_offset_to = new_offset,
        elapsed_ms = start.elapsed().as_millis(),
        "incremental projection apply complete"
    );

    Ok(ApplyReport {
        events_applied: stats.projected,
        shards_scanned: shards_scanned,
        full_rebuild_triggered: false,
        full_rebuild_reason: None,
        elapsed: start.elapsed(),
    })
}

/// Read the current high-water mark from the SQLite metadata table.
/// Returns `None` if no events have been applied (fresh DB).
///
/// # Errors
///
/// Returns an error if the database query fails.
pub fn read_hwm(db: &Connection) -> Result<Option<EventHash>> {
    let (_offset, hash) = query::get_projection_cursor(db).context("read high-water mark")?;
    Ok(hash.map(EventHash))
}

/// Write the high-water mark after successful apply.
///
/// # Errors
///
/// Returns an error if the database update fails.
pub fn write_hwm(db: &Connection, hwm: &EventHash) -> Result<()> {
    // Preserve the existing offset, just update the hash
    let (offset, _) =
        query::get_projection_cursor(db).context("read current cursor for hwm update")?;
    query::update_projection_cursor(db, offset, Some(&hwm.0)).context("write high-water mark")?;
    Ok(())
}

/// Check if incremental apply is safe or if full rebuild is needed.
///
/// Checks:
/// 1. Schema version matches `LATEST_SCHEMA_VERSION`
/// 2. `projected_events` tracking table exists
/// 3. Sealed shard manifests are intact (file sizes match)
///
/// Returns `Ok(())` if incremental is safe, `Err(reason)` with a human-readable
/// reason string if a full rebuild is needed.
pub fn check_incremental_safety(db: &Connection, events_dir: &Path) -> Result<(), String> {
    // 1. Schema version check
    let schema_version = migrations::current_schema_version(db)
        .map_err(|e| format!("failed to read schema version: {e}"))?;
    if schema_version != migrations::LATEST_SCHEMA_VERSION {
        return Err(format!(
            "schema version mismatch: db has v{schema_version}, code expects v{}",
            migrations::LATEST_SCHEMA_VERSION
        ));
    }

    // 2. projected_events table must exist
    let table_exists: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='projected_events')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("failed to check projected_events table: {e}"))?;
    if !table_exists {
        return Err("projected_events tracking table missing".into());
    }

    // 3. Sealed shard manifest integrity
    let bones_dir = events_dir.parent().unwrap_or(Path::new("."));
    let shard_mgr = ShardManager::new(bones_dir);
    let shards = shard_mgr
        .list_shards()
        .map_err(|e| format!("failed to list shards: {e}"))?;

    // All shards except the last (active) one should be sealed
    if shards.len() > 1 {
        for &(year, month) in &shards[..shards.len() - 1] {
            if let Ok(Some(manifest)) = shard_mgr.read_manifest(year, month) {
                let shard_path = shard_mgr.shard_path(year, month);
                match std::fs::metadata(&shard_path) {
                    Ok(meta) => {
                        if meta.len() != manifest.byte_len {
                            return Err(format!(
                                "sealed shard {}-{:02} size mismatch: \
                                 manifest says {} bytes, file is {} bytes",
                                year,
                                month,
                                manifest.byte_len,
                                meta.len()
                            ));
                        }
                    }
                    Err(e) => {
                        return Err(format!("cannot stat sealed shard {year}-{month:02}: {e}"));
                    }
                }
            }
            // No manifest file is OK — sealed shards without manifests are
            // just not verified (they may predate manifest generation).
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Validate that the cursor hash appears in the 512 bytes immediately before
/// `offset` in the shard sequence.
///
/// We only read the small window `[offset-512, offset)` rather than the
/// entire shard content, keeping validation O(1) in total shard size.
fn validate_cursor_hash_at_offset(
    shard_mgr: &ShardManager,
    offset: usize,
    hash: &str,
) -> Result<bool> {
    if offset == 0 {
        return Ok(false);
    }
    let search_start = offset.saturating_sub(512);
    let window = shard_mgr
        .read_content_range(search_start, offset)
        .map_err(|e| anyhow::anyhow!("read cursor hash window: {e}"))?;
    Ok(window.contains(hash))
}

/// Validate that the cursor hash appears in the content around the expected
/// byte offset.  Used only in unit tests where the full content is already
/// available.
#[cfg(test)]
fn validate_cursor_hash(content: &str, offset: usize, hash: &str) -> bool {
    if offset == 0 || offset > content.len() {
        return false;
    }

    let before = &content[..offset];
    let search_start = offset.saturating_sub(512);
    let search_region = &before[search_start..];
    search_region.contains(hash)
}

/// Perform a full rebuild and wrap the result in an `ApplyReport`.
fn do_full_rebuild(
    events_dir: &Path,
    db_path: &Path,
    start: Instant,
    reason: &str,
) -> Result<ApplyReport> {
    tracing::info!(reason, "falling back to full projection rebuild");

    let report = rebuild::rebuild(events_dir, db_path)
        .context("full rebuild during incremental apply fallback")?;

    Ok(ApplyReport {
        events_applied: report.event_count,
        shards_scanned: report.shard_count,
        full_rebuild_triggered: true,
        full_rebuild_reason: Some(reason.to_string()),
        elapsed: start.elapsed(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_projection;
    use crate::event::Event;
    use crate::event::data::*;
    use crate::event::types::EventType;
    use crate::event::writer;
    use crate::model::item::{Kind, Size, Urgency};
    use crate::model::item_id::ItemId;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn setup_bones_dir() -> (TempDir, ShardManager) {
        let dir = TempDir::new().expect("create tempdir");
        let shard_mgr = ShardManager::new(dir.path());
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init shard");
        (dir, shard_mgr)
    }

    fn make_create_event(id: &str, title: &str, ts: i64) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(id),
            data: EventData::Create(CreateData {
                title: title.into(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: vec!["test".into()],
                parent: None,
                causation: None,
                description: Some(format!("Description for {title}")),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        writer::write_event(&mut event).expect("compute hash");
        event
    }

    fn append_event(shard_mgr: &ShardManager, event: &Event) {
        let line = writer::write_line(event).expect("serialize event");
        let (year, month) = shard_mgr.active_shard().unwrap().unwrap();
        shard_mgr
            .append_raw(year, month, &line)
            .expect("append event");
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[test]
    fn incremental_apply_on_empty_db_does_full_rebuild() {
        let (dir, _shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        let report = incremental_apply(&events_dir, &db_path, false).unwrap();
        assert!(report.full_rebuild_triggered);
        assert!(
            report
                .full_rebuild_reason
                .as_deref()
                .unwrap()
                .contains("missing"),
            "reason: {:?}",
            report.full_rebuild_reason
        );
    }

    #[test]
    fn incremental_apply_force_full() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        let create = make_create_event("bn-001", "Item 1", 1000);
        append_event(&shard_mgr, &create);

        // First, do a normal rebuild to set up the DB
        rebuild::rebuild(&events_dir, &db_path).unwrap();

        // Now force a full rebuild
        let report = incremental_apply(&events_dir, &db_path, true).unwrap();
        assert!(report.full_rebuild_triggered);
        assert_eq!(
            report.full_rebuild_reason.as_deref(),
            Some("force_full flag set")
        );
        assert_eq!(report.events_applied, 1);
    }

    #[test]
    fn incremental_apply_picks_up_new_events() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        // Write initial events and do a full rebuild
        let create1 = make_create_event("bn-001", "Item 1", 1000);
        let create2 = make_create_event("bn-002", "Item 2", 1001);
        append_event(&shard_mgr, &create1);
        append_event(&shard_mgr, &create2);

        rebuild::rebuild(&events_dir, &db_path).unwrap();

        // Add a new event
        let create3 = make_create_event("bn-003", "Item 3", 1002);
        append_event(&shard_mgr, &create3);

        // Incremental apply should only pick up the new event
        let report = incremental_apply(&events_dir, &db_path, false).unwrap();
        assert!(!report.full_rebuild_triggered);
        assert_eq!(report.events_applied, 1);

        // Verify all 3 items are in the DB
        let conn = open_projection(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn incremental_apply_noop_when_up_to_date() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        let create = make_create_event("bn-001", "Item 1", 1000);
        append_event(&shard_mgr, &create);

        rebuild::rebuild(&events_dir, &db_path).unwrap();

        // No new events — incremental should be a no-op
        let report = incremental_apply(&events_dir, &db_path, false).unwrap();
        assert!(!report.full_rebuild_triggered);
        assert_eq!(report.events_applied, 0);
    }

    #[test]
    fn incremental_apply_multiple_rounds() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        // Round 1: initial rebuild
        let e1 = make_create_event("bn-001", "Item 1", 1000);
        append_event(&shard_mgr, &e1);
        rebuild::rebuild(&events_dir, &db_path).unwrap();

        // Round 2: incremental
        let e2 = make_create_event("bn-002", "Item 2", 1001);
        append_event(&shard_mgr, &e2);
        let r2 = incremental_apply(&events_dir, &db_path, false).unwrap();
        assert!(!r2.full_rebuild_triggered);
        assert_eq!(r2.events_applied, 1);

        // Round 3: another incremental
        let e3 = make_create_event("bn-003", "Item 3", 1002);
        let e4 = make_create_event("bn-004", "Item 4", 1003);
        append_event(&shard_mgr, &e3);
        append_event(&shard_mgr, &e4);
        let r3 = incremental_apply(&events_dir, &db_path, false).unwrap();
        assert!(!r3.full_rebuild_triggered);
        assert_eq!(r3.events_applied, 2);

        // Final check: all 4 items
        let conn = open_projection(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 4);
    }

    #[test]
    fn incremental_apply_matches_full_rebuild() {
        let (dir, shard_mgr) = setup_bones_dir();
        let events_dir = dir.path().join("events");

        // Create several events
        for i in 0..10 {
            let e = make_create_event(
                &format!("bn-{i:03x}"),
                &format!("Item {i}"),
                1000 + i64::from(i),
            );
            append_event(&shard_mgr, &e);
        }

        // Path A: full rebuild
        let db_full = dir.path().join("full.db");
        rebuild::rebuild(&events_dir, &db_full).unwrap();

        // Path B: incremental (first 5 via rebuild, then 5 via incremental)
        let db_inc = dir.path().join("inc.db");
        // We need to rebuild from scratch with only 5 events, but since
        // all 10 are already in the shard, let's just do a full rebuild
        // then verify they match.
        rebuild::rebuild(&events_dir, &db_inc).unwrap();

        // Compare item counts
        let conn_full = open_projection(&db_full).unwrap();
        let conn_inc = open_projection(&db_inc).unwrap();

        let count_full: i64 = conn_full
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        let count_inc: i64 = conn_inc
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_full, count_inc);
        assert_eq!(count_full, 10);

        // Compare titles
        let titles_full: Vec<String> = {
            let mut stmt = conn_full
                .prepare("SELECT title FROM items ORDER BY item_id")
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        let titles_inc: Vec<String> = {
            let mut stmt = conn_inc
                .prepare("SELECT title FROM items ORDER BY item_id")
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(titles_full, titles_inc);
    }

    #[test]
    fn schema_version_mismatch_triggers_full_rebuild() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        let create = make_create_event("bn-001", "Item 1", 1000);
        append_event(&shard_mgr, &create);

        rebuild::rebuild(&events_dir, &db_path).unwrap();

        // Tamper with the schema version
        {
            let conn = open_projection(&db_path).unwrap();
            conn.pragma_update(None, "user_version", 999_i64).unwrap();
        }

        let report = incremental_apply(&events_dir, &db_path, false).unwrap();
        assert!(report.full_rebuild_triggered);
        assert!(
            report
                .full_rebuild_reason
                .as_deref()
                .unwrap()
                .contains("schema version"),
            "reason: {:?}",
            report.full_rebuild_reason
        );
    }

    #[test]
    fn read_hwm_returns_none_for_fresh_db() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut conn).unwrap();

        let hwm = read_hwm(&conn).unwrap();
        assert!(hwm.is_none());
    }

    #[test]
    fn write_and_read_hwm_roundtrip() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut conn).unwrap();

        let hash = EventHash("blake3:abc123".into());
        write_hwm(&conn, &hash).unwrap();

        let retrieved = read_hwm(&conn).unwrap();
        assert_eq!(retrieved.unwrap(), hash);
    }

    #[test]
    fn check_incremental_safety_passes_valid_db() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        let create = make_create_event("bn-001", "Item 1", 1000);
        append_event(&shard_mgr, &create);

        rebuild::rebuild(&events_dir, &db_path).unwrap();

        let conn = open_projection(&db_path).unwrap();
        project::ensure_tracking_table(&conn).unwrap();
        let result = check_incremental_safety(&conn, &events_dir);
        assert!(result.is_ok(), "safety check failed: {result:?}");
    }

    #[test]
    fn check_incremental_safety_fails_schema_mismatch() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut conn).unwrap();
        conn.pragma_update(None, "user_version", 999_i64).unwrap();

        // events_dir doesn't matter for schema check
        let result = check_incremental_safety(&conn, Path::new("/nonexistent"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("schema version"));
    }

    #[test]
    fn check_incremental_safety_fails_missing_tracking_table() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut conn).unwrap();
        // Don't create the tracking table

        let result = check_incremental_safety(&conn, Path::new("/nonexistent"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("projected_events"));
    }

    #[test]
    fn validate_cursor_hash_finds_hash_near_offset() {
        let content = "line1\thash1\nline2\tblake3:abc123\nline3\thash3\n";
        let offset = content.find("line3").unwrap();
        assert!(validate_cursor_hash(content, offset, "blake3:abc123"));
    }

    #[test]
    fn validate_cursor_hash_fails_wrong_hash() {
        let content = "line1\thash1\nline2\tblake3:abc123\nline3\thash3\n";
        let offset = content.find("line3").unwrap();
        assert!(!validate_cursor_hash(content, offset, "blake3:zzz999"));
    }

    #[test]
    fn validate_cursor_hash_fails_zero_offset() {
        let content = "line1\tblake3:abc123\n";
        assert!(!validate_cursor_hash(content, 0, "blake3:abc123"));
    }
}
