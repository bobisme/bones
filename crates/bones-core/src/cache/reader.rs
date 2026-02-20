//! Binary cache file reader.
//!
//! [`CacheReader`] opens and validates a binary cache file, then provides
//! methods for decoding events — either all at once or a range — without
//! needing to know the encoding details.

use std::fs;
use std::path::Path;

use crate::cache::{CacheError, CacheHeader, decode_events};
use crate::event::Event;

// ---------------------------------------------------------------------------
// CacheReader
// ---------------------------------------------------------------------------

/// Reads events from a binary columnar cache file.
///
/// The reader validates magic bytes, version, and CRC on [`open`](Self::open),
/// then decodes columns using the [`ColumnCodec`](super::ColumnCodec) traits
/// to reconstruct [`Event`] structs.
///
/// # Example
///
/// ```rust,no_run
/// use bones_core::cache::reader::CacheReader;
///
/// let reader = CacheReader::open("path/to/events.bin").unwrap();
/// println!("cache contains {} events", reader.event_count());
/// let events = reader.read_all().unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct CacheReader {
    /// Decoded header metadata.
    header: CacheHeader,
    /// Raw file bytes (kept for range decoding).
    data: Vec<u8>,
}

impl CacheReader {
    /// Open and validate a cache file.
    ///
    /// Reads the file into memory, validates the magic bytes, format version,
    /// and CRC-64 checksum. Returns an error if any validation step fails.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError`] if the file cannot be read, or if magic,
    /// version, or CRC validation fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CacheReaderError> {
        let path = path.as_ref();
        let data = fs::read(path).map_err(|e| CacheReaderError::Io {
            path: path.display().to_string(),
            source: e,
        })?;

        // Validate by decoding the header (checks magic, version, CRC)
        let (header, _cols) = CacheHeader::decode(&data).map_err(CacheReaderError::Cache)?;

