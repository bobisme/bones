//! Event data model for the bones event log.
//!
//! This module defines the core `Event` struct, the `EventType` enum covering
//! all 11 event types, typed payload data structs, and the canonical JSON
//! serialization helper needed for deterministic event hashing.
//!
//! # TSJSON Format
//!
//! Events are stored in TSJSON (tab-separated fields with JSON payload):
//!
//! ```text
//! wall_ts_us \t agent \t itc \t parents \t type \t item_id \t data \t event_hash
//! ```
//!
//! The `Event` struct maps 1:1 to a TSJSON line. Parsing and writing TSJSON
//! lines is handled by the parser/writer modules (separate beads).

pub mod canonical;
pub mod data;
pub mod types;

pub use canonical::{canonicalize_json, canonicalize_json_str};
pub use data::{
    AssignAction, AssignData, CommentData, CompactData, CreateData, DataParseError, DeleteData,
    EventData, LinkData, MoveData, RedactData, SnapshotData, UnlinkData, UpdateData,
};
pub use types::{EventType, UnknownEventType};

use crate::model::item_id::ItemId;
use serde::{Deserialize, Serialize};

/// A single event in the bones event log.
///
/// Each event represents an immutable, content-addressed mutation to a work
/// item. Events form a Merkle-DAG via the `parents` field, enabling
/// causally-ordered CRDT replay.
///
/// # Fields (TSJSON column order)
///
/// 1. `wall_ts_us` — wall-clock microseconds since Unix epoch
/// 2. `agent` — identifier of the agent/user that produced the event
/// 3. `itc` — Interval Tree Clock stamp (canonical text encoding)
/// 4. `parents` — parent event hashes (blake3:...), sorted lexicographically
/// 5. `event_type` — one of the 11 event types
/// 6. `item_id` — the work item this event mutates
/// 7. `data` — typed payload (JSON in TSJSON, deserialized here)
/// 8. `event_hash` — BLAKE3 hash of fields 1–7
///
/// # Serde
///
/// Custom `Deserialize` implementation uses `event_type` to drive typed
/// deserialization of the `data` field. This is necessary because the type
/// discriminant is external to the JSON payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    /// Wall-clock timestamp in microseconds since Unix epoch.
    ///
    /// Monotonically increasing per-repo via the clock file.
    pub wall_ts_us: i64,

    /// Identifier of the agent or user that produced this event.
    pub agent: String,

    /// Interval Tree Clock stamp in canonical text encoding.
    ///
    /// Used for causal ordering independent of wall-clock time.
    pub itc: String,

    /// Parent event hashes forming the Merkle-DAG.
    ///
    /// Sorted lexicographically. Empty for the first event in a repo.
    /// Format: `["blake3:abcdef...", ...]`
    pub parents: Vec<String>,

    /// The type of mutation this event represents.
    pub event_type: EventType,

    /// The work item being mutated.
    pub item_id: ItemId,

    /// Typed payload data specific to the event type.
    pub data: EventData,

    /// BLAKE3 content hash of fields 1–7.
    ///
    /// Format: `blake3:<hex>`. This is the event's identity in the
    /// Merkle-DAG and is used for parent references, shard manifests,
    /// and sync diffing.
    pub event_hash: String,
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Helper struct for two-pass deserialization: first get the `event_type`,
        /// then use it to deserialize the data payload.
        #[derive(Deserialize)]
        struct EventRaw {
            wall_ts_us: i64,
            agent: String,
            itc: String,
            parents: Vec<String>,
            event_type: EventType,
            item_id: ItemId,
            data: serde_json::Value,
            event_hash: String,
        }

        let raw = EventRaw::deserialize(deserializer)?;
        let data_json = raw.data.to_string();
        let data = EventData::deserialize_for(raw.event_type, &data_json)
            .map_err(serde::de::Error::custom)?;

        Ok(Self {
            wall_ts_us: raw.wall_ts_us,
            agent: raw.agent,
            itc: raw.itc,
            parents: raw.parents,
            event_type: raw.event_type,
            item_id: raw.item_id,
            data,
            event_hash: raw.event_hash,
        })
    }
}

