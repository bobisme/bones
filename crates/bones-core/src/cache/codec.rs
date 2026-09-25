//! Column codec trait and implementations for the binary cache format.
//!
//! Each column type has a dedicated codec that exploits the statistical
//! properties of that column's data for compact encoding. All codecs
//! implement the [`ColumnCodec`] trait.
//!
//! See `docs/binary-cache-format.md` for byte-level format documentation.

use super::CacheError;
use crate::event::EventType;

// ---------------------------------------------------------------------------
// ColumnCodec trait
// ---------------------------------------------------------------------------

/// Trait for encoding and decoding a single column of event data.
///
/// Each column type has its own codec implementation optimised for the
/// data's statistical properties.
pub trait ColumnCodec {
    /// The decoded Rust type for elements in this column.
    type Item;

    /// Encode a slice of items into bytes.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError`] if encoding fails.
    fn encode(items: &[Self::Item], buf: &mut Vec<u8>) -> Result<(), CacheError>;

    /// Decode `count` items from a byte slice.
    ///
    /// Returns the decoded items and the number of bytes consumed.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError`] if decoding fails (truncated data, invalid
    /// encoding, etc.).
    fn decode(data: &[u8], count: usize) -> Result<(Vec<Self::Item>, usize), CacheError>;
}

// ---------------------------------------------------------------------------
// Varint helpers
// ---------------------------------------------------------------------------

/// Encode an unsigned 64-bit value as LEB128.
pub(crate) fn encode_varint(value: u64, buf: &mut Vec<u8>) {
    let mut v = value;
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            break;
        }
        buf.push(byte | 0x80);
    }
}

/// Decode a LEB128-encoded unsigned varint from `data`, returning the value
/// and bytes consumed.
///
/// Accepts only the canonical encoding that [`encode_varint`] produces, so
/// every value has exactly one accepted byte form.
///
/// # Errors
///
/// Returns [`CacheError::UnexpectedEof`] if the data is truncated, and
/// [`CacheError::DataCorrupted`] for an overlong encoding (a zero final
/// group after the first byte) or a 10th byte that overflows a `u64`.
pub(crate) fn decode_varint(data: &[u8]) -> Result<(u64, usize), CacheError> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    for (i, &byte) in data.iter().enumerate() {
        let low = u64::from(byte & 0x7F);
        // The 10th group holds only bit 63; anything more overflows a u64.
        if shift == 63 && byte > 1 {
            return Err(CacheError::DataCorrupted(
                "varint overflow: 10th byte exceeds u64".into(),
            ));
        }
        value |= low << shift;
        if byte & 0x80 == 0 {
            if byte == 0 && i > 0 {
                return Err(CacheError::DataCorrupted(
                    "varint overlong: trailing zero group".into(),
                ));
            }
            return Ok((value, i + 1));
        }
        shift += 7;
    }
    Err(CacheError::UnexpectedEof)
}

/// Zigzag-encode a signed value (maps negative numbers to odd positives).
#[inline]
pub(crate) const fn zigzag_encode(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)).cast_unsigned()
}

/// Zigzag-decode a value produced by [`zigzag_encode`].
#[inline]
pub(crate) const fn zigzag_decode(n: u64) -> i64 {
    (n >> 1).cast_signed() ^ -((n & 1).cast_signed())
}

// ---------------------------------------------------------------------------
// TimestampCodec
// ---------------------------------------------------------------------------

/// Delta-encoded varint timestamps.
///
/// The first value is stored as an absolute little-endian `i64` (8 bytes).
/// Subsequent values are stored as zigzag-encoded LEB128 varints representing
/// the delta from the previous timestamp.
pub struct TimestampCodec;

impl ColumnCodec for TimestampCodec {
    type Item = i64;

    fn encode(items: &[i64], buf: &mut Vec<u8>) -> Result<(), CacheError> {
        if items.is_empty() {
            return Ok(());
        }
        // First value: absolute i64 little-endian
        buf.extend_from_slice(&items[0].to_le_bytes());
        // Subsequent values: zigzag delta varints
        let mut prev = items[0];
        for &ts in &items[1..] {
            // Wrapping keeps any i64 pair representable; decode wraps back.
            let delta = ts.wrapping_sub(prev);
            encode_varint(zigzag_encode(delta), buf);
            prev = ts;
        }
        Ok(())
    }

    fn decode(data: &[u8], count: usize) -> Result<(Vec<i64>, usize), CacheError> {
        if count == 0 {
            return Ok((vec![], 0));
        }
        if data.len() < 8 {
            return Err(CacheError::UnexpectedEof);
        }
        let first = i64::from_le_bytes(data[..8].try_into().expect("slice is 8 bytes"));
        let mut result = Vec::with_capacity(count);
        result.push(first);

        let mut pos = 8;
        let mut prev = first;
        for _ in 1..count {
            if pos >= data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            let (zz, consumed) = decode_varint(&data[pos..])?;
            let delta = zigzag_decode(zz);
            let ts = prev.wrapping_add(delta);
            result.push(ts);
            prev = ts;
            pos += consumed;
        }
        Ok((result, pos))
    }
}

// ---------------------------------------------------------------------------
// InternedStringCodec
// ---------------------------------------------------------------------------

/// Interned string table with run-length-encoded `u16` references.
///
/// Used for agent IDs and parent hash lists, both of which have low
/// cardinality and high repetition.
///
/// Layout:
/// ```text
/// [table_count: u32 LE]
/// [len_0: u16 LE] [bytes_0...]
/// ...
/// [run_count: u32 LE]
/// [run_len_0: u16 LE] [index_0: u16 LE]
/// ...
/// ```
pub struct InternedStringCodec;