        Ok(Self { header, data })
    }

    /// Create a reader from raw bytes (useful for testing).
    ///
    /// # Errors
    ///
    /// Returns [`CacheError`] if validation fails.
    pub fn from_bytes(data: Vec<u8>) -> Result<Self, CacheReaderError> {
        let (header, _cols) = CacheHeader::decode(&data).map_err(CacheReaderError::Cache)?;
        Ok(Self { header, data })
    }

    /// Return the number of events (rows) in the cache file without decoding.
    #[must_use]
    pub fn event_count(&self) -> usize {
        self.header.row_count as usize
    }

    /// Return a reference to the cache header.
    #[must_use]
    pub fn header(&self) -> &CacheHeader {
        &self.header
    }

    /// Decode all events from the cache.
    ///
    /// **Note**: The `event_hash` field will be empty on reconstructed events.
    /// Callers needing hashes must recompute them.
    ///
    /// # Errors
    ///
    /// Returns [`CacheReaderError`] if column decoding fails.
    pub fn read_all(&self) -> Result<Vec<Event>, CacheReaderError> {
        let (_header, events) = decode_events(&self.data).map_err(CacheReaderError::Cache)?;
        Ok(events)
    }

    /// Decode a range of events from the cache.
    ///
    /// Returns events `[start .. start + count]`, clamped to the actual row
    /// count. If `start >= event_count()`, returns an empty Vec.
    ///
    /// **Implementation note**: The current columnar format requires decoding
    /// all rows and then slicing. A future optimisation could add per-column
    /// offset tables for truly random access.
    ///
    /// # Errors
    ///
    /// Returns [`CacheReaderError`] if column decoding fails.
    pub fn read_range(&self, start: usize, count: usize) -> Result<Vec<Event>, CacheReaderError> {
        if start >= self.event_count() {
            return Ok(Vec::new());
        }

        let all = self.read_all()?;
        let end = (start + count).min(all.len());
        Ok(all[start..end].to_vec())
    }

    /// Return the creation timestamp of the cache file (µs since epoch).
    #[must_use]
    pub fn created_at_us(&self) -> u64 {
        self.header.created_at_us
    }

    /// Return the stored CRC-64 checksum of the column data.
    #[must_use]
    pub fn data_crc64(&self) -> u64 {
        self.header.data_crc64
    }

    /// Return the total size of the raw cache data in bytes.
    #[must_use]
    pub fn file_size(&self) -> usize {
        self.data.len()
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors returned by [`CacheReader`].
#[derive(Debug, thiserror::Error)]
pub enum CacheReaderError {
    /// File I/O error.
    #[error("failed to read cache file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// Cache format/validation error.
    #[error("cache validation error: {0}")]
    Cache(#[from] CacheError),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::encode_events;
    use crate::event::data::{CreateData, MoveData};
    use crate::event::{Event, EventData, EventType};
    use crate::model::item::{Kind, State, Urgency};
    use crate::model::item_id::ItemId;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn make_event(ts: i64, agent: &str, et: EventType, item: &str) -> Event {
        let data = match et {
            EventType::Create => EventData::Create(CreateData {
                title: format!("Item {item}"),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
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
        Event {
            wall_ts_us: ts,
            agent: agent.to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: et,
            item_id: ItemId::new_unchecked(item),
            data,
            event_hash: format!("blake3:{ts:016x}"),
        }
    }

    fn write_cache_file(path: &Path, events: &[Event]) {
        let bytes = encode_events(events, 12345).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    // === open ==============================================================

    #[test]
    fn open_valid_cache_file() {
        let tmp = TempDir::new().unwrap();
        let cache_path = tmp.path().join("events.bin");
        let events = vec![
            make_event(1000, "alice", EventType::Create, "bn-001"),
            make_event(2000, "bob", EventType::Move, "bn-001"),
        ];
        write_cache_file(&cache_path, &events);

        let reader = CacheReader::open(&cache_path).unwrap();
        assert_eq!(reader.event_count(), 2);
        assert_eq!(reader.created_at_us(), 12345);
    }

    #[test]
    fn open_nonexistent_file_returns_io_error() {
        let err = CacheReader::open("/tmp/nonexistent-bones-cache.bin").unwrap_err();
        assert!(matches!(err, CacheReaderError::Io { .. }));
    }

    #[test]
    fn open_corrupt_file_returns_cache_error() {
        let tmp = TempDir::new().unwrap();
        let cache_path = tmp.path().join("corrupt.bin");
        std::fs::write(&cache_path, b"NOT A CACHE FILE").unwrap();

        let err = CacheReader::open(&cache_path).unwrap_err();
        assert!(matches!(err, CacheReaderError::Cache(_)));
    }

    // === from_bytes ========================================================

    #[test]
    fn from_bytes_valid() {
        let events = vec![make_event(1000, "alice", EventType::Create, "bn-001")];
        let bytes = encode_events(&events, 42).unwrap();
        let reader = CacheReader::from_bytes(bytes).unwrap();
        assert_eq!(reader.event_count(), 1);
    }

    // === read_all ==========================================================

    #[test]
    fn read_all_returns_all_events() {
        let events = vec![
            make_event(1000, "alice", EventType::Create, "bn-001"),
            make_event(2000, "bob", EventType::Create, "bn-002"),
            make_event(3000, "carol", EventType::Move, "bn-001"),
        ];
        let bytes = encode_events(&events, 0).unwrap();
        let reader = CacheReader::from_bytes(bytes).unwrap();

        let decoded = reader.read_all().unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0].agent, "alice");
        assert_eq!(decoded[1].agent, "bob");
        assert_eq!(decoded[2].event_type, EventType::Move);
    }

    #[test]
    fn read_all_empty_cache() {
        let bytes = encode_events(&[], 0).unwrap();
        let reader = CacheReader::from_bytes(bytes).unwrap();
        assert!(reader.read_all().unwrap().is_empty());
    }

    // === read_range ========================================================

    #[test]
    fn read_range_subset() {
        let events: Vec<Event> = (0..10)
            .map(|i| make_event(i * 1000, "agent", EventType::Create, &format!("bn-{i:03}")))
            .collect();
        let bytes = encode_events(&events, 0).unwrap();
        let reader = CacheReader::from_bytes(bytes).unwrap();

        let range = reader.read_range(3, 4).unwrap();
        assert_eq!(range.len(), 4);
        assert_eq!(range[0].wall_ts_us, 3000);
        assert_eq!(range[3].wall_ts_us, 6000);
    }

    #[test]
    fn read_range_clamped_to_end() {
        let events = vec![
            make_event(1000, "a", EventType::Create, "bn-001"),
            make_event(2000, "b", EventType::Create, "bn-002"),
        ];
        let bytes = encode_events(&events, 0).unwrap();
        let reader = CacheReader::from_bytes(bytes).unwrap();

        // Request more than available
        let range = reader.read_range(1, 100).unwrap();
        assert_eq!(range.len(), 1);
        assert_eq!(range[0].wall_ts_us, 2000);
    }

    #[test]
    fn read_range_start_past_end() {
        let events = vec![make_event(1000, "a", EventType::Create, "bn-001")];
        let bytes = encode_events(&events, 0).unwrap();
        let reader = CacheReader::from_bytes(bytes).unwrap();

        let range = reader.read_range(5, 10).unwrap();
        assert!(range.is_empty());
    }

    // === metadata accessors ================================================

    #[test]
    fn file_size_matches_encoded_bytes() {
        let events = vec![make_event(1000, "a", EventType::Create, "bn-001")];
        let bytes = encode_events(&events, 0).unwrap();
        let expected_size = bytes.len();
        let reader = CacheReader::from_bytes(bytes).unwrap();
        assert_eq!(reader.file_size(), expected_size);
    }

    #[test]
    fn data_crc64_is_nonzero_for_nonempty() {
        let events = vec![make_event(1000, "a", EventType::Create, "bn-001")];
        let bytes = encode_events(&events, 0).unwrap();
        let reader = CacheReader::from_bytes(bytes).unwrap();
        assert_ne!(reader.data_crc64(), 0);
    }
}
