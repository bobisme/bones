//! Compact binary serialization for Interval Tree Clock stamps.
//!
//! # Wire format (v1)
//!
//! ```text
//! version:            u8        (currently 1)
//! id_bits_len:        varint    (number of meaningful bits)
//! id_bits:            bytes     (packed MSB-first)
//! event_bits_len:     varint    (number of meaningful bits)
//! event_bits:         bytes     (packed MSB-first)
//! event_values_len:   varint    (number of bytes)
//! event_values:       bytes     (varint-encoded node values)
//! ```
//!
//! `Id` encoding (preorder):
//! - `0` bit = leaf, followed by 1 bit for value (`0` => `Id::Zero`, `1` => `Id::One`)
//! - `1` bit = branch, followed by left subtree then right subtree
//!
//! `Event` encoding (preorder):
//! - node kind bit: `0` leaf / `1` branch
//! - node value/base: unsigned varint
//! - branch then continues with left subtree, then right subtree
//!
//! This split representation keeps branch/leaf structure bit-packed while preserving
//! compact integer encoding for counters.

use super::itc::{Event, Id, Stamp};
use std::error::Error;
use std::fmt;

const FORMAT_VERSION: u8 = 1;
const VARINT_CONTINUATION_BIT: u8 = 0x80;
const VARINT_PAYLOAD_MASK: u8 = 0x7f;

/// Errors returned by compact ITC serialization/deserialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The input buffer was empty.
    EmptyInput,
    /// The input starts with an unknown format version.
    UnsupportedVersion(u8),
    /// More bytes or bits were required to complete decoding.
    UnexpectedEof,
    /// A varint value exceeded target integer capacity.
    VarintOverflow,
    /// A declared bit/byte length is internally inconsistent.
    InvalidLength,
    /// Bytes remained after a successful parse.
    TrailingBytes,
    /// Meaningful bits remained after a successful parse.
    TrailingBits,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyInput => write!(f, "input is empty"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported compact ITC version: {version}")
            }
            Self::UnexpectedEof => write!(f, "unexpected end of input"),
            Self::VarintOverflow => write!(f, "varint overflow"),
            Self::InvalidLength => write!(f, "invalid length prefix"),
            Self::TrailingBytes => write!(f, "trailing bytes after decode"),
            Self::TrailingBits => write!(f, "trailing bits after decode"),
        }
    }
}

impl Error for CodecError {}

impl Stamp {
    /// Serialize this stamp using the compact binary ITC format.
    #[must_use]
    pub fn serialize_compact(&self) -> Vec<u8> {
        let mut id_bits = BitWriter::default();
        encode_id(&self.id, &mut id_bits);
        let (id_bytes, id_bit_len) = id_bits.into_parts();

        let mut event_bits = BitWriter::default();
        let mut event_values = Vec::new();
        encode_event(&self.event, &mut event_bits, &mut event_values);
        let (event_bit_bytes, event_bit_len) = event_bits.into_parts();

        let mut out = Vec::with_capacity(
            1 + id_bytes.len() + event_bit_bytes.len() + event_values.len() + 12,
        );
        out.push(FORMAT_VERSION);
        encode_usize_varint(id_bit_len, &mut out);
        out.extend_from_slice(&id_bytes);
        encode_usize_varint(event_bit_len, &mut out);
        out.extend_from_slice(&event_bit_bytes);
        encode_usize_varint(event_values.len(), &mut out);
        out.extend_from_slice(&event_values);
        out
    }

    /// Deserialize a stamp from the compact binary ITC format.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when the input is malformed, truncated,
    /// has an unknown format version, or contains trailing data.
    pub fn deserialize_compact(input: &[u8]) -> Result<Self, CodecError> {
        if input.is_empty() {
            return Err(CodecError::EmptyInput);
        }

        let version = input[0];
        if version != FORMAT_VERSION {
            return Err(CodecError::UnsupportedVersion(version));
        }

        let mut cursor = 1usize;

        let id_bit_len = decode_usize_varint(input, &mut cursor)?;
        let id_byte_len = bytes_for_bits(id_bit_len);
        let id_bytes = take_slice(input, &mut cursor, id_byte_len)?;

        let event_bit_len = decode_usize_varint(input, &mut cursor)?;
        let event_bit_byte_len = bytes_for_bits(event_bit_len);
        let event_bits = take_slice(input, &mut cursor, event_bit_byte_len)?;

        let event_values_len = decode_usize_varint(input, &mut cursor)?;
        let event_values = take_slice(input, &mut cursor, event_values_len)?;

        if cursor != input.len() {
            return Err(CodecError::TrailingBytes);
        }

        let mut id_reader = BitReader::new(id_bytes, id_bit_len)?;
        let id = decode_id(&mut id_reader)?;
        if !id_reader.is_exhausted() {
            return Err(CodecError::TrailingBits);
        }

        let mut event_reader = BitReader::new(event_bits, event_bit_len)?;
        let mut event_cursor = 0usize;
        let event = decode_event(&mut event_reader, event_values, &mut event_cursor)?;
        if !event_reader.is_exhausted() {
            return Err(CodecError::TrailingBits);
        }
        if event_cursor != event_values.len() {
            return Err(CodecError::TrailingBytes);
        }

        Ok(Self::new(id, event).normalize())
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bit_len: usize,
}

impl BitWriter {
    fn push_bit(&mut self, bit: bool) {
        let byte_index = self.bit_len / 8;
        let bit_offset = 7 - (self.bit_len % 8);

        if byte_index == self.bytes.len() {
            self.bytes.push(0);
        }

        if bit {
            self.bytes[byte_index] |= 1u8 << bit_offset;
        }

        self.bit_len += 1;
    }

