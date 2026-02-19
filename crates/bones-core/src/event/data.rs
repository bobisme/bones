//! Typed payload data structs for each event type.
//!
//! Each event type has a corresponding data struct that defines the JSON
//! payload schema. Unknown fields are preserved via `#[serde(flatten)]`
//! for forward compatibility.

use crate::model::item::{Kind, Size, State, Urgency};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use super::types::EventType;

// ---------------------------------------------------------------------------
// EventData — the unified payload enum
// ---------------------------------------------------------------------------

/// Typed payload for an event. The discriminant comes from [`EventType`],
/// not from the JSON itself (it is an external tag in TSJSON).
///
/// **Serde note:** `EventData` implements `Serialize` manually (dispatching
/// to the inner struct) but does **not** implement `Deserialize` directly.
/// Use [`EventData::deserialize_for`] with the known [`EventType`] to
/// deserialize from JSON. The [`Event`](super::Event) struct handles this
/// in its custom `Deserialize` impl.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventData {
    /// Payload for `item.create`.
    Create(CreateData),
    /// Payload for `item.update`.
    Update(UpdateData),
    /// Payload for `item.move`.
    Move(MoveData),
    /// Payload for `item.assign`.
    Assign(AssignData),
    /// Payload for `item.comment`.
    Comment(CommentData),
    /// Payload for `item.link`.
    Link(LinkData),
    /// Payload for `item.unlink`.
    Unlink(UnlinkData),
    /// Payload for `item.delete`.
    Delete(DeleteData),
    /// Payload for `item.compact`.
    Compact(CompactData),
    /// Payload for `item.snapshot`.
    Snapshot(SnapshotData),
    /// Payload for `item.redact`.
    Redact(RedactData),
}

impl EventData {
    /// Deserialize a JSON string into the correct `EventData` variant based
    /// on the event type.
    ///
    /// This is the primary deserialization entry point since the type
    /// discriminant lives in a separate TSJSON field, not in the JSON payload.
    ///
    /// # Errors
    ///
    /// Returns a [`DataParseError`] if the JSON is malformed or does not match
    /// the expected schema for the given event type.
    pub fn deserialize_for(
        event_type: EventType,
        json: &str,
    ) -> Result<Self, DataParseError> {
        let result = match event_type {
            EventType::Create => {
                serde_json::from_str::<CreateData>(json).map(EventData::Create)
            }
            EventType::Update => {
                serde_json::from_str::<UpdateData>(json).map(EventData::Update)
            }
            EventType::Move => {
                serde_json::from_str::<MoveData>(json).map(EventData::Move)
            }
            EventType::Assign => {
                serde_json::from_str::<AssignData>(json).map(EventData::Assign)
            }
            EventType::Comment => {
                serde_json::from_str::<CommentData>(json).map(EventData::Comment)
            }
            EventType::Link => {
                serde_json::from_str::<LinkData>(json).map(EventData::Link)
            }
            EventType::Unlink => {
                serde_json::from_str::<UnlinkData>(json).map(EventData::Unlink)
            }
            EventType::Delete => {
                serde_json::from_str::<DeleteData>(json).map(EventData::Delete)
            }
            EventType::Compact => {
                serde_json::from_str::<CompactData>(json).map(EventData::Compact)
            }
            EventType::Snapshot => {
                serde_json::from_str::<SnapshotData>(json).map(EventData::Snapshot)
            }
            EventType::Redact => {
                serde_json::from_str::<RedactData>(json).map(EventData::Redact)
            }
        };

        result.map_err(|source| DataParseError {
            event_type,
            source,
        })
    }

    /// Serialize the payload to a [`serde_json::Value`].
    ///
    /// # Errors
    ///
    /// Returns an error if the inner struct fails to serialize (should not
    /// happen with well-formed data).
    pub fn to_json_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        match self {
            Self::Create(d) => serde_json::to_value(d),
            Self::Update(d) => serde_json::to_value(d),
            Self::Move(d) => serde_json::to_value(d),
            Self::Assign(d) => serde_json::to_value(d),
            Self::Comment(d) => serde_json::to_value(d),
            Self::Link(d) => serde_json::to_value(d),
            Self::Unlink(d) => serde_json::to_value(d),
            Self::Delete(d) => serde_json::to_value(d),
            Self::Compact(d) => serde_json::to_value(d),
            Self::Snapshot(d) => serde_json::to_value(d),
            Self::Redact(d) => serde_json::to_value(d),
        }
    }
}

