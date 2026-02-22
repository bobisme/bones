use crate::clock::itc::Stamp;

pub const ITC_TEXT_PREFIX: &str = "itc:v3:";
const LEGACY_ITC_TEXT_PREFIX: &str = "itc:v1:";
const COMPACT_ITC_VERSION: u8 = 1;
const SPARSE_ITC_WIRE_VERSION: u8 = 1;
const BASE64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

#[must_use]
pub fn stamp_to_text(stamp: &Stamp) -> String {
    let compact = stamp.serialize_compact();
    if let Some(sparse_payload) = compact_to_sparse_payload(&compact) {
        format!(
            "{ITC_TEXT_PREFIX}{}",
            encode_base64_url_no_pad(&sparse_payload)
        )
    } else {
        format!("{LEGACY_ITC_TEXT_PREFIX}{}", encode_hex(&compact))
    }
}

#[must_use]
pub fn stamp_from_text(raw: &str) -> Option<Stamp> {
    if let Some(encoded) = raw.strip_prefix(ITC_TEXT_PREFIX) {
        let sparse_payload = decode_base64_url_no_pad(encoded)?;
        let compact = sparse_payload_to_compact(&sparse_payload)?;
        return Stamp::deserialize_compact(&compact).ok();
    }

    let encoded = raw.strip_prefix(LEGACY_ITC_TEXT_PREFIX)?;
    let compact = decode_hex(encoded)?;
    Stamp::deserialize_compact(&compact).ok()
}

#[derive(Debug, Clone, Copy)]
struct CompactSections<'a> {
    id_bit_len: usize,
    id_bits: &'a [u8],
    event_bit_len: usize,
    event_bits: &'a [u8],
    event_values: &'a [u8],
}

fn compact_to_sparse_payload(compact: &[u8]) -> Option<Vec<u8>> {
    let sections = parse_compact_sections(compact)?;
    let values = decode_u32_varint_list(sections.event_values, sections.event_bit_len)?;

    let mut sparse = Vec::new();
    sparse.push(SPARSE_ITC_WIRE_VERSION);
    encode_varint_usize(sections.id_bit_len, &mut sparse);
    sparse.extend_from_slice(sections.id_bits);
    encode_varint_usize(sections.event_bit_len, &mut sparse);
    sparse.extend_from_slice(sections.event_bits);

    let non_zero_count = values.iter().filter(|&&value| value != 0).count();
    encode_varint_usize(non_zero_count, &mut sparse);

    let mut previous_index = 0usize;
    let mut wrote_any = false;
    for (index, value) in values.into_iter().enumerate() {
        if value == 0 {
            continue;
        }
        let delta = if wrote_any {
            index.checked_sub(previous_index)?
        } else {
            index
        };
        encode_varint_usize(delta, &mut sparse);
        encode_varint_u32(value, &mut sparse);
        previous_index = index;
        wrote_any = true;
    }

    Some(sparse)
}

fn sparse_payload_to_compact(sparse: &[u8]) -> Option<Vec<u8>> {
    let mut cursor = 0usize;
    let version = *sparse.get(cursor)?;
    cursor += 1;
    if version != SPARSE_ITC_WIRE_VERSION {
        return None;
    }

    let id_bit_len = decode_varint_usize(sparse, &mut cursor)?;
    let id_bits_len = bytes_for_bits(id_bit_len)?;
    let id_bits = take_slice(sparse, &mut cursor, id_bits_len)?;

    let event_bit_len = decode_varint_usize(sparse, &mut cursor)?;
    let event_bits_len = bytes_for_bits(event_bit_len)?;
    let event_bits = take_slice(sparse, &mut cursor, event_bits_len)?;

    let non_zero_count = decode_varint_usize(sparse, &mut cursor)?;

    let mut values = vec![0_u32; event_bit_len];
    let mut previous_index = 0usize;
    let mut has_previous = false;

    for _ in 0..non_zero_count {
        let delta = decode_varint_usize(sparse, &mut cursor)?;
        let index = if has_previous {
            previous_index.checked_add(delta)?
        } else {
            delta
        };
        if index >= values.len() {
            return None;
        }
        if values[index] != 0 {
            return None;
        }

        let value = decode_varint_u32(sparse, &mut cursor)?;
        if value == 0 {
            return None;
        }
        values[index] = value;
        previous_index = index;
        has_previous = true;
    }

    if cursor != sparse.len() {
        return None;
    }

    let mut event_values = Vec::new();
    for value in values {
        encode_varint_u32(value, &mut event_values);
    }

    let mut compact = Vec::new();
    compact.push(COMPACT_ITC_VERSION);
    encode_varint_usize(id_bit_len, &mut compact);
    compact.extend_from_slice(id_bits);
    encode_varint_usize(event_bit_len, &mut compact);
    compact.extend_from_slice(event_bits);
    encode_varint_usize(event_values.len(), &mut compact);
    compact.extend_from_slice(&event_values);
    Some(compact)
}

