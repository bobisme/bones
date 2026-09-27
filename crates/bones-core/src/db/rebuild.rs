//! Full projection rebuild from the event log.
//!
//! `bn admin rebuild` recreates the entire `SQLite` DB from the canonical
//! event log, proving the projection is disposable and reproducible.
//!
//! The rebuild builds a fresh database in a private staging file, then copies
//! it into the live `bones.db` with `SQLite`'s backup API in one step. The live
//! file is never deleted while it may be open (bn-x6aa):
//!
//! - A connection that stays open across the rebuild (a TUI session, another
//!   `bn` process) sees the old projection until the copy commits and the
//!   rebuilt one afterwards. Deleting the file instead left such a connection
//!   reading a deleted copy on Unix.
//! - Windows refuses to delete a file that another handle has open, so a
//!   delete-and-recreate rebuild failed there.
//!
//! The whole rebuild runs under the projection write lock
//! ([`crate::db::projection_lock`]). Only one rebuild uses the staging file at
//! a time, and no writer can commit event rows between the copy and its
//! cursor write.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::db::{open_projection, project};
use crate::event::Event;
use crate::shard::ShardManager;
use std::io;

const DEFAULT_REBUILD_BATCH_SIZE: usize = 4_000;

fn rebuild_batch_size() -> usize {
    std::env::var("BONES_REBUILD_BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_REBUILD_BATCH_SIZE)
}

fn configure_rebuild_pragmas(conn: &rusqlite::Connection) -> Result<()> {
    conn.pragma_update(None, "temp_store", "MEMORY")
        .context("PRAGMA temp_store = MEMORY")?;
    conn.pragma_update(None, "cache_size", -131_072_i64)
        .context("PRAGMA cache_size")?;
    conn.pragma_update(None, "mmap_size", 268_435_456_i64)
        .context("PRAGMA mmap_size")?;

    // Bulk-load tuning: this DB was just created and is about to be
    // populated from the canonical event log. If anything goes wrong the
    // whole file is discarded and the rebuild is re-run, so crash durability
    // during the rebuild window has no value. fsync is off.
    //
    // The journal stays, in memory: project_batch rolls a failed event back
    // to its savepoint, and with journal_mode=OFF SQLite cannot roll back.
    // The failed event's partial writes would then stay in a rebuilt
    // projection but not in an incremental one (bn-39tj).
    //
    // `configure_pragmas` in db/mod.rs restores the safe defaults
    // (journal_mode=WAL, synchronous=NORMAL) the next time this DB is
    // opened for normal use.
    conn.pragma_update(None, "synchronous", "OFF")
        .context("PRAGMA synchronous = OFF")?;
    let _: String = conn
        .query_row("PRAGMA journal_mode = MEMORY", [], |row| row.get(0))
        .context("PRAGMA journal_mode = MEMORY")?;
    let _: String = conn
        .query_row("PRAGMA locking_mode = EXCLUSIVE", [], |row| row.get(0))
        .context("PRAGMA locking_mode = EXCLUSIVE")?;
    Ok(())
}

/// Names of the FTS5 maintenance triggers on `items`. Kept in one place so
/// the rebuild path can drop them before bulk-loading and recreate them
/// after, matching the definitions in `db/schema.rs`.
const FTS_TRIGGERS: &[&str] = &["items_ai", "items_au", "items_ad"];

/// Drop the three FTS5 maintenance triggers so bulk inserts during rebuild
/// don't pay per-row FTS5 maintenance cost. The triggers are recreated by
/// [`restore_fts_triggers`] after the rebuild's FTS index has been
/// repopulated in bulk via [`crate::db::fts::rebuild_fts_index`].
fn drop_fts_triggers(conn: &rusqlite::Connection) -> Result<()> {
    for name in FTS_TRIGGERS {
        conn.execute_batch(&format!("DROP TRIGGER IF EXISTS {name}"))
            .with_context(|| format!("drop trigger {name}"))?;
    }
    Ok(())
}

/// Recreate the FTS5 maintenance triggers dropped by [`drop_fts_triggers`].
/// Kept byte-for-byte in sync with `db/schema.rs`.
fn restore_fts_triggers(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS items_ai
         AFTER INSERT ON items
         BEGIN
             INSERT INTO items_fts(rowid, title, description, labels, item_id)
             VALUES (
                 new.rowid,
                 new.title,
                 COALESCE(new.description, ''),
                 COALESCE(new.search_labels, ''),
                 new.item_id
             );
         END;

         CREATE TRIGGER IF NOT EXISTS items_au
         AFTER UPDATE ON items
         BEGIN
             DELETE FROM items_fts WHERE rowid = old.rowid;
             INSERT INTO items_fts(rowid, title, description, labels, item_id)
             VALUES (
                 new.rowid,
                 new.title,
                 COALESCE(new.description, ''),
                 COALESCE(new.search_labels, ''),
                 new.item_id
             );
         END;

         CREATE TRIGGER IF NOT EXISTS items_ad
         AFTER DELETE ON items
         BEGIN
             DELETE FROM items_fts WHERE rowid = old.rowid;
         END;",
    )
    .context("recreate FTS5 maintenance triggers after rebuild")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// RebuildReport
// ---------------------------------------------------------------------------

/// Report returned after a full projection rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildReport {
    /// Total events replayed from all shards.
    pub event_count: usize,
    /// Number of events that failed to project and were skipped during the
    /// rebuild. Non-zero means the rebuilt projection is still incomplete
    /// (e.g. the log was appended to mid-rebuild) and the dirty marker must
    /// be retained for a later retry.
    pub projection_errors: usize,
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

/// Validate sealed shard manifests, bailing on corruption.
fn check_sealed_shard_integrity(shard_mgr: &ShardManager) -> Result<()> {
    let issues = shard_mgr
        .validate_sealed_shards()
        .map_err(|e| anyhow::anyhow!("sealed shard validation: {e}"))?;
    if !issues.is_empty() {
        for issue in &issues {
            tracing::error!(
                shard = %issue.shard_name,
                problem = %issue.problem,
                "sealed shard integrity check failed"
            );
        }
        anyhow::bail!(
            "sealed shard corrupted: {} (run `bn doctor` to diagnose)",
            issues[0].problem
        );
    }
    Ok(())
}

/// Advance the local clock past the newest replayed event (bn-52i6).
/// Best-effort: a clock-file failure must not fail the projection.
pub(crate) fn observe_newest(shard_mgr: &ShardManager, newest_ts: i64) {
    if newest_ts == i64::MIN {
        return;
    }
    if let Err(err) = shard_mgr.observe_timestamp(newest_ts) {
        tracing::warn!(error = %err, "could not advance local clock past replayed events");
    }
}

/// Rebuild the projection from the canonical event log.
///
/// 1. Takes the projection write lock (see [`crate::db::projection_lock`]).
/// 2. Builds a fresh projection in the staging file `bones.db.rebuild`.
/// 3. Copies it into the live `bones.db` in one backup step, or renames it
///    into place if there is no live file yet.
///
/// # Arguments
///
/// * `events_dir` — Path to `.bones/events/` (the shard directory)
/// * `db_path` — Path to `.bones/bones.db` (the `SQLite` projection file)
///
/// # Errors
///
/// Returns an error if the projection lock times out, or if shard reading,
/// event parsing, projection or the install fails. A failed install leaves
/// the live projection as it was.
pub fn rebuild(events_dir: &Path, db_path: &Path) -> Result<RebuildReport> {
    let _lock = crate::db::lock_projection(db_path).context("lock projection for rebuild")?;
    rebuild_locked(events_dir, db_path)
}

/// [`rebuild`] for a caller that already holds the projection write lock.
///
/// Only the lock holder touches the staging file, so one fixed staging name
/// is safe.
pub(crate) fn rebuild_locked(events_dir: &Path, db_path: &Path) -> Result<RebuildReport> {
    let staging = staging_path(db_path);
    // A crashed earlier rebuild can leave a staging file behind.
    remove_db_files(&staging);

    let report = match build_projection(events_dir, &staging) {
        Ok(report) => report,
        Err(err) => {
            remove_db_files(&staging);
            return Err(err);
        }
    };
    #[cfg(test)]
    crate::db::project::fault::before_install();
    let installed = install_rebuilt(&staging, db_path, copy_busy_timeout());
    remove_db_files(&staging);
    installed?;
    Ok(report)
}

/// The private file a rebuild builds into: `bones.db.rebuild` beside the
/// live projection.
fn staging_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_owned();
    name.push(".rebuild");
    PathBuf::from(name)
}