impl ColumnCodec for InternedStringCodec {
    type Item = String;

    fn encode(items: &[String], buf: &mut Vec<u8>) -> Result<(), CacheError> {
        // Build string table (insertion order = index)
        let mut table: Vec<&str> = Vec::new();
        let mut index_of: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
        let mut indices: Vec<u16> = Vec::with_capacity(items.len());

        for item in items {
            let idx = if let Some(&i) = index_of.get(item.as_str()) {
                i
            } else {
                let i = u16::try_from(table.len()).map_err(|_| {
                    CacheError::DataCorrupted("string table exceeds 65535 entries".into())
                })?;
                table.push(item.as_str());
                index_of.insert(item.as_str(), i);
                i
            };
            indices.push(idx);
        }

        // Write string table
        let table_count = u32::try_from(table.len())
            .map_err(|_| CacheError::DataCorrupted("table too large".into()))?;
        buf.extend_from_slice(&table_count.to_le_bytes());
        for s in &table {
            let len = u16::try_from(s.len())
                .map_err(|_| CacheError::DataCorrupted("string too long for u16".into()))?;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }

        // Write RLE-encoded indices
        let runs = rle_encode_u16(&indices);
        let run_count = u32::try_from(runs.len())
            .map_err(|_| CacheError::DataCorrupted("run count overflow".into()))?;
        buf.extend_from_slice(&run_count.to_le_bytes());
        for (run_len, idx) in &runs {
            buf.extend_from_slice(&run_len.to_le_bytes());
            buf.extend_from_slice(&idx.to_le_bytes());
        }

        Ok(())
    }

    fn decode(data: &[u8], count: usize) -> Result<(Vec<String>, usize), CacheError> {
        let mut pos = 0;

        // Read string table
        if data.len() < 4 {
            return Err(CacheError::UnexpectedEof);
        }
        let table_count =
            u32::from_le_bytes(data[pos..pos + 4].try_into().expect("slice is 4 bytes")) as usize;
        pos += 4;

        let mut table: Vec<String> = Vec::with_capacity(table_count);
        for _ in 0..table_count {
            if pos + 2 > data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            let len = u16::from_le_bytes(data[pos..pos + 2].try_into().expect("slice is 2 bytes"))
                as usize;
            pos += 2;
            if pos + len > data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            let s = std::str::from_utf8(&data[pos..pos + len])
                .map_err(|e| CacheError::DataCorrupted(format!("invalid UTF-8 in string: {e}")))?
                .to_string();
            table.push(s);
            pos += len;
        }

        // Read RLE runs
        if pos + 4 > data.len() {
            return Err(CacheError::UnexpectedEof);
        }
        let run_count =
            u32::from_le_bytes(data[pos..pos + 4].try_into().expect("slice is 4 bytes")) as usize;
        pos += 4;

        let mut result = Vec::with_capacity(count);
        for _ in 0..run_count {
            if pos + 4 > data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            let run_len =
                u16::from_le_bytes(data[pos..pos + 2].try_into().expect("slice is 2 bytes"))
                    as usize;
            pos += 2;
            let idx = u16::from_le_bytes(data[pos..pos + 2].try_into().expect("slice is 2 bytes"))
                as usize;
            pos += 2;
            let s = table.get(idx).ok_or_else(|| {
                CacheError::DataCorrupted(format!("string index {idx} out of range"))
            })?;
            for _ in 0..run_len {
                result.push(s.clone());
            }
        }

        if result.len() != count {
            return Err(CacheError::DataCorrupted(format!(
                "expected {count} items, got {}",
                result.len()
            )));
        }

        Ok((result, pos))
    }
}

// ---------------------------------------------------------------------------
// EventTypeCodec
// ---------------------------------------------------------------------------

/// 4-bit RLE-encoded event types.
///
/// The 11 event types are mapped to values 0–10, packed two per byte (low
/// nibble first), then the packed bytes are RLE-compressed with
/// `[run_length: u8] [packed_byte: u8]` pairs.
///
/// Layout:
/// ```text
/// [packed_byte_count: u32 LE]
/// [run_count: u32 LE]
/// [run_len_0: u8] [packed_byte_0: u8]
/// ...
/// ```
pub struct EventTypeCodec;

impl EventTypeCodec {
    const fn type_to_nibble(et: EventType) -> u8 {
        match et {
            EventType::Create => 0,
            EventType::Update => 1,
            EventType::Move => 2,
            EventType::Assign => 3,
            EventType::Comment => 4,
            EventType::Link => 5,
            EventType::Unlink => 6,
            EventType::Delete => 7,
            EventType::Compact => 8,
            EventType::Snapshot => 9,
            EventType::Redact => 10,
        }
    }

    fn nibble_to_type(nibble: u8) -> Result<EventType, CacheError> {
        match nibble {
            0 => Ok(EventType::Create),
            1 => Ok(EventType::Update),
            2 => Ok(EventType::Move),
            3 => Ok(EventType::Assign),
            4 => Ok(EventType::Comment),
            5 => Ok(EventType::Link),
            6 => Ok(EventType::Unlink),
            7 => Ok(EventType::Delete),
            8 => Ok(EventType::Compact),
            9 => Ok(EventType::Snapshot),
            10 => Ok(EventType::Redact),
            _ => Err(CacheError::DataCorrupted(format!(
                "unknown event type nibble: {nibble}"
            ))),
        }
    }
}

impl ColumnCodec for EventTypeCodec {
    type Item = EventType;

