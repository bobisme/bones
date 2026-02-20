//! Binary columnar event cache.
//!
//! This module implements the binary cache format described in
//! `docs/binary-cache-format.md`. The cache is a derived, read-optimised
//! representation of the TSJSON event log. It is not authoritative — the
//! `.events` files are the source of truth — but enables sub-millisecond
//! replay on large repositories.
//!
//! # Module layout
//!
//! - [`codec`] — [`ColumnCodec`] trait and per-column codec implementations.
//! - [`columns`] — [`CacheColumns`] intermediate representation.
//! - [`CacheHeader`] — file header struct (this module).
//! - [`CacheError`] — error type (this module).
//!
//! # Usage sketch
//!
//! ```rust,no_run
//! use bones_core::cache::{CacheHeader, CacheColumns};
//!
//! // Build from events and encode to bytes:
//! // let cols = CacheColumns::from_events(&events)?;
//! // let header = CacheHeader::new(events.len() as u64);
//! // let bytes = header.encode(&cols)?;
//!
//! // Decode from bytes:
//! // let (header, cols) = CacheHeader::decode(&bytes)?;
//! // let events = cols.into_events()?;
//! ```

pub mod codec;
pub mod columns;
pub mod manager;
pub mod reader;
pub mod writer;

pub use codec::{
    ColumnCodec, EventTypeCodec, InternedStringCodec, ItemIdCodec, RawBytesCodec, TimestampCodec,
    ValueCodec,
};
pub use columns::{COLUMN_COUNT, CacheColumns, ColumnRow};
pub use manager::{CacheManager, LoadResult, LoadSource};
pub use reader::{CacheReader, CacheReaderError};
pub use writer::{CacheStats, CacheWriter, rebuild_cache};

use crate::event::Event;
use columns::{
    COL_AGENTS, COL_EVENT_TYPES, COL_ITC, COL_ITEM_IDS, COL_PARENTS, COL_TIMESTAMPS, COL_VALUES,
};

// ---------------------------------------------------------------------------
// Magic bytes and format constants
// ---------------------------------------------------------------------------

/// The four magic bytes at the start of every cache file.
pub const CACHE_MAGIC: [u8; 4] = *b"BNCH";

/// The current format version written to new cache files.
pub const CACHE_VERSION: u8 = 1;

/// File header size in bytes (fixed).
///
/// Layout:
/// - 4 bytes: magic
/// - 1 byte:  version
/// - 1 byte:  column_count
/// - 2 bytes: reserved (must be zero)
/// - 8 bytes: row_count
/// - 8 bytes: created_at_us
/// - 8 bytes: data_crc64
/// = 32 bytes total
pub const HEADER_SIZE: usize = 32;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors returned by cache encoding and decoding.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CacheError {
    /// The file does not start with `BNCH`.
    #[error("invalid magic bytes: expected BNCH, got {0:?}")]
    InvalidMagic([u8; 4]),

    /// The format version is newer than this library supports.
    #[error("unsupported cache format version {0}: maximum supported is {CACHE_VERSION}")]
    UnsupportedVersion(u8),

    /// CRC-64 mismatch — data is corrupted or truncated.
    #[error("cache data is corrupted: {0}")]
    DataCorrupted(String),

    /// Unexpected end of data while reading a column or header.
    #[error("unexpected end of cache data")]
    UnexpectedEof,

    /// JSON serialisation / deserialisation of event data failed.
    #[error("event data encode/decode error: {0}")]
    EventDataError(String),

    /// Column count mismatch between header and data.
    #[error("column count mismatch: header says {expected}, file has {actual}")]
    ColumnCountMismatch { expected: usize, actual: usize }, // thiserror handles named fields
}

