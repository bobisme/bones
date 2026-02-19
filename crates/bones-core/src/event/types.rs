//! Event type enum covering all 11 TSJSON event types.
//!
//! Each event type corresponds to a specific work-item mutation. The string
//! representation uses the `item.<verb>` dotted format used in the TSJSON
//! event log.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// The 11 event types in the bones event catalog.
///
/// String representation follows the `item.<verb>` convention used in the
/// TSJSON event log format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventType {
    /// Create a new work item.
    Create,
    /// Update fields (title, description, size, labels, etc.).
    Update,
    /// Transition to a new lifecycle state.
    Move,
    /// Assign or unassign an agent.
    Assign,
    /// Add a comment or note.
    Comment,
    /// Add a dependency or relationship.
    Link,
    /// Remove a dependency or relationship.
    Unlink,
    /// Soft-delete (tombstone).
    Delete,
    /// Replace description with summary (memory decay).
    Compact,
    /// Lattice-compacted state for a completed item.
    Snapshot,
    /// Replace event payload with [redacted] in projection.
    Redact,
}

/// Error returned when parsing an unknown event type string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownEventType {
    /// The unrecognised input string.
    pub raw: String,
}

impl fmt::Display for UnknownEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown event type '{}': expected one of item.create, item.update, \
             item.move, item.assign, item.comment, item.link, item.unlink, \
             item.delete, item.compact, item.snapshot, item.redact",
            self.raw
        )
    }
}

impl std::error::Error for UnknownEventType {}

impl EventType {
    /// All known event types in catalog order.
    pub const ALL: [Self; 11] = [
        Self::Create,
        Self::Update,
        Self::Move,
        Self::Assign,
        Self::Comment,
        Self::Link,
        Self::Unlink,
        Self::Delete,
        Self::Compact,
        Self::Snapshot,
        Self::Redact,
    ];

    /// Return the canonical `item.<verb>` string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "item.create",
            Self::Update => "item.update",
            Self::Move => "item.move",
            Self::Assign => "item.assign",
            Self::Comment => "item.comment",
            Self::Link => "item.link",
            Self::Unlink => "item.unlink",
            Self::Delete => "item.delete",
            Self::Compact => "item.compact",
            Self::Snapshot => "item.snapshot",
            Self::Redact => "item.redact",
        }
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventType {
    type Err = UnknownEventType;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "item.create" => Ok(Self::Create),
            "item.update" => Ok(Self::Update),
            "item.move" => Ok(Self::Move),
            "item.assign" => Ok(Self::Assign),
            "item.comment" => Ok(Self::Comment),
            "item.link" => Ok(Self::Link),
            "item.unlink" => Ok(Self::Unlink),
            "item.delete" => Ok(Self::Delete),
            "item.compact" => Ok(Self::Compact),
            "item.snapshot" => Ok(Self::Snapshot),
            "item.redact" => Ok(Self::Redact),
            _ => Err(UnknownEventType { raw: s.to_string() }),
        }
    }
}

// Custom serde: serialize as the `item.<verb>` string.
impl Serialize for EventType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EventType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_all_types() {
        let expected = [
            (EventType::Create, "item.create"),
            (EventType::Update, "item.update"),
            (EventType::Move, "item.move"),
            (EventType::Assign, "item.assign"),
            (EventType::Comment, "item.comment"),
            (EventType::Link, "item.link"),
            (EventType::Unlink, "item.unlink"),
            (EventType::Delete, "item.delete"),
            (EventType::Compact, "item.compact"),
            (EventType::Snapshot, "item.snapshot"),
            (EventType::Redact, "item.redact"),
        ];

        for (et, s) in expected {
            assert_eq!(et.to_string(), s);
            assert_eq!(et.as_str(), s);
        }
    }

    #[test]
    fn fromstr_all_types() {
        for et in EventType::ALL {
            let parsed: EventType = et.as_str().parse().expect("should parse");
            assert_eq!(parsed, et);
        }
    }

    #[test]
    fn display_fromstr_roundtrip() {
        for et in EventType::ALL {
            let s = et.to_string();
            let reparsed: EventType = s.parse().expect("should roundtrip");
            assert_eq!(et, reparsed);
        }
    }

    #[test]
    fn fromstr_rejects_unknown() {
        let err = "item.unknown".parse::<EventType>().unwrap_err();
        assert_eq!(err.raw, "item.unknown");
        assert!(err.to_string().contains("item.unknown"));
        assert!(err.to_string().contains("expected one of"));
    }

    #[test]
    fn fromstr_rejects_empty() {
        assert!("".parse::<EventType>().is_err());
    }

    #[test]
    fn fromstr_rejects_bare_verb() {
        // Must use full "item.<verb>" format
        assert!("create".parse::<EventType>().is_err());
    }

    #[test]
    fn serde_json_roundtrip() {
        for et in EventType::ALL {
            let json = serde_json::to_string(&et).expect("serialize");
            let expected = format!("\"{}\"", et.as_str());
            assert_eq!(json, expected);

            let deser: EventType = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(deser, et);
        }
    }

    #[test]
    fn serde_rejects_unknown_type() {
        let result = serde_json::from_str::<EventType>("\"item.foobar\"");
        assert!(result.is_err());
    }

    #[test]
    fn all_contains_exactly_11_types() {
        assert_eq!(EventType::ALL.len(), 11);
    }

    #[test]
    fn error_display_includes_valid_options() {
        let err = UnknownEventType { raw: "nope".into() };
        let msg = err.to_string();
        for et in EventType::ALL {
            assert!(msg.contains(et.as_str()), "missing {}", et.as_str());
        }
    }
}
