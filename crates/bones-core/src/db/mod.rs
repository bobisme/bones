//! `SQLite` projection database utilities.
//!
//! Runtime defaults are intentionally conservative:
//! - `journal_mode = WAL` to allow concurrent readers while writers append
//! - `busy_timeout = 5s` to reduce transient lock failures under contention
//! - `foreign_keys = ON` to protect relational integrity in projection tables

pub mod fts;
pub mod incremental;
pub mod migrations;
pub mod project;
pub mod query;
pub mod rebuild;
pub mod schema;

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::{path::Path, path::PathBuf, time::Duration};
use tracing::debug;

/// Busy timeout used for projection DB connections.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const PROJECTION_DIRTY_MARKER: &str = "cache/projection.dirty";

/// Open (or create) the projection `SQLite` database, apply runtime pragmas,
/// and migrate schema to the latest version.
///
/// # Errors
///
/// Returns an error if opening/configuring/migrating the database fails.
pub fn open_projection(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create projection db directory {}", parent.display()))?;
    }

    if let Err(err) = bones_sqlite_vec::register_auto_extension() {
        debug!(%err, "sqlite-vec auto-extension unavailable");
    }

    let mut conn = Connection::open(path)
        .with_context(|| format!("open projection database {}", path.display()))?;

    configure_connection(&conn).context("configure sqlite pragmas")?;
    migrations::migrate(&mut conn).context("apply projection migrations")?;

    Ok(conn)
}

/// Ensure the projection database exists and is up-to-date.
///
/// If the database is missing, corrupt, or behind the event log, an
/// incremental apply is triggered automatically. Returns `None` only if
/// the events directory itself does not exist (no bones project).
///
/// This is the recommended entry point for read commands — it eliminates
/// the need for users to run `bn admin rebuild` manually.
///
/// # Arguments
///
/// * `bones_dir` — Path to the `.bones/` directory.
///
/// # Errors
///
/// Returns an error if the rebuild or database open fails.
pub fn ensure_projection(bones_dir: &Path) -> Result<Option<Connection>> {
    let events_dir = bones_dir.join("events");
    if !events_dir.is_dir() {
        return Ok(None);
    }

    let db_path = bones_dir.join("bones.db");
    let dirty_marker = projection_dirty_marker_path(bones_dir);
    let marker_exists = dirty_marker.exists();

    let needs_rebuild = projection_needs_rebuild(bones_dir, &events_dir, &db_path, marker_exists)?;

    if needs_rebuild {
        debug!("projection stale or missing, running incremental rebuild");
        incremental::incremental_apply(&events_dir, &db_path, false)
            .context("auto-rebuild projection")?;
        if dirty_marker.exists() {
            let _ = std::fs::remove_file(&dirty_marker);
        }
    }

    // Re-open after potential rebuild (raw to avoid recursion).
    query::try_open_projection_raw(&db_path)
}

fn projection_needs_rebuild(
    bones_dir: &Path,
    events_dir: &Path,
    db_path: &Path,
    marker_exists: bool,
) -> Result<bool> {
    if marker_exists {
        return Ok(true);
    }

    let Some(conn) = query::try_open_projection_raw(db_path)? else {
        return Ok(true);
    };

    let (offset, hash) = query::get_projection_cursor(&conn).unwrap_or((0, None));
    if offset == 0 && hash.is_none() {
        return Ok(true);
    }

    let (total_bytes, last_hash) =
        incremental::event_log_cursor(events_dir).context("read event log cursor")?;
    let cursor = usize::try_from(offset).unwrap_or(usize::MAX);
    let stale = total_bytes != cursor || hash != last_hash;
    if stale {
        debug!(
            cursor,
            total_bytes,
            cursor_hash = ?hash,
            last_hash = ?last_hash,
            bones_dir = %bones_dir.display(),
            "projection cursor drift detected"
        );
    }

    Ok(stale)
}

fn configure_connection(conn: &Connection) -> anyhow::Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")
        .context("PRAGMA foreign_keys = ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .context("PRAGMA synchronous = NORMAL")?;
    let _journal_mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .context("PRAGMA journal_mode = WAL")?;
    conn.busy_timeout(DEFAULT_BUSY_TIMEOUT)
        .context("busy_timeout")?;
    Ok(())
}

/// Compute the marker path that signals projection drift.
#[must_use]
pub fn projection_dirty_marker_path(bones_dir: &Path) -> PathBuf {
    bones_dir.join(PROJECTION_DIRTY_MARKER)
}

/// Mark projection state as dirty so read paths trigger deterministic recovery.
///
/// # Errors
///
/// Returns an error if the marker directory cannot be created or marker file
/// cannot be written.
pub fn mark_projection_dirty(bones_dir: &Path, reason: &str) -> Result<()> {
    let marker = projection_dirty_marker_path(bones_dir);
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create projection marker dir {}", parent.display()))?;
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    std::fs::write(&marker, format!("{ts} {reason}\n"))
        .with_context(|| format!("write projection marker {}", marker.display()))?;
    Ok(())
}