fn parse_compact_sections(raw: &[u8]) -> Option<CompactSections<'_>> {
    let mut cursor = 0usize;
    let version = *raw.get(cursor)?;
    cursor += 1;
    if version != COMPACT_ITC_VERSION {
        return None;
    }

    let id_bit_len = decode_varint_usize(raw, &mut cursor)?;
    let id_bits_len = bytes_for_bits(id_bit_len)?;
    let id_bits = take_slice(raw, &mut cursor, id_bits_len)?;

    let event_bit_len = decode_varint_usize(raw, &mut cursor)?;
    let event_bits_len = bytes_for_bits(event_bit_len)?;
    let event_bits = take_slice(raw, &mut cursor, event_bits_len)?;

    let event_values_len = decode_varint_usize(raw, &mut cursor)?;
    let event_values = take_slice(raw, &mut cursor, event_values_len)?;

    if cursor != raw.len() {
        return None;
    }

    Some(CompactSections {
        id_bit_len,
        id_bits,
        event_bit_len,
        event_bits,
        event_values,
    })
}

fn decode_u32_varint_list(raw: &[u8], expected_len: usize) -> Option<Vec<u32>> {
    let mut cursor = 0usize;
    let mut out = Vec::with_capacity(expected_len);
    while out.len() < expected_len {
        out.push(decode_varint_u32(raw, &mut cursor)?);
    }
    if cursor != raw.len() {
        return None;
    }
    Some(out)
}

fn take_slice<'a>(raw: &'a [u8], cursor: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = cursor.checked_add(len)?;
    let slice = raw.get(*cursor..end)?;
    *cursor = end;
    Some(slice)
}

fn bytes_for_bits(bit_len: usize) -> Option<usize> {
    bit_len.checked_add(7).map(|value| value / 8)
}

fn encode_varint_usize(value: usize, out: &mut Vec<u8>) {
    encode_varint_u64(u64::try_from(value).unwrap_or(u64::MAX), out);
}

fn encode_varint_u32(value: u32, out: &mut Vec<u8>) {
    encode_varint_u64(u64::from(value), out);
}

fn encode_varint_u64(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        let lower = u8::try_from(value & 0x7f).unwrap_or(0);
        out.push(lower | 0x80);
        value >>= 7;
    }
    let final_byte = u8::try_from(value & 0x7f).unwrap_or(0);
    out.push(final_byte);
}

fn decode_varint_usize(raw: &[u8], cursor: &mut usize) -> Option<usize> {
    let value = decode_varint_u64(raw, cursor)?;
    usize::try_from(value).ok()
}

fn decode_varint_u32(raw: &[u8], cursor: &mut usize) -> Option<u32> {
    let value = decode_varint_u64(raw, cursor)?;
    u32::try_from(value).ok()
}

fn decode_varint_u64(raw: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut shift = 0_u32;
    let mut value = 0_u64;

    loop {
        let byte = *raw.get(*cursor)?;
        *cursor += 1;
        let payload = u64::from(byte & 0x7f);
        let shifted = payload.checked_shl(shift)?;
        value = value.checked_add(shifted)?;
        if (byte & 0x80) == 0 {
            return Some(value);
        }
        if shift >= 63 {
            return None;
        }
        shift += 7;
    }
}

