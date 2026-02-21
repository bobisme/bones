use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bones_core::clock::itc::Stamp;
use bones_core::clock::text::{stamp_from_text, stamp_to_text};
use bones_core::event::Event;

const AGENT_DEPTH_BITS: usize = 32;

pub(crate) fn assign_next_itc(project_root: &Path, event: &mut Event) -> Result<()> {
    event.itc = next_itc(project_root, &event.agent)?;
    Ok(())
}

pub(crate) fn next_itc(project_root: &Path, agent: &str) -> Result<String> {
    let state_path = itc_state_path(project_root, agent);
    if let Some(parent) = state_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut stamp = load_stamp(&state_path).unwrap_or_else(|| seed_for_agent(agent));
    stamp.event();

    let encoded = stamp_to_text(&stamp);
    let tmp = state_path.with_extension("tmp");
    fs::write(&tmp, encoded.as_bytes())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, &state_path)
        .with_context(|| format!("failed to persist {}", state_path.display()))?;

    Ok(encoded)
}

fn load_stamp(path: &Path) -> Option<Stamp> {
    let raw = fs::read_to_string(path).ok()?;
    stamp_from_text(raw.trim())
}

fn seed_for_agent(agent: &str) -> Stamp {
    let digest = blake3::hash(agent.as_bytes());
    let bytes = digest.as_bytes();

    let mut stamp = Stamp::seed();
    for idx in 0..AGENT_DEPTH_BITS {
        let byte = bytes[idx / 8];
        let bit = (byte >> (7 - (idx % 8))) & 1;
        let (left, right) = stamp.fork();
        stamp = if bit == 0 { left } else { right };
    }
    stamp
}

fn itc_state_path(project_root: &Path, agent: &str) -> PathBuf {
    project_root
        .join(".bones")
        .join("itc")
        .join("agents")
        .join(format!("{}.itc", encode_agent_id(agent)))
}

fn encode_agent_id(agent_id: &str) -> String {
    let mut encoded = String::with_capacity(agent_id.len());

    for byte in agent_id.bytes() {
        let is_safe = byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.';
        if is_safe {
            encoded.push(char::from(byte));
        } else {
            push_percent_encoded_byte(&mut encoded, byte);
        }
    }

    encoded
}

fn push_percent_encoded_byte(buffer: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    buffer.push('%');
    buffer.push(char::from(HEX[(byte >> 4) as usize]));
    buffer.push(char::from(HEX[(byte & 0x0F) as usize]));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_itc_persists_and_increments() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(temp.path().join(".bones")).expect("bones dir");

        let first = next_itc(temp.path(), "alice").expect("first stamp");
        let second = next_itc(temp.path(), "alice").expect("second stamp");

        assert_ne!(first, second);
        let first_stamp = stamp_from_text(&first).expect("decode first");
        let second_stamp = stamp_from_text(&second).expect("decode second");
        assert!(first_stamp.leq(&second_stamp));
        assert!(!second_stamp.leq(&first_stamp));
    }
}
