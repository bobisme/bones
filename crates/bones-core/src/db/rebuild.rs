//! Full projection rebuild from the event log.
//!
//! `bn rebuild` drops and recreates the entire SQLite DB from the canonical
//! event log, proving the projection is disposable and reproducible.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::db::{open_projection, project};
use crate::event::parser::parse_lines;
use crate::shard::ShardManager;

// ---------------------------------------------------------------------------
// RebuildReport
// ---------------------------------------------------------------------------

/// Report returned after a full projection rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildReport {
    /// Total events replayed from all shards.
    pub event_count: usize,
    /// Total unique items in the rebuilt projection.
    pub item_count: usize,
    /// Wall-clock elapsed time for the rebuild.
    pub elapsed: std::time::Duration,
    /// Number of shard files processed.
    pub shard_count: usize,
    /// Whether FTS5 index was rebuilt.
    pub fts5_rebuilt: bool,
}

// ---------------------------------------------------------------------------
// rebuild
// ---------------------------------------------------------------------------

/// Drop the existing DB and rebuild it from the canonical event log.
///
/// 1. Deletes the existing database file (if any)
/// 2. Creates a fresh schema via `open_projection`
/// 3. Replays all events from `events_dir` shards through the projector
/// 4. FTS5 index is maintained via triggers during projection
/// 5. Updates the projection cursor with the final offset
///
/// # Arguments
///
/// * `events_dir` — Path to `.bones/events/` (the shard directory)
/// * `db_path` — Path to `.bones/bones.db` (the SQLite projection file)
///
/// # Errors
///
/// Returns an error if shard reading, event parsing, or projection fails.
pub fn rebuild(events_dir: &Path, db_path: &Path) -> Result<RebuildReport> {
    let start = Instant::now();

    // 1. Delete existing database file
    if db_path.exists() {
        std::fs::remove_file(db_path)
            .with_context(|| format!("remove existing projection db {}", db_path.display()))?;
        // Also remove WAL and SHM files
        let wal_path = db_path.with_extension("db-wal");
        let shm_path = db_path.with_extension("db-shm");
        let _ = std::fs::remove_file(wal_path);
        let _ = std::fs::remove_file(shm_path);
    }

    // 2. Create fresh schema
    let conn = open_projection(db_path)
        .context("create fresh projection database")?;
    project::ensure_tracking_table(&conn)
        .context("create tracking table")?;

    // 3. Read and replay all events
    let bones_dir = events_dir
        .parent()
        .unwrap_or(Path::new("."));
    let shard_mgr = ShardManager::new(bones_dir);

    let shards = shard_mgr
        .list_shards()
        .map_err(|e| anyhow::anyhow!("list shards: {e}"))?;
    let shard_count = shards.len();

    // Read all shard content
    let content = shard_mgr
        .replay()
        .map_err(|e| anyhow::anyhow!("replay shards: {e}"))?;

    // Parse events (parse_lines handles comments/blanks internally)
    let events = parse_lines(&content)
        .map_err(|(line_num, e)| {
            anyhow::anyhow!("parse error at line {line_num}: {e}")
        })?;

    // 4. Project all events
    let projector = project::Projector::new(&conn);
    let stats = projector
        .project_batch(&events)
        .context("project events during rebuild")?;

    // 5. Update projection cursor
    let last_hash = events.last().map(|e| e.event_hash.as_str());
    let byte_offset = i64::try_from(content.len()).unwrap_or(i64::MAX);
    crate::db::query::update_projection_cursor(&conn, byte_offset, last_hash)
        .context("update projection cursor after rebuild")?;

    // Count unique items
    let item_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .context("count items after rebuild")?;

    let elapsed = start.elapsed();

    tracing::info!(
        event_count = stats.projected,
        duplicates = stats.duplicates,
        item_count,
        shard_count,
        elapsed_ms = elapsed.as_millis(),
        "projection rebuild complete"
    );

    Ok(RebuildReport {
        event_count: stats.projected,
        item_count: usize::try_from(item_count).unwrap_or(0),
        elapsed,
        shard_count,
        fts5_rebuilt: true, // FTS5 triggers fire during projection
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::data::*;
    use crate::event::types::EventType;
    use crate::event::writer;
    use crate::event::Event;
    use crate::model::item::{Kind, Size, Urgency};
    use crate::model::item_id::ItemId;
    use crate::shard::ShardManager;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

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
        // Compute hash
        writer::write_event(&mut event).expect("compute hash");
        event
    }

    fn make_move_event(id: &str, state: crate::model::item::State, ts: i64, parent_hash: &str) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![parent_hash.into()],
            event_type: EventType::Move,
            item_id: ItemId::new_unchecked(id),
            data: EventData::Move(MoveData {
                state,
                reason: None,
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
        shard_mgr.append_raw(year, month, &line).expect("append event");
    }

    #[test]
    fn rebuild_empty_event_log() {
        let (dir, _shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        let report = rebuild(&events_dir, &db_path).unwrap();
        assert_eq!(report.event_count, 0);
        assert_eq!(report.item_count, 0);
        assert_eq!(report.shard_count, 1); // init creates one shard
        assert!(report.fts5_rebuilt);

        // Verify DB exists and is valid
        let conn = open_projection(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn rebuild_with_events() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        // Write events
        let create1 = make_create_event("bn-001", "First item", 1000);
        let create2 = make_create_event("bn-002", "Second item", 1001);
        let mv = make_move_event("bn-001", crate::model::item::State::Doing, 2000, &create1.event_hash);

        append_event(&shard_mgr, &create1);
        append_event(&shard_mgr, &create2);
        append_event(&shard_mgr, &mv);

        let report = rebuild(&events_dir, &db_path).unwrap();
        assert_eq!(report.event_count, 3);
        assert_eq!(report.item_count, 2);

        // Verify items
        let conn = open_projection(&db_path).unwrap();
        let item: String = conn
            .query_row(
                "SELECT state FROM items WHERE item_id = 'bn-001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(item, "doing");
    }

    #[test]
    fn rebuild_replaces_existing_db() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        // First rebuild with 1 event
        let create1 = make_create_event("bn-001", "Item 1", 1000);
        append_event(&shard_mgr, &create1);

        let report1 = rebuild(&events_dir, &db_path).unwrap();
        assert_eq!(report1.event_count, 1);
        assert_eq!(report1.item_count, 1);

        // Add another event and rebuild again
        let create2 = make_create_event("bn-002", "Item 2", 1001);
        append_event(&shard_mgr, &create2);

        let report2 = rebuild(&events_dir, &db_path).unwrap();
        assert_eq!(report2.event_count, 2);
        assert_eq!(report2.item_count, 2);
    }

    #[test]
    fn rebuild_is_deterministic() {
        let (dir, shard_mgr) = setup_bones_dir();
        let events_dir = dir.path().join("events");

        let create1 = make_create_event("bn-001", "Deterministic test", 1000);
        let create2 = make_create_event("bn-002", "Another item", 1001);
        append_event(&shard_mgr, &create1);
        append_event(&shard_mgr, &create2);

        // Rebuild twice to different DB paths
        let db_path_a = dir.path().join("bones_a.db");
        let db_path_b = dir.path().join("bones_b.db");

        let report_a = rebuild(&events_dir, &db_path_a).unwrap();
        let report_b = rebuild(&events_dir, &db_path_b).unwrap();

        assert_eq!(report_a.event_count, report_b.event_count);
        assert_eq!(report_a.item_count, report_b.item_count);

        // Verify same items in both
        let conn_a = open_projection(&db_path_a).unwrap();
        let conn_b = open_projection(&db_path_b).unwrap();

        let titles_a: Vec<String> = {
            let mut stmt = conn_a
                .prepare("SELECT title FROM items ORDER BY item_id")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };

        let titles_b: Vec<String> = {
            let mut stmt = conn_b
                .prepare("SELECT title FROM items ORDER BY item_id")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };

        assert_eq!(titles_a, titles_b);
    }

    #[test]
    fn rebuild_populates_fts() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        let create = make_create_event("bn-001", "Authentication timeout fix", 1000);
        append_event(&shard_mgr, &create);

        rebuild(&events_dir, &db_path).unwrap();

        let conn = open_projection(&db_path).unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM items_fts WHERE items_fts MATCH 'authentication'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn rebuild_updates_projection_cursor() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        let create = make_create_event("bn-001", "Item", 1000);
        append_event(&shard_mgr, &create);

        rebuild(&events_dir, &db_path).unwrap();

        let conn = open_projection(&db_path).unwrap();
        let (offset, hash) = crate::db::query::get_projection_cursor(&conn).unwrap();
        assert!(offset > 0, "cursor offset should be non-zero after rebuild");
        assert!(hash.is_some(), "cursor hash should be set after rebuild");
    }

    #[test]
    fn rebuild_handles_duplicate_events() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        // Append same event twice (simulates git merge duplication)
        let create = make_create_event("bn-001", "Item", 1000);
        append_event(&shard_mgr, &create);
        append_event(&shard_mgr, &create);

        let report = rebuild(&events_dir, &db_path).unwrap();
        // Only 1 unique event projected, 1 duplicate skipped
        assert_eq!(report.event_count, 1);
        assert_eq!(report.item_count, 1);
    }

    #[test]
    fn rebuild_performance_reasonable() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        // Create 100 items — should be well under 1s
        for i in 0..100_u32 {
            let create = make_create_event(
                &format!("bn-{i:04x}"),
                &format!("Item {i}"),
                i64::from(i) * 1000,
            );
            append_event(&shard_mgr, &create);
        }

        let report = rebuild(&events_dir, &db_path).unwrap();
        assert_eq!(report.event_count, 100);
        assert_eq!(report.item_count, 100);
        assert!(
            report.elapsed.as_millis() < 1000,
            "rebuild of 100 items took {}ms, expected <1000ms",
            report.elapsed.as_millis()
        );
    }
}
