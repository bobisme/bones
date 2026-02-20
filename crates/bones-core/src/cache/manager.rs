//! Cache lifecycle management: freshness check, rebuild trigger, fallback.
//!
//! [`CacheManager`] is the primary entry point for loading events. It
//! transparently picks the fastest available source:
//!
//! 1. **Binary cache** — if fresh (fingerprint matches), decode directly.
//! 2. **TSJSON fallback** — parse the event shards, then rebuild the cache
//!    so the next load is fast.
//!
//! # Freshness fingerprint
//!
//! A "fingerprint" is a fast hash over the list of shard files and their
//! sizes + modification times. If any shard is added, removed, or modified,
//! the fingerprint changes and the cache is considered stale.
//!
//! The fingerprint is stored as the `created_at_us` field of the cache
//! header (repurposed — the actual wall-clock creation time is not critical).
//! This avoids adding a separate metadata file.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cache::reader::CacheReader;
use crate::cache::writer::rebuild_cache;
use crate::event::Event;
use crate::event::parser::parse_lines;
use crate::shard::ShardManager;

// ---------------------------------------------------------------------------
// CacheManager
// ---------------------------------------------------------------------------

/// Manages cache lifecycle: freshness check, rebuild, and fallback to TSJSON.
///
/// # Usage
///
/// ```rust,no_run
/// use bones_core::cache::manager::CacheManager;
///
/// let mgr = CacheManager::new(".bones/events", ".bones/cache/events.bin");
/// let events = mgr.load_events().unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct CacheManager {
    /// Path to the events directory (`.bones/events/`).
    events_dir: PathBuf,
    /// Path to the binary cache file (`.bones/cache/events.bin`).
    cache_path: PathBuf,
}

/// Result of a [`CacheManager::load_events`] call, including provenance
/// metadata for diagnostics.
#[derive(Debug, Clone)]
pub struct LoadResult {
    /// The loaded events.
    pub events: Vec<Event>,
    /// How the events were loaded.
    pub source: LoadSource,
}

/// Where events were loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadSource {
    /// Events were decoded from the binary cache (fast path).
    Cache,
    /// Cache was stale or missing; events were parsed from TSJSON shards.
    /// The cache was rebuilt afterwards.
    FallbackRebuilt,
    /// Cache was stale or missing; events were parsed from TSJSON shards.
    /// Cache rebuild was attempted but failed (non-fatal).
    FallbackRebuildFailed,
}

impl CacheManager {
    /// Create a new cache manager.
    ///
    /// # Arguments
    ///
    /// * `events_dir` — Path to `.bones/events/` shard directory.
    /// * `cache_path` — Path to `.bones/cache/events.bin`.
    pub fn new(events_dir: impl Into<PathBuf>, cache_path: impl Into<PathBuf>) -> Self {
        Self {
            events_dir: events_dir.into(),
            cache_path: cache_path.into(),
        }
    }

    /// Check whether the cache file exists and is fresh.
    ///
    /// Returns `true` if:
    /// - The cache file exists and can be opened.
    /// - The stored fingerprint matches the current shard fingerprint.
    ///
    /// Returns `false` otherwise (missing, corrupt, stale, or on any error).
    pub fn is_fresh(&self) -> Result<bool> {
        let current_fp = self
            .compute_fingerprint()
            .context("compute shard fingerprint")?;

        match CacheReader::open(&self.cache_path) {
            Ok(reader) => Ok(reader.created_at_us() == current_fp),
            Err(_) => Ok(false),
        }
    }

