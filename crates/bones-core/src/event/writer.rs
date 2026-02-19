//! TSJSON event writer/serializer.
//!
//! Serializes [`Event`] structs to TSJSON (tab-separated) lines. Guarantees:
//!
//! - Canonical JSON payload (keys sorted, compact, no whitespace).
//! - One-line invariant: no literal `\n` in the serialized JSON.
//! - Deterministic: same event always produces the same output bytes.
//! - Event hash is BLAKE3 of fields 1–7 joined by tabs, newline-terminated.
//!
//! # TSJSON Format
//!
//! ```text
//! {wall_ts_us}\t{agent}\t{itc}\t{parents}\t{type}\t{item_id}\t{data_json}\t{event_hash}\n
//! ```

use super::canonical::canonicalize_json;
use super::Event;

/// The shard header line written at the start of new event log files.
pub const SHARD_HEADER: &str = "# bones event log v1";

/// The field-description comment line written after the shard header.
pub const FIELD_COMMENT: &str =
    "# fields: wall_ts_us\tagent\titc\tparents\ttype\titem_id\tdata\tevent_hash";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during event writing.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// The serialized JSON payload contained a literal newline.
    #[error("JSON payload contains literal newline — one-line invariant violated")]
    NewlineInPayload,

    /// Failed to serialize the event data payload to JSON.
    #[error("failed to serialize event data: {0}")]
    SerializeData(#[from] serde_json::Error),
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Return the shard header block (header + field comment) for a new event file.
///
/// Includes the trailing newline on each line.
#[must_use]
pub fn shard_header() -> String {
    format!("{SHARD_HEADER}\n{FIELD_COMMENT}\n")
}

/// Serialize an [`Event`] to a single TSJSON line (without trailing newline).
///
/// The data payload is serialized as canonical JSON (sorted keys, compact).
/// The `event_hash` field on the Event is included as-is.
///
/// # Errors
///
/// Returns [`WriteError::NewlineInPayload`] if the canonical JSON contains
/// a literal newline (should never happen with valid data, but enforced).
///
/// Returns [`WriteError::SerializeData`] if the payload fails to serialize.
pub fn to_tsjson_line(event: &Event) -> Result<String, WriteError> {
    let data_json = canonical_data_json(event)?;

    // Enforce one-line invariant
    if data_json.contains('\n') {
        return Err(WriteError::NewlineInPayload);
    }

    let parents = event.parents_str();

    Ok(format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        event.wall_ts_us,
        event.agent,
        event.itc,
        parents,
        event.event_type,
        event.item_id,
        data_json,
        event.event_hash,
    ))
}

/// Serialize an [`Event`] to a TSJSON line with trailing newline.
///
/// Convenience wrapper around [`to_tsjson_line`] that appends `\n`.
///
/// # Errors
///
/// Same as [`to_tsjson_line`].
pub fn write_line(event: &Event) -> Result<String, WriteError> {
    let mut line = to_tsjson_line(event)?;
    line.push('\n');
    Ok(line)
}

/// Compute the BLAKE3 event hash from fields 1–7 of an Event.
///
/// The hash input is the UTF-8 bytes of:
/// `{wall_ts_us}\t{agent}\t{itc}\t{parents}\t{type}\t{item_id}\t{data_json}\n`
///
/// Returns the hash in `blake3:<hex>` format.
///
/// # Errors
///
/// Returns [`WriteError::SerializeData`] if the payload fails to serialize.
pub fn compute_event_hash(event: &Event) -> Result<String, WriteError> {
    let data_json = canonical_data_json(event)?;
    let parents = event.parents_str();

    let hash_input = format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        event.wall_ts_us,
        event.agent,
        event.itc,
        parents,
        event.event_type,
        event.item_id,
        data_json,
    );

    let hash = blake3::hash(hash_input.as_bytes());
    Ok(format!("blake3:{hash}"))
}