    fn encode(items: &[EventType], buf: &mut Vec<u8>) -> Result<(), CacheError> {
        // Pack nibbles: two event types per byte, low nibble first
        let packed_count = items.len().div_ceil(2);
        let mut packed: Vec<u8> = Vec::with_capacity(packed_count);
        for chunk in items.chunks(2) {
            let lo = Self::type_to_nibble(chunk[0]);
            let hi = if chunk.len() > 1 {
                Self::type_to_nibble(chunk[1])
            } else {
                0x0F // padding nibble
            };
            packed.push(lo | (hi << 4));
        }

        // RLE over packed bytes
        let runs = rle_encode_u8(&packed);

        // Write packed_byte_count + RLE runs
        let packed_count_u32 = u32::try_from(packed_count)
            .map_err(|_| CacheError::DataCorrupted("too many events".into()))?;
        buf.extend_from_slice(&packed_count_u32.to_le_bytes());

        let run_count = u32::try_from(runs.len())
            .map_err(|_| CacheError::DataCorrupted("run count overflow".into()))?;
        buf.extend_from_slice(&run_count.to_le_bytes());
        for (run_len, byte) in &runs {
            buf.push(*run_len);
            buf.push(*byte);
        }

        Ok(())
    }

    fn decode(data: &[u8], count: usize) -> Result<(Vec<EventType>, usize), CacheError> {
        let mut pos = 0;

        if pos + 4 > data.len() {
            return Err(CacheError::UnexpectedEof);
        }
        let _packed_count =
            u32::from_le_bytes(data[pos..pos + 4].try_into().expect("slice is 4 bytes")) as usize;
        pos += 4;

        if pos + 4 > data.len() {
            return Err(CacheError::UnexpectedEof);
        }
        let run_count =
            u32::from_le_bytes(data[pos..pos + 4].try_into().expect("slice is 4 bytes")) as usize;
        pos += 4;

        // Decode RLE runs into packed bytes
        let mut packed: Vec<u8> = Vec::new();
        for _ in 0..run_count {
            if pos + 2 > data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            let run_len = data[pos] as usize;
            let byte = data[pos + 1];
            pos += 2;
            for _ in 0..run_len {
                packed.push(byte);
            }
        }

        // Unpack nibbles into event types
        let mut result = Vec::with_capacity(count);
        for (i, &byte) in packed.iter().enumerate() {
            if result.len() >= count {
                break;
            }
            let lo = byte & 0x0F;
            result.push(Self::nibble_to_type(lo)?);
            if result.len() >= count {
                break;
            }
            // Don't decode the hi nibble of the last byte if it's padding
            let is_last = i == packed.len() - 1;
            if !is_last || count.is_multiple_of(2) {
                let hi = (byte >> 4) & 0x0F;
                result.push(Self::nibble_to_type(hi)?);
            }
        }

        if result.len() > count {
            result.truncate(count);
        }

        if result.len() != count {
            return Err(CacheError::DataCorrupted(format!(
                "expected {count} event types, decoded {}",
                result.len()
            )));
        }

        Ok((result, pos))
    }
}

// ---------------------------------------------------------------------------
// ItemIdCodec
// ---------------------------------------------------------------------------

/// Dictionary-encoded item IDs with RLE-encoded u32 indices.
///
/// Similar to `InternedStringCodec` but uses `u32` indices to support
/// repositories with more than 65535 unique item IDs.
///
/// Layout:
/// ```text
/// [dict_count: u32 LE]
/// [len_0: u16 LE] [bytes_0...]
/// ...
/// [run_count: u32 LE]
/// [run_len_0: u16 LE] [index_0: u32 LE]
/// ...
/// ```
pub struct ItemIdCodec;

impl ColumnCodec for ItemIdCodec {
    type Item = String;

    fn encode(items: &[String], buf: &mut Vec<u8>) -> Result<(), CacheError> {
        // Build dictionary
        let mut dict: Vec<&str> = Vec::new();
        let mut index_of: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
        let mut indices: Vec<u32> = Vec::with_capacity(items.len());

        for item in items {
            let idx = if let Some(&i) = index_of.get(item.as_str()) {
                i
            } else {
                let i = u32::try_from(dict.len()).map_err(|_| {
                    CacheError::DataCorrupted("item ID dict exceeds u32::MAX entries".into())
                })?;
                dict.push(item.as_str());
                index_of.insert(item.as_str(), i);
                i
            };
            indices.push(idx);
        }

        // Write dictionary
        let dict_count = u32::try_from(dict.len())
            .map_err(|_| CacheError::DataCorrupted("dict too large".into()))?;
        buf.extend_from_slice(&dict_count.to_le_bytes());
        for s in &dict {
            let len = u16::try_from(s.len())
                .map_err(|_| CacheError::DataCorrupted("item ID string too long".into()))?;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }

        // Write RLE-encoded u32 indices
        let runs = rle_encode_u32(&indices);
        let run_count = u32::try_from(runs.len())
            .map_err(|_| CacheError::DataCorrupted("run count overflow".into()))?;
        buf.extend_from_slice(&run_count.to_le_bytes());
        for (run_len, idx) in &runs {
            buf.extend_from_slice(&run_len.to_le_bytes());
            buf.extend_from_slice(&idx.to_le_bytes());
        }

        Ok(())
    }