fn encode_base64_url_no_pad(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    let mut idx = 0usize;

    while idx + 3 <= bytes.len() {
        let b0 = bytes[idx];
        let b1 = bytes[idx + 1];
        let b2 = bytes[idx + 2];
        out.push(char::from(BASE64_URL[usize::from(b0 >> 2)]));
        out.push(char::from(
            BASE64_URL[usize::from(((b0 & 0b0000_0011) << 4) | (b1 >> 4))],
        ));
        out.push(char::from(
            BASE64_URL[usize::from(((b1 & 0b0000_1111) << 2) | (b2 >> 6))],
        ));
        out.push(char::from(BASE64_URL[usize::from(b2 & 0b0011_1111)]));
        idx += 3;
    }

    let remainder = bytes.len() - idx;
    if remainder == 1 {
        let b0 = bytes[idx];
        out.push(char::from(BASE64_URL[usize::from(b0 >> 2)]));
        out.push(char::from(BASE64_URL[usize::from((b0 & 0b0000_0011) << 4)]));
    } else if remainder == 2 {
        let b0 = bytes[idx];
        let b1 = bytes[idx + 1];
        out.push(char::from(BASE64_URL[usize::from(b0 >> 2)]));
        out.push(char::from(
            BASE64_URL[usize::from(((b0 & 0b0000_0011) << 4) | (b1 >> 4))],
        ));
        out.push(char::from(BASE64_URL[usize::from((b1 & 0b0000_1111) << 2)]));
    }

    out
}

fn decode_base64_url_no_pad(raw: &str) -> Option<Vec<u8>> {
    let input = raw.as_bytes();
    if input.len() % 4 == 1 {
        return None;
    }

    let mut out = Vec::with_capacity(input.len() * 3 / 4 + 2);
    let mut cursor = 0usize;

    while cursor + 4 <= input.len() {
        let a = decode_base64_url_digit(*input.get(cursor)?)?;
        let b = decode_base64_url_digit(*input.get(cursor + 1)?)?;
        let c = decode_base64_url_digit(*input.get(cursor + 2)?)?;
        let d = decode_base64_url_digit(*input.get(cursor + 3)?)?;
        out.push((a << 2) | (b >> 4));
        out.push(((b & 0b0000_1111) << 4) | (c >> 2));
        out.push(((c & 0b0000_0011) << 6) | d);
        cursor += 4;
    }

    let remainder = input.len() - cursor;
    if remainder == 2 {
        let a = decode_base64_url_digit(*input.get(cursor)?)?;
        let b = decode_base64_url_digit(*input.get(cursor + 1)?)?;
        out.push((a << 2) | (b >> 4));
    } else if remainder == 3 {
        let a = decode_base64_url_digit(*input.get(cursor)?)?;
        let b = decode_base64_url_digit(*input.get(cursor + 1)?)?;
        let c = decode_base64_url_digit(*input.get(cursor + 2)?)?;
        out.push((a << 2) | (b >> 4));
        out.push(((b & 0b0000_1111) << 4) | (c >> 2));
    } else if remainder != 0 {
        return None;
    }

    Some(out)
}