impl Serialize for EventData {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Create(d) => d.serialize(serializer),
            Self::Update(d) => d.serialize(serializer),
            Self::Move(d) => d.serialize(serializer),
            Self::Assign(d) => d.serialize(serializer),
            Self::Comment(d) => d.serialize(serializer),
            Self::Link(d) => d.serialize(serializer),
            Self::Unlink(d) => d.serialize(serializer),
            Self::Delete(d) => d.serialize(serializer),
            Self::Compact(d) => d.serialize(serializer),
            Self::Snapshot(d) => d.serialize(serializer),
            Self::Redact(d) => d.serialize(serializer),
        }
    }
}

// ---------------------------------------------------------------------------
// DataParseError
// ---------------------------------------------------------------------------

/// Error returned when deserializing an event's JSON payload fails.
#[derive(Debug)]
pub struct DataParseError {
    /// The event type that was being deserialized.
    pub event_type: EventType,
    /// The underlying JSON parse error.
    pub source: serde_json::Error,
}

impl fmt::Display for DataParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid {} data payload: {}",
            self.event_type, self.source
        )
    }
}

impl std::error::Error for DataParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

// ---------------------------------------------------------------------------
// AssignAction enum
// ---------------------------------------------------------------------------

/// Whether an agent is being assigned or unassigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssignAction {
    /// Assign an agent to the work item.
    Assign,
    /// Remove an agent from the work item.
    Unassign,
}

impl AssignAction {
    /// Return the canonical string form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Assign => "assign",
            Self::Unassign => "unassign",
        }
    }
}

impl fmt::Display for AssignAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AssignAction {
    type Err = ParseAssignActionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "assign" => Ok(Self::Assign),
            "unassign" => Ok(Self::Unassign),
            _ => Err(ParseAssignActionError(s.to_string())),
        }
    }
}

/// Error returned when parsing an invalid assign action string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseAssignActionError(pub String);

impl fmt::Display for ParseAssignActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid assign action '{}': expected 'assign' or 'unassign'",
            self.0
        )
    }
}

impl std::error::Error for ParseAssignActionError {}

// ---------------------------------------------------------------------------
// Payload structs — one per event type
// ---------------------------------------------------------------------------

/// Payload for `item.create`.
///
/// Creates a new work item with initial field values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateData {
    /// Title of the new work item (required).
    pub title: String,

    /// Kind of work item.
    pub kind: Kind,

    /// Optional t-shirt size estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Size>,

    /// Priority override.
    #[serde(default, skip_serializing_if = "is_default_urgency")]
    pub urgency: Urgency,

    /// Initial labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,

    /// Optional parent item ID (for hierarchical items).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,

    /// Optional causation reference — the item that triggered this creation.
    /// Purely audit-trail metadata, not a dependency link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation: Option<String>,

    /// Optional initial description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Unknown fields preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if requires &T -> bool
fn is_default_urgency(u: &Urgency) -> bool {
    *u == Urgency::Default
}

/// Payload for `item.update`.
///
/// Updates a single field on a work item. The `value` is a dynamic JSON
/// value since different fields have different types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateData {
    /// Name of the field being updated (e.g. "title", "description", "size").
    pub field: String,

    /// New value for the field.
    pub value: serde_json::Value,

    /// Unknown fields preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Payload for `item.move`.
///
/// Transitions a work item to a new lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveData {
    /// Target state.
    pub state: State,

    /// Optional reason for the transition (e.g. "Shipped in commit 9f3a2b1").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// Unknown fields preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Payload for `item.assign`.
///
/// Assigns or unassigns an agent to/from a work item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignData {
    /// Agent identifier being assigned/unassigned.
    pub agent: String,

    /// Whether this is an assignment or removal.
    pub action: AssignAction,

    /// Unknown fields preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Payload for `item.comment`.
///
/// Adds a comment or note to a work item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentData {
    /// Comment body text.
    pub body: String,

    /// Unknown fields preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Payload for `item.link`.
///
/// Adds a dependency or relationship between work items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkData {
    /// Target item ID of the link.
    pub target: String,

    /// Type of relationship (e.g. `blocks`, `related_to`).
    pub link_type: String,

    /// Unknown fields preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Payload for `item.unlink`.
///
/// Removes a dependency or relationship between work items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnlinkData {
    /// Target item ID to unlink.
    pub target: String,

    /// Type of relationship being removed (e.g. `blocks`, `related_to`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_type: Option<String>,

    /// Unknown fields preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Payload for `item.delete`.