/// Compute the event hash and set it on a mutable Event, then serialize.
///
/// This is the primary write path: it computes the content hash, stores it
/// in `event.event_hash`, and returns the full TSJSON line (with newline).
///
/// # Errors
///
/// Same as [`to_tsjson_line`].
pub fn write_event(event: &mut Event) -> Result<String, WriteError> {
    event.event_hash = compute_event_hash(event)?;
    write_line(event)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Serialize the event data payload to canonical JSON.
fn canonical_data_json(event: &Event) -> Result<String, WriteError> {
    let value = event.data.to_json_value()?;
    Ok(canonicalize_json(&value))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::data::*;
    use crate::event::types::EventType;
    use crate::model::item_id::ItemId;
    use std::collections::BTreeMap;

    fn sample_create_event() -> Event {
        Event {
            wall_ts_us: 1_708_012_200_123_456,
            agent: "claude-abc".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-a3f8"),
            data: EventData::Create(CreateData {
                title: "Fix auth retry".into(),
                kind: crate::model::item::Kind::Task,
                size: Some(crate::model::item::Size::M),
                urgency: crate::model::item::Urgency::Default,
                labels: vec!["backend".into()],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: "blake3:placeholder".into(),
        }
    }

    fn sample_move_event() -> Event {
        Event {
            wall_ts_us: 1_708_012_201_000_000,
            agent: "claude-abc".into(),
            itc: "itc:AQ.1".into(),
            parents: vec!["blake3:a1b2c3d4e5f6".into()],
            event_type: EventType::Move,
            item_id: ItemId::new_unchecked("bn-a3f8"),
            data: EventData::Move(MoveData {
                state: crate::model::item::State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
            event_hash: "blake3:d4e5f6789abc".into(),
        }
    }

    #[test]
    fn shard_header_format() {
        let header = shard_header();
        assert!(header.starts_with("# bones event log v1\n"));
        assert!(header.contains("# fields:"));
        assert!(header.ends_with('\n'));
        // Should have exactly 2 lines
        assert_eq!(header.lines().count(), 2);
    }

    #[test]
    fn to_tsjson_line_create_event() {
        let event = sample_create_event();
        let line = to_tsjson_line(&event).expect("should serialize");

        // Should be tab-separated with 8 fields
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 8, "expected 8 tab-separated fields");

        // Field 1: timestamp
        assert_eq!(fields[0], "1708012200123456");
        // Field 2: agent
        assert_eq!(fields[1], "claude-abc");
        // Field 3: itc
        assert_eq!(fields[2], "itc:AQ");
        // Field 4: parents (empty for root)
        assert_eq!(fields[3], "");
        // Field 5: event type
        assert_eq!(fields[4], "item.create");
        // Field 6: item_id
        assert_eq!(fields[5], "bn-a3f8");
        // Field 7: canonical JSON data
        assert!(fields[6].starts_with('{'));
        assert!(fields[6].ends_with('}'));
        // Field 8: event hash
        assert_eq!(fields[7], "blake3:placeholder");

        // No newline in the output
        assert!(!line.contains('\n'));
    }

    #[test]
    fn to_tsjson_line_with_parents() {
        let event = sample_move_event();
        let line = to_tsjson_line(&event).expect("should serialize");
        let fields: Vec<&str> = line.split('\t').collect();

        // Parents field should have the hash
        assert_eq!(fields[3], "blake3:a1b2c3d4e5f6");
    }

    #[test]
    fn to_tsjson_line_multiple_parents() {
        let mut event = sample_move_event();
        event.parents = vec!["blake3:aaa".into(), "blake3:bbb".into()];
        let line = to_tsjson_line(&event).expect("should serialize");
        let fields: Vec<&str> = line.split('\t').collect();

        assert_eq!(fields[3], "blake3:aaa,blake3:bbb");
    }

    #[test]
    fn write_line_has_trailing_newline() {
        let event = sample_create_event();
        let line = write_line(&event).expect("should serialize");
        assert!(line.ends_with('\n'));
        // Only one newline, at the end
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn canonical_json_keys_sorted() {
        let event = sample_create_event();
        let line = to_tsjson_line(&event).expect("should serialize");
        let fields: Vec<&str> = line.split('\t').collect();
        let json_str = fields[6];

        // Parse back and check key order — canonical means sorted
        // For CreateData, keys should be alphabetically ordered
        let val: serde_json::Value =
            serde_json::from_str(json_str).expect("valid JSON");
        let obj = val.as_object().expect("should be object");
        let keys: Vec<&String> = obj.keys().collect();

        // Verify sorted
        let mut sorted_keys = keys.clone();
        sorted_keys.sort();
        assert_eq!(keys, sorted_keys, "JSON keys should be sorted");
    }

    #[test]
    fn json_payload_no_whitespace() {
        let event = sample_create_event();
        let line = to_tsjson_line(&event).expect("should serialize");
        let fields: Vec<&str> = line.split('\t').collect();
        let json_str = fields[6];

        // Canonical JSON should have no spaces outside of string values
        // Quick check: no " : " or ", " patterns
        assert!(!json_str.contains(" :"));
        assert!(!json_str.contains(": "));
        // It's OK for string VALUES to have spaces (e.g., "Fix auth retry")
    }

    #[test]
    fn compute_event_hash_deterministic() {
        let event = sample_create_event();
        let hash1 = compute_event_hash(&event).expect("hash");
        let hash2 = compute_event_hash(&event).expect("hash");
        assert_eq!(hash1, hash2, "same event should produce same hash");
        assert!(hash1.starts_with("blake3:"), "hash should have blake3: prefix");
    }

    #[test]
    fn compute_event_hash_changes_with_data() {
        let event1 = sample_create_event();
        let mut event2 = sample_create_event();
        event2.wall_ts_us += 1;

        let hash1 = compute_event_hash(&event1).expect("hash");
        let hash2 = compute_event_hash(&event2).expect("hash");
        assert_ne!(hash1, hash2, "different events should have different hashes");
    }

    #[test]
    fn write_event_sets_hash() {
        let mut event = sample_create_event();
        assert_eq!(event.event_hash, "blake3:placeholder");

        let line = write_event(&mut event).expect("write");
        assert_ne!(event.event_hash, "blake3:placeholder");
        assert!(event.event_hash.starts_with("blake3:"));

        // The line should contain the computed hash
        assert!(line.contains(&event.event_hash));
    }

    #[test]
    fn deterministic_output() {
        let event = sample_create_event();
        let line1 = to_tsjson_line(&event).expect("serialize");
        let line2 = to_tsjson_line(&event).expect("serialize");
        assert_eq!(line1, line2, "same event should produce same line");
    }

    #[test]
    fn all_event_types_serialize() {
        use crate::model::item::{Kind, State, Urgency};
        use serde_json::json;

        let base_event = |event_type: EventType, data: EventData| Event {
            wall_ts_us: 1_000_000,
            agent: "agent".into(),
            itc: "itc:X".into(),
            parents: vec![],
            event_type,
            item_id: ItemId::new_unchecked("bn-a7x"),
            data,
            event_hash: "blake3:000".into(),
        };

        let events = vec![
            base_event(
                EventType::Create,
                EventData::Create(CreateData {
                    title: "T".into(),
                    kind: Kind::Task,
                    size: None,
                    urgency: Urgency::Default,
                    labels: vec![],
                    parent: None,
                    causation: None,
                    description: None,
                    extra: BTreeMap::new(),
                }),
            ),
            base_event(
                EventType::Update,
                EventData::Update(UpdateData {
                    field: "title".into(),
                    value: json!("New"),
                    extra: BTreeMap::new(),
                }),
            ),
            base_event(
                EventType::Move,
                EventData::Move(MoveData {
                    state: State::Done,
                    reason: Some("done".into()),
                    extra: BTreeMap::new(),
                }),
            ),
            base_event(
                EventType::Assign,
                EventData::Assign(AssignData {
                    agent: "alice".into(),
                    action: AssignAction::Assign,
                    extra: BTreeMap::new(),
                }),
            ),
            base_event(
                EventType::Comment,
                EventData::Comment(CommentData {
                    body: "Note".into(),
                    extra: BTreeMap::new(),
                }),
            ),
            base_event(
                EventType::Link,
                EventData::Link(LinkData {
                    target: "bn-b8y".into(),
                    link_type: "blocks".into(),
                    extra: BTreeMap::new(),
                }),
            ),
            base_event(
                EventType::Unlink,
                EventData::Unlink(UnlinkData {
                    target: "bn-b8y".into(),
                    link_type: None,
                    extra: BTreeMap::new(),
                }),
            ),
            base_event(
                EventType::Delete,
                EventData::Delete(DeleteData {
                    reason: None,
                    extra: BTreeMap::new(),
                }),
            ),
            base_event(
                EventType::Compact,
                EventData::Compact(CompactData {
                    summary: "TL;DR".into(),
                    extra: BTreeMap::new(),
                }),
            ),
            base_event(
                EventType::Snapshot,
                EventData::Snapshot(SnapshotData {
                    state: json!({"id": "bn-a7x"}),
                    extra: BTreeMap::new(),
                }),
            ),
            base_event(
                EventType::Redact,
                EventData::Redact(RedactData {
                    target_hash: "blake3:xyz".into(),
                    reason: "oops".into(),
                    extra: BTreeMap::new(),
                }),
            ),
        ];

        assert_eq!(events.len(), 11, "should cover all 11 event types");

        for event in &events {
            let result = to_tsjson_line(event);
            assert!(
                result.is_ok(),
                "failed to serialize {}: {:?}",
                event.event_type,
                result.err()
            );
            let line = result.expect("checked above");
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(
                fields.len(),
                8,
                "wrong field count for {}",
                event.event_type
            );
            // Verify no newlines
            assert!(
                !line.contains('\n'),
                "newline in output for {}",
                event.event_type
            );
        }
    }

    #[test]
    fn write_event_roundtrip_hash() {
        // write_event should compute hash, then the line should contain
        // that exact hash
        let mut event = sample_move_event();
        let line = write_event(&mut event).expect("write");

        // Extract hash from the line
        let fields: Vec<&str> = line.trim_end().split('\t').collect();
        let line_hash = fields[7];
        assert_eq!(line_hash, event.event_hash);

        // Recompute hash independently
        let recomputed = compute_event_hash(&event).expect("hash");
        assert_eq!(recomputed, event.event_hash);
    }

    #[test]
    fn empty_extra_fields_not_in_json() {
        // BTreeMap extras should not appear when empty
        let event = sample_create_event();
        let line = to_tsjson_line(&event).expect("serialize");
        let fields: Vec<&str> = line.split('\t').collect();
        let json_str = fields[6];

        // "extra" should not appear in the JSON (flatten with empty map)
        // Note: serde flatten with empty BTreeMap produces no extra keys
        let val: serde_json::Value = serde_json::from_str(json_str).expect("parse");
        // The only keys should be the ones defined in CreateData
        let obj = val.as_object().expect("object");
        for key in obj.keys() {
            assert!(
                ["title", "kind", "size", "urgency", "labels", "parent", "causation", "description"]
                    .contains(&key.as_str()),
                "unexpected key in JSON: {key}"
            );
        }
    }
}
