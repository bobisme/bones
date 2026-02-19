# Binary Columnar Event Cache Format

**Version**: 1  
**Magic bytes**: `BNCH` (0x42 0x4E 0x43 0x48)  
**File location**: `.bones/cache/events.bin`

## Overview

The binary columnar event cache is a compact, read-optimised representation of
the bones TSJSON event log. It stores events in column-major order, exploiting
the statistical properties of event data (high repetition in agent IDs, event
types, and item IDs) for high compression ratios.

The cache is **derived, not authoritative**. The TSJSON `.events` files are the
source of truth. The cache can be rebuilt at any time with `bn rebuild
--incremental`.

---

## File Layout

```
+------------------+
|  File Header     |  Fixed-size, 32 bytes
+------------------+
|  Column Offsets  |  8 bytes × column_count
+------------------+
|  Column 0        |  timestamp column (delta varint)
|  Column 1        |  agent column (interned strings + RLE)
|  Column 2        |  event_type column (4-bit RLE)
|  Column 3        |  item_id column (dictionary encoded)
|  Column 4        |  parents column (interned strings, RLE)
|  Column 5        |  itc column (raw bytes, length-prefixed)
|  Column 6        |  value column (type-specific encoding)
+------------------+
```

---

## File Header (32 bytes)

| Offset | Size | Field           | Description                              |
|--------|------|-----------------|------------------------------------------|
| 0      | 4    | `magic`         | `BNCH` (0x42 0x4E 0x43 0x48)             |
| 4      | 1    | `version`       | Format version — currently `1`           |
| 5      | 1    | `column_count`  | Number of columns present                |
| 6      | 2    | `_reserved`     | Reserved, must be zero                   |
| 8      | 8    | `row_count`     | Number of events (rows) in the file      |
| 16     | 8    | `created_at_us` | Wall-clock µs at cache creation time     |
| 24     | 8    | `data_crc64`    | CRC-64/ECMA over all column data bytes   |

Immediately following the header are `column_count × 8` bytes of column offsets
(one `u64` per column), each giving the byte position of that column's data
relative to the start of the file.

---

## Column Encodings

### Column 0 — Timestamps (`TimestampCodec`)

Wall-clock microseconds since Unix epoch (`i64`).

- **First value**: stored as absolute little-endian `i64` (8 bytes).
- **Subsequent values**: stored as signed zigzag-encoded varints representing
  the delta from the previous value. Positive deltas (forward in time) are
  common and compress extremely well.

**Varint encoding**: standard LEB128 for unsigned values, zigzag for signed
(zigzag(n) = (n << 1) ^ (n >> 63)).

**Layout**:
```
[first_ts: i64 LE] [delta_varint_1] [delta_varint_2] ...
```

### Column 1 — Agent IDs (`InternedStringCodec`)

Agent identifier strings (e.g. `claude-abc`, `gemini-xyz`). Low cardinality,
high repetition.

**Layout**:
```
[string_table_count: u32 LE]
[string_0_len: u16 LE] [string_0_bytes: UTF-8]
[string_1_len: u16 LE] [string_1_bytes: UTF-8]
...
[rle_encoded_indices...]
```

The string table is followed by run-length encoded `u16` indices into the
table. RLE format: `[run_length: u16 LE] [index: u16 LE]` for each run.

### Column 2 — Event Types (`EventTypeCodec`)

11 event types fit in 4 bits (values 0–10). Two event types are packed per
byte using 4-bit nibbles (low nibble first), then the packed bytes are
RLE-compressed.

**Type encoding**:
| Value | Event Type     |
|-------|---------------|
| 0     | item.create   |
| 1     | item.update   |
| 2     | item.move     |
| 3     | item.assign   |
| 4     | item.comment  |
| 5     | item.link     |
| 6     | item.unlink   |
| 7     | item.delete   |
| 8     | item.compact  |
| 9     | item.snapshot |
| 10    | item.redact   |

**Layout**:
```
[packed_byte_count: u32 LE]   ← ceil(row_count / 2)
[packed_nibbles: ...bytes]    ← RLE over packed bytes
```

RLE: `[run_length: u8] [value: u8]` for each run.

### Column 3 — Item IDs (`ItemIdCodec`)

Item identifier strings (e.g. `bn-a7x`, `bn-a3f8`). Higher cardinality than
agent IDs but still benefits from dictionary encoding.

Same format as `InternedStringCodec` (Column 1) with `u32` indices instead of
`u16` (supports up to ~4 billion unique items).

**Layout**:
```
[dict_count: u32 LE]
[item_0_len: u16 LE] [item_0_bytes: UTF-8]
...
[rle_encoded_u32_indices...]
```

### Column 4 — Parent Hashes (`InternedStringCodec`)

Parent event hash strings (e.g. `blake3:a1b2...`). Most events in linear
history share the same single parent, so this compresses well.

Encoded as a list of comma-separated hash strings per row. First, all unique
hash strings are interned into a table, then each row stores the count of
parents followed by RLE indices.

**Layout** (per row):
```
[parent_count: u8]         ← 0 for root events
[index_0: u16 LE] ...      ← one u16 index per parent into string table
```

Preceded by the same string table preamble as Column 1.

### Column 5 — ITC Stamps (`RawBytesCodec`)

Interval Tree Clock stamps are variable-length text strings. Stored as
length-prefixed raw bytes.

**Layout**:
```
[itc_0_len: u16 LE] [itc_0_bytes: UTF-8]
[itc_1_len: u16 LE] [itc_1_bytes: UTF-8]
...
```

### Column 6 — Event Values (`ValueCodec`)

Type-specific event payload encoding. The payload data is JSON-encoded per
event and stored as length-prefixed raw bytes. The value column trades
compression for full fidelity across all 11 payload types.

Future versions may split this into per-type sub-columns for better
compression of structured payloads.

**Layout**:
```
[payload_0_len: u32 LE] [payload_0_bytes: UTF-8 JSON]
[payload_1_len: u32 LE] [payload_1_bytes: UTF-8 JSON]
...
```

---

## Error Handling

- `BNCH` magic mismatch → `CacheError::InvalidMagic`
- Version > supported → `CacheError::UnsupportedVersion`
- CRC mismatch → `CacheError::DataCorrupted`
- Truncated column data → `CacheError::UnexpectedEof`

On any error, the reader falls back to rebuilding from the TSJSON event log.

---

## Versioning

The `version` byte in the header allows forward-compatible format evolution.
Readers that encounter an unknown version return `CacheError::UnsupportedVersion`
and the caller triggers a full rebuild. Backwards-incompatible changes bump the
version.

---

## Why Columnar?

Column-major layout allows the reader to load only the columns needed for a
given query (e.g. only timestamps + event types for a count-by-type query).
This reduces I/O and cache pressure for partial scans, which is the dominant
access pattern in bones.

---

## Compression Ratios (expected)

| Column       | Expected ratio | Reason                               |
|--------------|---------------|--------------------------------------|
| timestamps   | 4–8×           | Delta encoding + varint              |
| agents       | 10–50×         | Very low cardinality, long runs      |
| event_types  | 8–20×          | 11 values, 4-bit packing + RLE       |
| item_ids     | 3–8×           | Many events per item                 |
| parents      | 4–10×          | Linear chains dominant               |
| itc          | 1.5–3×         | Length-prefixed, low redundancy      |
| values       | 1–2×           | JSON, low structural redundancy      |