    fn decode(data: &[u8], count: usize) -> Result<(Vec<String>, usize), CacheError> {
        let mut pos = 0;

        // Read dictionary
        if pos + 4 > data.len() {
            return Err(CacheError::UnexpectedEof);
        }
        let dict_count =
            u32::from_le_bytes(data[pos..pos + 4].try_into().expect("slice is 4 bytes")) as usize;
        pos += 4;

        let mut dict: Vec<String> = Vec::with_capacity(dict_count);
        for _ in 0..dict_count {
            if pos + 2 > data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            let len = u16::from_le_bytes(data[pos..pos + 2].try_into().expect("slice is 2 bytes"))
                as usize;
            pos += 2;
            if pos + len > data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            let s = std::str::from_utf8(&data[pos..pos + len])
                .map_err(|e| CacheError::DataCorrupted(format!("invalid UTF-8 in item ID: {e}")))?
                .to_string();
            dict.push(s);
            pos += len;
        }

        // Read RLE runs
        if pos + 4 > data.len() {
            return Err(CacheError::UnexpectedEof);
        }
        let run_count =
            u32::from_le_bytes(data[pos..pos + 4].try_into().expect("slice is 4 bytes")) as usize;
        pos += 4;

        let mut result = Vec::with_capacity(count);
        for _ in 0..run_count {
            if pos + 6 > data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            let run_len =
                u16::from_le_bytes(data[pos..pos + 2].try_into().expect("slice is 2 bytes"))
                    as usize;
            pos += 2;
            let idx = u32::from_le_bytes(data[pos..pos + 4].try_into().expect("slice is 4 bytes"))
                as usize;
            pos += 4;
            let s = dict.get(idx).ok_or_else(|| {
                CacheError::DataCorrupted(format!("item ID index {idx} out of range"))
            })?;
            for _ in 0..run_len {
                result.push(s.clone());
            }
        }

        if result.len() != count {
            return Err(CacheError::DataCorrupted(format!(
                "expected {count} item IDs, got {}",
                result.len()
            )));
        }

        Ok((result, pos))
    }
}

// ---------------------------------------------------------------------------
// RawBytesCodec
// ---------------------------------------------------------------------------

/// Length-prefixed raw byte strings.
///
/// Used for ITC stamps and other variable-length fields that don't benefit
/// from string interning or dictionary encoding.
///
/// Layout:
/// ```text
/// [len_0: u16 LE] [bytes_0...]
/// [len_1: u16 LE] [bytes_1...]
/// ...
/// ```
pub struct RawBytesCodec;

impl ColumnCodec for RawBytesCodec {
    type Item = String;

    fn encode(items: &[String], buf: &mut Vec<u8>) -> Result<(), CacheError> {
        for s in items {
            let len = u16::try_from(s.len()).map_err(|_| {
                CacheError::DataCorrupted("string too long for u16 length prefix".into())
            })?;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Ok(())
    }

    fn decode(data: &[u8], count: usize) -> Result<(Vec<String>, usize), CacheError> {
        let mut pos = 0;
        let mut result = Vec::with_capacity(count);
        for _ in 0..count {
            if pos + 2 > data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            let len = u16::from_le_bytes(data[pos..pos + 2].try_into().expect("slice is 2 bytes"))
                as usize;
            pos += 2;
            if pos + len > data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            let s = std::str::from_utf8(&data[pos..pos + len])
                .map_err(|e| CacheError::DataCorrupted(format!("invalid UTF-8 in raw bytes: {e}")))?
                .to_string();
            result.push(s);
            pos += len;
        }
        Ok((result, pos))
    }
}

// ---------------------------------------------------------------------------
// ValueCodec
// ---------------------------------------------------------------------------

/// Type-specific value encoding for event payloads.
///
/// Stores each event's JSON payload as a length-prefixed UTF-8 string with
/// a `u32` length prefix to support payloads larger than 64 KiB.
///
/// Layout:
/// ```text
/// [len_0: u32 LE] [json_0: UTF-8]
/// [len_1: u32 LE] [json_1: UTF-8]
/// ...
/// ```
pub struct ValueCodec;

impl ColumnCodec for ValueCodec {
    type Item = String;