fn decode_base64_url_digit(raw: u8) -> Option<u8> {
    match raw {
        b'A'..=b'Z' => Some(raw - b'A'),
        b'a'..=b'z' => Some(raw - b'a' + 26),
        b'0'..=b'9' => Some(raw - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex(raw: &str) -> Option<Vec<u8>> {
    if raw.len() % 2 != 0 {
        return None;
    }

    let mut out = Vec::with_capacity(raw.len() / 2);
    let chars: Vec<char> = raw.chars().collect();
    let mut idx = 0;
    while idx < chars.len() {
        let hi = decode_hex_nibble(chars[idx])?;
        let lo = decode_hex_nibble(chars[idx + 1])?;
        out.push((hi << 4) | lo);
        idx += 2;
    }

    Some(out)
}

fn decode_hex_nibble(c: char) -> Option<u8> {
    match c {
        '0'..='9' => Some((c as u8) - b'0'),
        'a'..='f' => Some((c as u8) - b'a' + 10),
        'A'..='F' => Some((c as u8) - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_text_roundtrip_v3() {
        let mut stamp = Stamp::seed();
        stamp.event();
        stamp.event();

        let encoded = stamp_to_text(&stamp);
        assert!(encoded.starts_with(ITC_TEXT_PREFIX));
        let decoded = stamp_from_text(&encoded).expect("parse encoded stamp");
        assert_eq!(decoded, stamp);
    }

    #[test]
    fn stamp_text_roundtrip_v1_legacy_decode() {
        let mut stamp = Stamp::seed();
        for _ in 0..8 {
            stamp.event();
        }

        let legacy = format!(
            "{LEGACY_ITC_TEXT_PREFIX}{}",
            encode_hex(&stamp.serialize_compact())
        );
        let decoded = stamp_from_text(&legacy).expect("parse legacy stamp");
        assert_eq!(decoded, stamp);
    }

    #[test]
    fn sparse_payload_smaller_than_legacy_hex() {
        let mut stamp = Stamp::seed();
        for _ in 0..1024 {
            stamp.event();
        }

        let compact = stamp.serialize_compact();
        let sparse = compact_to_sparse_payload(&compact).expect("sparse payload");
        let legacy_text = encode_hex(&compact);
        let sparse_text = encode_base64_url_no_pad(&sparse);

        assert!(sparse_text.len() < legacy_text.len());
    }

    #[test]
    fn decode_rejects_malformed_v3_payloads() {
        assert!(stamp_from_text("itc:v3:^").is_none());

        // valid base64url but invalid sparse payload version
        let encoded = encode_base64_url_no_pad(&[99, 0]);
        assert!(stamp_from_text(&format!("itc:v3:{encoded}")).is_none());
    }

    #[test]
    fn sparse_compact_roundtrip_preserves_bytes() {
        let mut stamp = Stamp::seed();
        for _ in 0..64 {
            stamp.event();
        }

        let compact = stamp.serialize_compact();
        let sparse = compact_to_sparse_payload(&compact).expect("to sparse");
        let reconstructed = sparse_payload_to_compact(&sparse).expect("to compact");
        assert_eq!(reconstructed, compact);
    }

    #[test]
    fn sparse_decode_rejects_duplicate_indices() {
        let mut sparse = Vec::new();
        sparse.push(SPARSE_ITC_WIRE_VERSION);
        encode_varint_usize(2, &mut sparse); // id bits len
        sparse.push(0b0100_0000); // Id::One leaf
        encode_varint_usize(1, &mut sparse); // event bits len
        sparse.push(0); // Event::Leaf kind bit
        encode_varint_usize(2, &mut sparse); // two non-zero values
        encode_varint_usize(0, &mut sparse); // first index delta
        encode_varint_u32(1, &mut sparse);
        encode_varint_usize(0, &mut sparse); // duplicate index delta
        encode_varint_u32(2, &mut sparse);

        assert!(sparse_payload_to_compact(&sparse).is_none());
    }

    #[test]
    fn sparse_decode_rejects_out_of_range_index() {
        let mut sparse = Vec::new();
        sparse.push(SPARSE_ITC_WIRE_VERSION);
        encode_varint_usize(2, &mut sparse); // id bits len
        sparse.push(0b0100_0000); // Id::One leaf
        encode_varint_usize(1, &mut sparse); // event bits len
        sparse.push(0); // Event::Leaf kind bit
        encode_varint_usize(1, &mut sparse); // one non-zero value
        encode_varint_usize(1, &mut sparse); // out-of-range index
        encode_varint_u32(3, &mut sparse);

        assert!(sparse_payload_to_compact(&sparse).is_none());
    }

    #[test]
    fn decode_rejects_bad_input() {
        assert!(stamp_from_text("itc:v1:not-hex").is_none());
        assert!(stamp_from_text("itc:v1:abc").is_none());
        assert!(stamp_from_text("itc:v3:abcde").is_none());
        assert!(stamp_from_text("itc:AQ").is_none());
    }
}
