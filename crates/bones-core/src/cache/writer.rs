//! File I/O helpers for the binary columnar cache.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::cache::{decode_events, encode_events};
use crate::event::Event;
use crate::event::parser::parse_lines;
use crate::shard::ShardManager;

/// Statistics returned after writing a cache file.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheStats {
    /// Total events encoded in the cache file.
    pub total_events: usize,
    /// Final cache file size in bytes.
    pub file_size_bytes: u64,
    /// Approximate compressed size / source size ratio.
    pub compression_ratio: f64,
}

/// Writes events to on-disk binary cache format.
#[derive(Debug, Default)]
pub struct CacheWriter {
    events: Vec<Event>,
}

impl CacheWriter {
    /// Create an empty cache writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one event to the writer buffer.
    pub fn push_event(&mut self, event: &Event) {
        self.events.push(event.clone());
    }

    /// Encode all buffered events and write them to `path`.
    ///
    /// Parent directories are created automatically.
    ///
    /// # Errors
    ///
    /// Returns an error if encoding or file I/O fails.
    pub fn write_to_file(&self, path: &Path) -> Result<CacheStats> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create cache dir {}", parent.display()))?;
        }

        let created_at_us = now_us();
        let bytes = encode_events(&self.events, created_at_us)
            .map_err(|e| anyhow::anyhow!("encode cache events: {e}"))?;

        fs::write(path, &bytes).with_context(|| format!("write cache file {}", path.display()))?;

        let source_bytes = estimated_source_bytes(&self.events);
        let compression_ratio = if source_bytes == 0 {
            1.0
        } else {
            bytes.len() as f64 / source_bytes as f64
        };

        Ok(CacheStats {
            total_events: self.events.len(),
            file_size_bytes: bytes.len() as u64,
            compression_ratio,
        })
    }

    /// Append `new_events` to an existing cache file, rewriting the whole file.
    ///
    /// If the cache file does not exist, this behaves like a normal write with
    /// just `new_events`.
    ///
    /// # Errors
    ///
    /// Returns an error if decoding existing data, encoding, or file I/O fails.
    pub fn append_incremental(existing: &Path, new_events: &[Event]) -> Result<CacheStats> {
        let mut all_events = if existing.exists() {
            let data = fs::read(existing)
                .with_context(|| format!("read existing cache {}", existing.display()))?;
            let (_header, events) =
                decode_events(&data).map_err(|e| anyhow::anyhow!("decode existing cache: {e}"))?;
            events
        } else {
            Vec::new()
        };

        all_events.extend_from_slice(new_events);

        let writer = Self { events: all_events };
        writer.write_to_file(existing)
    }
}

/// Rebuild cache from `.bones/events` shards and write to `cache_path`.
///
/// # Errors
///
/// Returns an error if shard replay, parsing, encoding, or file I/O fails.
pub fn rebuild_cache(events_dir: &Path, cache_path: &Path) -> Result<CacheStats> {
    let bones_dir = events_dir.parent().unwrap_or(Path::new("."));
    let shard_mgr = ShardManager::new(bones_dir);

    let content = shard_mgr
        .replay()
        .map_err(|e| anyhow::anyhow!("replay shards: {e}"))?;

    let events = parse_lines(&content)
        .map_err(|(line, e)| anyhow::anyhow!("parse error at line {line}: {e}"))?;

    let mut writer = CacheWriter::new();
    for event in &events {
        writer.push_event(event);
    }

    writer.write_to_file(cache_path)
}

fn estimated_source_bytes(events: &[Event]) -> usize {
    events
        .iter()
        .map(|event| serde_json::to_vec(event).map_or(0, |v| v.len() + 1))
        .sum()
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::TempDir;

    use super::*;
    use crate::event::data::{CreateData, EventData, MoveData};
    use crate::event::types::EventType;
    use crate::event::{self, Event};
    use crate::model::item::{Kind, State, Urgency};
    use crate::model::item_id::ItemId;

    fn make_event(item_id: &str, ts: i64, kind: EventType) -> Event {
        let data = match kind {
            EventType::Create => EventData::Create(CreateData {
                title: format!("Item {item_id}"),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: Vec::new(),
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            _ => EventData::Move(MoveData {
                state: State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
        };

        let mut event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: Vec::new(),
            event_type: kind,
            item_id: ItemId::new_unchecked(item_id),
            data,
            event_hash: String::new(),
        };
        let _ = event::writer::write_event(&mut event);
        event
    }

    #[test]
    fn write_to_file_round_trips_events() {
        let tmp = TempDir::new().expect("tempdir");
        let cache_path = tmp.path().join(".bones/cache/events.bin");

        let mut writer = CacheWriter::new();
        writer.push_event(&make_event("bn-a1", 1000, EventType::Create));
        writer.push_event(&make_event("bn-a1", 2000, EventType::Move));

        let stats = writer.write_to_file(&cache_path).expect("write cache");
        assert_eq!(stats.total_events, 2);
        assert!(stats.file_size_bytes > 0);

        let data = fs::read(cache_path).expect("read cache file");
        let (_header, decoded) = decode_events(&data).expect("decode cache file");
        assert_eq!(decoded.len(), 2);
    }

    #[test]
    fn append_incremental_appends_new_events() {
        let tmp = TempDir::new().expect("tempdir");
        let cache_path = tmp.path().join(".bones/cache/events.bin");

        let mut writer = CacheWriter::new();
        writer.push_event(&make_event("bn-a1", 1000, EventType::Create));
        writer.write_to_file(&cache_path).expect("seed cache");

        let new_events = vec![make_event("bn-a2", 2000, EventType::Create)];
        let stats = CacheWriter::append_incremental(&cache_path, &new_events)
            .expect("append cache incrementally");

        assert_eq!(stats.total_events, 2);

        let data = fs::read(cache_path).expect("read cache file");
        let (_header, decoded) = decode_events(&data).expect("decode cache file");
        assert_eq!(decoded.len(), 2);
    }

    #[test]
    fn rebuild_cache_reads_events_shards() {
        let tmp = TempDir::new().expect("tempdir");
        let bones_dir = tmp.path().join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init");

        let mut event = make_event("bn-z9", 42, EventType::Create);
        let line = event::writer::write_line(&event).expect("line");
        let (year, month) = shard_mgr
            .active_shard()
            .expect("active shard")
            .expect("active shard value");
        shard_mgr
            .append_raw(year, month, &line)
            .expect("append event");

        let events_dir = bones_dir.join("events");
        let cache_path = bones_dir.join("cache/events.bin");
        let stats = rebuild_cache(&events_dir, &cache_path).expect("rebuild cache");

        assert_eq!(stats.total_events, 1);
        assert!(cache_path.exists());

        // ensure parse/hash fields remain valid enough to decode
        event.event_hash.clear();
    }
}