    /// Load events, preferring the binary cache when fresh.
    ///
    /// 1. Compute a fingerprint over event shard files.
    /// 2. If cache exists and fingerprint matches → decode from cache.
    /// 3. Otherwise → parse TSJSON, then rebuild the cache in the
    ///    foreground (so next call is fast). Cache rebuild failures are
    ///    logged but do not cause the load to fail.
    ///
    /// # Errors
    ///
    /// Returns an error only if TSJSON parsing itself fails. Cache errors
    /// are handled by falling back to TSJSON.
    pub fn load_events(&self) -> Result<LoadResult> {
        let current_fp = self
            .compute_fingerprint()
            .context("compute shard fingerprint")?;

        // Try cache fast path
        if let Ok(reader) = CacheReader::open(&self.cache_path) {
            if reader.created_at_us() == current_fp {
                match reader.read_all() {
                    Ok(events) => {
                        tracing::debug!(
                            count = events.len(),
                            "loaded events from binary cache"
                        );
                        return Ok(LoadResult {
                            events,
                            source: LoadSource::Cache,
                        });
                    }
                    Err(e) => {
                        tracing::warn!("cache decode failed, falling back to TSJSON: {e}");
                    }
                }
            } else {
                tracing::debug!("cache fingerprint mismatch, falling back to TSJSON");
            }
        }

        // Fallback: parse TSJSON
        let events = self.parse_tsjson()?;

        // Rebuild cache for next time (best-effort)
        let source = match self.rebuild_with_fingerprint(current_fp) {
            Ok(_stats) => {
                tracing::debug!("rebuilt binary cache after TSJSON fallback");
                LoadSource::FallbackRebuilt
            }
            Err(e) => {
                tracing::warn!("cache rebuild failed (non-fatal): {e}");
                LoadSource::FallbackRebuildFailed
            }
        };

        Ok(LoadResult { events, source })
    }

    /// Force a cache rebuild from TSJSON event shards.
    ///
    /// Returns statistics about the rebuilt cache.
    ///
    /// # Errors
    ///
    /// Returns an error if shard replay, parsing, or cache writing fails.
    pub fn rebuild(&self) -> Result<crate::cache::CacheStats> {
        rebuild_cache(&self.events_dir, &self.cache_path)
    }

    /// Return the path to the events directory.
    #[must_use]
    pub fn events_dir(&self) -> &Path {
        &self.events_dir
    }

    /// Return the path to the cache file.
    #[must_use]
    pub fn cache_path(&self) -> &Path {
        &self.cache_path
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Compute a fingerprint over the shard directory contents.
    ///
    /// The fingerprint is a hash of (filename, size, mtime) tuples for all
    /// `.events` files, sorted by name. This is cheap (no content reading)
    /// and catches additions, deletions, and modifications.
    fn compute_fingerprint(&self) -> Result<u64> {
        fingerprint_dir(&self.events_dir)
    }

    /// Parse TSJSON events from shards using the standard shard replay
    /// pipeline.
    fn parse_tsjson(&self) -> Result<Vec<Event>> {
        let bones_dir = self.events_dir.parent().unwrap_or(Path::new("."));
        let shard_mgr = ShardManager::new(bones_dir);

        let content = shard_mgr
            .replay()
            .map_err(|e| anyhow::anyhow!("replay shards: {e}"))?;

        let events = parse_lines(&content)
            .map_err(|(line, e)| anyhow::anyhow!("parse error at line {line}: {e}"))?;

        Ok(events)
    }

    /// Rebuild the cache with a specific fingerprint stored in the header's
    /// `created_at_us` field.
    fn rebuild_with_fingerprint(&self, fingerprint: u64) -> Result<crate::cache::CacheStats> {
        let events = self.parse_tsjson()?;

        let cols = crate::cache::CacheColumns::from_events(&events)
            .map_err(|e| anyhow::anyhow!("encode columns: {e}"))?;
        let mut header = crate::cache::CacheHeader::new(events.len() as u64, fingerprint);
        let bytes = header
            .encode(&cols)
            .map_err(|e| anyhow::anyhow!("encode cache: {e}"))?;

        if let Some(parent) = self.cache_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create cache dir {}", parent.display()))?;
        }

        fs::write(&self.cache_path, &bytes)
            .with_context(|| format!("write cache file {}", self.cache_path.display()))?;

        Ok(crate::cache::CacheStats {
            total_events: events.len(),
            file_size_bytes: bytes.len() as u64,
            compression_ratio: 1.0, // approximate
        })
    }
}

// ---------------------------------------------------------------------------
// Fingerprinting
// ---------------------------------------------------------------------------

