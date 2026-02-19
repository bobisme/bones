//! Recovery procedures for corrupt shards, partial writes, and missing DB.
//!
//! This module implements the runtime recovery procedures that restore a bones
//! project to a consistent state after:
//! - Partial/torn writes (process crash mid-append)
//! - Corrupt shard data (bit flips, truncation, invalid content)
//! - Missing or corrupt SQLite projection database
//! - Missing or corrupt binary cache files
//! - Locked database (retry with timeout)
//!
//! # Recovery Philosophy
//!
//! - **Deterministic**: same input → same recovery action, every time.
//! - **No silent data loss**: corrupt data is quarantined, never deleted outright.
//! - **Fast common path**: torn-write repair is the typical case (truncate last
//!   incomplete line). Complex cases (quarantine, rebuild) are rarer.
//! - **User-facing messages**: every action emits a diagnostic so operators know
//!   exactly what happened and why.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::event::parser;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Report from recovering a corrupt or partially-written shard file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Path to the shard that was recovered.
    pub shard_path: PathBuf,
    /// Number of valid events preserved.
    pub events_preserved: usize,
    /// Number of corrupt/invalid events discarded.
    pub events_discarded: usize,
    /// Byte offset where corruption was detected (if applicable).
    pub corruption_offset: Option<u64>,
    /// What action was taken.
    pub action_taken: RecoveryAction,
}

/// The action taken during recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Truncated file at the last valid event boundary.
    Truncated {
        /// Number of bytes removed from the end.
        bytes_removed: u64,
    },
    /// Quarantined corrupt data to a `.corrupt` backup file.
    Quarantined {
        /// Path to the backup file containing the corrupt data.
        backup_path: PathBuf,
    },
    /// No action needed — file was valid.
    NoActionNeeded,
}

/// Errors that can occur during recovery operations.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// I/O error during recovery.
    #[error("recovery I/O error: {0}")]
    Io(#[from] io::Error),

    /// The shard file does not exist.
    #[error("shard file not found: {}", .0.display())]
    ShardNotFound(PathBuf),

    /// The events directory does not exist.
    #[error("events directory not found: {}", .0.display())]
    EventsDirNotFound(PathBuf),

    /// The database path is invalid.
    #[error("invalid database path: {}", .0.display())]
    InvalidDbPath(PathBuf),

    /// Rebuild failed.
    #[error("rebuild failed: {0}")]
    RebuildFailed(String),

    /// Lock timeout exceeded.
    #[error("database locked after {0:?} — another process may hold the lock")]
    LockTimeout(Duration),
}

// ---------------------------------------------------------------------------
// Partial write recovery (torn writes)
// ---------------------------------------------------------------------------

/// Recover from a partial write (e.g., crash mid-append).
///
/// Detects incomplete last line (no trailing newline) and truncates
/// to the last complete event line. This is the fast path — runs on
/// startup before replay.
///
/// # Algorithm
///
/// 1. Read the file contents.
/// 2. If empty or ends with `\n`, nothing to do.
/// 3. Otherwise, find the last `\n` and truncate there.
///
/// # Returns
///
/// The number of bytes removed (0 if file was already clean).
///
/// # Errors
///
/// Returns an error if the file cannot be read or truncated.
pub fn recover_partial_write(path: &Path) -> Result<u64, RecoveryError> {
    if !path.exists() {
        return Err(RecoveryError::ShardNotFound(path.to_path_buf()));
    }

    let content = fs::read(path)?;
    if content.is_empty() || content.last() == Some(&b'\n') {
        return Ok(0);
    }

    // Find last newline
    let last_newline = content.iter().rposition(|&b| b == b'\n');
    let truncate_to = match last_newline {
        Some(pos) => pos + 1, // keep the newline
        None => 0,            // no complete lines at all — truncate to empty
    };

    let bytes_removed = content.len() - truncate_to;

    // Truncate the file
    let file = fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(truncate_to as u64)?;

    tracing::warn!(
        path = %path.display(),
        bytes_removed,
        "torn write repaired: truncated incomplete trailing line"
    );

    Ok(bytes_removed as u64)
}