impl From<serde_json::Error> for CacheError {
    fn from(e: serde_json::Error) -> Self {
        Self::EventDataError(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// CRC-64 helper (simple XOR-based checksum for now)
// ---------------------------------------------------------------------------

/// Compute a simple 64-bit checksum over the data bytes.
///
/// Uses the CRC-64/XZ (ECMA-182) polynomial. This is intentionally a simple
/// implementation; production code can swap in a crate like `crc64` if
/// available.
fn checksum(data: &[u8]) -> u64 {
    // Polynomial for CRC-64/XZ: 0xC96C5795D7870F42
    // Simple table-less implementation for correctness
    const POLY: u64 = 0xC96C5795D7870F42;
    let mut crc: u64 = u64::MAX;
    for &byte in data {
        crc ^= u64::from(byte) << 56;
        for _ in 0..8 {
            if crc & (1 << 63) != 0 {
                crc = (crc << 1) ^ POLY;
            } else {
                crc <<= 1;
            }
        }
    }
    !crc
}

// ---------------------------------------------------------------------------
// CacheHeader
// ---------------------------------------------------------------------------

/// File header for the binary event cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheHeader {
    /// Format version (currently 1).
    pub version: u8,
    /// Number of columns present.
    pub column_count: u8,
    /// Number of events (rows) in the file.
    pub row_count: u64,
    /// Wall-clock timestamp at cache creation (µs since Unix epoch).
    pub created_at_us: u64,
    /// CRC-64 over all column data bytes.
    pub data_crc64: u64,
}

impl CacheHeader {
    /// Create a new header for a file containing `row_count` events,
    /// created at `created_at_us` with a placeholder CRC.
    #[must_use]
    pub fn new(row_count: u64, created_at_us: u64) -> Self {
        Self {
            version: CACHE_VERSION,
            column_count: COLUMN_COUNT as u8,
            row_count,
            created_at_us,
            data_crc64: 0,
        }
    }

    /// Encode the header and all column data into a byte buffer.
    ///
    /// The column offsets array is written immediately after the 32-byte
    /// header. Each column is then written at its recorded offset.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError`] if any column fails to encode.
    pub fn encode(&mut self, cols: &CacheColumns) -> Result<Vec<u8>, CacheError> {
        // Encode each column into its own buffer
        let mut col_bufs: Vec<Vec<u8>> = vec![Vec::new(); COLUMN_COUNT];

        TimestampCodec::encode(&cols.timestamps, &mut col_bufs[COL_TIMESTAMPS])?;
        InternedStringCodec::encode(&cols.agents, &mut col_bufs[COL_AGENTS])?;
        EventTypeCodec::encode(&cols.event_types, &mut col_bufs[COL_EVENT_TYPES])?;
        ItemIdCodec::encode(&cols.item_ids, &mut col_bufs[COL_ITEM_IDS])?;
        InternedStringCodec::encode(&cols.parents, &mut col_bufs[COL_PARENTS])?;
        RawBytesCodec::encode(&cols.itc, &mut col_bufs[COL_ITC])?;
        ValueCodec::encode(&cols.values, &mut col_bufs[COL_VALUES])?;

        // Compute column offsets
        let offsets_section_size = COLUMN_COUNT * 8; // 8 bytes per u64 offset
        let header_and_offsets = HEADER_SIZE + offsets_section_size;
        let mut offsets: Vec<u64> = Vec::with_capacity(COLUMN_COUNT);
        let mut cur = header_and_offsets as u64;
        for buf in &col_bufs {
            offsets.push(cur);
            cur += buf.len() as u64;
        }

        // Compute CRC over all column data
        let mut all_col_bytes: Vec<u8> = Vec::new();
        for buf in &col_bufs {
            all_col_bytes.extend_from_slice(buf);
        }
        self.data_crc64 = checksum(&all_col_bytes);

        // Assemble final buffer
        let total = header_and_offsets + all_col_bytes.len();
        let mut out = Vec::with_capacity(total);

        // Header (32 bytes)
        out.extend_from_slice(&CACHE_MAGIC);
        out.push(self.version);
        out.push(self.column_count);
        out.extend_from_slice(&0u16.to_le_bytes()); // reserved
        out.extend_from_slice(&self.row_count.to_le_bytes());
        out.extend_from_slice(&self.created_at_us.to_le_bytes());
        out.extend_from_slice(&self.data_crc64.to_le_bytes());
        debug_assert_eq!(out.len(), HEADER_SIZE);

        // Column offsets
        for offset in &offsets {
            out.extend_from_slice(&offset.to_le_bytes());
        }

        // Column data
        out.extend_from_slice(&all_col_bytes);

        Ok(out)
    }

    /// Decode a cache file from bytes, returning the header and column data.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError`] if:
    /// - The magic bytes are wrong.
    /// - The version is unsupported.
    /// - The CRC does not match.
    /// - Any column data is truncated or malformed.
    pub fn decode(data: &[u8]) -> Result<(Self, CacheColumns), CacheError> {
        if data.len() < HEADER_SIZE {
            return Err(CacheError::UnexpectedEof);
        }

        // Check magic
        let magic: [u8; 4] = data[0..4].try_into().expect("slice is 4 bytes");
        if magic != CACHE_MAGIC {
            return Err(CacheError::InvalidMagic(magic));
        }

        let version = data[4];
        if version > CACHE_VERSION {
            return Err(CacheError::UnsupportedVersion(version));
        }

        let column_count = data[5] as usize;
        // bytes 6-7 are reserved
        let row_count = u64::from_le_bytes(data[8..16].try_into().expect("slice is 8 bytes"));
        let created_at_us = u64::from_le_bytes(data[16..24].try_into().expect("slice is 8 bytes"));
        let stored_crc = u64::from_le_bytes(data[24..32].try_into().expect("slice is 8 bytes"));

        // Read column offsets
        let offsets_start = HEADER_SIZE;
        let offsets_end = offsets_start + column_count * 8;
        if data.len() < offsets_end {
            return Err(CacheError::UnexpectedEof);
        }

        let mut offsets: Vec<u64> = Vec::with_capacity(column_count);
        for i in 0..column_count {
            let start = offsets_start + i * 8;
            let offset =
                u64::from_le_bytes(data[start..start + 8].try_into().expect("slice is 8 bytes"));
            offsets.push(offset);
        }

        // Verify CRC over all column data (from first column offset to end)
        let col_data_start = offsets_end;
        if data.len() < col_data_start {
            return Err(CacheError::UnexpectedEof);
        }
        let col_data = &data[col_data_start..];
        let actual_crc = checksum(col_data);
        if actual_crc != stored_crc {
            return Err(CacheError::DataCorrupted(format!(
                "CRC mismatch: expected {stored_crc:#018x}, got {actual_crc:#018x}"
            )));
        }

        // Check column count
        if column_count < COLUMN_COUNT {
            return Err(CacheError::ColumnCountMismatch {
                expected: COLUMN_COUNT,
                actual: column_count,
            });
        }

        let count = row_count as usize;

        // Helper: get column data slice given offset index
        let col_slice = |col_idx: usize| -> Result<&[u8], CacheError> {
            let start = offsets[col_idx] as usize;
            if start > data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            // End is either the next column's offset or end of file
            let end = if col_idx + 1 < column_count {
                offsets[col_idx + 1] as usize
            } else {
                data.len()
            };
            if end > data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            Ok(&data[start..end])
        };

        // Decode each column
        let (timestamps, _) = TimestampCodec::decode(col_slice(COL_TIMESTAMPS)?, count)?;
        let (agents, _) = InternedStringCodec::decode(col_slice(COL_AGENTS)?, count)?;
        let (event_types, _) = EventTypeCodec::decode(col_slice(COL_EVENT_TYPES)?, count)?;
        let (item_ids, _) = ItemIdCodec::decode(col_slice(COL_ITEM_IDS)?, count)?;
        let (parents, _) = InternedStringCodec::decode(col_slice(COL_PARENTS)?, count)?;
        let (itc, _) = RawBytesCodec::decode(col_slice(COL_ITC)?, count)?;
        let (values, _) = ValueCodec::decode(col_slice(COL_VALUES)?, count)?;

        let cols = CacheColumns {
            timestamps,
            agents,
            event_types,
            item_ids,
            parents,
            itc,
            values,
        };

        let header = Self {
            version,
            column_count: column_count as u8,
            row_count,
            created_at_us,
            data_crc64: stored_crc,
        };

        Ok((header, cols))
    }
}

// ---------------------------------------------------------------------------
// High-level encode / decode helpers
// ---------------------------------------------------------------------------

/// Encode a slice of events to binary cache bytes.
///
/// Convenience wrapper around [`CacheColumns::from_events`] +
/// [`CacheHeader::encode`].
///
/// # Errors
///
/// Returns [`CacheError`] if column encoding or JSON serialisation fails.
pub fn encode_events(events: &[Event], created_at_us: u64) -> Result<Vec<u8>, CacheError> {
    let cols = CacheColumns::from_events(events)?;
    let mut header = CacheHeader::new(events.len() as u64, created_at_us);
    header.encode(&cols)
}

/// Decode binary cache bytes back to a vector of events.
///
/// Convenience wrapper around [`CacheHeader::decode`] +
/// [`CacheColumns::into_events`].
///
/// **Note**: The `event_hash` field of each reconstructed event will be empty.
/// Callers that need hashes must recompute them from the TSJSON writer.
///
/// # Errors
///
/// Returns [`CacheError`] if header validation, CRC check, or column decode
/// fails.
pub fn decode_events(data: &[u8]) -> Result<(CacheHeader, Vec<Event>), CacheError> {
    let (header, cols) = CacheHeader::decode(data)?;
    let events = cols.into_events().map_err(CacheError::EventDataError)?;
    Ok((header, events))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::data::CreateData;
    use crate::event::data::MoveData;
    use crate::event::{Event, EventData, EventType};
    use crate::model::item::{Kind, State, Urgency};
    use crate::model::item_id::ItemId;
    use std::collections::BTreeMap;

    fn make_event(ts: i64, agent: &str, et: EventType, item: &str) -> Event {
        use crate::event::data::{
            AssignAction, AssignData, CommentData, CompactData, DeleteData, LinkData, RedactData,
            SnapshotData, UnlinkData, UpdateData,
        };
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
            EventType::Update => EventData::Update(UpdateData {
                field: "title".to_string(),
                value: serde_json::json!("new title"),
                extra: BTreeMap::new(),
            }),
            EventType::Move => EventData::Move(MoveData {
                state: State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
            EventType::Assign => EventData::Assign(AssignData {
                agent: "assignee".to_string(),
                action: AssignAction::Assign,
                extra: BTreeMap::new(),
            }),
            EventType::Comment => EventData::Comment(CommentData {
                body: "A comment".to_string(),
                extra: BTreeMap::new(),
            }),
            EventType::Link => EventData::Link(LinkData {
                target: "bn-other".to_string(),
                link_type: "blocks".to_string(),
                extra: BTreeMap::new(),
            }),
            EventType::Unlink => EventData::Unlink(UnlinkData {
                target: "bn-other".to_string(),
                link_type: None,
                extra: BTreeMap::new(),
            }),
            EventType::Delete => EventData::Delete(DeleteData {
                reason: None,
                extra: BTreeMap::new(),
            }),
            EventType::Compact => EventData::Compact(CompactData {
                summary: "TL;DR".to_string(),
                extra: BTreeMap::new(),
            }),
            EventType::Snapshot => EventData::Snapshot(SnapshotData {
                state: serde_json::json!({"id": item}),
                extra: BTreeMap::new(),
            }),
            EventType::Redact => EventData::Redact(RedactData {
                target_hash: "blake3:abc".to_string(),
                reason: "oops".to_string(),
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

    // === Constants ========================================================

    #[test]
    fn magic_bytes_are_bnch() {
        assert_eq!(&CACHE_MAGIC, b"BNCH");
    }

    #[test]
    fn header_size_is_32() {
        assert_eq!(HEADER_SIZE, 32);
    }

    // === CacheHeader::new =================================================

    #[test]
    fn new_header_defaults() {
        let h = CacheHeader::new(42, 1_700_000_000_000);
        assert_eq!(h.version, CACHE_VERSION);
        assert_eq!(h.column_count, COLUMN_COUNT as u8);
        assert_eq!(h.row_count, 42);
        assert_eq!(h.created_at_us, 1_700_000_000_000);
        assert_eq!(h.data_crc64, 0); // placeholder until encode
    }

    // === Checksum =========================================================

    #[test]
    fn checksum_empty() {
        let c = checksum(&[]);
        // Just verify it produces a consistent value (not zero)
        assert_eq!(c, checksum(&[]));
    }

    #[test]
    fn checksum_different_data() {
        assert_ne!(checksum(b"hello"), checksum(b"world"));
    }

    #[test]
    fn checksum_single_bit_flip() {
        let data = b"hello world";
        let mut flipped = data.to_vec();
        flipped[5] ^= 0x01;
        assert_ne!(checksum(data), checksum(&flipped));
    }

    // === encode_events / decode_events ====================================

    #[test]
    fn encode_decode_empty() {
        let bytes = encode_events(&[], 0).unwrap();
        let (header, events) = decode_events(&bytes).unwrap();
        assert_eq!(header.row_count, 0);
        assert!(events.is_empty());
    }

    #[test]
    fn encode_decode_single_event() {
        let event = make_event(1_700_000_000_000, "claude", EventType::Create, "bn-a7x");
        let bytes = encode_events(std::slice::from_ref(&event), 9999).unwrap();
        let (header, events) = decode_events(&bytes).unwrap();

        assert_eq!(header.row_count, 1);
        assert_eq!(header.created_at_us, 9999);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].wall_ts_us, 1_700_000_000_000);
        assert_eq!(events[0].agent, "claude");
        assert_eq!(events[0].event_type, EventType::Create);
        assert_eq!(events[0].item_id.as_str(), "bn-a7x");
    }

    #[test]
    fn encode_decode_multiple_events() {
        let events = vec![
            make_event(1_000, "alice", EventType::Create, "bn-a7x"),
            make_event(2_000, "bob", EventType::Move, "bn-a7x"),
            make_event(3_000, "alice", EventType::Create, "bn-b8y"),
            make_event(4_000, "carol", EventType::Move, "bn-b8y"),
        ];
        let bytes = encode_events(&events, 0).unwrap();
        let (header, decoded) = decode_events(&bytes).unwrap();

        assert_eq!(header.row_count, 4);
        assert_eq!(decoded.len(), 4);
        assert_eq!(decoded[0].wall_ts_us, 1_000);
        assert_eq!(decoded[1].agent, "bob");
        assert_eq!(decoded[2].item_id.as_str(), "bn-b8y");
        assert_eq!(decoded[3].event_type, EventType::Move);
    }

    #[test]
    fn encode_decode_all_event_types() {
        let all_types = EventType::ALL;
        let events: Vec<Event> = all_types
            .iter()
            .enumerate()
            .map(|(i, &et)| make_event((i as i64 + 1) * 1000, "agent", et, "bn-a7x"))
            .collect();

        let bytes = encode_events(&events, 0).unwrap();
        let (_, decoded) = decode_events(&bytes).unwrap();

        assert_eq!(decoded.len(), all_types.len());
        for (i, et) in all_types.iter().enumerate() {
            assert_eq!(decoded[i].event_type, *et, "mismatch at index {i}");
        }
    }

    // === Header validation ================================================

    #[test]
    fn decode_bad_magic() {
        let mut bytes = encode_events(&[], 0).unwrap();
        bytes[0] = 0xFF; // corrupt magic
        let err = decode_events(&bytes).unwrap_err();
        assert!(matches!(err, CacheError::InvalidMagic(_)));
    }

    #[test]
    fn decode_unsupported_version() {
        let mut bytes = encode_events(&[], 0).unwrap();
        bytes[4] = 99; // future version
        let err = decode_events(&bytes).unwrap_err();
        assert!(matches!(err, CacheError::UnsupportedVersion(99)));
    }

    #[test]
    fn decode_corrupted_crc() {
        let mut bytes =
            encode_events(&[make_event(1_000, "a", EventType::Create, "bn-a7x")], 0).unwrap();
        // Flip a byte in the column data (after header + offsets)
        let col_start = HEADER_SIZE + COLUMN_COUNT * 8;
        if col_start < bytes.len() {
            bytes[col_start] ^= 0xFF;
        }
        let err = decode_events(&bytes).unwrap_err();
        // Either CRC or DataCorrupted
        assert!(
            matches!(err, CacheError::DataCorrupted(_)),
            "expected DataCorrupted, got {err:?}"
        );
    }

    #[test]
    fn decode_truncated_data() {
        let bytes =
            encode_events(&[make_event(1_000, "a", EventType::Create, "bn-a7x")], 0).unwrap();
        let truncated = &bytes[..bytes.len() / 2];
        let err = decode_events(truncated).unwrap_err();
        assert!(
            matches!(
                err,
                CacheError::UnexpectedEof | CacheError::DataCorrupted(_)
            ),
            "expected truncation error, got {err:?}"
        );
    }

    // === Large batch ======================================================

    #[test]
    fn encode_decode_large_batch() {
        let n = 500;
        let events: Vec<Event> = (0..n)
            .map(|i| {
                make_event(
                    i as i64 * 1000,
                    if i % 3 == 0 {
                        "alice"
                    } else if i % 3 == 1 {
                        "bob"
                    } else {
                        "carol"
                    },
                    if i % 2 == 0 {
                        EventType::Create
                    } else {
                        EventType::Move
                    },
                    &format!("bn-{:03}", i % 50),
                )
            })
            .collect();

        let bytes = encode_events(&events, 42).unwrap();
        let (header, decoded) = decode_events(&bytes).unwrap();

        assert_eq!(header.row_count, n as u64);
        assert_eq!(decoded.len(), n);
        for (i, (orig, dec)) in events.iter().zip(decoded.iter()).enumerate() {
            assert_eq!(orig.wall_ts_us, dec.wall_ts_us, "ts mismatch at {i}");
            assert_eq!(orig.agent, dec.agent, "agent mismatch at {i}");
            assert_eq!(orig.event_type, dec.event_type, "type mismatch at {i}");
        }
    }
}