/// Remove a database file and its `SQLite` side files, ignoring errors.
fn remove_db_files(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
}

/// Retry pause while the live file stays busy after the first step.
const COPY_RETRY_PAUSE: Duration = Duration::from_millis(50);

#[cfg(test)]
thread_local! {
    static COPY_BUSY_TIMEOUT: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Shorten the install's busy timeout on this thread (tests only).
#[cfg(test)]
fn set_copy_busy_timeout(timeout: Option<Duration>) {
    COPY_BUSY_TIMEOUT.with(|t| t.set(timeout));
}

// Not const: the test build reads a thread-local override.
#[allow(clippy::missing_const_for_fn)]
fn copy_busy_timeout() -> Duration {
    #[cfg(test)]
    if let Some(timeout) = COPY_BUSY_TIMEOUT.with(std::cell::Cell::get) {
        return timeout;
    }
    crate::db::DEFAULT_BUSY_TIMEOUT
}

/// Make the staged projection the live one.
///
/// With no live file yet, the staging file is renamed into place: nothing
/// can have it open. Otherwise the staged pages are copied into the live
/// file in one backup step, so its open connections stay valid and see the
/// rebuilt data once the step commits.
///
/// The live file is replaced (deleted, then renamed over) only when it is not
/// a usable database. Any other copy failure, such as a writer that holds
/// the database past twice the busy timeout, returns an error and leaves the live
/// file alone: deleting a file that others have open is the bug this module
/// avoids, and Windows refuses it anyway.
fn install_rebuilt(staging: &Path, db_path: &Path, busy_timeout: Duration) -> Result<()> {
    if !db_path.exists() {
        return std::fs::rename(staging, db_path).with_context(|| {
            format!(
                "move rebuilt projection {} to {}",
                staging.display(),
                db_path.display()
            )
        });
    }
    let source = open_staged(staging)?;
    match copy_into_live(&source, db_path, busy_timeout) {
        Ok(()) => Ok(()),
        Err(err) if live_is_unusable(&err) => {
            drop(source);
            tracing::warn!(
                error = %err,
                path = %db_path.display(),
                "live projection is not a usable database; replacing the file"
            );
            remove_db_files(db_path);
            std::fs::rename(staging, db_path).with_context(|| {
                format!(
                    "replace unusable projection db {} (copy failed: {err})",
                    db_path.display()
                )
            })
        }
        Err(err) => Err(anyhow::Error::new(err).context(format!(
            "copy rebuilt projection into {}; the live projection is unchanged \
             (another process may hold a write transaction on it)",
            db_path.display()
        ))),
    }
}

/// Open the staged projection and check that it reads, so a later copy error
/// is known to come from the live file.
fn open_staged(staging: &Path) -> Result<rusqlite::Connection> {
    let source = rusqlite::Connection::open(staging)
        .with_context(|| format!("open staged projection {}", staging.display()))?;
    let _: i64 = source
        .query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| row.get(0))
        .with_context(|| format!("read staged projection {}", staging.display()))?;
    Ok(source)
}