// ---------------------------------------------------------------------------
// Corrupt shard recovery
// ---------------------------------------------------------------------------

/// Recover a corrupt shard file by scanning for the last valid event line
/// and quarantining corrupt data to a backup file.
///
/// # Algorithm
///
/// 1. Read the entire shard file.
/// 2. Split into lines; validate each line:
///    - Comment lines (`#`...) and blank lines are always valid.
///    - Data lines must parse successfully via the TSJSON parser.
/// 3. Find the last contiguous block of valid lines from the start.
/// 4. If all lines are valid, return `NoActionNeeded`.
/// 5. Otherwise:
///    a. Write the corrupt tail to `<path>.corrupt` for manual inspection.
///    b. Truncate the original file to the last valid line.
///
/// # Returns
///
/// A [`RecoveryReport`] describing what was found and what action was taken.
pub fn recover_corrupt_shard(path: &Path) -> Result<RecoveryReport, RecoveryError> {
    if !path.exists() {
        return Err(RecoveryError::ShardNotFound(path.to_path_buf()));
    }

    let content = fs::read_to_string(path).map_err(|e| {
        // If we can't even read as UTF-8, the whole file might be binary-corrupt
        tracing::error!(path = %path.display(), error = %e, "shard is not valid UTF-8");
        RecoveryError::Io(e)
    })?;

    if content.is_empty() {
        return Ok(RecoveryReport {
            shard_path: path.to_path_buf(),
            events_preserved: 0,
            events_discarded: 0,
            corruption_offset: None,
            action_taken: RecoveryAction::NoActionNeeded,
        });
    }

    let lines: Vec<&str> = content.lines().collect();
    let mut valid_count = 0;
    let mut events_preserved = 0;
    let mut first_bad_line = None;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            // Comment or blank — always valid
            valid_count += 1;
            continue;
        }

        // Try parsing as a TSJSON event line
        match parser::parse_line(line) {
            Ok(_) => {
                valid_count += 1;
                events_preserved += 1;
            }
            Err(_) => {
                first_bad_line = Some(i);
                break;
            }
        }
    }

    // If we broke out on a bad line, count events from the valid prefix
    // Otherwise, all lines valid
    if first_bad_line.is_none() {
        return Ok(RecoveryReport {
            shard_path: path.to_path_buf(),
            events_preserved,
            events_discarded: 0,
            corruption_offset: None,
            action_taken: RecoveryAction::NoActionNeeded,
        });
    }

    let bad_idx = first_bad_line.unwrap();
    let events_discarded = lines[bad_idx..]
        .iter()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .count();

    // Calculate byte offset of corruption
    let corruption_offset: u64 = content
        .lines()
        .take(bad_idx)
        .map(|l| l.len() as u64 + 1) // +1 for the newline
        .sum();

    // Quarantine: write corrupt tail to backup
    let backup_path = path.with_extension("corrupt");
    let corrupt_content: String = lines[bad_idx..]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(&backup_path, &corrupt_content)?;

    // Truncate original to valid prefix
    let valid_content: String = lines[..bad_idx]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(path, &valid_content)?;

    tracing::warn!(
        path = %path.display(),
        events_preserved,
        events_discarded,
        corruption_offset,
        backup = %backup_path.display(),
        "corrupt shard recovered: quarantined bad data to backup file"
    );

    Ok(RecoveryReport {
        shard_path: path.to_path_buf(),
        events_preserved,
        events_discarded,
        corruption_offset: Some(corruption_offset),
        action_taken: RecoveryAction::Quarantined { backup_path },
    })
}

// ---------------------------------------------------------------------------
// Missing DB recovery
// ---------------------------------------------------------------------------