    fn into_parts(self) -> (Vec<u8>, usize) {
        (self.bytes, self.bit_len)
    }
}

struct BitReader<'a> {
    bytes: &'a [u8],
    bit_len: usize,
    cursor: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8], bit_len: usize) -> Result<Self, CodecError> {
        let total_bits = bytes
            .len()
            .checked_mul(8)
            .ok_or(CodecError::InvalidLength)?;
        if bit_len > total_bits {
            return Err(CodecError::InvalidLength);
        }

        Ok(Self {
            bytes,
            bit_len,
            cursor: 0,
        })
    }

    fn read_bit(&mut self) -> Result<bool, CodecError> {
        if self.cursor >= self.bit_len {
            return Err(CodecError::UnexpectedEof);
        }

        let byte_index = self.cursor / 8;
        let bit_offset = 7 - (self.cursor % 8);
        let bit = ((self.bytes[byte_index] >> bit_offset) & 1u8) == 1u8;
        self.cursor += 1;
        Ok(bit)
    }

    fn is_exhausted(&self) -> bool {
        self.cursor == self.bit_len
    }
}

fn bytes_for_bits(bit_len: usize) -> usize {
    bit_len.div_ceil(8)
}

fn take_slice<'a>(input: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8], CodecError> {
    let end = cursor.checked_add(len).ok_or(CodecError::InvalidLength)?;
    if end > input.len() {
        return Err(CodecError::UnexpectedEof);
    }

    let slice = &input[*cursor..end];
    *cursor = end;
    Ok(slice)
}

fn encode_id(id: &Id, out: &mut BitWriter) {
    match id {
        Id::Zero => {
            out.push_bit(false);
            out.push_bit(false);
        }
        Id::One => {
            out.push_bit(false);
            out.push_bit(true);
        }
        Id::Branch(left, right) => {
            out.push_bit(true);
            encode_id(left, out);
            encode_id(right, out);
        }
    }
}

fn decode_id(bits: &mut BitReader<'_>) -> Result<Id, CodecError> {
    let is_branch = bits.read_bit()?;
    if !is_branch {
        let leaf_is_one = bits.read_bit()?;
        return Ok(if leaf_is_one { Id::one() } else { Id::zero() });
    }

    let left = decode_id(bits)?;
    let right = decode_id(bits)?;
    Ok(Id::branch(left, right))
}

fn encode_event(event: &Event, bit_out: &mut BitWriter, value_out: &mut Vec<u8>) {
    match event {
        Event::Leaf(value) => {
            bit_out.push_bit(false);
            encode_u32_varint(*value, value_out);
        }
        Event::Branch(base, left, right) => {
            bit_out.push_bit(true);
            encode_u32_varint(*base, value_out);
            encode_event(left, bit_out, value_out);
            encode_event(right, bit_out, value_out);
        }
    }
}

fn decode_event(
    bits: &mut BitReader<'_>,
    values: &[u8],
    value_cursor: &mut usize,
) -> Result<Event, CodecError> {
    let is_branch = bits.read_bit()?;
    let value = decode_u32_varint(values, value_cursor)?;

    if !is_branch {
        return Ok(Event::leaf(value));
    }

    let left = decode_event(bits, values, value_cursor)?;
    let right = decode_event(bits, values, value_cursor)?;
    Ok(Event::branch(value, left, right))
}

fn encode_usize_varint(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let low_bits = value & usize::from(VARINT_PAYLOAD_MASK);
        let [mut byte, ..] = low_bits.to_le_bytes();

        value >>= 7;
        if value != 0 {
            byte |= VARINT_CONTINUATION_BIT;
            out.push(byte);
            continue;
        }

        out.push(byte);
        break;
    }
}

fn decode_usize_varint(input: &[u8], cursor: &mut usize) -> Result<usize, CodecError> {
    let mut value = 0usize;
    let mut shift = 0u32;

    loop {
        if *cursor >= input.len() {
            return Err(CodecError::UnexpectedEof);
        }

        let byte = input[*cursor];
        *cursor += 1;

        let payload = usize::from(byte & VARINT_PAYLOAD_MASK);
        let shifted = payload
            .checked_shl(shift)
            .ok_or(CodecError::VarintOverflow)?;
        value = value
            .checked_add(shifted)
            .ok_or(CodecError::VarintOverflow)?;

        if (byte & VARINT_CONTINUATION_BIT) == 0 {
            return Ok(value);
        }

        shift = shift.checked_add(7).ok_or(CodecError::VarintOverflow)?;
        if shift >= usize::BITS {
            return Err(CodecError::VarintOverflow);
        }
    }
}