///
/// Soft-deletes a work item (tombstone). Minimal payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteData {
    /// Optional reason for deletion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// Unknown fields preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Payload for `item.compact`.
///
/// Replaces the item's description with a summary (memory decay).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactData {
    /// Summary text replacing the original description.
    pub summary: String,

    /// Unknown fields preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Payload for `item.snapshot`.
///
/// Lattice-compacted full state for a completed item. Replaces the event
/// history with a single snapshot event during log compaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotData {
    /// The full compacted state of the work item as a JSON object.
    pub state: serde_json::Value,

    /// Unknown fields preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Payload for `item.redact`.
///
/// Targets a prior event by hash for payload redaction in the projection.
/// The original event remains in the log (Merkle integrity preserved) but
/// projections hide the content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactData {
    /// Hash of the event whose payload should be redacted.
    pub target_hash: String,

    /// Reason for redaction (e.g. "accidental secret exposure", "legal erasure").
    pub reason: String,

    /// Unknown fields preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // === CreateData =========================================================

    #[test]
    fn create_data_full_roundtrip() {
        let data = CreateData {
            title: "Fix auth retry".into(),
            kind: Kind::Task,
            size: Some(Size::M),
            urgency: Urgency::Default,
            labels: vec!["backend".into()],
            parent: None,
            causation: Some("bn-x1y2".into()),
            description: None,
            extra: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).expect("serialize");
        let deser: CreateData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, deser);
    }

    #[test]
    fn create_data_minimal() {
        let json = r#"{"title":"Hello","kind":"task"}"#;
        let data: CreateData = serde_json::from_str(json).expect("deserialize");
        assert_eq!(data.title, "Hello");
        assert_eq!(data.kind, Kind::Task);
        assert_eq!(data.urgency, Urgency::Default);
        assert!(data.labels.is_empty());
        assert!(data.parent.is_none());
        assert!(data.causation.is_none());
        assert!(data.description.is_none());
    }

    #[test]
    fn create_data_with_unknown_fields() {
        let json = r#"{"title":"Test","kind":"bug","future_field":"value123"}"#;
        let data: CreateData = serde_json::from_str(json).expect("deserialize");
        assert_eq!(data.title, "Test");
        assert_eq!(data.kind, Kind::Bug);
        assert_eq!(
            data.extra.get("future_field"),
            Some(&json!("value123"))
        );

        // Roundtrip preserves the unknown field
        let reserialized = serde_json::to_string(&data).expect("serialize");
        assert!(reserialized.contains("future_field"));
    }

    #[test]
    fn create_data_plan_example() {
        // From plan.md example line
        let json = r#"{"kind":"task","labels":["backend"],"size":"m","title":"Fix auth retry"}"#;
        let data: CreateData = serde_json::from_str(json).expect("deserialize");
        assert_eq!(data.title, "Fix auth retry");
        assert_eq!(data.kind, Kind::Task);
        assert_eq!(data.size, Some(Size::M));
        assert_eq!(data.labels, vec!["backend"]);
    }

    // === UpdateData =========================================================

    #[test]
    fn update_data_string_field() {
        let data = UpdateData {
            field: "title".into(),
            value: json!("New title"),
            extra: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).expect("serialize");
        let deser: UpdateData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, deser);
    }

    #[test]
    fn update_data_array_field() {
        let data = UpdateData {
            field: "labels".into(),
            value: json!(["frontend", "urgent"]),
            extra: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).expect("serialize");
        let deser: UpdateData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, deser);
    }

    // === MoveData ===========================================================

    #[test]
    fn move_data_without_reason() {
        let json = r#"{"state":"doing"}"#;
        let data: MoveData = serde_json::from_str(json).expect("deserialize");
        assert_eq!(data.state, State::Doing);
        assert!(data.reason.is_none());
    }

    #[test]
    fn move_data_with_reason() {
        let json = r#"{"state":"done","reason":"Shipped in commit 9f3a2b1"}"#;
        let data: MoveData = serde_json::from_str(json).expect("deserialize");
        assert_eq!(data.state, State::Done);
        assert_eq!(data.reason.as_deref(), Some("Shipped in commit 9f3a2b1"));
    }

    #[test]
    fn move_data_roundtrip() {
        let data = MoveData {
            state: State::Archived,
            reason: Some("No longer needed".into()),
            extra: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).expect("serialize");
        let deser: MoveData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, deser);
    }

    // === AssignData =========================================================

    #[test]
    fn assign_data_roundtrip() {
        let data = AssignData {
            agent: "claude-abc".into(),
            action: AssignAction::Assign,
            extra: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).expect("serialize");
        let deser: AssignData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, deser);
    }

    #[test]
    fn assign_data_unassign() {
        let json = r#"{"agent":"gemini-xyz","action":"unassign"}"#;
        let data: AssignData = serde_json::from_str(json).expect("deserialize");
        assert_eq!(data.agent, "gemini-xyz");
        assert_eq!(data.action, AssignAction::Unassign);
    }

    // === CommentData ========================================================

    #[test]
    fn comment_data_roundtrip() {
        let data = CommentData {
            body: "Root cause is a race in token refresh.".into(),
            extra: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).expect("serialize");
        let deser: CommentData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, deser);
    }

    // === LinkData ===========================================================

    #[test]
    fn link_data_blocks() {
        let data = LinkData {
            target: "bn-c7d2".into(),
            link_type: "blocks".into(),
            extra: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).expect("serialize");
        let deser: LinkData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, deser);
    }

    #[test]
    fn link_data_related() {
        let data = LinkData {
            target: "bn-a7x".into(),
            link_type: "related_to".into(),
            extra: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).expect("serialize");
        assert!(json.contains("related_to"));
    }

    // === UnlinkData =========================================================

    #[test]
    fn unlink_data_roundtrip() {
        let data = UnlinkData {
            target: "bn-c7d2".into(),
            link_type: Some("blocks".into()),
            extra: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).expect("serialize");
        let deser: UnlinkData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, deser);
    }

    #[test]
    fn unlink_data_without_link_type() {
        let json = r#"{"target":"bn-a7x"}"#;
        let data: UnlinkData = serde_json::from_str(json).expect("deserialize");
        assert_eq!(data.target, "bn-a7x");
        assert!(data.link_type.is_none());
    }

    // === DeleteData =========================================================

    #[test]
    fn delete_data_empty() {
        let json = "{}";
        let data: DeleteData = serde_json::from_str(json).expect("deserialize");
        assert!(data.reason.is_none());
    }

    #[test]
    fn delete_data_with_reason() {
        let data = DeleteData {
            reason: Some("Duplicate of bn-xyz".into()),
            extra: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).expect("serialize");
        let deser: DeleteData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, deser);
    }

    // === CompactData ========================================================

    #[test]
    fn compact_data_roundtrip() {
        let data = CompactData {
            summary: "Auth token refresh race condition fix.".into(),
            extra: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).expect("serialize");
        let deser: CompactData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, deser);
    }

    // === SnapshotData =======================================================

    #[test]
    fn snapshot_data_roundtrip() {
        let state = json!({
            "id": "bn-a3f8",
            "title": "Fix auth retry",
            "kind": "task",
            "state": "done",
            "urgency": "default",
            "labels": ["backend"],
            "assignees": ["claude-abc"]
        });
        let data = SnapshotData {
            state,
            extra: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).expect("serialize");
        let deser: SnapshotData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, deser);
    }

    // === RedactData =========================================================

    #[test]
    fn redact_data_roundtrip() {
        let data = RedactData {
            target_hash: "blake3:a1b2c3d4e5f6".into(),
            reason: "Accidental secret exposure".into(),
            extra: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).expect("serialize");
        let deser: RedactData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, deser);
    }

    // === EventData::deserialize_for =========================================

    #[test]
    fn deserialize_for_create() {
        let json = r#"{"title":"Test","kind":"task"}"#;
        let data = EventData::deserialize_for(EventType::Create, json).expect("should parse");
        assert!(matches!(data, EventData::Create(_)));
    }

    #[test]
    fn deserialize_for_update() {
        let json = r#"{"field":"title","value":"New"}"#;
        let data = EventData::deserialize_for(EventType::Update, json).expect("should parse");
        assert!(matches!(data, EventData::Update(_)));
    }

    #[test]
    fn deserialize_for_move() {
        let json = r#"{"state":"doing"}"#;
        let data = EventData::deserialize_for(EventType::Move, json).expect("should parse");
        assert!(matches!(data, EventData::Move(_)));
    }

    #[test]
    fn deserialize_for_assign() {
        let json = r#"{"agent":"alice","action":"assign"}"#;
        let data = EventData::deserialize_for(EventType::Assign, json).expect("should parse");
        assert!(matches!(data, EventData::Assign(_)));
    }

    #[test]
    fn deserialize_for_comment() {
        let json = r#"{"body":"Hello world"}"#;
        let data = EventData::deserialize_for(EventType::Comment, json).expect("should parse");
        assert!(matches!(data, EventData::Comment(_)));
    }

    #[test]
    fn deserialize_for_link() {
        let json = r#"{"target":"bn-abc","link_type":"blocks"}"#;
        let data = EventData::deserialize_for(EventType::Link, json).expect("should parse");
        assert!(matches!(data, EventData::Link(_)));
    }

    #[test]
    fn deserialize_for_unlink() {
        let json = r#"{"target":"bn-abc"}"#;
        let data = EventData::deserialize_for(EventType::Unlink, json).expect("should parse");
        assert!(matches!(data, EventData::Unlink(_)));
    }

    #[test]
    fn deserialize_for_delete() {
        let json = "{}";
        let data = EventData::deserialize_for(EventType::Delete, json).expect("should parse");
        assert!(matches!(data, EventData::Delete(_)));
    }

    #[test]
    fn deserialize_for_compact() {
        let json = r#"{"summary":"TL;DR"}"#;
        let data = EventData::deserialize_for(EventType::Compact, json).expect("should parse");
        assert!(matches!(data, EventData::Compact(_)));
    }

    #[test]
    fn deserialize_for_snapshot() {
        let json = r#"{"state":{"id":"bn-a3f8","title":"Test"}}"#;
        let data = EventData::deserialize_for(EventType::Snapshot, json).expect("should parse");
        assert!(matches!(data, EventData::Snapshot(_)));
    }

    #[test]
    fn deserialize_for_redact() {
        let json = r#"{"target_hash":"blake3:abc","reason":"oops"}"#;
        let data = EventData::deserialize_for(EventType::Redact, json).expect("should parse");
        assert!(matches!(data, EventData::Redact(_)));
    }

    #[test]
    fn deserialize_for_error_includes_event_type() {
        let err = EventData::deserialize_for(EventType::Create, "not json")
            .expect_err("should fail");
        assert!(err.to_string().contains("item.create"));
    }

    #[test]
    fn deserialize_for_error_missing_required_field() {
        // CreateData requires title and kind
        let err = EventData::deserialize_for(EventType::Create, r#"{"kind":"task"}"#)
            .expect_err("should fail");
        assert!(err.to_string().contains("item.create"));
    }

    // === AssignAction =======================================================

    #[test]
    fn assign_action_display_fromstr_roundtrip() {
        for action in [AssignAction::Assign, AssignAction::Unassign] {
            let s = action.to_string();
            let reparsed: AssignAction = s.parse().expect("should parse");
            assert_eq!(action, reparsed);
        }
    }

    #[test]
    fn assign_action_rejects_unknown() {
        assert!("add".parse::<AssignAction>().is_err());
    }

    // === Forward compatibility =============================================

    #[test]
    fn all_payload_types_preserve_unknown_fields() {
        // Test that every payload struct preserves unknown fields via #[serde(flatten)]
        let test_cases: Vec<(&str, EventType)> = vec![
            (r#"{"title":"T","kind":"task","x":1}"#, EventType::Create),
            (r#"{"field":"f","value":"v","x":1}"#, EventType::Update),
            (r#"{"state":"open","x":1}"#, EventType::Move),
            (r#"{"agent":"a","action":"assign","x":1}"#, EventType::Assign),
            (r#"{"body":"b","x":1}"#, EventType::Comment),
            (r#"{"target":"t","link_type":"blocks","x":1}"#, EventType::Link),
            (r#"{"target":"t","x":1}"#, EventType::Unlink),
            (r#"{"x":1}"#, EventType::Delete),
            (r#"{"summary":"s","x":1}"#, EventType::Compact),
            (r#"{"state":{},"x":1}"#, EventType::Snapshot),
            (r#"{"target_hash":"h","reason":"r","x":1}"#, EventType::Redact),
        ];

        for (json_str, event_type) in test_cases {
            let data = EventData::deserialize_for(event_type, json_str)
                .unwrap_or_else(|e| panic!("failed for {event_type}: {e}"));

            // Roundtrip serialization should preserve the unknown "x" field
            let reserialized = serde_json::to_string(&data).expect("serialize");
            assert!(
                reserialized.contains("\"x\":1") || reserialized.contains("\"x\": 1"),
                "unknown field lost for {event_type}: {reserialized}"
            );
        }
    }
}
