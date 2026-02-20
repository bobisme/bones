//! Time-sharded event file management.
//!
//! Events are stored in monthly shard files under `.bones/events/YYYY-MM.events`.
//! This module manages the directory layout, shard rotation, atomic append
//! operations, the `current.events` symlink, and replay (reading all shards
//! in chronological order).
//!
//! # Directory Layout
//!
//! ```text
//! .bones/
//!   events/
//!     2026-01.events      # sealed shard
//!     2026-01.manifest    # manifest for sealed shard
//!     2026-02.events      # active shard
//!     current.events -> 2026-02.events   # symlink to active
//!   cache/
//!     clock               # monotonic wall-clock file (microseconds)
//!   lock                  # repo-wide advisory lock
//! ```
//!
//! # Invariants
//!
//! - Sealed (frozen) shards are never modified after rotation.
//! - The active shard is the only one that receives appends.
//! - `current.events` always points to the active shard.
//! - Each append uses `O_APPEND` + `write_all` + `flush` for crash consistency.
//! - Torn-write recovery truncates incomplete trailing lines on startup.
//! - Monotonic timestamps: `wall_ts_us = max(system_time_us, last + 1)`.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::Datelike;

use crate::event::writer::shard_header;
use crate::lock::ShardLock;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors that can occur during shard operations.
#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    /// I/O error during shard operations.
    #[error("shard I/O error: {0}")]
    Io(#[from] io::Error),

    /// Lock acquisition failed.
    #[error("lock error: {0}")]
    Lock(#[from] crate::lock::LockError),

    /// The `.bones` directory does not exist and could not be created.
    #[error("failed to initialize .bones directory: {0}")]
    InitFailed(io::Error),

    /// Shard file name does not match expected `YYYY-MM.events` pattern.
    #[error("invalid shard filename: {0}")]
    InvalidShardName(String),
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// Manifest for a sealed shard file, recording integrity metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardManifest {
    /// Shard file name (e.g., `"2026-01.events"`).
    pub shard_name: String,
    /// Number of event lines (excluding comments and blanks).
    pub event_count: u64,
    /// Total byte length of the shard file.
    pub byte_len: u64,
    /// BLAKE3 hash of the entire shard file contents.
    pub file_hash: String,
}

impl ShardManifest {
    /// Serialize manifest to a human-readable format.
    #[must_use]
    pub fn to_string_repr(&self) -> String {
        format!(
            "shard: {}\nevent_count: {}\nbyte_len: {}\nfile_hash: {}\n",
            self.shard_name, self.event_count, self.byte_len, self.file_hash
        )
    }

    /// Parse a manifest from its string representation.
    ///
    /// Returns `None` if required fields are missing or unparseable.
    #[must_use]
    pub fn from_string_repr(s: &str) -> Option<Self> {
        let mut shard_name = None;
        let mut event_count = None;
        let mut byte_len = None;
        let mut file_hash = None;

        for line in s.lines() {
            if let Some(val) = line.strip_prefix("shard: ") {
                shard_name = Some(val.to_string());
            } else if let Some(val) = line.strip_prefix("event_count: ") {
                event_count = val.parse().ok();
            } else if let Some(val) = line.strip_prefix("byte_len: ") {
                byte_len = val.parse().ok();
            } else if let Some(val) = line.strip_prefix("file_hash: ") {
                file_hash = Some(val.to_string());
            }
        }

        Some(Self {
            shard_name: shard_name?,
            event_count: event_count?,
            byte_len: byte_len?,
            file_hash: file_hash?,
        })
    }
}

// ---------------------------------------------------------------------------
// ShardManager
// ---------------------------------------------------------------------------

/// Manages time-sharded event files in a `.bones` repository.
///
/// The shard manager handles:
/// - Directory initialization
/// - Shard rotation on month boundaries
/// - Atomic append with advisory locking
/// - Monotonic clock maintenance
/// - Torn-write recovery
/// - Replay (reading all shards chronologically)
/// - Sealed shard manifest generation
pub struct ShardManager {
    /// Root of the `.bones` directory.
    bones_dir: PathBuf,
}

impl ShardManager {
    /// Create a new `ShardManager` for the given `.bones` directory.
    ///
    /// Does not create directories on construction; call [`init`](Self::init)
    /// or [`ensure_dirs`](Self::ensure_dirs) first if needed.
    #[must_use]
    pub fn new(bones_dir: impl Into<PathBuf>) -> Self {
        Self {
            bones_dir: bones_dir.into(),
        }
    }

    /// Path to the events directory.
    #[must_use]
    pub fn events_dir(&self) -> PathBuf {
        self.bones_dir.join("events")
    }

    /// Path to the advisory lock file.
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.bones_dir.join("lock")
    }

    /// Path to the monotonic clock file.
    #[must_use]
    pub fn clock_path(&self) -> PathBuf {
        self.bones_dir.join("cache").join("clock")
    }

    /// Path to the `current.events` symlink.
    #[must_use]
    pub fn current_symlink(&self) -> PathBuf {
        self.events_dir().join("current.events")
    }

    /// Generate the shard filename for a given year and month.
    #[must_use]
    pub fn shard_filename(year: i32, month: u32) -> String {
        format!("{year:04}-{month:02}.events")
    }

    /// Path to a specific shard file.
    #[must_use]
    pub fn shard_path(&self, year: i32, month: u32) -> PathBuf {
        self.events_dir().join(Self::shard_filename(year, month))
    }

    /// Path to a manifest file for a given shard.
    #[must_use]
    pub fn manifest_path(&self, year: i32, month: u32) -> PathBuf {
        self.events_dir()
            .join(format!("{year:04}-{month:02}.manifest"))
    }

    // -----------------------------------------------------------------------
    // Initialization
    // -----------------------------------------------------------------------

    /// Create the `.bones/events/` and `.bones/cache/` directories if they
    /// don't exist. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::InitFailed`] if directory creation fails.
    pub fn ensure_dirs(&self) -> Result<(), ShardError> {
        fs::create_dir_all(self.events_dir()).map_err(ShardError::InitFailed)?;
        fs::create_dir_all(self.bones_dir.join("cache")).map_err(ShardError::InitFailed)?;
        Ok(())
    }

    /// Initialize the shard directory and create the first shard file
    /// with the standard header if no shards exist.
    ///
    /// Returns the (year, month) of the active shard.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError`] on I/O failure or if directories cannot be
    /// created.
    pub fn init(&self) -> Result<(i32, u32), ShardError> {
        self.ensure_dirs()?;

        let shards = self.list_shards()?;
        if shards.is_empty() {
            let (year, month) = current_year_month();
            self.create_shard(year, month)?;
            self.update_symlink(year, month)?;
            Ok((year, month))
        } else if let Some(&(year, month)) = shards.last() {
            self.update_symlink(year, month)?;
            Ok((year, month))
        } else {
            unreachable!("shards is non-empty")
        }
    }

    // -----------------------------------------------------------------------
    // Shard listing
    // -----------------------------------------------------------------------

    /// List all shard files in chronological order as (year, month) pairs.
    ///
    /// Shard filenames must match `YYYY-MM.events`. Invalid filenames are
    /// silently skipped.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if the directory cannot be read.
    pub fn list_shards(&self) -> Result<Vec<(i32, u32)>, ShardError> {
        let events_dir = self.events_dir();
        if !events_dir.exists() {
            return Ok(Vec::new());
        }

        let mut shards = Vec::new();
        for entry in fs::read_dir(&events_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if let Some(ym) = parse_shard_filename(&name_str) {
                shards.push(ym);
            }
        }
        shards.sort_unstable();
        Ok(shards)
    }

    /// Get the active (most recent) shard, if any.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if the directory cannot be read.
    pub fn active_shard(&self) -> Result<Option<(i32, u32)>, ShardError> {
        let shards = self.list_shards()?;
        Ok(shards.last().copied())
    }

    // -----------------------------------------------------------------------
    // Shard creation and rotation
    // -----------------------------------------------------------------------

    /// Create a new shard file with the standard header.
    ///
    /// Returns the path of the created file. Does nothing if the file
    /// already exists.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if the file cannot be written.
    pub fn create_shard(&self, year: i32, month: u32) -> Result<PathBuf, ShardError> {
        let path = self.shard_path(year, month);
        if path.exists() {
            return Ok(path);
        }

        let header = shard_header();
        fs::write(&path, header)?;
        Ok(path)
    }

    /// Update the `current.events` symlink to point to the given shard.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if the symlink cannot be created.
    pub fn update_symlink(&self, year: i32, month: u32) -> Result<(), ShardError> {
        let symlink = self.current_symlink();
        let target = Self::shard_filename(year, month);

        // Remove existing symlink (or file) if present
        if symlink.exists() || symlink.symlink_metadata().is_ok() {
            fs::remove_file(&symlink)?;
        }

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &symlink)?;

        #[cfg(not(unix))]
        fs::write(&symlink, &target)?;

        Ok(())
    }

    /// Check if the current month differs from the active shard's month.
    /// If so, seal the old shard (generate manifest) and create a new one.
    ///
    /// Returns the (year, month) of the now-active shard.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError`] on I/O failure during rotation.
    pub fn rotate_if_needed(&self) -> Result<(i32, u32), ShardError> {
        let (current_year, current_month) = current_year_month();
        let active = self.active_shard()?;

        match active {
            Some((y, m)) if y == current_year && m == current_month => Ok((y, m)),
            Some((y, m)) => {
                // Seal the old shard with a manifest
                self.write_manifest(y, m)?;
                // Create new shard
                self.create_shard(current_year, current_month)?;
                self.update_symlink(current_year, current_month)?;
                Ok((current_year, current_month))
            }
            None => {
                // No shards exist yet, create first one
                self.create_shard(current_year, current_month)?;
                self.update_symlink(current_year, current_month)?;
                Ok((current_year, current_month))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Manifest generation
    // -----------------------------------------------------------------------

    /// Generate and write a manifest file for a sealed shard.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if the shard file cannot be read or the
    /// manifest cannot be written.
    pub fn write_manifest(&self, year: i32, month: u32) -> Result<ShardManifest, ShardError> {
        let shard_path = self.shard_path(year, month);
        let content = fs::read(&shard_path)?;
        let content_str = String::from_utf8_lossy(&content);

        // Count event lines (non-comment, non-blank)
        let event_count = content_str
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.trim().is_empty())
            .count() as u64;

        let byte_len = content.len() as u64;
        let file_hash = format!("blake3:{}", blake3::hash(&content).to_hex());

        let manifest = ShardManifest {
            shard_name: Self::shard_filename(year, month),
            event_count,
            byte_len,
            file_hash,
        };

        let manifest_path = self.manifest_path(year, month);
        fs::write(&manifest_path, manifest.to_string_repr())?;

        Ok(manifest)
    }

    /// Read a manifest file if it exists.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if the manifest file exists but cannot
    /// be read.
    pub fn read_manifest(
        &self,
        year: i32,
        month: u32,
    ) -> Result<Option<ShardManifest>, ShardError> {
        let manifest_path = self.manifest_path(year, month);
        if !manifest_path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&manifest_path)?;
        Ok(ShardManifest::from_string_repr(&content))
    }

    // -----------------------------------------------------------------------
    // Append
    // -----------------------------------------------------------------------

    /// Append an event line to the active shard.
    ///
    /// This method:
    /// 1. Acquires the repo-wide advisory lock.
    /// 2. Rotates shards if the month has changed.
    /// 3. Reads and updates the monotonic clock.
    /// 4. Appends the line using `O_APPEND` + `write_all` + `flush`.
    /// 5. Optionally calls `sync_data` if `durable` is true.
    /// 6. Releases the lock.
    ///
    /// The `line` must be a complete TSJSON line ending with `\n`.
    ///
    /// Returns the monotonic timestamp used.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Lock`] if the lock cannot be acquired within
    /// `lock_timeout`, or [`ShardError::Io`] on write failure.
    pub fn append(
        &self,
        line: &str,
        durable: bool,
        lock_timeout: Duration,
    ) -> Result<i64, ShardError> {
        self.ensure_dirs()?;

        let _lock = ShardLock::acquire(&self.lock_path(), lock_timeout)?;

        // Rotate if month changed
        let (year, month) = self.rotate_if_needed()?;
        let shard_path = self.shard_path(year, month);

        // Update monotonic clock
        let ts = self.next_timestamp()?;

        // Append with O_APPEND
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&shard_path)?;

        file.write_all(line.as_bytes())?;
        file.flush()?;

        if durable {
            file.sync_data()?;
        }

        Ok(ts)
    }

    /// Append a raw line without locking or clock update.
    ///
    /// Used internally and in tests. The caller is responsible for
    /// holding the lock and managing the clock.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] on write failure.
    pub fn append_raw(&self, year: i32, month: u32, line: &str) -> Result<(), ShardError> {
        let shard_path = self.shard_path(year, month);

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&shard_path)?;

        file.write_all(line.as_bytes())?;
        file.flush()?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Monotonic clock
    // -----------------------------------------------------------------------

    /// Read the current monotonic clock value.
    ///
    /// Returns 0 if the clock file doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if the clock file exists but cannot be
    /// read.
    pub fn read_clock(&self) -> Result<i64, ShardError> {
        let path = self.clock_path();
        if !path.exists() {
            return Ok(0);
        }
        let content = fs::read_to_string(&path)?;
        Ok(content.trim().parse::<i64>().unwrap_or(0))
    }

    /// Compute the next monotonic timestamp and write it to the clock file.
    ///
    /// `next = max(system_time_us, last + 1)`
    ///
    /// The caller must hold the repo lock.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if the clock file cannot be read or
    /// written.
    pub fn next_timestamp(&self) -> Result<i64, ShardError> {
        let last = self.read_clock()?;
        let now = system_time_us();
        let next = std::cmp::max(now, last + 1);
        self.write_clock(next)?;
        Ok(next)
    }

    /// Write a clock value to the clock file.
    fn write_clock(&self, value: i64) -> Result<(), ShardError> {
        let path = self.clock_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, value.to_string())?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Torn-write recovery
    // -----------------------------------------------------------------------

    /// Scan the active shard for torn writes and truncate incomplete
    /// trailing lines.
    ///
    /// A torn write leaves a partial line (no terminating `\n`) at the end
    /// of the file. This method finds the last complete newline and
    /// truncates everything after it.
    ///
    /// Returns `Ok(Some(bytes_truncated))` if a torn write was repaired,
    /// or `Ok(None)` if the file was clean.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if the shard file cannot be read or
    /// truncated.
    pub fn recover_torn_writes(&self) -> Result<Option<u64>, ShardError> {
        let Some(active) = self.active_shard()? else {
            return Ok(None);
        };

        let shard_path = self.shard_path(active.0, active.1);
        recover_shard_torn_write(&shard_path)
    }

    // -----------------------------------------------------------------------
    // Replay
    // -----------------------------------------------------------------------

    /// Read all event lines from all shards in chronological order.
    ///
    /// Shards are read in lexicographic order (`YYYY-MM` sorts correctly).
    /// Returns the concatenated content of all shard files.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if any shard file cannot be read.
    pub fn replay(&self) -> Result<String, ShardError> {
        let shards = self.list_shards()?;
        let mut content = String::new();

        for (year, month) in shards {
            let path = self.shard_path(year, month);
            let shard_content = fs::read_to_string(&path)?;
            content.push_str(&shard_content);
        }

        Ok(content)
    }

    /// Read event lines from a specific shard.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if the shard file cannot be read.
    pub fn read_shard(&self, year: i32, month: u32) -> Result<String, ShardError> {
        let path = self.shard_path(year, month);
        Ok(fs::read_to_string(&path)?)
    }

    /// Compute the total concatenated byte size of all shards without reading
    /// their full contents.
    ///
    /// This is used for advancing the projection cursor without paying the
    /// cost of a full replay.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if any shard file metadata cannot be read.
    pub fn total_content_len(&self) -> Result<usize, ShardError> {
        let shards = self.list_shards()?;
        let mut total = 0usize;
        for (year, month) in shards {
            let path = self.shard_path(year, month);
            let meta = fs::metadata(&path)?;
            total = total.saturating_add(meta.len() as usize);
        }
        Ok(total)
    }

    /// Read shard content starting from the given absolute byte offset in the
    /// concatenated shard sequence.
    ///
    /// Sealed shards that end entirely before `offset` are skipped without
    /// reading their contents — only their file sizes are stat(2)'d.
    /// Only content from `offset` onward is returned, bounding memory use to
    /// new/unseen events rather than the full log.
    ///
    /// Returns `(new_content, total_len)` where:
    /// - `new_content` is the bytes from `offset` to the end of all shards.
    /// - `total_len` is the total byte size of all shards concatenated (usable
    ///   as the new cursor offset after processing `new_content`).
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if shard metadata or file reads fail.
    pub fn replay_from_offset(&self, offset: usize) -> Result<(String, usize), ShardError> {
        let shards = self.list_shards()?;
        let mut cumulative: usize = 0;
        let mut result = String::new();
        let mut found_start = false;

        for (year, month) in shards {
            let path = self.shard_path(year, month);
            let shard_len = fs::metadata(&path)?.len() as usize;

            let shard_end = cumulative.saturating_add(shard_len);

            if shard_end <= offset {
                // This shard ends at or before the cursor — skip entirely.
                cumulative = shard_end;
                continue;
            }

            // This shard overlaps with or is entirely after the cursor.
            let shard_content = fs::read_to_string(&path)?;

            if !found_start {
                // Calculate the within-shard byte offset.
                let within = if cumulative < offset {
                    offset - cumulative
                } else {
                    0
                };
                // Guard: within must not exceed shard content length.
                let within = within.min(shard_content.len());
                result.push_str(&shard_content[within..]);
                found_start = true;
            } else {
                result.push_str(&shard_content);
            }

            cumulative = shard_end;
        }

        Ok((result, cumulative))
    }

    /// Read bytes from the concatenated shard sequence in `[start_offset, end_offset)`.
    ///
    /// Only shards that overlap with the requested range are read.
    /// Shards entirely outside the range are stat(2)'d but not read.
    ///
    /// This is used to read a small window around the projection cursor for
    /// hash validation without loading the full shard content.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if any shard file cannot be read.
    pub fn read_content_range(
        &self,
        start_offset: usize,
        end_offset: usize,
    ) -> Result<String, ShardError> {
        if start_offset >= end_offset {
            return Ok(String::new());
        }

        let shards = self.list_shards()?;
        let mut cumulative: usize = 0;
        let mut result = String::new();

        for (year, month) in shards {
            let path = self.shard_path(year, month);
            let shard_len = fs::metadata(&path)?.len() as usize;
            let shard_end = cumulative.saturating_add(shard_len);

            if shard_end <= start_offset {
                // Shard ends before our range — skip without reading.
                cumulative = shard_end;
                continue;
            }

            if cumulative >= end_offset {
                // Shard starts after our range — done.
                break;
            }

            let shard_content = fs::read_to_string(&path)?;

            // Clip to the slice of this shard that overlaps with [start, end).
            let within_start = if cumulative < start_offset {
                (start_offset - cumulative).min(shard_content.len())
            } else {
                0
            };
            let within_end = if shard_end > end_offset {
                (end_offset - cumulative).min(shard_content.len())
            } else {
                shard_content.len()
            };

            result.push_str(&shard_content[within_start..within_end]);
            cumulative = shard_end;
        }

        Ok(result)
    }

    /// Count event lines across all shards (excluding comments and blanks).
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if any shard file cannot be read.
    pub fn event_count(&self) -> Result<u64, ShardError> {
        let content = self.replay()?;
        let count = content
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.trim().is_empty())
            .count();
        Ok(count as u64)
    }

    /// Check if the repository has any event shards.
    ///
    /// # Errors
    ///
    /// Returns [`ShardError::Io`] if the events directory cannot be read.
    pub fn is_empty(&self) -> Result<bool, ShardError> {
        let shards = self.list_shards()?;
        Ok(shards.is_empty())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get the current year and month from system time.
#[must_use]
fn current_year_month() -> (i32, u32) {
    let now = chrono::Utc::now();
    (now.year(), now.month())
}

/// Get the current system time in microseconds since Unix epoch.
#[allow(clippy::cast_possible_truncation)]
#[must_use]
fn system_time_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Parse a shard filename like `"2026-02.events"` into (year, month).
fn parse_shard_filename(name: &str) -> Option<(i32, u32)> {
    let stem = name.strip_suffix(".events")?;
    // Must not be "current"
    if stem == "current" {
        return None;
    }
    let (year_str, month_str) = stem.split_once('-')?;
    let year: i32 = year_str.parse().ok()?;
    let month: u32 = month_str.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    Some((year, month))
}

/// Recover torn writes for a specific shard file.
///
/// Returns `Ok(Some(bytes_truncated))` if repair was needed,
/// or `Ok(None)` if the file was clean.
fn recover_shard_torn_write(path: &Path) -> Result<Option<u64>, ShardError> {
    let metadata = fs::metadata(path)?;
    let file_len = metadata.len();
    if file_len == 0 {
        return Ok(None);
    }

    let content = fs::read(path)?;

    // Find the last newline
    let last_newline = content.iter().rposition(|&b| b == b'\n');

    if let Some(pos) = last_newline {
        let expected_len = (pos + 1) as u64;
        if expected_len < file_len {
            // There are bytes after the last newline — torn write
            let truncated = file_len - expected_len;
            let file = OpenOptions::new().write(true).open(path)?;
            file.set_len(expected_len)?;
            Ok(Some(truncated))
        } else {
            // File ends with newline — clean
            Ok(None)
        }
    } else {
        // No newline at all — entire content is a torn write
        // (or a corrupt file). Truncate to zero.
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(0)?;
        Ok(Some(file_len))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, ShardManager) {
        let tmp = TempDir::new().expect("tempdir");
        let bones_dir = tmp.path().join(".bones");
        let mgr = ShardManager::new(&bones_dir);
        (tmp, mgr)
    }

    // -----------------------------------------------------------------------
    // parse_shard_filename
    // -----------------------------------------------------------------------

    #[test]
    fn parse_valid_shard_filenames() {
        assert_eq!(parse_shard_filename("2026-01.events"), Some((2026, 1)));
        assert_eq!(parse_shard_filename("2026-12.events"), Some((2026, 12)));
        assert_eq!(parse_shard_filename("1999-06.events"), Some((1999, 6)));
    }

    #[test]
    fn parse_invalid_shard_filenames() {
        assert_eq!(parse_shard_filename("current.events"), None);
        assert_eq!(parse_shard_filename("2026-13.events"), None); // month > 12
        assert_eq!(parse_shard_filename("2026-00.events"), None); // month 0
        assert_eq!(parse_shard_filename("not-a-shard.txt"), None);
        assert_eq!(parse_shard_filename("2026-01.manifest"), None);
        assert_eq!(parse_shard_filename(""), None);
    }

    // -----------------------------------------------------------------------
    // ShardManager::new, paths
    // -----------------------------------------------------------------------

    #[test]
    fn shard_manager_paths() {
        let mgr = ShardManager::new("/repo/.bones");
        assert_eq!(mgr.events_dir(), PathBuf::from("/repo/.bones/events"));
        assert_eq!(mgr.lock_path(), PathBuf::from("/repo/.bones/lock"));
        assert_eq!(mgr.clock_path(), PathBuf::from("/repo/.bones/cache/clock"));
        assert_eq!(
            mgr.current_symlink(),
            PathBuf::from("/repo/.bones/events/current.events")
        );
        assert_eq!(
            mgr.shard_path(2026, 2),
            PathBuf::from("/repo/.bones/events/2026-02.events")
        );
        assert_eq!(
            mgr.manifest_path(2026, 1),
            PathBuf::from("/repo/.bones/events/2026-01.manifest")
        );
    }

    #[test]
    fn shard_filename_format() {
        assert_eq!(ShardManager::shard_filename(2026, 1), "2026-01.events");
        assert_eq!(ShardManager::shard_filename(2026, 12), "2026-12.events");
        assert_eq!(ShardManager::shard_filename(1999, 6), "1999-06.events");
    }

    // -----------------------------------------------------------------------
    // ensure_dirs / init
    // -----------------------------------------------------------------------

    #[test]
    fn ensure_dirs_creates_directories() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("should create dirs");
        assert!(mgr.events_dir().exists());
        assert!(mgr.bones_dir.join("cache").exists());
    }

    #[test]
    fn ensure_dirs_is_idempotent() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("first");
        mgr.ensure_dirs().expect("second");
        assert!(mgr.events_dir().exists());
    }

    #[test]
    fn init_creates_first_shard() {
        let (_tmp, mgr) = setup();
        let (year, month) = mgr.init().expect("init");

        let (expected_year, expected_month) = current_year_month();
        assert_eq!(year, expected_year);
        assert_eq!(month, expected_month);

        // Shard file exists with header
        let shard_path = mgr.shard_path(year, month);
        assert!(shard_path.exists());
        let content = fs::read_to_string(&shard_path).expect("read");
        assert!(content.starts_with("# bones event log v1"));

        // Symlink exists
        let symlink = mgr.current_symlink();
        assert!(symlink.exists() || symlink.symlink_metadata().is_ok());
    }

    #[test]
    fn init_is_idempotent() {
        let (_tmp, mgr) = setup();
        let first = mgr.init().expect("first");
        let second = mgr.init().expect("second");
        assert_eq!(first, second);
    }

    // -----------------------------------------------------------------------
    // Shard listing
    // -----------------------------------------------------------------------

    #[test]
    fn list_shards_empty() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        let shards = mgr.list_shards().expect("list");
        assert!(shards.is_empty());
    }

    #[test]
    fn list_shards_returns_sorted() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");

        // Create shards in reverse order
        mgr.create_shard(2026, 3).expect("create");
        mgr.create_shard(2026, 1).expect("create");
        mgr.create_shard(2026, 2).expect("create");

        let shards = mgr.list_shards().expect("list");
        assert_eq!(shards, vec![(2026, 1), (2026, 2), (2026, 3)]);
    }

    #[test]
    fn list_shards_skips_non_shard_files() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("create");

        // Create non-shard files
        fs::write(mgr.events_dir().join("readme.txt"), "hi").expect("write");
        fs::write(mgr.events_dir().join("2026-01.manifest"), "manifest").expect("write");

        let shards = mgr.list_shards().expect("list");
        assert_eq!(shards, vec![(2026, 1)]);
    }

    #[test]
    fn list_shards_no_events_dir() {
        let (_tmp, mgr) = setup();
        // Don't create any dirs
        let shards = mgr.list_shards().expect("list");
        assert!(shards.is_empty());
    }

    // -----------------------------------------------------------------------
    // Shard creation
    // -----------------------------------------------------------------------

    #[test]
    fn create_shard_writes_header() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        let path = mgr.create_shard(2026, 2).expect("create");

        let content = fs::read_to_string(&path).expect("read");
        assert!(content.starts_with("# bones event log v1"));
        assert!(content.contains("# fields:"));
        assert_eq!(content.lines().count(), 2);
    }

    #[test]
    fn create_shard_idempotent() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        let p1 = mgr.create_shard(2026, 2).expect("first");
        // Write something extra
        fs::write(&p1, "modified").expect("write");
        // Second create should NOT overwrite
        let p2 = mgr.create_shard(2026, 2).expect("second");
        assert_eq!(p1, p2);
        let content = fs::read_to_string(&p2).expect("read");
        assert_eq!(content, "modified");
    }

    // -----------------------------------------------------------------------
    // Symlink
    // -----------------------------------------------------------------------

    #[test]
    fn update_symlink_creates_link() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 2).expect("create");
        mgr.update_symlink(2026, 2).expect("symlink");

        let symlink = mgr.current_symlink();
        assert!(symlink.symlink_metadata().is_ok());

        #[cfg(unix)]
        {
            let target = fs::read_link(&symlink).expect("readlink");
            assert_eq!(target, PathBuf::from("2026-02.events"));
        }
    }

    #[test]
    fn update_symlink_replaces_existing() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("create");
        mgr.create_shard(2026, 2).expect("create");

        mgr.update_symlink(2026, 1).expect("first");
        mgr.update_symlink(2026, 2).expect("second");

        #[cfg(unix)]
        {
            let target = fs::read_link(&mgr.current_symlink()).expect("readlink");
            assert_eq!(target, PathBuf::from("2026-02.events"));
        }
    }

    // -----------------------------------------------------------------------
    // Monotonic clock
    // -----------------------------------------------------------------------

    #[test]
    fn clock_starts_at_zero() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        let ts = mgr.read_clock().expect("read");
        assert_eq!(ts, 0);
    }

    #[test]
    fn clock_is_monotonic() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        let t1 = mgr.next_timestamp().expect("t1");
        let t2 = mgr.next_timestamp().expect("t2");
        let t3 = mgr.next_timestamp().expect("t3");
        assert!(t2 > t1);
        assert!(t3 > t2);
    }

    #[test]
    fn clock_reads_back_written_value() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.write_clock(42_000_000).expect("write");
        let ts = mgr.read_clock().expect("read");
        assert_eq!(ts, 42_000_000);
    }

    #[test]
    fn clock_never_goes_backward() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");

        // Set clock far in the future
        let future = system_time_us() + 10_000_000;
        mgr.write_clock(future).expect("write");

        let next = mgr.next_timestamp().expect("next");
        assert!(next > future, "clock should advance past future value");
    }

    // -----------------------------------------------------------------------
    // Append
    // -----------------------------------------------------------------------

    #[test]
    fn append_raw_adds_line() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 2).expect("create");

        mgr.append_raw(2026, 2, "event line 1\n").expect("append");
        mgr.append_raw(2026, 2, "event line 2\n").expect("append");

        let content = mgr.read_shard(2026, 2).expect("read");
        assert!(content.contains("event line 1"));
        assert!(content.contains("event line 2"));
    }

    #[test]
    fn append_with_lock() {
        let (_tmp, mgr) = setup();
        mgr.init().expect("init");

        let _ts = mgr
            .append("test event line\n", false, Duration::from_secs(1))
            .expect("append");

        let content = mgr.replay().expect("replay");
        assert!(content.contains("test event line"));
    }

    #[test]
    fn append_returns_monotonic_timestamps() {
        let (_tmp, mgr) = setup();
        mgr.init().expect("init");

        let t1 = mgr
            .append("line1\n", false, Duration::from_secs(1))
            .expect("t1");
        let t2 = mgr
            .append("line2\n", false, Duration::from_secs(1))
            .expect("t2");

        assert!(t2 > t1);
    }

    // -----------------------------------------------------------------------
    // Torn-write recovery
    // -----------------------------------------------------------------------

    #[test]
    fn recover_clean_file() {
        let (_tmp, mgr) = setup();
        mgr.init().expect("init");

        let (y, m) = current_year_month();
        mgr.append_raw(y, m, "complete line\n").expect("append");

        let recovered = mgr.recover_torn_writes().expect("recover");
        assert_eq!(recovered, None);
    }

    #[test]
    fn recover_torn_write_truncates() {
        let (_tmp, mgr) = setup();
        let (y, m) = mgr.init().expect("init");
        let shard_path = mgr.shard_path(y, m);

        // Write a complete line followed by a partial line
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(&shard_path)
                .expect("open");
            f.write_all(b"complete line\npartial line without newline")
                .expect("write");
            f.flush().expect("flush");
        }

        let recovered = mgr.recover_torn_writes().expect("recover");
        assert!(recovered.is_some());

        let truncated = recovered.expect("checked is_some");
        assert_eq!(truncated, "partial line without newline".len() as u64);

        // Verify file now ends with newline
        let content = fs::read_to_string(&shard_path).expect("read");
        assert!(content.ends_with('\n'));
        assert!(content.contains("complete line"));
        assert!(!content.contains("partial line without newline"));
    }

    #[test]
    fn recover_no_newline_at_all() {
        let (_tmp, mgr) = setup();
        let (y, m) = mgr.init().expect("init");
        let shard_path = mgr.shard_path(y, m);

        // Overwrite entire file with no newlines
        fs::write(&shard_path, "no newlines here").expect("write");

        let recovered = mgr.recover_torn_writes().expect("recover");
        assert_eq!(recovered, Some("no newlines here".len() as u64));

        // File should be empty
        let content = fs::read_to_string(&shard_path).expect("read");
        assert!(content.is_empty());
    }

    #[test]
    fn recover_empty_file() {
        let (_tmp, mgr) = setup();
        let (y, m) = mgr.init().expect("init");
        let shard_path = mgr.shard_path(y, m);

        // Empty the file
        fs::write(&shard_path, "").expect("write");

        let recovered = mgr.recover_torn_writes().expect("recover");
        assert_eq!(recovered, None);
    }

    #[test]
    fn recover_no_active_shard() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");

        let recovered = mgr.recover_torn_writes().expect("recover");
        assert_eq!(recovered, None);
    }

    // -----------------------------------------------------------------------
    // Replay
    // -----------------------------------------------------------------------

    #[test]
    fn replay_empty_repo() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        let content = mgr.replay().expect("replay");
        assert!(content.is_empty());
    }

    #[test]
    fn replay_single_shard() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("create");
        mgr.append_raw(2026, 1, "event-a\n").expect("append");

        let content = mgr.replay().expect("replay");
        assert!(content.contains("event-a"));
    }

    #[test]
    fn replay_multiple_shards_in_order() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");

        mgr.create_shard(2026, 1).expect("create");
        mgr.create_shard(2026, 2).expect("create");
        mgr.create_shard(2026, 3).expect("create");

        mgr.append_raw(2026, 1, "event-jan\n").expect("append");
        mgr.append_raw(2026, 2, "event-feb\n").expect("append");
        mgr.append_raw(2026, 3, "event-mar\n").expect("append");

        let content = mgr.replay().expect("replay");

        // Events should appear in chronological order
        let jan_pos = content.find("event-jan").expect("jan");
        let feb_pos = content.find("event-feb").expect("feb");
        let mar_pos = content.find("event-mar").expect("mar");
        assert!(jan_pos < feb_pos);
        assert!(feb_pos < mar_pos);
    }

    // -----------------------------------------------------------------------
    // Event count
    // -----------------------------------------------------------------------

    #[test]
    fn event_count_empty() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        assert_eq!(mgr.event_count().expect("count"), 0);
    }

    #[test]
    fn event_count_excludes_comments_and_blanks() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("create");
        // Header has 2 comment lines, then we add events
        mgr.append_raw(2026, 1, "event1\n").expect("append");
        mgr.append_raw(2026, 1, "event2\n").expect("append");
        mgr.append_raw(2026, 1, "\n").expect("blank");

        assert_eq!(mgr.event_count().expect("count"), 2);
    }

    // -----------------------------------------------------------------------
    // is_empty
    // -----------------------------------------------------------------------

    #[test]
    fn is_empty_no_shards() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        assert!(mgr.is_empty().expect("empty"));
    }

    #[test]
    fn is_empty_with_shards() {
        let (_tmp, mgr) = setup();
        mgr.init().expect("init");
        assert!(!mgr.is_empty().expect("empty"));
    }

    // -----------------------------------------------------------------------
    // Manifest
    // -----------------------------------------------------------------------

    #[test]
    fn write_and_read_manifest() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("create");
        mgr.append_raw(2026, 1, "event-line-1\n").expect("append");
        mgr.append_raw(2026, 1, "event-line-2\n").expect("append");

        let written = mgr.write_manifest(2026, 1).expect("write manifest");
        assert_eq!(written.shard_name, "2026-01.events");
        assert_eq!(written.event_count, 2);
        assert!(written.byte_len > 0);
        assert!(written.file_hash.starts_with("blake3:"));

        let read = mgr
            .read_manifest(2026, 1)
            .expect("read")
            .expect("should exist");
        assert_eq!(read, written);
    }

    #[test]
    fn manifest_roundtrip() {
        let manifest = ShardManifest {
            shard_name: "2026-01.events".into(),
            event_count: 42,
            byte_len: 12345,
            file_hash: "blake3:abcdef0123456789".into(),
        };

        let repr = manifest.to_string_repr();
        let parsed = ShardManifest::from_string_repr(&repr).expect("parse");
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn read_manifest_missing() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        let result = mgr.read_manifest(2026, 1).expect("read");
        assert!(result.is_none());
    }

    #[test]
    fn manifest_event_count_excludes_comments() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("create");
        // Header has 2 comment lines
        mgr.append_raw(2026, 1, "event1\n").expect("append");

        let manifest = mgr.write_manifest(2026, 1).expect("manifest");
        // Only 1 event line, not the 2 header lines
        assert_eq!(manifest.event_count, 1);
    }

    // -----------------------------------------------------------------------
    // Rotation
    // -----------------------------------------------------------------------

    #[test]
    fn rotate_creates_shard_if_none_exist() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");

        let (y, m) = mgr.rotate_if_needed().expect("rotate");
        let (ey, em) = current_year_month();
        assert_eq!((y, m), (ey, em));

        assert!(mgr.shard_path(y, m).exists());
    }

    #[test]
    fn rotate_no_op_same_month() {
        let (_tmp, mgr) = setup();
        let (y, m) = mgr.init().expect("init");

        let (y2, m2) = mgr.rotate_if_needed().expect("rotate");
        assert_eq!((y, m), (y2, m2));
    }

    #[test]
    fn rotate_different_month_seals_and_creates() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");

        // Create an old shard
        mgr.create_shard(2025, 11).expect("create");
        mgr.append_raw(2025, 11, "old event\n").expect("append");
        mgr.update_symlink(2025, 11).expect("symlink");

        // Rotate should seal old and create new
        let (y, m) = mgr.rotate_if_needed().expect("rotate");
        let (ey, em) = current_year_month();
        assert_eq!((y, m), (ey, em));

        // Old shard should have a manifest
        assert!(mgr.manifest_path(2025, 11).exists());

        // New shard should exist
        assert!(mgr.shard_path(ey, em).exists());

        // Symlink should point to new shard
        #[cfg(unix)]
        {
            let target = fs::read_link(mgr.current_symlink()).expect("readlink");
            assert_eq!(target, PathBuf::from(ShardManager::shard_filename(ey, em)));
        }
    }

    // -----------------------------------------------------------------------
    // Frozen shards
    // -----------------------------------------------------------------------

    #[test]
    fn frozen_shard_not_modified_by_append() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");

        // Create and populate old shard
        mgr.create_shard(2025, 6).expect("create");
        mgr.append_raw(2025, 6, "old event\n").expect("append");
        let old_content = mgr.read_shard(2025, 6).expect("read");

        // Init creates current month shard
        mgr.init().expect("init");

        // Append only goes to active shard
        mgr.append("new event\n", false, Duration::from_secs(1))
            .expect("append");

        // Old shard is unchanged
        let after_content = mgr.read_shard(2025, 6).expect("read");
        assert_eq!(old_content, after_content);
    }

    // -----------------------------------------------------------------------
    // system_time_us
    // -----------------------------------------------------------------------

    #[test]
    fn system_time_us_is_positive() {
        let ts = system_time_us();
        assert!(ts > 0, "system time should be positive: {ts}");
    }

    #[test]
    fn system_time_us_is_reasonable() {
        let ts = system_time_us();
        // Should be after 2020-01-01 in microseconds
        let jan_2020_us: i64 = 1_577_836_800_000_000;
        assert!(ts > jan_2020_us, "system time too small: {ts}");
    }

    // -----------------------------------------------------------------------
    // total_content_len
    // -----------------------------------------------------------------------

    #[test]
    fn total_content_len_empty_repo() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        let len = mgr.total_content_len().expect("len");
        assert_eq!(len, 0);
    }

    #[test]
    fn total_content_len_single_shard() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("create");
        mgr.append_raw(2026, 1, "line1\n").expect("append");
        mgr.append_raw(2026, 1, "line2\n").expect("append");

        let full = mgr.replay().expect("replay");
        let len = mgr.total_content_len().expect("len");
        assert_eq!(len, full.len());
    }

    #[test]
    fn total_content_len_multiple_shards() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("shard 1");
        mgr.create_shard(2026, 2).expect("shard 2");
        mgr.append_raw(2026, 1, "jan-event\n").expect("append jan");
        mgr.append_raw(2026, 2, "feb-event\n").expect("append feb");

        let full = mgr.replay().expect("replay");
        let len = mgr.total_content_len().expect("len");
        assert_eq!(len, full.len(), "total_content_len must match replay len");
    }

    // -----------------------------------------------------------------------
    // read_content_range
    // -----------------------------------------------------------------------

    #[test]
    fn read_content_range_empty_range() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("create");
        mgr.append_raw(2026, 1, "event\n").expect("append");

        let result = mgr.read_content_range(5, 5).expect("range");
        assert!(result.is_empty());
    }

    #[test]
    fn read_content_range_within_single_shard() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("create");
        // shard header is 2 lines; add a known event line
        mgr.append_raw(2026, 1, "ABCDEF\n").expect("append");

        let full = mgr.replay().expect("replay");
        // Find the position of "ABCDEF"
        let pos = full.find("ABCDEF").expect("ABCDEF must be in shard");
        let range = mgr.read_content_range(pos, pos + 7).expect("range");
        assert_eq!(range, "ABCDEF\n");
    }

    #[test]
    fn read_content_range_across_shard_boundary() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("shard 1");
        mgr.create_shard(2026, 2).expect("shard 2");
        mgr.append_raw(2026, 1, "jan-last-line\n").expect("jan");
        mgr.append_raw(2026, 2, "feb-first-line\n").expect("feb");

        let full = mgr.replay().expect("replay");
        // Read entire concatenation as a range
        let range = mgr.read_content_range(0, full.len()).expect("full range");
        assert_eq!(range, full);
    }

    #[test]
    fn read_content_range_beyond_end() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("create");
        mgr.append_raw(2026, 1, "event\n").expect("append");

        let full = mgr.replay().expect("replay");
        // Requesting a range beyond the end should return empty
        let range = mgr
            .read_content_range(full.len(), full.len() + 100)
            .expect("beyond end");
        assert!(range.is_empty());
    }

    // -----------------------------------------------------------------------
    // replay_from_offset
    // -----------------------------------------------------------------------

    #[test]
    fn replay_from_offset_zero_returns_full_content() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("create");
        mgr.append_raw(2026, 1, "event1\n").expect("e1");
        mgr.append_raw(2026, 1, "event2\n").expect("e2");

        let full = mgr.replay().expect("full replay");
        let (from_zero, total_len) = mgr.replay_from_offset(0).expect("from 0");
        assert_eq!(from_zero, full);
        assert_eq!(total_len, full.len());
    }

    #[test]
    fn replay_from_offset_skips_content_before_cursor() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("create");
        mgr.append_raw(2026, 1, "event1\n").expect("e1");
        mgr.append_raw(2026, 1, "event2\n").expect("e2");
        mgr.append_raw(2026, 1, "event3\n").expect("e3");

        let full = mgr.replay().expect("full replay");

        // Find offset just after event2
        let cursor = full.find("event3").expect("event3 in content");
        let (tail, total_len) = mgr.replay_from_offset(cursor).expect("from cursor");
        assert_eq!(tail, "event3\n");
        assert_eq!(total_len, full.len());
    }

    #[test]
    fn replay_from_offset_at_end_returns_empty() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("create");
        mgr.append_raw(2026, 1, "event1\n").expect("e1");

        let full = mgr.replay().expect("full replay");
        let (tail, total_len) = mgr.replay_from_offset(full.len()).expect("at end");
        assert!(tail.is_empty(), "tail should be empty at end of content");
        assert_eq!(total_len, full.len());
    }

    #[test]
    fn replay_from_offset_skips_sealed_shards_before_cursor() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");

        // Two shards: a sealed shard (jan) and an active shard (feb)
        mgr.create_shard(2026, 1).expect("jan");
        mgr.create_shard(2026, 2).expect("feb");
        mgr.append_raw(2026, 1, "jan-event1\n").expect("jan e1");
        mgr.append_raw(2026, 1, "jan-event2\n").expect("jan e2");
        mgr.append_raw(2026, 2, "feb-event1\n").expect("feb e1");
        mgr.append_raw(2026, 2, "feb-event2\n").expect("feb e2");

        let full = mgr.replay().expect("full replay");
        let jan_shard_len = mgr.read_shard(2026, 1).expect("read jan").len();

        // Cursor is at the end of the jan shard — feb events are new
        let (tail, total_len) = mgr
            .replay_from_offset(jan_shard_len)
            .expect("from feb start");
        assert!(
            !tail.contains("jan-event"),
            "jan events should not appear in tail"
        );
        assert!(tail.contains("feb-event1"), "feb events must be in tail");
        assert!(tail.contains("feb-event2"), "feb events must be in tail");
        assert_eq!(total_len, full.len());
    }

    #[test]
    fn replay_from_offset_total_len_equals_total_content_len() {
        let (_tmp, mgr) = setup();
        mgr.ensure_dirs().expect("dirs");
        mgr.create_shard(2026, 1).expect("shard 1");
        mgr.create_shard(2026, 2).expect("shard 2");
        mgr.append_raw(2026, 1, "event-a\n").expect("ea");
        mgr.append_raw(2026, 2, "event-b\n").expect("eb");

        let total = mgr.total_content_len().expect("total_content_len");
        let (_, replay_total) = mgr.replay_from_offset(0).expect("replay_from_offset");
        assert_eq!(
            total, replay_total,
            "total_content_len and replay_from_offset total must agree"
        );
    }
}
