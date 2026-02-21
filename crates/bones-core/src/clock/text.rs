use crate::clock::itc::Stamp;

pub const ITC_TEXT_PREFIX: &str = "itc:v1:";

#[must_use]
pub fn stamp_to_text(stamp: &Stamp) -> String {
    let bytes = stamp.serialize_compact();
    format!("{ITC_TEXT_PREFIX}{}", encode_hex(&bytes))
}

#[must_use]
pub fn stamp_from_text(raw: &str) -> Option<Stamp> {
    let encoded = raw.strip_prefix(ITC_TEXT_PREFIX)?;
    let bytes = decode_hex(encoded)?;
    Stamp::deserialize_compact(&bytes).ok()
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
    fn stamp_text_roundtrip() {
        let mut stamp = Stamp::seed();
        stamp.event();
        stamp.event();

        let encoded = stamp_to_text(&stamp);
        let decoded = stamp_from_text(&encoded).expect("parse encoded stamp");
        assert_eq!(decoded, stamp);
    }

    #[test]
    fn decode_rejects_bad_input() {
        assert!(stamp_from_text("itc:v1:not-hex").is_none());
        assert!(stamp_from_text("itc:v1:abc").is_none());
        assert!(stamp_from_text("itc:AQ").is_none());
    }
}