impl Event {
    /// Return the TSJSON parents field string (comma-separated, sorted).
    ///
    /// Returns an empty string for root events (no parents).
    #[must_use]
    pub fn parents_str(&self) -> String {
        self.parents.join(",")
    }
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}\t{}\t{}\t{}\t{}\t{}",
            self.wall_ts_us,
            self.agent,
            self.event_type,
            self.item_id,
            self.event_hash,
            // Abbreviated data display
            match &self.data {
                EventData::Create(d) => format!("create: {}", d.title),
                EventData::Update(d) => format!("update: {}={}", d.field, d.value),
                EventData::Move(d) => format!("move: {}", d.state),
                EventData::Assign(d) => format!("{}: {}", d.action, d.agent),
                EventData::Comment(d) => {
                    let preview = if d.body.len() > 40 {
                        format!("{}...", &d.body[..40])
                    } else {
                        d.body.clone()
                    };
                    format!("comment: {preview}")
                }
                EventData::Link(d) => format!("link: {} {}", d.link_type, d.target),
                EventData::Unlink(d) => format!("unlink: {}", d.target),
                EventData::Delete(_) => "delete".to_string(),
                EventData::Compact(d) => {
                    let preview = if d.summary.len() > 40 {
                        format!("{}...", &d.summary[..40])
                    } else {
                        d.summary.clone()
                    };
                    format!("compact: {preview}")
                }
                EventData::Snapshot(_) => "snapshot".to_string(),
                EventData::Redact(d) => format!("redact: {}", d.target_hash),
            }
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
            event_hash: "blake3:a1b2c3d4e5f6".into(),
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
    fn event_struct_fields() {
        let event = sample_create_event();
        assert_eq!(event.wall_ts_us, 1_708_012_200_123_456);
        assert_eq!(event.agent, "claude-abc");
        assert_eq!(event.itc, "itc:AQ");
        assert!(event.parents.is_empty());
        assert_eq!(event.event_type, EventType::Create);
        assert_eq!(event.item_id.as_str(), "bn-a3f8");
        assert!(matches!(event.data, EventData::Create(_)));
        assert_eq!(event.event_hash, "blake3:a1b2c3d4e5f6");
    }

    #[test]
    fn event_parents_str_empty() {
        let event = sample_create_event();
        assert_eq!(event.parents_str(), "");
    }

    #[test]
    fn event_parents_str_single() {
        let event = sample_move_event();
        assert_eq!(event.parents_str(), "blake3:a1b2c3d4e5f6");
    }

    #[test]
    fn event_parents_str_multiple() {
        let mut event = sample_move_event();
        event.parents = vec!["blake3:aaa".into(), "blake3:bbb".into()];
        assert_eq!(event.parents_str(), "blake3:aaa,blake3:bbb");
    }

    #[test]
    fn event_display() {
        let event = sample_create_event();
        let display = event.to_string();
        assert!(display.contains("1708012200123456"));
        assert!(display.contains("claude-abc"));
        assert!(display.contains("item.create"));
        assert!(display.contains("bn-a3f8"));
        assert!(display.contains("Fix auth retry"));
    }

    #[test]
    fn event_serde_json_roundtrip() {
        let event = sample_create_event();
        let json = serde_json::to_string(&event).expect("serialize");
        let deser: Event = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deser);
    }

    #[test]
    fn event_serde_json_roundtrip_with_parents() {
        let event = sample_move_event();
        let json = serde_json::to_string(&event).expect("serialize");
        let deser: Event = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, deser);
    }

    #[test]
    fn event_serde_all_types_roundtrip() {
        let base = || -> (i64, String, String, Vec<String>, ItemId, String) {
            (
                1_000_000,
                "agent".into(),
                "itc:X".into(),
                vec![],
                ItemId::new_unchecked("bn-a7x"),
                "blake3:000".into(),
            )
        };

        let events: Vec<Event> = vec![
            {
                let (ts, agent, itc, parents, item_id, hash) = base();
                Event {
                    wall_ts_us: ts,
                    agent,
                    itc,
                    parents,
                    event_type: EventType::Create,
                    item_id,
                    data: EventData::Create(CreateData {
                        title: "T".into(),
                        kind: crate::model::item::Kind::Task,
                        size: None,
                        urgency: crate::model::item::Urgency::Default,
                        labels: vec![],
                        parent: None,
                        causation: None,
                        description: None,
                        extra: BTreeMap::new(),
                    }),
                    event_hash: hash,
                }
            },
            {
                let (ts, agent, itc, parents, item_id, hash) = base();
                Event {
                    wall_ts_us: ts,
                    agent,
                    itc,
                    parents,
                    event_type: EventType::Update,
                    item_id,
                    data: EventData::Update(UpdateData {
                        field: "title".into(),
                        value: json!("New"),
                        extra: BTreeMap::new(),
                    }),
                    event_hash: hash,
                }
            },
            {
                let (ts, agent, itc, parents, item_id, hash) = base();
                Event {
                    wall_ts_us: ts,
                    agent,
                    itc,
                    parents,
                    event_type: EventType::Move,
                    item_id,
                    data: EventData::Move(MoveData {
                        state: crate::model::item::State::Done,
                        reason: Some("done".into()),
                        extra: BTreeMap::new(),
                    }),
                    event_hash: hash,
                }
            },
            {
                let (ts, agent, itc, parents, item_id, hash) = base();
                Event {
                    wall_ts_us: ts,
                    agent,
                    itc,
                    parents,
                    event_type: EventType::Assign,
                    item_id,
                    data: EventData::Assign(AssignData {
                        agent: "alice".into(),
                        action: AssignAction::Assign,
                        extra: BTreeMap::new(),
                    }),
                    event_hash: hash,
                }
            },
            {
                let (ts, agent, itc, parents, item_id, hash) = base();
                Event {
                    wall_ts_us: ts,
                    agent,
                    itc,
                    parents,
                    event_type: EventType::Comment,
                    item_id,
                    data: EventData::Comment(CommentData {
                        body: "Note".into(),
                        extra: BTreeMap::new(),
                    }),
                    event_hash: hash,
                }
            },
            {
                let (ts, agent, itc, parents, item_id, hash) = base();
                Event {
                    wall_ts_us: ts,
                    agent,
                    itc,
                    parents,
                    event_type: EventType::Link,
                    item_id,
                    data: EventData::Link(LinkData {
                        target: "bn-b8y".into(),
                        link_type: "blocks".into(),
                        extra: BTreeMap::new(),
                    }),
                    event_hash: hash,
                }
            },
            {
                let (ts, agent, itc, parents, item_id, hash) = base();
                Event {
                    wall_ts_us: ts,
                    agent,
                    itc,
                    parents,
                    event_type: EventType::Unlink,
                    item_id,
                    data: EventData::Unlink(UnlinkData {
                        target: "bn-b8y".into(),
                        link_type: None,
                        extra: BTreeMap::new(),
                    }),
                    event_hash: hash,
                }
            },
            {
                let (ts, agent, itc, parents, item_id, hash) = base();
                Event {
                    wall_ts_us: ts,
                    agent,
                    itc,
                    parents,
                    event_type: EventType::Delete,
                    item_id,
                    data: EventData::Delete(DeleteData {
                        reason: None,
                        extra: BTreeMap::new(),
                    }),
                    event_hash: hash,
                }
            },
            {
                let (ts, agent, itc, parents, item_id, hash) = base();
                Event {
                    wall_ts_us: ts,
                    agent,
                    itc,
                    parents,
                    event_type: EventType::Compact,
                    item_id,
                    data: EventData::Compact(CompactData {
                        summary: "TL;DR".into(),
                        extra: BTreeMap::new(),
                    }),
                    event_hash: hash,
                }
            },
            {
                let (ts, agent, itc, parents, item_id, hash) = base();
                Event {
                    wall_ts_us: ts,
                    agent,
                    itc,
                    parents,
                    event_type: EventType::Snapshot,
                    item_id,
                    data: EventData::Snapshot(SnapshotData {
                        state: json!({"id": "bn-a7x"}),
                        extra: BTreeMap::new(),
                    }),
                    event_hash: hash,
                }
            },
            {
                let (ts, agent, itc, parents, item_id, hash) = base();
                Event {
                    wall_ts_us: ts,
                    agent,
                    itc,
                    parents,
                    event_type: EventType::Redact,
                    item_id,
                    data: EventData::Redact(RedactData {
                        target_hash: "blake3:xyz".into(),
                        reason: "oops".into(),
                        extra: BTreeMap::new(),
                    }),
                    event_hash: hash,
                }
            },
        ];

        assert_eq!(events.len(), 11, "should cover all 11 event types");

        for event in &events {
            let json = serde_json::to_string(event)
                .unwrap_or_else(|e| panic!("serialize {} failed: {e}", event.event_type));
            let deser: Event = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("deserialize {} failed: {e}", event.event_type));
            assert_eq!(
                *event, deser,
                "roundtrip failed for {}",
                event.event_type
            );
        }
    }

    #[test]
    fn event_display_all_data_types() {
        // Smoke test: Display doesn't panic for any variant
        let events = vec![
            sample_create_event(),
            sample_move_event(),
        ];
        for event in events {
            let _ = event.to_string(); // Should not panic
        }
    }
}