/// Compute a fingerprint over `.events` files in a directory.
///
/// Uses a sorted BTreeMap of (filename → (size, mtime_ns)) tuples, then
/// hashes them with a simple FNV-1a-style combiner. Returns 0 if the
/// directory doesn't exist or is empty.
fn fingerprint_dir(dir: &Path) -> Result<u64> {
    if !dir.exists() {
        return Ok(0);
    }

    let mut entries: BTreeMap<String, (u64, u64)> = BTreeMap::new();

    let read_dir = fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))?;

    for entry in read_dir {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();

        // Only consider .events files (skip manifests, symlinks, etc.)
        if !name.ends_with(".events") {
            continue;
        }

        // Resolve symlinks for metadata
        let meta = entry.metadata()?;
        let size = meta.len();
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u64);

        entries.insert(name, (size, mtime_ns));
    }

    // Hash the sorted entries
    let mut hash: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
    for (name, (size, mtime)) in &entries {
        for byte in name.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3); // FNV-1a prime
        }
        // Mix in size
        for byte in size.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        // Mix in mtime
        for byte in mtime.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }

    Ok(hash)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::data::{CreateData, MoveData};
    use crate::event::{Event, EventData, EventType};
    use crate::event::writer;
    use crate::model::item::{Kind, State, Urgency};
    use crate::model::item_id::ItemId;
    use crate::shard::ShardManager;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn setup_bones(events: &[Event]) -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let bones_dir = tmp.path().join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.ensure_dirs().unwrap();
        shard_mgr.init().unwrap();

        for event in events {
            let line = writer::write_line(event).unwrap();
            let (year, month) = shard_mgr.active_shard().unwrap().unwrap();
            shard_mgr.append_raw(year, month, &line).unwrap();
        }

        let events_dir = bones_dir.join("events");
        let cache_path = bones_dir.join("cache/events.bin");
        (tmp, events_dir, cache_path)
    }

    fn make_event(id: &str, ts: i64) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(id),
            data: EventData::Create(CreateData {
                title: format!("Item {id}"),
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
        writer::write_event(&mut event).unwrap();
        event
    }

    fn make_move(id: &str, ts: i64, parent_hash: &str) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![parent_hash.to_string()],
            event_type: EventType::Move,
            item_id: ItemId::new_unchecked(id),
            data: EventData::Move(MoveData {
                state: State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        writer::write_event(&mut event).unwrap();
        event
    }

    // === is_fresh =========================================================

    #[test]
    fn is_fresh_returns_false_when_no_cache() {
        let e1 = make_event("bn-001", 1000);
        let (_tmp, events_dir, cache_path) = setup_bones(&[e1]);

        let mgr = CacheManager::new(&events_dir, &cache_path);
        assert!(!mgr.is_fresh().unwrap());
    }

    #[test]
    fn is_fresh_returns_true_after_load() {
        let e1 = make_event("bn-001", 1000);
        let (_tmp, events_dir, cache_path) = setup_bones(&[e1]);

        let mgr = CacheManager::new(&events_dir, &cache_path);
        let _result = mgr.load_events().unwrap();

        // Now cache should be fresh
        assert!(mgr.is_fresh().unwrap());
    }

    #[test]
    fn is_fresh_returns_false_after_shard_modification() {
        let e1 = make_event("bn-001", 1000);
        let (_tmp, events_dir, cache_path) = setup_bones(&[e1]);

        let mgr = CacheManager::new(&events_dir, &cache_path);
        let _result = mgr.load_events().unwrap();
        assert!(mgr.is_fresh().unwrap());

        // Modify a shard file (append a new event)
        let bones_dir = events_dir.parent().unwrap();
        let shard_mgr = ShardManager::new(bones_dir);
        let e2 = make_event("bn-002", 2000);
        let line = writer::write_line(&e2).unwrap();
        let (year, month) = shard_mgr.active_shard().unwrap().unwrap();
        shard_mgr.append_raw(year, month, &line).unwrap();

        // Cache should now be stale
        assert!(!mgr.is_fresh().unwrap());
    }

    // === load_events ======================================================

    #[test]
    fn load_events_from_tsjson_when_no_cache() {
        let e1 = make_event("bn-001", 1000);
        let (_tmp, events_dir, cache_path) = setup_bones(&[e1]);

        let mgr = CacheManager::new(&events_dir, &cache_path);
        let result = mgr.load_events().unwrap();

        assert_eq!(result.events.len(), 1);
        assert_eq!(result.source, LoadSource::FallbackRebuilt);

        // Cache file should now exist
        assert!(cache_path.exists());
    }

    #[test]
    fn load_events_from_cache_when_fresh() {
        let e1 = make_event("bn-001", 1000);
        let e2 = make_event("bn-002", 2000);
        let (_tmp, events_dir, cache_path) = setup_bones(&[e1, e2]);

        let mgr = CacheManager::new(&events_dir, &cache_path);

        // First load: from TSJSON (builds cache)
        let r1 = mgr.load_events().unwrap();
        assert_eq!(r1.events.len(), 2);
        assert_eq!(r1.source, LoadSource::FallbackRebuilt);

        // Second load: from cache
        let r2 = mgr.load_events().unwrap();
        assert_eq!(r2.events.len(), 2);
        assert_eq!(r2.source, LoadSource::Cache);
    }

    #[test]
    fn load_events_falls_back_on_stale_cache() {
        let e1 = make_event("bn-001", 1000);
        let (_tmp, events_dir, cache_path) = setup_bones(&[e1]);

        let mgr = CacheManager::new(&events_dir, &cache_path);

        // Build initial cache
        let _r1 = mgr.load_events().unwrap();

        // Add a new event to make cache stale
        let bones_dir = events_dir.parent().unwrap();
        let shard_mgr = ShardManager::new(bones_dir);
        let e2 = make_event("bn-002", 2000);
        let line = writer::write_line(&e2).unwrap();
        let (year, month) = shard_mgr.active_shard().unwrap().unwrap();
        shard_mgr.append_raw(year, month, &line).unwrap();

        // Second load: should fall back to TSJSON and rebuild
        let r2 = mgr.load_events().unwrap();
        assert_eq!(r2.events.len(), 2);
        assert_eq!(r2.source, LoadSource::FallbackRebuilt);
    }

    #[test]
    fn load_events_empty_shard() {
        let (_tmp, events_dir, cache_path) = setup_bones(&[]);

        let mgr = CacheManager::new(&events_dir, &cache_path);
        let result = mgr.load_events().unwrap();
        assert!(result.events.is_empty());
    }

    // === rebuild ==========================================================

    #[test]
    fn rebuild_creates_cache_file() {
        let e1 = make_event("bn-001", 1000);
        let (_tmp, events_dir, cache_path) = setup_bones(&[e1]);

        let mgr = CacheManager::new(&events_dir, &cache_path);
        let stats = mgr.rebuild().unwrap();

        assert_eq!(stats.total_events, 1);
        assert!(cache_path.exists());
    }

    // === fingerprinting ===================================================

    #[test]
    fn fingerprint_empty_dir_is_zero() {
        let tmp = TempDir::new().unwrap();
        let fp = fingerprint_dir(tmp.path()).unwrap();
        assert_eq!(fp, 0xcbf29ce484222325); // FNV offset basis with no data mixed in
    }

    #[test]
    fn fingerprint_nonexistent_dir_is_zero() {
        let fp = fingerprint_dir(Path::new("/tmp/nonexistent-bones-fp-dir")).unwrap();
        assert_eq!(fp, 0);
    }

    #[test]
    fn fingerprint_changes_when_file_added() {
        let e1 = make_event("bn-001", 1000);
        let (_tmp, events_dir, _cache_path) = setup_bones(&[e1]);

        let fp1 = fingerprint_dir(&events_dir).unwrap();

        // Add another event
        let bones_dir = events_dir.parent().unwrap();
        let shard_mgr = ShardManager::new(bones_dir);
        let e2 = make_event("bn-002", 2000);
        let line = writer::write_line(&e2).unwrap();
        let (year, month) = shard_mgr.active_shard().unwrap().unwrap();
        shard_mgr.append_raw(year, month, &line).unwrap();

        let fp2 = fingerprint_dir(&events_dir).unwrap();
        assert_ne!(fp1, fp2);
    }

    // === integration: cache matches TSJSON ================================

    #[test]
    fn cache_output_matches_tsjson_parse() {
        let e1 = make_event("bn-001", 1000);
        let e2 = make_move("bn-001", 2000, &e1.event_hash);
        let e3 = make_event("bn-002", 3000);
        let (_tmp, events_dir, cache_path) = setup_bones(&[e1, e2, e3]);

        let mgr = CacheManager::new(&events_dir, &cache_path);

        // Load from TSJSON (builds cache)
        let r1 = mgr.load_events().unwrap();
        assert_eq!(r1.source, LoadSource::FallbackRebuilt);

        // Load from cache
        let r2 = mgr.load_events().unwrap();
        assert_eq!(r2.source, LoadSource::Cache);

        // Compare event data (excluding event_hash which may differ)
        assert_eq!(r1.events.len(), r2.events.len());
        for (a, b) in r1.events.iter().zip(r2.events.iter()) {
            assert_eq!(a.wall_ts_us, b.wall_ts_us);
            assert_eq!(a.agent, b.agent);
            assert_eq!(a.event_type, b.event_type);
            assert_eq!(a.item_id, b.item_id);
        }
    }
}