/// The live file cannot be a database at all, so replacing it loses nothing
/// and nobody can be using it.
fn live_is_unusable(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
    )
}

/// Copy every page of `source` into the live database in one backup step.
fn copy_into_live(
    source: &rusqlite::Connection,
    db_path: &Path,
    busy_timeout: Duration,
) -> rusqlite::Result<()> {
    use rusqlite::backup::{Backup, StepResult};

    let mut live = rusqlite::Connection::open(db_path)?;
    live.busy_timeout(busy_timeout)?;
    // Read the header first: a file that is not a database fails here with
    // SQLITE_NOTADB, before the backup starts.
    let _: i64 = live.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
    let backup = Backup::new(source, &mut live)?;
    // -1 copies all pages in one step, so readers never see a partial copy.
    //
    // The first step waits up to the busy timeout for a writer. SQLite does
    // not reset its busy count between backup steps, so a later step returns
    // BUSY at once. Retry with a short pause until twice the busy timeout
    // has passed: the worst case is bounded, and the pause gives a short
    // writer time to finish.
    let deadline = Instant::now() + busy_timeout * 2;
    loop {
        let step = backup.step(-1)?;
        if step == StepResult::Done {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some(format!(
                    "projection database stayed busy during rebuild copy \
                     ({step:?} after {:?})",
                    busy_timeout * 2
                )),
            ));
        }
        std::thread::sleep(COPY_RETRY_PAUSE);
    }
}