/// Mark projection dirty by resolving the active database path from a connection.
///
/// # Errors
///
/// Returns an error if database metadata cannot be read or if writing the
/// marker file fails after locating a `.bones` database path.
pub fn mark_projection_dirty_from_connection(conn: &Connection, reason: &str) -> Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA database_list")
        .context("prepare PRAGMA database_list")?;
    let mut rows = stmt.query([]).context("query PRAGMA database_list")?;

    while let Some(row) = rows.next().context("iterate PRAGMA database_list")? {
        let name: String = row.get(1).context("read database_list name")?;
        if name != "main" {
            continue;
        }
        let path: String = row.get(2).context("read database_list path")?;
        if path.is_empty() {
            return Ok(());
        }
        if let Some(bones_dir) = std::path::Path::new(&path).parent()
            && bones_dir.ends_with(".bones")
        {
            return mark_projection_dirty(bones_dir, reason);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_BUSY_TIMEOUT, open_projection};
    use crate::db::migrations;
    use crate::db::{ensure_projection, mark_projection_dirty, projection_dirty_marker_path};
    use crate::event::Event;
    use crate::event::data::{CreateData, EventData};
    use crate::event::types::EventType;
    use crate::event::writer;
    use crate::model::item::{Kind, Urgency};
    use crate::model::item_id::ItemId;
    use crate::shard::ShardManager;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn temp_db_path() -> (TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("bones-projection.sqlite3");
        (dir, path)
    }

    fn make_create(item_id: &str, title: &str, ts: i64) -> Event {
        Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: title.to_string(),
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
        }
    }

    #[test]
    fn open_projection_sets_wal_busy_timeout_and_fk() {
        let (_dir, path) = temp_db_path();
        let conn = open_projection(&path).expect("open projection db");

        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("query journal_mode");
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

        let busy_timeout_ms: u64 = conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .expect("query busy_timeout");
        assert_eq!(
            u128::from(busy_timeout_ms),
            DEFAULT_BUSY_TIMEOUT.as_millis()
        );

        let foreign_keys: i64 = conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("query foreign_keys");
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn open_projection_runs_migrations() {
        let (_dir, path) = temp_db_path();
        let conn = open_projection(&path).expect("open projection db");

        let version = migrations::current_schema_version(&conn).expect("schema version query");
        assert_eq!(version, migrations::LATEST_SCHEMA_VERSION);

        let projection_version: i64 = conn
            .query_row(
                "SELECT schema_version FROM projection_meta WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .expect("projection_meta schema version");
        assert_eq!(
            projection_version,
            i64::from(migrations::LATEST_SCHEMA_VERSION)
        );
    }

    #[test]
    fn mark_projection_dirty_creates_marker_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).expect("events dir");

        mark_projection_dirty(&bones_dir, "test reason").expect("mark projection dirty");

        let marker = projection_dirty_marker_path(&bones_dir);
        assert!(marker.exists(), "dirty marker should be created");
    }

    #[test]
    fn ensure_projection_rebuild_clears_dirty_marker() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).expect("events dir");
        std::fs::create_dir_all(bones_dir.join("cache")).expect("cache dir");

        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().expect("init shard");
        let (year, month) = shard_mgr
            .active_shard()
            .expect("active shard")
            .expect("some shard");

        let mut create = Event {
            wall_ts_us: 1_700_000_000_000_000,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-marker"),
            data: EventData::Create(CreateData {
                title: "marker test".to_string(),
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
        let line = writer::write_event(&mut create).expect("serialize create event");
        shard_mgr
            .append_raw(year, month, &line)
            .expect("append create event");

        mark_projection_dirty(&bones_dir, "simulate projection failure").expect("mark dirty");
        let marker = projection_dirty_marker_path(&bones_dir);
        assert!(marker.exists(), "precondition: marker exists");

        let conn = ensure_projection(&bones_dir)
            .expect("ensure projection")
            .expect("projection connection");
        let item_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .expect("count items");
        assert_eq!(item_count, 1);
        assert!(
            !marker.exists(),
            "dirty marker should be cleared after successful recovery"
        );
    }

    #[test]
    fn ensure_projection_rebuilds_when_log_hash_changes_without_size_change() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).expect("events dir");

        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().expect("init shard");
        let (year, month) = shard_mgr
            .active_shard()
            .expect("active shard")
            .expect("some shard");

        let mut first = make_create("bn-alpha", "first title", 1_700_000_000_000_000);
        let first_line = writer::write_event(&mut first).expect("serialize first create");
        shard_mgr
            .append_raw(year, month, &first_line)
            .expect("append first event");

        let conn = ensure_projection(&bones_dir)
            .expect("ensure projection")
            .expect("projection connection");
        let first_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM items WHERE item_id = 'bn-alpha'",
                [],
                |row| row.get(0),
            )
            .expect("count first item");
        assert_eq!(first_count, 1);
        drop(conn);

        let mut second = make_create("bn-bravo", "other title", 1_700_000_000_000_000);
        let second_line = writer::write_event(&mut second).expect("serialize second create");
        assert_ne!(first.event_hash, second.event_hash);
        assert_eq!(
            first_line.len(),
            second_line.len(),
            "test setup needs a same-length event-log rewrite"
        );

        let shard_path = shard_mgr.shard_path(year, month);
        let original_content = std::fs::read_to_string(&shard_path).expect("read shard");
        let event_start = original_content
            .rfind(&first_line)
            .expect("original event line present");
        let replacement = format!("{}{}", &original_content[..event_start], second_line);
        assert_eq!(original_content.len(), replacement.len());
        std::fs::write(&shard_path, replacement).expect("rewrite shard with same byte length");

        let conn = ensure_projection(&bones_dir)
            .expect("ensure projection after rewrite")
            .expect("projection connection");
        let counts: (i64, i64) = conn
            .query_row(
                "SELECT
                    SUM(CASE WHEN item_id = 'bn-alpha' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN item_id = 'bn-bravo' THEN 1 ELSE 0 END)
                 FROM items",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("count rewritten items");
        assert_eq!(counts, (0, 1));
    }
}