fn encode_u32_varint(mut value: u32, out: &mut Vec<u8>) {
    loop {
        let low_bits = value & u32::from(VARINT_PAYLOAD_MASK);
        let [mut byte, ..] = low_bits.to_le_bytes();

        value >>= 7;
        if value != 0 {
            byte |= VARINT_CONTINUATION_BIT;
            out.push(byte);
            continue;
        }

        out.push(byte);
        break;
    }
}

fn decode_u32_varint(input: &[u8], cursor: &mut usize) -> Result<u32, CodecError> {
    let mut value = 0u32;
    let mut shift = 0u32;

    loop {
        if *cursor >= input.len() {
            return Err(CodecError::UnexpectedEof);
        }

        let byte = input[*cursor];
        *cursor += 1;

        let payload = u32::from(byte & VARINT_PAYLOAD_MASK);
        let shifted = payload
            .checked_shl(shift)
            .ok_or(CodecError::VarintOverflow)?;
        value = value
            .checked_add(shifted)
            .ok_or(CodecError::VarintOverflow)?;

        if (byte & VARINT_CONTINUATION_BIT) == 0 {
            return Ok(value);
        }

        shift = shift.checked_add(7).ok_or(CodecError::VarintOverflow)?;
        if shift >= u32::BITS {
            return Err(CodecError::VarintOverflow);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn compact_roundtrip_seed_stamp() {
        let stamp = Stamp::seed();
        let bytes = stamp.serialize_compact();
        let decoded = Stamp::deserialize_compact(&bytes);
        assert_eq!(decoded, Ok(stamp));
    }

    #[test]
    fn compact_roundtrip_complex_stamp() {
        let stamp = sample_eight_agent_stamp();
        let bytes = stamp.serialize_compact();
        let decoded = Stamp::deserialize_compact(&bytes);
        assert_eq!(decoded, Ok(stamp));
    }

    #[test]
    fn compact_single_agent_size_stays_small() {
        let stamp = Stamp::seed();
        let bytes = stamp.serialize_compact();
        assert!(
            bytes.len() <= 20,
            "single-agent compact stamp too large: {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn compact_eight_agent_size_stays_under_target() {
        let stamp = sample_eight_agent_stamp();
        let bytes = stamp.serialize_compact();
        assert!(
            bytes.len() <= 50,
            "8-agent compact stamp too large: {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn rejects_unknown_version() {
        let err = Stamp::deserialize_compact(&[99]);
        assert_eq!(err, Err(CodecError::UnsupportedVersion(99)));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = Stamp::seed().serialize_compact();
        bytes.push(0);
        let err = Stamp::deserialize_compact(&bytes);
        assert_eq!(err, Err(CodecError::TrailingBytes));
    }

    proptest! {
        #[test]
        fn random_stamps_roundtrip(stamp in arb_stamp()) {
            let bytes = stamp.serialize_compact();
            let decoded = Stamp::deserialize_compact(&bytes);
            prop_assert_eq!(decoded, Ok(stamp));
        }
    }

    fn sample_eight_agent_stamp() -> Stamp {
        let id = Id::branch(
            Id::branch(
                Id::branch(Id::one(), Id::zero()),
                Id::branch(Id::zero(), Id::one()),
            ),
            Id::branch(
                Id::branch(Id::one(), Id::zero()),
                Id::branch(Id::zero(), Id::one()),
            ),
        );

        let event = Event::branch(
            1,
            Event::branch(
                0,
                Event::branch(0, Event::leaf(3), Event::leaf(1)),
                Event::branch(1, Event::leaf(2), Event::leaf(0)),
            ),
            Event::branch(
                0,
                Event::branch(2, Event::leaf(1), Event::leaf(0)),
                Event::branch(0, Event::leaf(4), Event::leaf(2)),
            ),
        );

        Stamp::new(id, event).normalize()
    }

    fn arb_stamp() -> impl Strategy<Value = Stamp> {
        (arb_id(), arb_event()).prop_map(|(id, event)| Stamp::new(id, event).normalize())
    }

    fn arb_id() -> impl Strategy<Value = Id> {
        let leaf = prop_oneof![Just(Id::zero()), Just(Id::one())];
        leaf.prop_recursive(4, 64, 2, |inner| {
            (inner.clone(), inner).prop_map(|(left, right)| Id::branch(left, right))
        })
    }

    fn arb_event() -> impl Strategy<Value = Event> {
        let leaf = (0u32..=25).prop_map(Event::leaf);
        leaf.prop_recursive(4, 128, 2, |inner| {
            (0u32..=10, inner.clone(), inner)
                .prop_map(|(base, left, right)| Event::branch(base, left, right))
        })
    }
}