    fn encode(items: &[String], buf: &mut Vec<u8>) -> Result<(), CacheError> {
        for s in items {
            let len = u32::try_from(s.len()).map_err(|_| {
                CacheError::DataCorrupted("payload too large for u32 length prefix".into())
            })?;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Ok(())
    }

    fn decode(data: &[u8], count: usize) -> Result<(Vec<String>, usize), CacheError> {
        let mut pos = 0;
        let mut result = Vec::with_capacity(count);
        for _ in 0..count {
            if pos + 4 > data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            let len = u32::from_le_bytes(data[pos..pos + 4].try_into().expect("slice is 4 bytes"))
                as usize;
            pos += 4;
            if pos + len > data.len() {
                return Err(CacheError::UnexpectedEof);
            }
            let s = std::str::from_utf8(&data[pos..pos + len])
                .map_err(|e| {
                    CacheError::DataCorrupted(format!("invalid UTF-8 in value payload: {e}"))
                })?
                .to_string();
            result.push(s);
            pos += len;
        }
        Ok((result, pos))
    }
}

// ---------------------------------------------------------------------------
// RLE helpers
// ---------------------------------------------------------------------------

/// Run-length encode a `u8` slice into `(run_length, value)` pairs.
fn rle_encode_u8(items: &[u8]) -> Vec<(u8, u8)> {
    let mut runs: Vec<(u8, u8)> = Vec::new();
    if items.is_empty() {
        return runs;
    }
    let mut current = items[0];
    let mut count: u8 = 1;
    for &item in &items[1..] {
        if item == current && count < u8::MAX {
            count += 1;
        } else {
            runs.push((count, current));
            current = item;
            count = 1;
        }
    }
    runs.push((count, current));
    runs
}

/// Run-length encode a `u16` slice into `(run_length, value)` pairs.
fn rle_encode_u16(items: &[u16]) -> Vec<(u16, u16)> {
    let mut runs: Vec<(u16, u16)> = Vec::new();
    if items.is_empty() {
        return runs;
    }
    let mut current = items[0];
    let mut count: u16 = 1;
    for &item in &items[1..] {
        if item == current && count < u16::MAX {
            count += 1;
        } else {
            runs.push((count, current));
            current = item;
            count = 1;
        }
    }
    runs.push((count, current));
    runs
}

/// Run-length encode a `u32` slice into `(run_length, value)` pairs.
fn rle_encode_u32(items: &[u32]) -> Vec<(u16, u32)> {
    let mut runs: Vec<(u16, u32)> = Vec::new();
    if items.is_empty() {
        return runs;
    }
    let mut current = items[0];
    let mut count: u16 = 1;
    for &item in &items[1..] {
        if item == current && count < u16::MAX {
            count += 1;
        } else {
            runs.push((count, current));
            current = item;
            count = 1;
        }
    }
    runs.push((count, current));
    runs
}

// ---------------------------------------------------------------------------
// Kani proofs
// ---------------------------------------------------------------------------

/// Bounded model-checking harnesses for the codec primitives.
///
/// Run with `just kani`. Varint and zigzag harnesses cover the full input
/// domain: a `u64` needs at most `ceil(64 / 7) = 10` LEB128 groups, so
/// `unwind(11)` is a complete bound, not an approximation. Column codec
/// harnesses bound the item count (stated per harness); their per-item logic
/// has no cross-item state beyond the running delta / packing position, so
/// small counts exercise every branch.
#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Longest valid varint: 10 groups for a `u64`.
    const MAX_VARINT: usize = 10;

    /// `decode_varint(encode_varint(v)) == (v, len)` for every `u64`, and the
    /// encoding never exceeds 10 bytes.
    #[kani::proof]
    #[kani::unwind(11)]
    fn varint_round_trips() {
        let v: u64 = kani::any();
        let mut buf = Vec::new();
        encode_varint(v, &mut buf);
        assert!(buf.len() <= MAX_VARINT);
        assert!(matches!(decode_varint(&buf), Ok((d, n)) if d == v && n == buf.len()));
    }

    /// For any input of up to 11 bytes, `decode_varint` does not panic, and
    /// accepts only the canonical encoding: when it returns `(v, n)`, the
    /// first `n` bytes are exactly `encode_varint(v)`. This makes decode
    /// injective, so corrupted bytes cannot silently alias a valid value.
    #[kani::proof]
    #[kani::unwind(12)]
    fn varint_decode_accepts_only_canonical() {
        let data: [u8; MAX_VARINT + 1] = kani::any();
        let len: usize = kani::any_where(|&l| l <= data.len());
        if let Ok((v, n)) = decode_varint(&data[..len]) {
            assert!(n <= len);
            let mut canonical = Vec::new();
            encode_varint(v, &mut canonical);
            assert!(canonical.len() == n);
            assert!(canonical[..] == data[..n]);
        }
    }

    /// Zigzag is a bijection between `i64` and `u64`.
    #[kani::proof]
    fn zigzag_is_bijective() {
        let n: i64 = kani::any();
        assert!(zigzag_decode(zigzag_encode(n)) == n);
        let u: u64 = kani::any();
        assert!(zigzag_encode(zigzag_decode(u)) == u);
    }

    /// Timestamp columns round-trip for any two `i64` values, including
    /// pairs whose difference overflows `i64`. Bound: each later item is
    /// encoded from its predecessor alone, so one delta covers the per-item
    /// logic and longer columns follow by induction on the item index.
    #[kani::proof]
    #[kani::unwind(12)]
    fn timestamp_codec_round_trips() {
        let items: [i64; 2] = kani::any();
        let mut buf = Vec::new();
        assert!(TimestampCodec::encode(&items, &mut buf).is_ok());
        let decoded = TimestampCodec::decode(&buf, items.len());
        // Element-wise: slice equality lowers to a byte memcmp loop.
        assert!(matches!(decoded, Ok((d, n))
            if d.len() == 2 && d[0] == items[0] && d[1] == items[1] && n == buf.len()));
    }

    /// Decoding arbitrary bytes as a timestamp column never panics.
    /// Bound: header plus two maximal varints, up to 3 items.
    #[kani::proof]
    #[kani::unwind(12)]
    fn timestamp_codec_decode_never_panics() {
        let data: [u8; 8 + 2 * MAX_VARINT] = kani::any();
        let len: usize = kani::any_where(|&l| l <= data.len());
        let count: usize = kani::any_where(|&c| c <= 3);
        if let Ok((d, n)) = TimestampCodec::decode(&data[..len], count) {
            assert!(d.len() == count);
            assert!(n <= len);
        }
    }

    // Event type codec: no Kani harness. Fixed-length round trips exceeded
    // 12 GB and the decode harness exceeded 600 s. The domain is small, so
    // the unit test `event_type_codec_exhaustive_round_trip` enumerates it,
    // and `event_type_codec_decode_arbitrary_bytes` samples decode inputs.

    /// Assert that `runs` is an exact run-length encoding of `items`: every
    /// run is non-empty, runs tile `items` in order, and every item in a run
    /// equals the run's value. Loops only over `items` and `runs`, never over
    /// a run length, so the unwind bound stays at the item count.
    fn assert_runs_encode<L: Copy + Into<usize>, T: Copy + PartialEq>(
        runs: &[(L, T)],
        items: &[T],
    ) {
        let mut start = 0;
        for &(run_len, value) in runs {
            let run_len: usize = run_len.into();
            assert!(run_len > 0);
            assert!(start + run_len <= items.len());
            for (k, item) in items.iter().enumerate() {
                if k >= start && k < start + run_len {
                    assert!(*item == value);
                }
            }
            start += run_len;
        }
        assert!(start == items.len());
    }