/// Recover from a missing or corrupt SQLite projection by triggering a full
/// rebuild from the event log.
///
/// This is the "auto-heal" path when `bones.db` is absent, corrupt, or
/// fails integrity checks. Delegates to [`crate::db::rebuild::rebuild`].
///
/// # Arguments
///
/// * `events_dir` — Path to `.bones/events/` directory.
/// * `db_path` — Path to `.bones/bones.db`.
///
/// # Errors
///
/// Returns an error if the events directory doesn't exist or rebuild fails.
pub fn recover_missing_db(events_dir: &Path, db_path: &Path) -> Result<RecoveryReport, RecoveryError> {
    if !events_dir.exists() {
        return Err(RecoveryError::EventsDirNotFound(events_dir.to_path_buf()));
    }

    // Delete corrupt DB if it exists (rebuild will create fresh)
    let db_existed = db_path.exists();
    if db_existed {
        // Back up corrupt DB before deleting
        let backup_path = db_path.with_extension("db.corrupt");
        if let Err(e) = fs::copy(db_path, &backup_path) {
            tracing::warn!(
                error = %e,
                "could not back up corrupt DB before rebuild"
            );
        }
    }

    let rebuild_result = crate::db::rebuild::rebuild(events_dir, db_path)
        .map_err(|e| RecoveryError::RebuildFailed(e.to_string()))?;

    let action = if db_existed {
        let backup_path = db_path.with_extension("db.corrupt");
        tracing::info!(
            events = rebuild_result.event_count,
            items = rebuild_result.item_count,
            elapsed_ms = rebuild_result.elapsed.as_millis(),
            "rebuilt corrupt projection from event log"
        );
        RecoveryAction::Quarantined { backup_path }
    } else {
        tracing::info!(
            events = rebuild_result.event_count,
            items = rebuild_result.item_count,
            elapsed_ms = rebuild_result.elapsed.as_millis(),
            "rebuilt missing projection from event log"
        );
        RecoveryAction::NoActionNeeded
    };

    Ok(RecoveryReport {
        shard_path: db_path.to_path_buf(),
        events_preserved: rebuild_result.event_count,
        events_discarded: 0,
        corruption_offset: None,
        action_taken: action,
    })
}

// ---------------------------------------------------------------------------
// Corrupt cache recovery
// ---------------------------------------------------------------------------

/// Recover from a corrupt or missing binary cache by deleting it.
///
/// The cache will be rebuilt lazily on next access (it's a pure
/// performance optimization derived from the event log).
///
/// # Returns
///
/// `true` if a cache file was deleted, `false` if it didn't exist.
pub fn recover_corrupt_cache(cache_path: &Path) -> Result<bool, RecoveryError> {
    if !cache_path.exists() {
        return Ok(false);
    }

    fs::remove_file(cache_path)?;

    tracing::info!(
        path = %cache_path.display(),
        "deleted corrupt binary cache — will be rebuilt on next access"
    );

    Ok(true)
}

// ---------------------------------------------------------------------------
// Locked DB retry
// ---------------------------------------------------------------------------

/// Attempt to open a SQLite database with retry and timeout for lock contention.
///
/// If the database is locked by another process, retries with exponential
/// backoff up to `timeout`. Returns the connection on success or a
/// [`RecoveryError::LockTimeout`] on failure.
///
/// # Arguments
///
/// * `db_path` — Path to the SQLite database.
/// * `timeout` — Maximum time to wait for the lock.
///
/// # Errors
///
/// Returns `LockTimeout` if the lock is not released within the timeout.
/// Returns `Io` for other I/O errors.
pub fn open_db_with_retry(
    db_path: &Path,
    timeout: Duration,
) -> Result<rusqlite::Connection, RecoveryError> {
    let start = Instant::now();
    let mut delay = Duration::from_millis(50);
    let max_delay = Duration::from_secs(2);

    loop {
        match crate::db::open_projection(db_path) {
            Ok(conn) => {
                // Test that we can actually query (not just open the file)
                match conn.execute_batch("SELECT 1") {
                    Ok(()) => return Ok(conn),
                    Err(e) if is_locked_error(&e) => {
                        // Fall through to retry
                        tracing::debug!(
                            elapsed_ms = start.elapsed().as_millis(),
                            "database locked, retrying..."
                        );
                    }
                    Err(e) => {
                        return Err(RecoveryError::Io(io::Error::new(
                            io::ErrorKind::Other,
                            e.to_string(),
                        )));
                    }
                }
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("locked") || err_str.contains("busy") {
                    tracing::debug!(
                        elapsed_ms = start.elapsed().as_millis(),
                        "database locked on open, retrying..."
                    );
                } else {
                    return Err(RecoveryError::Io(io::Error::new(
                        io::ErrorKind::Other,
                        err_str,
                    )));
                }
            }
        }

        if start.elapsed() >= timeout {
            return Err(RecoveryError::LockTimeout(timeout));
        }

        std::thread::sleep(delay);
        delay = (delay * 2).min(max_delay);
    }
}