/// Build a complete projection from the event log into a new file at
/// `db_path`, which must not exist.
#[allow(clippy::too_many_lines)]
fn build_projection(events_dir: &Path, db_path: &Path) -> Result<RebuildReport> {
    let start = Instant::now();

    // 1. Create fresh schema
    let conn = open_projection(db_path).context("create fresh projection database")?;
    configure_rebuild_pragmas(&conn).context("configure rebuild sqlite pragmas")?;
    project::ensure_tracking_table(&conn).context("create tracking table")?;

    // 2b. Disable FTS5 maintenance triggers for the bulk-load. Every
    // row projection would otherwise incur an insert into items_fts; we
    // repopulate the whole FTS5 index once at the end in a single
    // query, which is ~5x cheaper on the bench corpus.
    drop_fts_triggers(&conn).context("drop FTS5 triggers for bulk rebuild")?;

    // 3. Read and replay all events in streaming batches
    let bones_dir = events_dir.parent().unwrap_or_else(|| Path::new("."));
    let shard_mgr = ShardManager::new(bones_dir);

    let shards = shard_mgr
        .list_shards()
        .map_err(|e| anyhow::anyhow!("list shards: {e}"))?;
    let shard_count = shards.len();

    check_sealed_shard_integrity(&shard_mgr)?;

    // We need a custom loop because EventParser expects Iterator<Item = String>
    // and returns Result<Event, ...>.
    let mut version_checked = false;
    let mut shard_version = crate::event::parser::CURRENT_VERSION;
    let mut line_no = 0;
    let mut total_projected = 0;
    let mut total_duplicates = 0;
    let mut total_errors = 0;
    let mut last_event_hash = None;
    let mut total_byte_len = 0;

    let batch_size = rebuild_batch_size();
    let mut current_batch: Vec<Event> = Vec::with_capacity(batch_size);
    let projector = project::Projector::new(&conn);

    let shard_line_iter = shard_mgr.replay_lines()?;
    // Digest the exact lines replayed; see incremental::LogDigest.
    let mut digest = crate::db::incremental::LogDigest::new();
    let mut newest_ts = i64::MIN;

    for line_res in shard_line_iter {
        let (offset, line): (usize, String) =
            line_res.map_err(|e: io::Error| anyhow::anyhow!("read shard line: {e}"))?;
        line_no += 1;
        total_byte_len = offset + line.len();
        digest.update(offset, &line);

        if !version_checked && line.trim_start().starts_with("# bones event log v") {
            version_checked = true;
            shard_version = crate::event::parser::detect_version(&line)
                .map_err(|msg| anyhow::anyhow!("version check failed at line {line_no}: {msg}"))?;
            continue;
        }

        match crate::event::parser::parse_line(&line) {
            Ok(crate::event::parser::ParsedLine::Event(event)) => {
                let event = crate::event::migrate_event(*event, shard_version)
                    .map_err(|e| anyhow::anyhow!("migration failed at line {line_no}: {e}"))?;

                last_event_hash = Some(event.event_hash.clone());
                newest_ts = newest_ts.max(event.wall_ts_us);
                current_batch.push(event);

                if current_batch.len() >= batch_size {
                    let stats = projector
                        .project_batch(&current_batch)
                        .context("project batch during rebuild")?;
                    total_projected += stats.projected;
                    total_duplicates += stats.duplicates;
                    total_errors += stats.errors;
                    current_batch.clear();
                }
            }
            Ok(
                crate::event::parser::ParsedLine::Comment(_)
                | crate::event::parser::ParsedLine::Blank,
            ) => {}
            Err(crate::event::parser::ParseError::InvalidEventType(raw)) => {
                tracing::warn!(line = line_no, event_type = %raw, "skipping unknown event type");
            }
            Err(e) => anyhow::bail!("parse error at line {line_no}: {e}"),
        }
    }

    // Final batch
    if !current_batch.is_empty() {
        let stats = projector
            .project_batch(&current_batch)
            .context("project final batch during rebuild")?;
        total_projected += stats.projected;
        total_duplicates += stats.duplicates;
        total_errors += stats.errors;
    }

    // 4b. Rebuild the FTS5 index in bulk now that all items are in place,
    // then recreate the maintenance triggers so subsequent incremental
    // projections keep the FTS5 index in sync row-by-row.
    crate::db::fts::rebuild_fts_index(&conn).context("rebuild FTS5 index after bulk load")?;
    restore_fts_triggers(&conn).context("restore FTS5 triggers after bulk rebuild")?;

    // 5. Update projection cursor
    let byte_offset_i64 = i64::try_from(total_byte_len).unwrap_or(i64::MAX);
    crate::db::query::update_projection_cursor(&conn, byte_offset_i64, last_event_hash.as_deref())
        .context("update projection cursor after rebuild")?;
    crate::db::query::set_projection_prefix_digest(&conn, Some(&digest.finish()))
        .context("record cursor prefix after rebuild")?;
    observe_newest(&shard_mgr, newest_ts);

    // Count unique items
    let item_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .context("count items after rebuild")?;

    // Leave the staged file in WAL mode, the mode of a live projection. A
    // staged file renamed into place is then WAL from its first open, and a
    // copy into a live WAL file keeps WAL.
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .context("PRAGMA journal_mode = WAL after rebuild")?;
    anyhow::ensure!(
        mode.eq_ignore_ascii_case("wal"),
        "staged projection did not switch to WAL (journal_mode={mode})"
    );

    let elapsed = start.elapsed();

    tracing::info!(
        event_count = total_projected,
        duplicates = total_duplicates,
        errors = total_errors,
        batch_size,
        item_count,
        shard_count,
        elapsed_ms = elapsed.as_millis(),
        "projection rebuild complete"
    );

    Ok(RebuildReport {
        event_count: total_projected,
        projection_errors: total_errors,
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
    use crate::event::Event;
    use crate::event::data::*;
    use crate::event::types::EventType;
    use crate::event::writer;
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

    fn make_move_event(
        id: &str,
        state: crate::model::item::State,
        ts: i64,
        parent_hash: &str,
    ) -> Event {
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
        shard_mgr
            .append_raw(year, month, &line)
            .expect("append event");
    }

    /// Items, field keys and projected hashes of a projection file.
    fn dump(db_path: &Path) -> Vec<String> {
        let conn = rusqlite::Connection::open(db_path).expect("open projection");
        let mut out = Vec::new();
        for sql in [
            "SELECT item_id, title, description, kind, state, size, is_deleted, \
             created_at_us, updated_at_us FROM items ORDER BY item_id",
            "SELECT item_id, field, wall_ts_us, agent, event_hash FROM field_clocks \
             ORDER BY item_id, field",
            "SELECT item_id, label FROM item_labels ORDER BY item_id, label",
            "SELECT event_hash FROM projected_events ORDER BY event_hash",
        ] {
            let mut stmt = conn.prepare(sql).expect("prepare dump");
            let columns = stmt.column_count();
            let rows = stmt
                .query_map([], |row| {
                    (0..columns)
                        .map(|i| {
                            row.get::<_, rusqlite::types::Value>(i)
                                .map(|v| format!("{v:?}"))
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .expect("query dump");
            for row in rows {
                out.push(row.expect("read dump row").join(" | "));
            }
        }
        out
    }

    #[test]
    fn failed_event_leaves_no_partial_writes_in_rebuild_or_incremental() {
        // bn-39tj: project_batch rolls a failed event back to its savepoint.
        // The rebuild ran with journal_mode=OFF, where SQLite cannot roll
        // back, so a rebuilt projection kept the failed event's writes and
        // an incremental one did not.
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");
        append_event(&shard_mgr, &make_create_event("bn-001", "Item", 1_000));

        let mut doomed = Event {
            wall_ts_us: 2_000,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Update,
            item_id: ItemId::new_unchecked("bn-001"),
            data: EventData::Update(UpdateData {
                field: "labels".into(),
                value: serde_json::json!({"action": "add", "label": "doomed"}),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        writer::write_event(&mut doomed).expect("compute hash");

        // Incremental: the failing event arrives after a rebuild.
        rebuild(&events_dir, &db_path).unwrap();
        append_event(&shard_mgr, &doomed);
        crate::db::project::fault::fail_after_handler(Some(&doomed.event_hash));
        let report = crate::db::incremental::incremental_apply(&events_dir, &db_path, false)
            .expect("incremental apply");
        assert!(!report.full_rebuild_triggered);
        assert_eq!(report.projection_errors, 1);
        let incremental = dump(&db_path);

        // Full rebuild of the same log.
        let report = rebuild(&events_dir, &db_path).unwrap();
        assert_eq!(report.projection_errors, 1);
        let rebuilt = dump(&db_path);
        crate::db::project::fault::fail_after_handler(None);

        assert_eq!(rebuilt, incremental);
        assert!(
            rebuilt.iter().all(|row| !row.contains(&doomed.event_hash)),
            "no field key of the failed event: {rebuilt:#?}"
        );
        assert!(rebuilt.iter().all(|row| !row.contains("doomed")));
        assert!(
            rebuilt[0].ends_with("Integer(1000) | Integer(1000)"),
            "updated_at_us of the failed event rolled back: {}",
            rebuilt[0]
        );
    }

    /// A connection that stays open across a rebuild, as a TUI session does,
    /// must see the rebuilt projection, not a deleted copy. On Windows the
    /// rebuild must not fail because the file is open (bn-x6aa).
    #[test]
    fn connection_open_across_rebuild_sees_rebuilt_data() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");
        append_event(&shard_mgr, &make_create_event("bn-one", "One", 1_000));
        rebuild(&events_dir, &db_path).expect("first rebuild");

        let reader = open_projection(&db_path).expect("open reader");
        let count = |conn: &rusqlite::Connection| -> i64 {
            conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
                .expect("count items")
        };
        assert_eq!(count(&reader), 1);

        append_event(&shard_mgr, &make_create_event("bn-two", "Two", 2_000));
        rebuild(&events_dir, &db_path).expect("rebuild with a reader open");

        assert_eq!(count(&reader), 2, "the open reader sees the rebuilt data");
        let fresh = open_projection(&db_path).expect("open fresh");
        assert_eq!(count(&fresh), 2);
    }

    /// The rebuild keeps the projection file itself, so no open handle is
    /// left on a deleted copy (bn-x6aa).
    #[cfg(unix)]
    #[test]
    fn rebuild_keeps_the_same_file() {
        use std::os::unix::fs::MetadataExt;
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");
        append_event(&shard_mgr, &make_create_event("bn-one", "One", 1_000));
        rebuild(&events_dir, &db_path).expect("first rebuild");
        let inode = std::fs::metadata(&db_path).expect("stat").ino();

        rebuild(&events_dir, &db_path).expect("second rebuild");
        assert_eq!(std::fs::metadata(&db_path).expect("stat").ino(), inode);
    }

    /// A live file that is not a valid database cannot take the copy, so the
    /// rebuild replaces it.
    #[test]
    fn rebuild_replaces_a_live_file_that_is_not_a_database() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");
        append_event(&shard_mgr, &make_create_event("bn-one", "One", 1_000));
        std::fs::write(&db_path, b"this is not a sqlite database").expect("write junk");

        let report = rebuild(&events_dir, &db_path).expect("rebuild over junk");
        assert_eq!(report.item_count, 1);
        let conn = open_projection(&db_path).expect("open rebuilt");
        let items: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .expect("count");
        assert_eq!(items, 1);
    }

    /// A staging file left by a crashed rebuild is discarded, and no staging
    /// file remains afterwards.
    #[test]
    fn rebuild_discards_stale_staging_and_leaves_none() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");
        append_event(&shard_mgr, &make_create_event("bn-one", "One", 1_000));
        rebuild(&events_dir, &db_path).expect("first rebuild");

        let staging = staging_path(&db_path);
        std::fs::write(&staging, b"left by a crash").expect("write stale staging");
        rebuild(&events_dir, &db_path).expect("rebuild with stale staging");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".rebuild"))
            .collect();
        assert!(leftovers.is_empty(), "staging files left: {leftovers:?}");
    }

    fn item_count(conn: &rusqlite::Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .expect("count items")
    }

    /// Concurrent rebuilds shared one staging file: one deleted the other's
    /// file mid-build, and the fallback then installed a half-built
    /// projection or deleted the live one (bn-x6aa review). The projection
    /// lock runs them one at a time.
    #[test]
    fn concurrent_rebuilds_keep_every_item() {
        use std::sync::{Arc, Barrier};
        const ITEMS: i64 = 2_000;
        const THREADS: usize = 3;
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");
        for i in 0..ITEMS {
            append_event(
                &shard_mgr,
                &make_create_event(&format!("bn-{i:05}"), "T", 1_000 + i),
            );
        }
        rebuild(&events_dir, &db_path).expect("first rebuild");

        let mut failures = Vec::new();
        for round in 0..4 {
            let barrier = Arc::new(Barrier::new(THREADS));
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let (events_dir, db_path) = (events_dir.clone(), db_path.clone());
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        rebuild(&events_dir, &db_path)
                            .map(|report| report.item_count)
                            .map_err(|err| format!("{err:#}"))
                    })
                })
                .collect();
            for handle in handles {
                let result = handle.join().expect("rebuild thread panicked");
                if result != Ok(usize::try_from(ITEMS).unwrap()) {
                    failures.push(format!("round {round}: {result:?}"));
                }
            }
            match open_projection(&db_path).map(|conn| item_count(&conn)) {
                Ok(ITEMS) => {}
                other => failures.push(format!(
                    "round {round}: live db {other:?}, exists={}",
                    db_path.exists()
                )),
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// Run `write` (which projects `bn-two` and then writes the cursor)
    /// against a rebuild that staged the log before `bn-two` was appended.
    /// Test hooks ask the rebuild to install in the writer's gap between its
    /// event rows and its cursor write. Returns the items a fresh
    /// `ensure_projection` sees.
    fn race_writer_against_rebuild(write: impl FnOnce(&Path, &Path, &Event)) -> i64 {
        use crate::db::project::fault;
        use std::sync::mpsc;
        use std::time::Duration;

        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");
        append_event(&shard_mgr, &make_create_event("bn-one", "One", 1_000));
        rebuild(&events_dir, &db_path).expect("first rebuild");

        let (staged_tx, staged_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (installed_tx, installed_rx) = mpsc::channel::<()>();
        let rebuilder = {
            let (events_dir, db_path) = (events_dir.clone(), db_path.clone());
            std::thread::spawn(move || {
                fault::set_before_install(Some(Box::new(move || {
                    let _ = staged_tx.send(());
                    // Wait for the writer's gap. Under the lock the writer
                    // cannot start, so give up after a short wait.
                    let _ = go_rx.recv_timeout(Duration::from_millis(500));
                })));
                let result = rebuild(&events_dir, &db_path).map_err(|e| format!("{e:#}"));
                let _ = installed_tx.send(());
                result
            })
        };
        staged_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("rebuild staged");

        // The writer appends after the rebuild read the log.
        let two = make_create_event("bn-two", "Two", 2_000);
        append_event(&shard_mgr, &two);
        fault::set_before_cursor_write(Some(Box::new(move || {
            let _ = go_tx.send(());
            let _ = installed_rx.recv_timeout(Duration::from_secs(2));
        })));
        write(&events_dir, &db_path, &two);
        fault::set_before_cursor_write(None);

        rebuilder
            .join()
            .expect("rebuild thread panicked")
            .expect("racing rebuild");
        let conn = crate::db::ensure_projection(dir.path())
            .expect("ensure projection")
            .expect("projection exists");
        item_count(&conn)
    }

    /// A single-event projection holds the projection lock across its rows
    /// and its cursor write. A rebuild that copied in between left the
    /// cursor covering an event it had removed, so the event was lost
    /// (bn-x6aa review).
    #[test]
    fn rebuild_cannot_land_between_event_rows_and_cursor() {
        let items = race_writer_against_rebuild(|_, db_path, event| {
            let writer = open_projection(db_path).expect("open writer");
            crate::db::project::Projector::new(&writer)
                .project_event(event)
                .expect("project event");
        });
        assert_eq!(items, 2, "bn-two lost after a racing rebuild");
    }

    /// The same race for an incremental apply: its replayed rows and its
    /// cursor write are one locked unit.
    #[test]
    fn rebuild_cannot_land_between_incremental_rows_and_cursor() {
        let items = race_writer_against_rebuild(|events_dir, db_path, _| {
            let report = crate::db::incremental::incremental_apply(events_dir, db_path, false)
                .expect("incremental apply");
            assert!(!report.full_rebuild_triggered, "{report:?}");
        });
        assert_eq!(items, 2, "bn-two lost after a racing rebuild");
    }

    /// A writer that holds the live database past the busy timeout makes the
    /// rebuild fail. The live file is not deleted: others may have it open.
    #[test]
    fn busy_live_database_fails_the_rebuild_and_stays() {
        use std::time::Duration;
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");
        append_event(&shard_mgr, &make_create_event("bn-one", "One", 1_000));
        rebuild(&events_dir, &db_path).expect("first rebuild");
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&db_path).expect("stat").ino()
        };

        let holder = open_projection(&db_path).expect("open holder");
        holder
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold write lock");
        append_event(&shard_mgr, &make_create_event("bn-two", "Two", 2_000));

        let timeout = Duration::from_millis(300);
        set_copy_busy_timeout(Some(timeout));
        let result = rebuild(&events_dir, &db_path);
        set_copy_busy_timeout(None);
        // Time only the copy step, not the build, so a loaded machine does
        // not make the bound flaky.
        let staging = staging_path(&db_path);
        build_projection(&events_dir, &staging).expect("stage");
        let started = Instant::now();
        let copy = install_rebuilt(&staging, &db_path, timeout);
        let waited = started.elapsed();
        remove_db_files(&staging);
        assert!(copy.is_err(), "copy must fail while the live db is busy");

        let err = result.expect_err("rebuild must fail while the live db is busy");
        assert!(format!("{err:#}").contains("unchanged"), "{err:#}");
        // The copy waits for the writer, but only about twice the timeout.
        assert!(waited >= timeout, "copy gave up after {waited:?}");
        assert!(
            waited < timeout * 2 + Duration::from_millis(400),
            "copy retries took {waited:?}"
        );
        assert!(db_path.exists(), "live projection deleted");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(&db_path).expect("stat").ino(), inode);
        }
        holder.execute_batch("ROLLBACK").expect("release");
        assert_eq!(item_count(&holder), 1, "live data intact");
        drop(holder);
        assert_eq!(item_count(&open_projection(&db_path).expect("open")), 1);
    }

    /// A rebuilt projection is in WAL mode at the latest schema version,
    /// both when it is renamed into place and when it is copied in.
    #[test]
    fn rebuilt_projection_is_wal_at_latest_schema() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");
        append_event(&shard_mgr, &make_create_event("bn-one", "One", 1_000));

        for pass in ["rename", "copy"] {
            rebuild(&events_dir, &db_path).expect("rebuild");
            // A raw connection reports the mode stored in the file.
            let raw = rusqlite::Connection::open(&db_path).expect("open raw");
            let mode: String = raw
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                .expect("journal_mode");
            let version: u32 = raw
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .expect("user_version");
            assert_eq!(mode, "wal", "{pass}");
            assert_eq!(
                version,
                crate::db::migrations::LATEST_SCHEMA_VERSION,
                "{pass}"
            );
        }
    }

    /// A reader inside a read transaction keeps its snapshot across the
    /// rebuild, then sees the rebuilt data once the transaction ends.
    #[test]
    fn reader_snapshot_survives_rebuild() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");
        append_event(&shard_mgr, &make_create_event("bn-one", "One", 1_000));
        rebuild(&events_dir, &db_path).expect("first rebuild");

        let reader = open_projection(&db_path).expect("open reader");
        reader.execute_batch("BEGIN").expect("begin");
        assert_eq!(item_count(&reader), 1);

        append_event(&shard_mgr, &make_create_event("bn-two", "Two", 2_000));
        rebuild(&events_dir, &db_path).expect("rebuild with a read transaction open");

        assert_eq!(
            item_count(&reader),
            1,
            "snapshot kept inside the transaction"
        );
        reader.execute_batch("COMMIT").expect("commit");
        assert_eq!(item_count(&reader), 2, "rebuilt data after the transaction");
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
        let mv = make_move_event(
            "bn-001",
            crate::model::item::State::Doing,
            2000,
            &create1.event_hash,
        );

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
    fn rebuild_with_bd_prefix_events() {
        let (dir, shard_mgr) = setup_bones_dir();
        let db_path = dir.path().join("bones.db");
        let events_dir = dir.path().join("events");

        // Write events with bd- prefix (migrated bead IDs)
        let create1 = make_create_event("bd-9mx", "Parent item", 1000);
        let create2 = make_create_event("bd-4kz", "Child item", 1001);

        append_event(&shard_mgr, &create1);
        append_event(&shard_mgr, &create2);

        let report = rebuild(&events_dir, &db_path).unwrap();
        assert_eq!(
            report.event_count, 2,
            "should project 2 events with bd- prefix"
        );
        assert_eq!(report.item_count, 2, "should have 2 items with bd- prefix");
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
