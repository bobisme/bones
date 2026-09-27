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
    // An existing projection from before REBUILD_REQUIRED_BELOW lacks data
    // the current code depends on (per-field winner keys in the current
    // format). Mark it dirty so
    // the next ensure_projection forces a full rebuild. A cleared cursor
    // alone is not enough: a single-event write can advance it first. The
    // marker is written before migrating, so a crash in between cannot
    // leave a migrated database without it.
    let before = migrations::current_schema_version(&conn).context("read schema version")?;
    // A projection from a newer bn uses formats this binary does not know
    // (e.g. field key layouts). Writing to it would mix formats. Leave it
    // untouched, mark it dirty, and fail: read paths then rebuild it at this
    // binary's schema (bn-2h7c).
    if before > migrations::LATEST_SCHEMA_VERSION {
        if let Some(bones_dir) = path.parent() {
            mark_projection_dirty(
                bones_dir,
                &format!(
                    "projection schema v{before} is newer than this bn (v{}): full rebuild required",
                    migrations::LATEST_SCHEMA_VERSION
                ),
            )?;
        }
        anyhow::bail!(
            "projection {} has schema v{before}, newer than this bn supports (v{})",
            path.display(),
            migrations::LATEST_SCHEMA_VERSION
        );
    }
    if before > 0
        && before < migrations::REBUILD_REQUIRED_BELOW
        && let Some(bones_dir) = path.parent()
    {
        mark_projection_dirty(
            bones_dir,
            &format!("schema upgrade from v{before}: full rebuild required"),
        )?;
    }
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
        // When the projection was explicitly marked dirty, a prior projection
        // error skipped one or more events. Those events sit *behind* the
        // cursor, so an incremental apply can never reproject them — force a
        // full rebuild to replay the whole log from scratch.
        let report = incremental::incremental_apply(&events_dir, &db_path, marker_exists)
            .context("auto-rebuild projection")?;
        // Only clear the dirty marker if the rebuild fully succeeded. If the
        // rebuild itself skipped events (e.g. the log was being appended by a
        // concurrent writer), keep the marker so a later, non-racing
        // invocation retries the full rebuild instead of silently dropping the
        // event forever.
        if dirty_marker.exists() && report.projection_errors == 0 {
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
    let mut stale = total_bytes != cursor || hash != last_hash;
    if !stale {
        // Same length and last hash can still hide a rewrite before the end.
        let shard_mgr = crate::shard::ShardManager::new(bones_dir);
        stale = !incremental::cursor_prefix_matches(&conn, &shard_mgr, cursor)?;
    }
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
    use std::path::{Path, PathBuf};
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

    /// Regression for bn-1ugh review: a pre-v3 projection opened by a write
    /// command (`open_projection`, then a single-event projection) must still
    /// be rebuilt, so rows projected before v3 get their field keys.
    /// A log with one create, and the projection built from it.
    fn built_projection() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).expect("events dir");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().expect("init shard");
        let (year, month) = shard_mgr
            .active_shard()
            .expect("active shard")
            .expect("some shard");
        let mut create = make_create("bn-one", "one", 1_000);
        let line = writer::write_event(&mut create).expect("serialize create");
        shard_mgr
            .append_raw(year, month, &line)
            .expect("append create");
        ensure_projection(&bones_dir)
            .expect("ensure projection")
            .expect("projection connection");
        (dir, bones_dir)
    }

    fn user_version(db_path: &Path) -> i64 {
        rusqlite::Connection::open(db_path)
            .expect("open raw")
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("user_version")
    }

    #[test]
    fn newer_schema_projection_is_left_alone_and_rebuilt() {
        let (_dir, bones_dir) = built_projection();
        let db_path = bones_dir.join("bones.db");
        let newer = i64::from(migrations::LATEST_SCHEMA_VERSION) + 1;
        rusqlite::Connection::open(&db_path)
            .expect("open raw")
            .pragma_update(None, "user_version", newer)
            .expect("set newer version");

        // A direct writer must not get a connection to the newer database.
        assert!(open_projection(&db_path).is_err());
        assert_eq!(user_version(&db_path), newer, "database left untouched");
        assert!(projection_dirty_marker_path(&bones_dir).exists());

        // A read path rebuilds it at this binary's schema.
        let conn = ensure_projection(&bones_dir)
            .expect("ensure projection")
            .expect("projection connection");
        assert_eq!(
            migrations::current_schema_version(&conn).expect("version"),
            migrations::LATEST_SCHEMA_VERSION
        );
        let item = crate::db::query::get_item(&conn, "bn-one", false).expect("query");
        assert!(item.is_some(), "rebuilt projection holds the item");
        assert!(!projection_dirty_marker_path(&bones_dir).exists());
    }

    #[test]
    fn v4_projection_upgrade_marks_dirty_and_clears_cursor() {
        let (_dir, bones_dir) = built_projection();
        let db_path = bones_dir.join("bones.db");
        rusqlite::Connection::open(&db_path)
            .expect("open raw")
            .pragma_update(None, "user_version", 4_i64)
            .expect("set v4");

        let conn = open_projection(&db_path).expect("open and migrate");
        assert!(
            projection_dirty_marker_path(&bones_dir).exists(),
            "v4 link keys need a rebuild"
        );
        let (offset, hash) = crate::db::query::get_projection_cursor(&conn).expect("cursor");
        assert_eq!((offset, hash), (0, None));
        assert_eq!(crate::db::query::get_projection_prefix_digest(&conn), None);
    }

    #[test]
    fn v6_projection_is_rebuilt_under_v7_semantics() {
        // bn-18fs: rows a v6 bn projected can differ from a replay by this
        // bn (empty descriptions, malformed values, snapshots, deleted_at).
        // Without a rebuild such a row would stay until its fields change.
        let (_dir, bones_dir) = built_projection();
        let db_path = bones_dir.join("bones.db");
        {
            let conn = rusqlite::Connection::open(&db_path).expect("open raw");
            conn.execute_batch(
                "UPDATE items SET description = '' WHERE item_id = 'bn-one';
                 UPDATE projection_meta SET schema_version = 6 WHERE id = 1;
                 PRAGMA user_version = 6;",
            )
            .expect("make v6 projection");
        }

        {
            let conn = open_projection(&db_path).expect("open and migrate");
            assert!(
                projection_dirty_marker_path(&bones_dir).exists(),
                "v6 rows need a rebuild"
            );
            let (offset, hash) = crate::db::query::get_projection_cursor(&conn).expect("cursor");
            assert_eq!((offset, hash), (0, None));
            assert_eq!(
                migrations::current_schema_version(&conn).expect("version"),
                migrations::LATEST_SCHEMA_VERSION
            );
        }

        let conn = ensure_projection(&bones_dir)
            .expect("ensure projection")
            .expect("projection connection");
        let description: Option<String> = conn
            .query_row(
                "SELECT description FROM items WHERE item_id = 'bn-one'",
                [],
                |row| row.get(0),
            )
            .expect("read description");
        assert_eq!(description, None, "rebuilt row");
        assert!(!projection_dirty_marker_path(&bones_dir).exists());
    }

    #[test]
    fn v7_projection_is_rebuilt_under_v8_semantics() {
        // bn-1npc: a v7 bn kept no snapshot sources, so a redaction of a
        // snapshot's source left the snapshot's labels and JSON in place,
        // and it projected members with a NUL differently. Without a
        // rebuild such rows would stay.
        let (_dir, bones_dir) = built_projection();
        let db_path = bones_dir.join("bones.db");
        {
            let conn = rusqlite::Connection::open(&db_path).expect("open raw");
            conn.execute_batch(
                "UPDATE items SET title = 'stale v7 row' WHERE item_id = 'bn-one';
                 DROP TABLE snapshot_sources;
                 UPDATE projection_meta SET schema_version = 7 WHERE id = 1;
                 PRAGMA user_version = 7;",
            )
            .expect("make v7 projection");
        }

        {
            let conn = open_projection(&db_path).expect("open and migrate");
            assert!(
                projection_dirty_marker_path(&bones_dir).exists(),
                "v7 rows need a rebuild"
            );
            let (offset, hash) = crate::db::query::get_projection_cursor(&conn).expect("cursor");
            assert_eq!((offset, hash), (0, None));
            assert_eq!(
                migrations::current_schema_version(&conn).expect("version"),
                8
            );
            let sources: i64 = conn
                .query_row("SELECT COUNT(*) FROM snapshot_sources", [], |row| {
                    row.get(0)
                })
                .expect("snapshot_sources exists");
            assert_eq!(sources, 0);
        }

        let conn = ensure_projection(&bones_dir)
            .expect("ensure projection")
            .expect("projection connection");
        let title: String = conn
            .query_row(
                "SELECT title FROM items WHERE item_id = 'bn-one'",
                [],
                |row| row.get(0),
            )
            .expect("read title");
        assert_eq!(title, "one", "rebuilt row");
        assert!(!projection_dirty_marker_path(&bones_dir).exists());
    }

    #[test]
    fn v5_projection_with_zero_created_at_is_rebuilt() {
        // bn-t37g: a v5 bn folded time-0 events (e.g. `bn migrate` links)
        // into created_at_us as 0. The v6 rule reads 0 as "unknown", so
        // without a rebuild the next event would set created_at to its own
        // time, and this replica would differ from a rebuilt one forever.
        use crate::db::project::Projector;
        use crate::event::data::UpdateData;

        let (_dir, bones_dir) = built_projection();
        let db_path = bones_dir.join("bones.db");
        let shard_mgr = ShardManager::new(&bones_dir);
        let (year, month) = shard_mgr
            .active_shard()
            .expect("active shard")
            .expect("some shard");
        let title = |value: &str, ts: i64| {
            let mut event = make_create("bn-one", "unused", ts);
            event.event_type = EventType::Update;
            event.data = EventData::Update(UpdateData {
                field: "title".to_string(),
                value: serde_json::json!(value),
                extra: BTreeMap::new(),
            });
            let line = writer::write_event(&mut event).expect("serialize update");
            shard_mgr
                .append_raw(year, month, &line)
                .expect("append update");
            event
        };
        title("zero", 0);
        ensure_projection(&bones_dir)
            .expect("ensure projection")
            .expect("projection connection");

        // What a v5 bn leaves: created_at 0, cursor at the log end.
        {
            let conn = rusqlite::Connection::open(&db_path).expect("open raw");
            conn.execute_batch(
                "UPDATE items SET created_at_us = 0 WHERE item_id = 'bn-one';
                 UPDATE projection_meta SET schema_version = 5 WHERE id = 1;
                 PRAGMA user_version = 5;",
            )
            .expect("make v5 projection");
        }

        // A write command with the new binary: open, append, project one event.
        let late = title("late", 5_000);
        {
            let conn = open_projection(&db_path).expect("open and migrate");
            assert!(
                projection_dirty_marker_path(&bones_dir).exists(),
                "v5 created_at values need a rebuild"
            );
            Projector::new(&conn)
                .project_event(&late)
                .expect("project late update");
        }

        let conn = ensure_projection(&bones_dir)
            .expect("ensure projection")
            .expect("projection connection");
        let created: i64 = conn
            .query_row(
                "SELECT created_at_us FROM items WHERE item_id = 'bn-one'",
                [],
                |row| row.get(0),
            )
            .expect("read created_at");
        assert_eq!(created, 1_000, "created_at of the rebuilt projection");
    }

    #[test]
    fn pre_v3_projection_rebuilds_after_single_write() {
        use crate::db::{project::Projector, query, schema};

        let dir = tempfile::tempdir().expect("create temp dir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).expect("events dir");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().expect("init shard");
        let (year, month) = shard_mgr
            .active_shard()
            .expect("active shard")
            .expect("some shard");

        let mut old = make_create("bn-old", "old item", 1_000);
        let line = writer::write_event(&mut old).expect("serialize old create");
        shard_mgr
            .append_raw(year, month, &line)
            .expect("append old create");

        // A v2 projection that already holds bn-old, with its cursor at the
        // log end: what an older bn binary leaves behind.
        let db_path = bones_dir.join("bones.db");
        {
            let conn = rusqlite::Connection::open(&db_path).expect("open v2 db");
            conn.execute_batch(schema::MIGRATION_V1_SQL).expect("v1");
            conn.execute_batch(schema::MIGRATION_V2_SQL).expect("v2");
            conn.pragma_update(None, "user_version", 2_i64)
                .expect("set v2");
            conn.execute(
                "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted,
                     search_labels, created_at_us, updated_at_us)
                 VALUES ('bn-old', 'old item', 'task', 'open', 'default', 0, '', 1000, 1000)",
                [],
            )
            .expect("insert v2 row");
            let len = shard_mgr.total_content_len().expect("log length");
            query::update_projection_cursor(
                &conn,
                i64::try_from(len).expect("small log"),
                Some(&old.event_hash),
            )
            .expect("set v2 cursor");
        }

        // A write command: open (migrates to v3), append, project one event.
        let mut new = make_create("bn-new", "new item", 2_000);
        let line = writer::write_event(&mut new).expect("serialize new create");
        shard_mgr
            .append_raw(year, month, &line)
            .expect("append new create");
        {
            let conn = open_projection(&db_path).expect("open and migrate");
            Projector::new(&conn)
                .project_event(&new)
                .expect("project new create");
        }

        // The next read must rebuild, which gives bn-old its field keys.
        let conn = ensure_projection(&bones_dir)
            .expect("ensure projection")
            .expect("projection connection");
        let old_keys: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM field_clocks WHERE item_id = 'bn-old'",
                [],
                |row| row.get(0),
            )
            .expect("count bn-old keys");
        assert!(old_keys > 0, "bn-old was not re-projected with field keys");
        assert!(!projection_dirty_marker_path(&bones_dir).exists());
    }

    /// Regression for bn-r2zy: a `item.create` event that was skipped mid-batch
    /// (projection error) must be recovered on the next read, not lost forever.
    ///
    /// Reproduces the failure state observed in the field: an item exists in the
    /// event log but is absent from the projection, the cursor has already
    /// advanced *past* that event (so an incremental apply sees no new content),
    /// and the dirty marker is set. Before the fix, `ensure_projection` ran an
    /// incremental apply (which recovered nothing) and then cleared the marker,
    /// stranding the item permanently. The fix forces a full rebuild whenever the
    /// marker is present and keeps the marker if the rebuild still had errors.
    #[test]
    fn ensure_projection_recovers_skipped_event_behind_cursor() {
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

        // Two independent create events in the log.
        for (id, title) in [("bn-keep", "kept item"), ("bn-lost", "skipped item")] {
            let mut create = make_create(id, title, 1_700_000_000_000_000);
            let line = writer::write_event(&mut create).expect("serialize create event");
            shard_mgr
                .append_raw(year, month, &line)
                .expect("append create event");
        }

        // Initial projection: both items land, cursor advances to log end.
        {
            let conn = ensure_projection(&bones_dir)
                .expect("initial ensure projection")
                .expect("projection connection");
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
                .expect("count items");
            assert_eq!(count, 2, "precondition: both items projected initially");
        }

        // Simulate the skip: bn-lost is missing from the projection even though
        // its create event is in the log and the cursor sits past it. This is
        // exactly what `project_batch` leaves behind when it counts an error but
        // `incremental_apply` still advances the cursor.
        {
            let conn = open_projection(&bones_dir.join("bones.db")).expect("open projection");
            conn.execute("DELETE FROM items WHERE item_id = 'bn-lost'", [])
                .expect("delete skipped item");
        }
        mark_projection_dirty(&bones_dir, "simulate skipped item.create").expect("mark dirty");
        let marker = projection_dirty_marker_path(&bones_dir);
        assert!(marker.exists(), "precondition: dirty marker set");

        // The read path must recover the skipped item via a full rebuild.
        let conn = ensure_projection(&bones_dir)
            .expect("recovery ensure projection")
            .expect("projection connection");
        let recovered: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM items WHERE item_id = 'bn-lost'",
                [],
                |row| row.get(0),
            )
            .expect("count recovered item");
        assert_eq!(
            recovered, 1,
            "skipped item must be recovered by dirty-marker full rebuild"
        );
        assert!(
            !marker.exists(),
            "marker should be cleared after a clean recovery rebuild"
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

    /// Init a `.bones/` dir with one shard; return its manager and bones dir.
    fn init_bones_dir(dir: &TempDir) -> (std::path::PathBuf, ShardManager) {
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).expect("events dir");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().expect("init shard");
        (bones_dir, shard_mgr)
    }

    /// Serialize and append a create event; return the event and its line.
    fn append_create(shard_mgr: &ShardManager, id: &str, title: &str, ts: i64) -> (Event, String) {
        let (year, month) = shard_mgr
            .active_shard()
            .expect("active shard")
            .expect("some shard");
        let mut event = make_create(id, title, ts);
        let line = writer::write_event(&mut event).expect("serialize create");
        shard_mgr
            .append_raw(year, month, &line)
            .expect("append create");
        (event, line)
    }

    /// Overwrite one projected title directly in `SQLite`. Only a full
    /// rebuild from the log restores it, so it shows whether one ran.
    fn plant_canary(bones_dir: &std::path::Path, item_id: &str) {
        let conn = open_projection(&bones_dir.join("bones.db")).expect("open projection");
        let changed = conn
            .execute(
                "UPDATE items SET title = 'CANARY' WHERE item_id = ?1",
                [item_id],
            )
            .expect("plant canary");
        assert_eq!(changed, 1, "canary item must exist");
    }

    fn projected_title(conn: &rusqlite::Connection, item_id: &str) -> String {
        conn.query_row(
            "SELECT title FROM items WHERE item_id = ?1",
            [item_id],
            |row| row.get(0),
        )
        .expect("read projected title")
    }

    /// A rewrite that swaps two earlier lines keeps the log length and the
    /// last event hash. `ensure_projection` must still see it (prefix digest)
    /// and run a full rebuild.
    #[test]
    fn ensure_projection_rebuilds_when_earlier_lines_swap() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let (bones_dir, shard_mgr) = init_bones_dir(&dir);
        let (_, line_a) = append_create(&shard_mgr, "bn-a", "alpha", 1_700_000_000_000_000);
        let (_, line_b) = append_create(&shard_mgr, "bn-b", "bravo", 1_700_000_000_000_001);
        let (last, _) = append_create(&shard_mgr, "bn-c", "charlie", 1_700_000_000_000_002);

        drop(
            ensure_projection(&bones_dir)
                .expect("ensure")
                .expect("conn"),
        );
        let before = super::incremental::event_log_cursor(&bones_dir.join("events"))
            .expect("log cursor before");
        plant_canary(&bones_dir, "bn-a");

        // Swap the first two event lines in place.
        let (year, month) = shard_mgr.active_shard().expect("shard").expect("some");
        let path = shard_mgr.shard_path(year, month);
        let content = std::fs::read_to_string(&path).expect("read shard");
        let ab = format!("{line_a}{line_b}");
        assert!(content.contains(&ab), "lines a and b must be adjacent");
        std::fs::write(&path, content.replace(&ab, &format!("{line_b}{line_a}")))
            .expect("rewrite shard");

        let after = super::incremental::event_log_cursor(&bones_dir.join("events"))
            .expect("log cursor after");
        assert_eq!(before, after, "rewrite must keep length and last hash");
        assert_eq!(after.1.as_deref(), Some(last.event_hash.as_str()));

        let conn = ensure_projection(&bones_dir)
            .expect("ensure")
            .expect("conn");
        assert_eq!(
            projected_title(&conn, "bn-a"),
            "alpha",
            "full rebuild must replace the canary"
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .expect("count items");
        assert_eq!(count, 3);
        assert_eq!(projected_title(&conn, "bn-b"), "bravo");
        assert_eq!(projected_title(&conn, "bn-c"), "charlie");

        // The rebuild stored the digest of the rewritten log.
        let (offset, _) = super::query::get_projection_cursor(&conn).expect("cursor");
        let offset = usize::try_from(offset).expect("offset");
        assert_eq!(
            super::query::get_projection_prefix_digest(&conn),
            super::incremental::log_prefix_digest(&shard_mgr, offset).expect("digest"),
        );
    }

    /// The CLI write path (append a line, then `Projector::project_event`,
    /// which advances the cursor) records a prefix digest that matches the
    /// log. The next `ensure_projection` must trust it and not rebuild.
    #[test]
    fn single_event_write_keeps_next_ensure_projection_incremental() {
        use crate::db::project::Projector;

        let dir = tempfile::tempdir().expect("create temp dir");
        let (bones_dir, shard_mgr) = init_bones_dir(&dir);
        append_create(&shard_mgr, "bn-a", "alpha", 1_700_000_000_000_000);
        drop(
            ensure_projection(&bones_dir)
                .expect("ensure")
                .expect("conn"),
        );

        // What `bn create` does: append, open the projection, project.
        let (event, _) = append_create(&shard_mgr, "bn-b", "bravo", 1_700_000_000_000_001);
        {
            let conn = open_projection(&bones_dir.join("bones.db")).expect("open projection");
            assert!(
                Projector::new(&conn)
                    .project_event(&event)
                    .expect("project")
            );

            let (offset, hash) = super::query::get_projection_cursor(&conn).expect("cursor");
            let offset = usize::try_from(offset).expect("offset");
            assert_eq!(offset, shard_mgr.total_content_len().expect("log length"));
            assert_eq!(hash.as_deref(), Some(event.event_hash.as_str()));
            let stored = super::query::get_projection_prefix_digest(&conn);
            assert!(stored.is_some());
            assert_eq!(
                stored,
                super::incremental::log_prefix_digest(&shard_mgr, offset).expect("digest"),
                "stored digest must match the log"
            );
        }

        plant_canary(&bones_dir, "bn-a");
        let conn = ensure_projection(&bones_dir)
            .expect("ensure")
            .expect("conn");
        assert_eq!(
            projected_title(&conn, "bn-a"),
            "CANARY",
            "ensure_projection rebuilt after a single-event write"
        );
        assert_eq!(projected_title(&conn, "bn-b"), "bravo");
    }
}
