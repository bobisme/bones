//! Text encoding/decoding helpers for BLAKE3 hashes.

pub const BLAKE3_PREFIX: &str = "blake3:";

const BASE64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encode a BLAKE3 digest as `blake3:<base64url-no-pad>`.
#[must_use]
pub fn encode_blake3_hash(hash: &blake3::Hash) -> String {
    format!(
        "{BLAKE3_PREFIX}{}",
        encode_base64_url_no_pad(hash.as_bytes())
    )
}

/// Decode `blake3:<...>` text into 32 raw bytes.
///
/// Accepts:
/// - Legacy hex payload (`64` hex chars)
/// - Base64url no-pad payload (`43` chars for 32-byte digests)
#[must_use]
pub fn decode_blake3_hash(raw: &str) -> Option<[u8; 32]> {
    let payload = raw.strip_prefix(BLAKE3_PREFIX)?;

    if payload.len() == 64 && payload.chars().all(|c| c.is_ascii_hexdigit()) {
        let bytes = decode_hex(payload)?;
        return bytes.try_into().ok();
    }

    let bytes = decode_base64_url_no_pad(payload)?;
    bytes.try_into().ok()
}

#[must_use]
pub fn is_valid_blake3_hash(raw: &str) -> bool {
    decode_blake3_hash(raw).is_some()
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

fn decode_hex(raw: &str) -> Option<Vec<u8>> {
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(raw.len() / 2);
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let hi = decode_hex_nibble(*bytes.get(i)?)?;
        let lo = decode_hex_nibble(*bytes.get(i + 1)?)?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn decode_hex_nibble(raw: u8) -> Option<u8> {
    match raw {
        b'0'..=b'9' => Some(raw - b'0'),
        b'a'..=b'f' => Some(raw - b'a' + 10),
        b'A'..=b'F' => Some(raw - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip_for_blake3_digest() {
        let digest = blake3::hash(b"hello-world");
        let encoded = encode_blake3_hash(&digest);
        let decoded = decode_blake3_hash(&encoded).expect("decode");
        assert_eq!(decoded, *digest.as_bytes());
    }

    #[test]
    fn accepts_legacy_hex_payload() {
        let digest = blake3::hash(b"legacy");
        let legacy = format!("blake3:{}", digest.to_hex());
        let decoded = decode_blake3_hash(&legacy).expect("decode");
        assert_eq!(decoded, *digest.as_bytes());
    }

    #[test]
    fn rejects_invalid_payloads() {
        assert!(decode_blake3_hash("blake3:").is_none());
        assert!(decode_blake3_hash("blake3:abc").is_none());
        assert!(decode_blake3_hash("blake3:xyz!").is_none());
        assert!(decode_blake3_hash("sha256:abcd").is_none());
    }
}