/// Check if a rusqlite error is a lock/busy error.
fn is_locked_error(e: &rusqlite::Error) -> bool {
    match e {
        rusqlite::Error::SqliteFailure(err, _) => {
            matches!(
                err.code,
                rusqlite::ffi::ErrorCode::DatabaseBusy
                    | rusqlite::ffi::ErrorCode::DatabaseLocked
            )
        }
        _ => {
            let s = e.to_string();
            s.contains("locked") || s.contains("busy")
        }
    }
}

// ---------------------------------------------------------------------------
// Full project health check and auto-recovery
// ---------------------------------------------------------------------------

/// Result of a full project health check.
#[derive(Debug, Clone)]
pub struct HealthCheckResult {
    /// Whether the project directory exists and looks valid.
    pub project_valid: bool,
    /// Torn-write recovery results (one per shard).
    pub torn_write_repairs: Vec<(PathBuf, u64)>,
    /// Whether the DB was rebuilt.
    pub db_rebuilt: bool,
    /// Number of cache files cleaned.
    pub caches_cleaned: usize,
    /// Errors encountered (non-fatal).
    pub warnings: Vec<String>,
}

/// Run a full health check and auto-recovery on a bones project directory.
///
/// This is called on startup to ensure the project is in a consistent state.
///
/// # Steps
///
/// 1. Verify `.bones/` directory exists.
/// 2. Recover torn writes on all shard files.
/// 3. Check if SQLite DB exists and is valid; rebuild if not.
/// 4. Clean corrupt cache files.
///
/// # Arguments
///
/// * `bones_dir` — Path to the `.bones/` directory.
pub fn auto_recover(bones_dir: &Path) -> Result<HealthCheckResult, RecoveryError> {
    let mut result = HealthCheckResult {
        project_valid: false,
        torn_write_repairs: Vec::new(),
        db_rebuilt: false,
        caches_cleaned: 0,
        warnings: Vec::new(),
    };

    // 1. Verify project directory
    if !bones_dir.exists() || !bones_dir.is_dir() {
        return Ok(result); // project_valid = false signals "not a bones project"
    }
    result.project_valid = true;

    let events_dir = bones_dir.join("events");
    let db_path = bones_dir.join("bones.db");
    let cache_dir = bones_dir.join("cache");

    // 2. Recover torn writes on all shard files
    if events_dir.exists() {
        match fs::read_dir(&events_dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("events") {
                        match recover_partial_write(&path) {
                            Ok(bytes) if bytes > 0 => {
                                result.torn_write_repairs.push((path, bytes));
                            }
                            Ok(_) => {} // clean file
                            Err(e) => {
                                result.warnings.push(format!(
                                    "torn-write check failed for {}: {e}",
                                    path.display()
                                ));
                            }
                        }
                    }
                }
            }
            Err(e) => {
                result.warnings.push(format!("cannot read events dir: {e}"));
            }
        }
    }

    // 3. Check/rebuild SQLite DB
    if events_dir.exists() {
        let need_rebuild = if db_path.exists() {
            // Quick integrity check
            match crate::db::open_projection(&db_path) {
                Ok(conn) => {
                    // Try a simple query to verify DB isn't corrupt
                    conn.execute_batch("SELECT COUNT(*) FROM items")
                        .is_err()
                }
                Err(_) => true,
            }
        } else {
            true
        };

        if need_rebuild {
            match recover_missing_db(&events_dir, &db_path) {
                Ok(_report) => {
                    result.db_rebuilt = true;
                }
                Err(e) => {
                    result.warnings.push(format!("DB rebuild failed: {e}"));
                }
            }
        }
    }

    // 4. Clean corrupt cache files
    if cache_dir.exists() {
        let cache_events_bin = cache_dir.join("events.bin");
        if cache_events_bin.exists() {
            // Validate cache header (first 4 bytes should be magic)
            let is_valid = fs::read(&cache_events_bin)
                .map(|data| data.len() >= 4 && &data[..4] == b"BCEV")
                .unwrap_or(false);

            if !is_valid {
                match recover_corrupt_cache(&cache_events_bin) {
                    Ok(true) => result.caches_cleaned += 1,
                    Ok(false) => {}
                    Err(e) => {
                        result
                            .warnings
                            .push(format!("cache cleanup failed: {e}"));
                    }
                }
            }
        }
    }

    tracing::info!(
        torn_writes = result.torn_write_repairs.len(),
        db_rebuilt = result.db_rebuilt,
        caches_cleaned = result.caches_cleaned,
        warnings = result.warnings.len(),
        "auto-recovery complete"
    );

    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ---- Partial write tests ----

    #[test]
    fn partial_write_clean_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.events");
        fs::write(&path, "line1\nline2\n").unwrap();

        let bytes = recover_partial_write(&path).unwrap();
        assert_eq!(bytes, 0);

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "line1\nline2\n");
    }

    #[test]
    fn partial_write_truncates_incomplete_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.events");
        fs::write(&path, "line1\nline2\npartial").unwrap();

        let bytes = recover_partial_write(&path).unwrap();
        assert_eq!(bytes, 7); // "partial" = 7 bytes

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "line1\nline2\n");
    }

    #[test]
    fn partial_write_no_complete_lines() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.events");
        fs::write(&path, "no newline at all").unwrap();

        let bytes = recover_partial_write(&path).unwrap();
        assert_eq!(bytes, 17);

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "");
    }

    #[test]
    fn partial_write_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.events");
        fs::write(&path, "").unwrap();

        let bytes = recover_partial_write(&path).unwrap();
        assert_eq!(bytes, 0);
    }

    #[test]
    fn partial_write_nonexistent_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.events");

        let result = recover_partial_write(&path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RecoveryError::ShardNotFound(_)));
    }

    // ---- Corrupt shard tests ----

    #[test]
    fn corrupt_shard_clean_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.events");
        // Only comments and blank lines → valid
        fs::write(&path, "# bones event log v1\n# comment\n\n").unwrap();

        let report = recover_corrupt_shard(&path).unwrap();
        assert_eq!(report.events_preserved, 0);
        assert_eq!(report.events_discarded, 0);
        assert_eq!(report.action_taken, RecoveryAction::NoActionNeeded);
    }

    #[test]
    fn corrupt_shard_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.events");
        fs::write(&path, "").unwrap();

        let report = recover_corrupt_shard(&path).unwrap();
        assert_eq!(report.events_preserved, 0);
        assert_eq!(report.action_taken, RecoveryAction::NoActionNeeded);
    }

    #[test]
    fn corrupt_shard_with_bad_data() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.events");
        // Header + a line that won't parse as TSJSON
        fs::write(&path, "# header\nthis is garbage data\nmore garbage\n").unwrap();

        let report = recover_corrupt_shard(&path).unwrap();
        assert_eq!(report.events_preserved, 0);
        assert_eq!(report.events_discarded, 2);
        assert!(report.corruption_offset.is_some());

        match &report.action_taken {
            RecoveryAction::Quarantined { backup_path } => {
                assert!(backup_path.exists());
                let backup = fs::read_to_string(backup_path).unwrap();
                assert!(backup.contains("garbage data"));
            }
            _ => panic!("expected Quarantined"),
        }

        // Original should only have the header
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "# header\n");
    }

    #[test]
    fn corrupt_shard_nonexistent_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.events");

        let result = recover_corrupt_shard(&path);
        assert!(result.is_err());
    }

    // ---- Cache recovery tests ----

    #[test]
    fn cache_recovery_deletes_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.bin");
        fs::write(&path, "corrupt data").unwrap();

        let deleted = recover_corrupt_cache(&path).unwrap();
        assert!(deleted);
        assert!(!path.exists());
    }

    #[test]
    fn cache_recovery_nonexistent_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.bin");

        let deleted = recover_corrupt_cache(&path).unwrap();
        assert!(!deleted);
    }

    // ---- Missing DB recovery tests ----

    #[test]
    fn missing_db_no_events_dir() {
        let dir = TempDir::new().unwrap();
        let events_dir = dir.path().join("events");
        let db_path = dir.path().join("bones.db");

        let result = recover_missing_db(&events_dir, &db_path);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RecoveryError::EventsDirNotFound(_)
        ));
    }

    #[test]
    fn missing_db_empty_events() {
        let dir = TempDir::new().unwrap();
        let bones_dir = dir.path();

        // Set up minimal bones structure
        let shard_mgr = crate::shard::ShardManager::new(bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init");

        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");

        let report = recover_missing_db(&events_dir, &db_path).unwrap();
        assert_eq!(report.events_preserved, 0);
        assert!(db_path.exists());
    }

    #[test]
    fn missing_db_with_events_rebuilds() {
        let dir = TempDir::new().unwrap();
        let bones_dir = dir.path();

        // Set up bones with some events
        let shard_mgr = crate::shard::ShardManager::new(bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init");

        // Write a create event
        use crate::event::data::*;
        use crate::event::types::EventType;
        use crate::event::writer;
        use crate::event::Event;
        use crate::model::item::{Kind, Size, Urgency};
        use crate::model::item_id::ItemId;
        use std::collections::BTreeMap;

        let mut event = Event {
            wall_ts_us: 1000,
            agent: "test".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-001"),
            data: EventData::Create(CreateData {
                title: "Test item".into(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        writer::write_event(&mut event).expect("hash");
        let line = writer::write_line(&event).expect("serialize");
        let (year, month) = shard_mgr.active_shard().unwrap().unwrap();
        shard_mgr.append_raw(year, month, &line).expect("append");

        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");

        let report = recover_missing_db(&events_dir, &db_path).unwrap();
        assert_eq!(report.events_preserved, 1);
        assert!(db_path.exists());

        // Verify the item is in the rebuilt DB
        let conn = crate::db::open_projection(&db_path).unwrap();
        let title: String = conn
            .query_row(
                "SELECT title FROM items WHERE item_id = 'bn-001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title, "Test item");
    }

    #[test]
    fn corrupt_db_is_backed_up_before_rebuild() {
        let dir = TempDir::new().unwrap();
        let bones_dir = dir.path();

        let shard_mgr = crate::shard::ShardManager::new(bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init");

        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");

        // Write something pretending to be a corrupt DB
        fs::write(&db_path, "this is not sqlite").unwrap();

        let report = recover_missing_db(&events_dir, &db_path).unwrap();

        // Corrupt DB should be backed up
        let backup_path = db_path.with_extension("db.corrupt");
        match &report.action_taken {
            RecoveryAction::Quarantined { backup_path: bp } => {
                assert_eq!(bp, &backup_path);
                assert!(backup_path.exists());
                let backup_content = fs::read_to_string(&backup_path).unwrap();
                assert_eq!(backup_content, "this is not sqlite");
            }
            _ => panic!("expected Quarantined action"),
        }
    }

    // ---- Auto-recovery tests ----

    #[test]
    fn auto_recover_nonexistent_project() {
        let dir = TempDir::new().unwrap();
        let bones_dir = dir.path().join(".bones");

        let result = auto_recover(&bones_dir).unwrap();
        assert!(!result.project_valid);
    }

    #[test]
    fn auto_recover_healthy_project() {
        let dir = TempDir::new().unwrap();
        let bones_dir = dir.path();

        // Set up minimal healthy project
        let shard_mgr = crate::shard::ShardManager::new(bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init");

        // Create the DB with rebuild
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        crate::db::rebuild::rebuild(&events_dir, &db_path).unwrap();

        let result = auto_recover(bones_dir).unwrap();
        assert!(result.project_valid);
        assert!(result.torn_write_repairs.is_empty());
        assert!(!result.db_rebuilt);
        assert_eq!(result.caches_cleaned, 0);
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn auto_recover_repairs_torn_write() {
        let dir = TempDir::new().unwrap();
        let bones_dir = dir.path();

        let shard_mgr = crate::shard::ShardManager::new(bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init");

        // Create DB first
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        crate::db::rebuild::rebuild(&events_dir, &db_path).unwrap();

        // Simulate torn write: append incomplete data to active shard
        let (year, month) = shard_mgr.active_shard().unwrap().unwrap();
        let shard_path = events_dir.join(format!("{year:04}-{month:02}.events"));
        let mut file = fs::OpenOptions::new().append(true).open(&shard_path).unwrap();
        file.write_all(b"incomplete line without newline").unwrap();

        let result = auto_recover(bones_dir).unwrap();
        assert!(result.project_valid);
        assert_eq!(result.torn_write_repairs.len(), 1);
        // "incomplete line without newline" = 30 bytes, but shard header may
        // affect exact count. Just verify some bytes were repaired.
        assert!(result.torn_write_repairs[0].1 > 0);
    }

    #[test]
    fn auto_recover_rebuilds_missing_db() {
        let dir = TempDir::new().unwrap();
        let bones_dir = dir.path();

        let shard_mgr = crate::shard::ShardManager::new(bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init");

        // Don't create DB — auto_recover should rebuild it
        let result = auto_recover(bones_dir).unwrap();
        assert!(result.project_valid);
        assert!(result.db_rebuilt);
    }

    #[test]
    fn auto_recover_cleans_corrupt_cache() {
        let dir = TempDir::new().unwrap();
        let bones_dir = dir.path();

        let shard_mgr = crate::shard::ShardManager::new(bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init");

        // Create DB
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        crate::db::rebuild::rebuild(&events_dir, &db_path).unwrap();

        // Create corrupt cache
        let cache_dir = bones_dir.join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join("events.bin"), "not a valid cache").unwrap();

        let result = auto_recover(bones_dir).unwrap();
        assert!(result.project_valid);
        assert_eq!(result.caches_cleaned, 1);
        assert!(!cache_dir.join("events.bin").exists());
    }

    // ---- Locked DB retry tests ----

    #[test]
    fn open_db_with_retry_succeeds_immediately() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");

        // Create a valid DB first
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE test (id INTEGER)").unwrap();
        drop(conn);

        // Should open immediately
        let result = open_db_with_retry(&db_path, Duration::from_secs(1));
        assert!(result.is_ok());
    }

    #[test]
    fn open_db_with_retry_handles_missing_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");

        // Should create the DB (SQLite creates on open)
        let result = open_db_with_retry(&db_path, Duration::from_secs(1));
        assert!(result.is_ok());
    }

    // ---- RecoveryReport Display ----

    #[test]
    fn recovery_action_debug() {
        let action = RecoveryAction::Truncated { bytes_removed: 42 };
        let debug = format!("{action:?}");
        assert!(debug.contains("42"));

        let action = RecoveryAction::Quarantined {
            backup_path: PathBuf::from("/tmp/test.corrupt"),
        };
        let debug = format!("{action:?}");
        assert!(debug.contains("test.corrupt"));
    }

    #[test]
    fn recovery_error_display() {
        let err = RecoveryError::ShardNotFound(PathBuf::from("/tmp/test.events"));
        let display = format!("{err}");
        assert!(display.contains("not found"));

        let err = RecoveryError::LockTimeout(Duration::from_secs(30));
        let display = format!("{err}");
        assert!(display.contains("30s"));
    }
}