    // RLE bound: exactly 4 items. The encoder has no state beyond the current
    // run, so 4 items reach every branch except `count == MAX` run splitting,
    // which the unit test `rle_splits_runs_at_counter_max` covers. Symbolic slice lengths made CBMC exceed
    // 16 GB, so the length is fixed.

    /// `rle_encode_u8` is lossless for 4 items.
    #[kani::proof]
    #[kani::unwind(6)]
    fn rle_encode_u8_is_lossless() {
        let items: [u8; 4] = kani::any();
        assert_runs_encode(&rle_encode_u8(&items), &items);
    }

    /// `rle_encode_u16` is lossless for 4 items.
    #[kani::proof]
    #[kani::unwind(6)]
    fn rle_encode_u16_is_lossless() {
        let items: [u16; 4] = kani::any();
        assert_runs_encode(&rle_encode_u16(&items), &items);
    }

    /// `rle_encode_u32` is lossless for 4 items.
    #[kani::proof]
    #[kani::unwind(6)]
    fn rle_encode_u32_is_lossless() {
        let items: [u32; 4] = kani::any();
        assert_runs_encode(&rle_encode_u32(&items), &items);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventType;

    // === Varint helpers ====================================================

    #[test]
    fn varint_roundtrip_small() {
        for n in [0u64, 1, 127, 128, 255, 300, 16383, 16384, u32::MAX as u64] {
            let mut buf = Vec::new();
            encode_varint(n, &mut buf);
            let (decoded, consumed) = decode_varint(&buf).expect("decode");
            assert_eq!(decoded, n, "roundtrip failed for {n}");
            assert_eq!(consumed, buf.len(), "consumed all bytes for {n}");
        }
    }

    #[test]
    fn varint_decode_truncated() {
        // Varint with continuation bit set but no more bytes
        let truncated = &[0x80u8];
        assert!(matches!(
            decode_varint(truncated),
            Err(CacheError::UnexpectedEof)
        ));
    }

    #[test]
    fn varint_rejects_tenth_byte_overflow() {
        // Nine 0xFF groups fill bits 0..63; the 10th byte may only set bit 63.
        let mut max = vec![0xFFu8; 9];
        max.push(0x01);
        assert!(matches!(decode_varint(&max), Ok((u64::MAX, 10))));

        let mut overflow = vec![0xFFu8; 9];
        overflow.push(0x02);
        assert!(matches!(
            decode_varint(&overflow),
            Err(CacheError::DataCorrupted(_))
        ));
    }

    #[test]
    fn varint_rejects_overlong_encoding() {
        // 0 encoded in two bytes instead of one.
        assert!(matches!(
            decode_varint(&[0x80, 0x00]),
            Err(CacheError::DataCorrupted(_))
        ));
        assert!(matches!(decode_varint(&[0x00]), Ok((0, 1))));
    }

    #[test]
    fn timestamp_codec_handles_overflowing_deltas() {
        let items = [i64::MIN, i64::MAX, i64::MIN, 0];
        let mut buf = Vec::new();
        TimestampCodec::encode(&items, &mut buf).expect("encode");
        let (decoded, consumed) = TimestampCodec::decode(&buf, items.len()).expect("decode");
        assert_eq!(decoded, items);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn timestamp_codec_decode_does_not_panic_on_overflow() {
        // First value i64::MAX, then a delta of +1 (zigzag 2).
        let mut data = i64::MAX.to_le_bytes().to_vec();
        data.push(0x02);
        let (decoded, _) = TimestampCodec::decode(&data, 2).expect("decode");
        assert_eq!(decoded, vec![i64::MAX, i64::MIN]);
    }

    const ALL_EVENT_TYPES: [EventType; 11] = [
        EventType::Create,
        EventType::Update,
        EventType::Move,
        EventType::Assign,
        EventType::Comment,
        EventType::Link,
        EventType::Unlink,
        EventType::Delete,
        EventType::Compact,
        EventType::Snapshot,
        EventType::Redact,
    ];

    /// Every sequence of 0 to 4 event types (16,105 sequences) round-trips.
    /// Four items reach every branch: full bytes, a padded last byte, and
    /// RLE runs over repeated packed bytes.
    #[test]
    fn event_type_codec_exhaustive_round_trip() {
        let mut sequences: Vec<Vec<EventType>> = vec![vec![]];
        let mut frontier: Vec<Vec<EventType>> = vec![vec![]];
        for _ in 0..4 {
            frontier = frontier
                .iter()
                .flat_map(|seq| {
                    ALL_EVENT_TYPES.iter().map(move |&et| {
                        let mut next = seq.clone();
                        next.push(et);
                        next
                    })
                })
                .collect();
            sequences.extend(frontier.iter().cloned());
        }
        assert_eq!(sequences.len(), 1 + 11 + 121 + 1331 + 14641);
        for items in sequences {
            let mut buf = Vec::new();
            EventTypeCodec::encode(&items, &mut buf).expect("encode");
            let (decoded, consumed) = EventTypeCodec::decode(&buf, items.len()).expect("decode");
            assert_eq!(decoded, items);
            assert_eq!(consumed, buf.len());
        }
    }

    proptest::proptest! {
        /// Decoding arbitrary bytes as an event type column never panics,
        /// and a success yields exactly `count` items.
        #[test]
        fn event_type_codec_decode_arbitrary_bytes(
            data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..24),
            count in 0usize..8,
        ) {
            if let Ok((decoded, consumed)) = EventTypeCodec::decode(&data, count) {
                proptest::prop_assert_eq!(decoded.len(), count);
                proptest::prop_assert!(consumed <= data.len());
            }
        }
    }

    #[test]
    fn rle_splits_runs_at_counter_max() {
        let items = vec![7u8; 300];
        assert_eq!(rle_encode_u8(&items), vec![(255, 7), (45, 7)]);
        let items = vec![9u16; 70_000];
        assert_eq!(rle_encode_u16(&items), vec![(65_535, 9), (4_465, 9)]);
        let items = vec![3u32; 70_000];
        assert_eq!(rle_encode_u32(&items), vec![(65_535, 3), (4_465, 3)]);
    }

    #[test]
    fn zigzag_roundtrip() {
        for n in [0i64, 1, -1, i64::MIN, i64::MAX, -1000, 1000] {
            assert_eq!(zigzag_decode(zigzag_encode(n)), n, "zigzag failed for {n}");
        }
    }

    // === TimestampCodec ====================================================

    #[test]
    fn timestamp_empty() {
        let mut buf = Vec::new();
        TimestampCodec::encode(&[], &mut buf).unwrap();
        assert!(buf.is_empty());
        let (decoded, consumed) = TimestampCodec::decode(&[], 0).unwrap();
        assert!(decoded.is_empty());
        assert_eq!(consumed, 0);
    }

    #[test]
    fn timestamp_single() {
        let ts = [1_708_012_200_000_000i64];
        let mut buf = Vec::new();
        TimestampCodec::encode(&ts, &mut buf).unwrap();
        let (decoded, _) = TimestampCodec::decode(&buf, 1).unwrap();
        assert_eq!(decoded, ts);
    }

    #[test]
    fn timestamp_roundtrip_ascending() {
        let timestamps: Vec<i64> = (0..100).map(|i| 1_700_000_000_000i64 + i * 1000).collect();
        let mut buf = Vec::new();
        TimestampCodec::encode(&timestamps, &mut buf).unwrap();
        let (decoded, consumed) = TimestampCodec::decode(&buf, timestamps.len()).unwrap();
        assert_eq!(decoded, timestamps);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn timestamp_roundtrip_with_negative_delta() {
        // Out-of-order timestamps (can happen in multi-writer scenarios)
        let timestamps: Vec<i64> = vec![
            1_700_000_000_000,
            1_700_000_001_000,
            1_700_000_000_500, // slight regression
            1_700_000_002_000,
        ];
        let mut buf = Vec::new();
        TimestampCodec::encode(&timestamps, &mut buf).unwrap();
        let (decoded, _) = TimestampCodec::decode(&buf, timestamps.len()).unwrap();
        assert_eq!(decoded, timestamps);
    }

    #[test]
    fn timestamp_delta_encodes_compactly() {
        // Ascending timestamps should encode smaller than 8 bytes each after first
        let base: i64 = 1_700_000_000_000;
        let timestamps: Vec<i64> = (0..10).map(|i| base + i * 1000).collect();
        let mut buf = Vec::new();
        TimestampCodec::encode(&timestamps, &mut buf).unwrap();
        // 8 bytes for first + 9 delta varints (delta=1000 encodes in 2 bytes)
        assert!(buf.len() < 8 + 9 * 4, "expected compact encoding");
    }

    // === InternedStringCodec ==============================================

    #[test]
    fn interned_string_empty() {
        let mut buf = Vec::new();
        InternedStringCodec::encode(&[], &mut buf).unwrap();
        let (decoded, _) = InternedStringCodec::decode(&buf, 0).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn interned_string_single() {
        let items = vec!["claude-abc".to_string()];
        let mut buf = Vec::new();
        InternedStringCodec::encode(&items, &mut buf).unwrap();
        let (decoded, _) = InternedStringCodec::decode(&buf, 1).unwrap();
        assert_eq!(decoded, items);
    }

    #[test]
    fn interned_string_roundtrip_repeated() {
        let items: Vec<String> = [
            "claude-abc",
            "gemini-xyz",
            "claude-abc",
            "claude-abc",
            "gemini-xyz",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut buf = Vec::new();
        InternedStringCodec::encode(&items, &mut buf).unwrap();
        let (decoded, consumed) = InternedStringCodec::decode(&buf, items.len()).unwrap();
        assert_eq!(decoded, items);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn interned_string_compresses_repeated_values() {
        // 1000 entries: 500 "agent-one" followed by 500 "agent-two" → 2 RLE runs
        let items: Vec<String> = (0..1000)
            .map(|i| {
                if i < 500 {
                    "agent-one".to_string()
                } else {
                    "agent-two".to_string()
                }
            })
            .collect();
        let mut buf = Vec::new();
        InternedStringCodec::encode(&items, &mut buf).unwrap();
        // 2 strings (9 bytes each ≈ 22 bytes) + table header (4) + 2 runs (8 bytes) + run header (4)
        // ≈ 38 bytes total vs 9000 bytes raw
        assert!(
            buf.len() < 60,
            "should compress well: got {} bytes",
            buf.len()
        );
        let (decoded, _) = InternedStringCodec::decode(&buf, 1000).unwrap();
        assert_eq!(decoded, items);
    }

    // === EventTypeCodec ===================================================

    #[test]
    fn event_type_empty() {
        let mut buf = Vec::new();
        EventTypeCodec::encode(&[], &mut buf).unwrap();
        let (decoded, _) = EventTypeCodec::decode(&buf, 0).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn event_type_roundtrip_all_types() {
        let items: Vec<EventType> = EventType::ALL.to_vec();
        let mut buf = Vec::new();
        EventTypeCodec::encode(&items, &mut buf).unwrap();
        let (decoded, consumed) = EventTypeCodec::decode(&buf, items.len()).unwrap();
        assert_eq!(decoded, items);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn event_type_odd_count() {
        // Odd count should not confuse padding nibble
        let items = vec![EventType::Create, EventType::Update, EventType::Move];
        let mut buf = Vec::new();
        EventTypeCodec::encode(&items, &mut buf).unwrap();
        let (decoded, _) = EventTypeCodec::decode(&buf, 3).unwrap();
        assert_eq!(decoded, items);
    }

    #[test]
    fn event_type_single() {
        let items = vec![EventType::Comment];
        let mut buf = Vec::new();
        EventTypeCodec::encode(&items, &mut buf).unwrap();
        let (decoded, _) = EventTypeCodec::decode(&buf, 1).unwrap();
        assert_eq!(decoded, items);
    }

    #[test]
    fn event_type_compresses_homogeneous_stream() {
        // 1000 create events should RLE down to a handful of bytes
        let items: Vec<EventType> = vec![EventType::Create; 1000];
        let mut buf = Vec::new();
        EventTypeCodec::encode(&items, &mut buf).unwrap();
        // 500 packed bytes RLE into ~5 runs, each 2 bytes = ~10 bytes + headers
        assert!(
            buf.len() < 50,
            "expected compact encoding: {} bytes",
            buf.len()
        );
        let (decoded, _) = EventTypeCodec::decode(&buf, 1000).unwrap();
        assert_eq!(decoded, items);
    }

    // === ItemIdCodec ======================================================

    #[test]
    fn item_id_empty() {
        let mut buf = Vec::new();
        ItemIdCodec::encode(&[], &mut buf).unwrap();
        let (decoded, _) = ItemIdCodec::decode(&buf, 0).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn item_id_roundtrip() {
        let items: Vec<String> = vec![
            "bn-a7x".to_string(),
            "bn-b8y".to_string(),
            "bn-a7x".to_string(),
            "bn-c9z".to_string(),
            "bn-a7x".to_string(),
        ];
        let mut buf = Vec::new();
        ItemIdCodec::encode(&items, &mut buf).unwrap();
        let (decoded, consumed) = ItemIdCodec::decode(&buf, items.len()).unwrap();
        assert_eq!(decoded, items);
        assert_eq!(consumed, buf.len());
    }

    // === RawBytesCodec ====================================================

    #[test]
    fn raw_bytes_empty() {
        let mut buf = Vec::new();
        RawBytesCodec::encode(&[], &mut buf).unwrap();
        let (decoded, _) = RawBytesCodec::decode(&buf, 0).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn raw_bytes_roundtrip() {
        let items: Vec<String> = vec![
            "itc:AQ".to_string(),
            "itc:AQ.1".to_string(),
            "itc:Bg".to_string(),
        ];
        let mut buf = Vec::new();
        RawBytesCodec::encode(&items, &mut buf).unwrap();
        let (decoded, consumed) = RawBytesCodec::decode(&buf, items.len()).unwrap();
        assert_eq!(decoded, items);
        assert_eq!(consumed, buf.len());
    }

    // === ValueCodec =======================================================

    #[test]
    fn value_codec_empty() {
        let mut buf = Vec::new();
        ValueCodec::encode(&[], &mut buf).unwrap();
        let (decoded, _) = ValueCodec::decode(&buf, 0).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn value_codec_roundtrip() {
        let items: Vec<String> = vec![
            r#"{"title":"Fix auth retry","kind":"task"}"#.to_string(),
            r#"{"field":"title","value":"New title"}"#.to_string(),
            r#"{"state":"doing"}"#.to_string(),
        ];
        let mut buf = Vec::new();
        ValueCodec::encode(&items, &mut buf).unwrap();
        let (decoded, consumed) = ValueCodec::decode(&buf, items.len()).unwrap();
        assert_eq!(decoded, items);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn value_codec_large_payload() {
        // Simulate a snapshot event with a large JSON payload
        let big = "x".repeat(100_000);
        let items = vec![big.clone()];
        let mut buf = Vec::new();
        ValueCodec::encode(&items, &mut buf).unwrap();
        let (decoded, _) = ValueCodec::decode(&buf, 1).unwrap();
        assert_eq!(decoded[0], big);
    }

    // === RLE helpers ======================================================

    #[test]
    fn rle_u8_empty() {
        let runs = rle_encode_u8(&[]);
        assert!(runs.is_empty());
    }

    #[test]
    fn rle_u8_single_run() {
        let runs = rle_encode_u8(&[42, 42, 42]);
        assert_eq!(runs, vec![(3, 42)]);
    }

    #[test]
    fn rle_u8_mixed_runs() {
        let runs = rle_encode_u8(&[1, 1, 2, 3, 3, 3]);
        assert_eq!(runs, vec![(2, 1), (1, 2), (3, 3)]);
    }

    #[test]
    fn rle_u16_roundtrip() {
        let items = vec![0u16, 0, 1, 1, 1, 2, 2];
        let runs = rle_encode_u16(&items);
        // Decode manually
        let mut decoded = Vec::new();
        for (count, val) in &runs {
            for _ in 0..*count {
                decoded.push(*val);
            }
        }
        assert_eq!(decoded, items);
    }
}
