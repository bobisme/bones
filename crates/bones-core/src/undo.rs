//! Compensating event generation for `bn undo`.
//!
//! Events are immutable and append-only. `undo` does **not** delete or modify
//! events — it emits *compensating events* that reverse the observable effect:
//!
//! | Original event | Compensating event |
//! |---|---|
//! | `item.create` | `item.delete` |
//! | `item.update` | `item.update` with prior field value |
//! | `item.move` | `item.move` back to prior state |
//! | `item.assign(assign)` | `item.assign(unassign)` |
//! | `item.assign(unassign)` | `item.assign(assign)` |
//! | `item.link` | `item.unlink` |
//! | `item.unlink` | `item.link` |
//! | `item.delete` | `item.create` (reconstruct from history) |
//!
//! Events that **cannot** be undone (grow-only by design):
//! - `item.comment` — G-Set: comments are permanent
//! - `item.compact` — compaction is not reversible without original events
//! - `item.snapshot` — same as compact
//! - `item.redact` — intentionally permanent

use crate::event::data::{
    AssignAction, AssignData, CreateData, DeleteData, EventData, LinkData, MoveData, UnlinkData,
    UpdateData,
};
use crate::event::{Event, EventType};
use crate::model::item::State;
use crate::model::item_id::ItemId;
use std::collections::BTreeMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Reason a compensating event cannot be generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoError {
    /// The event type uses a grow-only CRDT and cannot be undone.
    GrowOnly(EventType),
    /// Context from prior events is needed but unavailable or insufficient.
    NoPriorState(String),
}

impl fmt::Display for UndoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GrowOnly(et) => write!(
                f,
                "cannot undo {et}: this event type is grow-only and permanently recorded"
            ),
            Self::NoPriorState(msg) => write!(f, "cannot undo: {msg}"),
        }
    }
}

impl std::error::Error for UndoError {}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Generate a compensating event that reverses the effect of `original`.
///
/// `prior_events` must be all events for the same item that occurred
/// **before** `original`, sorted in ascending chronological order. This
/// context is required for:
/// - `item.move` — to find the prior lifecycle state
/// - `item.update` — to find the prior field value
/// - `item.delete` — to reconstruct the original `item.create` data
///
/// The returned event has `event_hash` set to an empty string; callers are
/// responsible for calling [`crate::event::writer::write_event`] to compute
/// and fill the hash before appending to the shard.
///
/// # Errors
///
/// - [`UndoError::GrowOnly`] — for `item.comment`, `item.compact`,
///   `item.snapshot`, and `item.redact`.
/// - [`UndoError::NoPriorState`] — when prior event context is needed but
///   cannot be found (e.g. undo `item.delete` with no prior `item.create`).
pub fn compensating_event(
    original: &Event,
    prior_events: &[&Event],
    current_agent: &str,
    now: i64,
) -> Result<Event, UndoError> {
    let item_id = original.item_id.clone();
    let parents = vec![original.event_hash.clone()];

    let (event_type, data) = match &original.data {
        // item.create → item.delete
        EventData::Create(_) => (
            EventType::Delete,
            EventData::Delete(DeleteData {
                reason: Some(format!(
                    "undo create (compensating for {})",
                    original.event_hash
                )),
                extra: BTreeMap::new(),
            }),
        ),

        // item.update → item.update with previous field value
        EventData::Update(d) => {
            let prev = find_previous_field_value(prior_events, &d.field).ok_or_else(|| {
                UndoError::NoPriorState(format!(
                    "no prior value for field '{}' found in event history",
                    d.field
                ))
            })?;
            (
                EventType::Update,
                EventData::Update(UpdateData {
                    field: d.field.clone(),
                    value: prev,
                    extra: BTreeMap::new(),
                }),
            )
        }

        // item.move → item.move back to prior state
        EventData::Move(d) => {
            let prior_state = find_previous_state(prior_events).unwrap_or(State::Open);
            (
                EventType::Move,
                EventData::Move(MoveData {
                    state: prior_state,
                    reason: Some(format!(
                        "undo move from {} (compensating for {})",
                        d.state, original.event_hash
                    )),
                    extra: BTreeMap::new(),
                }),
            )
        }

        // item.assign(assign) → item.assign(unassign), and vice-versa
        EventData::Assign(d) => {
            let inverse = match d.action {
                AssignAction::Assign => AssignAction::Unassign,
                AssignAction::Unassign => AssignAction::Assign,
            };
            (
                EventType::Assign,
                EventData::Assign(AssignData {
                    agent: d.agent.clone(),
                    action: inverse,
                    extra: BTreeMap::new(),
                }),
            )
        }

        // item.link → item.unlink
        EventData::Link(d) => (
            EventType::Unlink,
            EventData::Unlink(UnlinkData {
                target: d.target.clone(),
                link_type: Some(d.link_type.clone()),
                extra: BTreeMap::new(),
            }),
        ),

        // item.unlink → item.link (restore with original link_type or "related_to")
        EventData::Unlink(d) => (
            EventType::Link,
            EventData::Link(LinkData {
                target: d.target.clone(),
                link_type: d
                    .link_type
                    .clone()
                    .unwrap_or_else(|| "related_to".to_string()),
                extra: BTreeMap::new(),
            }),
        ),

        // item.delete → item.create (reconstruct from history)
        EventData::Delete(_) => {
            let create_data = build_create_from_history(prior_events).ok_or_else(|| {
                UndoError::NoPriorState(
                    "no prior item.create event found to reconstruct item for undelete".to_string(),
                )
            })?;
            (EventType::Create, EventData::Create(create_data))
        }

        // Grow-only / irreversible event types
        EventData::Comment(_) => return Err(UndoError::GrowOnly(EventType::Comment)),
        EventData::Compact(_) => return Err(UndoError::GrowOnly(EventType::Compact)),
        EventData::Snapshot(_) => return Err(UndoError::GrowOnly(EventType::Snapshot)),
        EventData::Redact(_) => return Err(UndoError::GrowOnly(EventType::Redact)),
    };

    Ok(Event {
        wall_ts_us: now,
        agent: current_agent.to_string(),
        itc: "itc:AQ".to_string(),
        parents,
        event_type,
        item_id: ItemId::new_unchecked(item_id.as_str()),
        data,
        event_hash: String::new(), // filled by write_event
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Find the most recent lifecycle state from the events preceding `original`.
///
/// Scans backwards through prior events looking for `item.move` or
/// `item.create`. Falls back to `None` if none found (caller should default
/// to `State::Open`).
fn find_previous_state(prior_events: &[&Event]) -> Option<State> {
    for event in prior_events.iter().rev() {
        match &event.data {
            EventData::Move(d) => return Some(d.state),
            EventData::Create(_) => return Some(State::Open),
            _ => {}
        }
    }
    None
}

/// Find the most recent value for `field` from prior events.
///
/// Scans backwards through prior events looking for `item.update` targeting
/// the same field, then falls back to the initial value from `item.create`.
fn find_previous_field_value(prior_events: &[&Event], field: &str) -> Option<serde_json::Value> {
    for event in prior_events.iter().rev() {
        match &event.data {
            EventData::Update(d) if d.field == field => return Some(d.value.clone()),
            EventData::Create(d) => return initial_create_field_value(d, field),
            _ => {}
        }
    }
    None
}

/// Extract the initial value for `field` from an `item.create` payload.
fn initial_create_field_value(create: &CreateData, field: &str) -> Option<serde_json::Value> {
    match field {
        "title" => Some(serde_json::Value::String(create.title.clone())),
        "description" => create
            .description
            .as_ref()
            .map(|d| serde_json::Value::String(d.clone())),
        "size" => create
            .size
            .map(|s| serde_json::to_value(s).unwrap_or_else(|_| serde_json::Value::Null)),
        "urgency" => serde_json::to_value(create.urgency).ok(),
        "labels" => Some(serde_json::Value::Array(
            create
                .labels
                .iter()
                .map(|l| serde_json::Value::String(l.clone()))
                .collect(),
        )),
        "kind" => serde_json::to_value(create.kind).ok(),
        _ => None,
    }
}

/// Reconstruct `CreateData` from prior event history (for undo of delete).
///
/// Finds the original `item.create` event and applies any subsequent
/// `item.update` events to reflect the item's state just before deletion.
fn build_create_from_history(prior_events: &[&Event]) -> Option<CreateData> {
    // Find the original create data
    let create_idx = prior_events
        .iter()
        .position(|e| matches!(e.data, EventData::Create(_)))?;

    let mut create_data = match &prior_events[create_idx].data {
        EventData::Create(d) => d.clone(),
        _ => unreachable!(),
    };

    // Apply subsequent update events to reflect the latest field values
    for event in &prior_events[create_idx + 1..] {
        if let EventData::Update(u) = &event.data {
            apply_update_to_create(&mut create_data, &u.field, &u.value);
        }
    }

    Some(create_data)
}

/// Apply a single field update to a `CreateData` struct (for undo-delete reconstruction).
fn apply_update_to_create(create: &mut CreateData, field: &str, value: &serde_json::Value) {
    match field {
        "title" => {
            if let Some(s) = value.as_str() {
                create.title = s.to_string();
            }
        }
        "description" => {
            create.description = value.as_str().map(String::from);
        }
        "labels" => {
            if let Some(arr) = value.as_array() {
                create.labels = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
            }
        }
        "size" => {
            create.size = serde_json::from_value(value.clone()).ok();
        }
        "urgency" => {
            if let Ok(u) = serde_json::from_value(value.clone()) {
                create.urgency = u;
            }
        }
        "kind" => {
            if let Ok(k) = serde_json::from_value(value.clone()) {
                create.kind = k;
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::data::{CommentData, CreateData, MoveData};
    use crate::model::item::{Kind, State, Urgency};

    fn make_event(event_type: EventType, data: EventData, hash: &str) -> Event {
        Event {
            wall_ts_us: 1_000_000,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type,
            item_id: ItemId::new_unchecked("bn-test"),
            data,
            event_hash: hash.to_string(),
        }
    }

    fn minimal_create() -> Event {
        make_event(
            EventType::Create,
            EventData::Create(CreateData {
                title: "Test item".into(),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            "blake3:create001",
        )
    }

    #[test]
    fn undo_create_emits_delete() {
        let create_event = minimal_create();
        let result = compensating_event(&create_event, &[], "agent", 2_000_000);
        assert!(result.is_ok());
        let comp = result.unwrap();
        assert_eq!(comp.event_type, EventType::Delete);
        assert!(matches!(comp.data, EventData::Delete(_)));
        assert_eq!(comp.parents, vec!["blake3:create001"]);
        assert_eq!(comp.agent, "agent");
    }

    #[test]
    fn undo_assign_flips_to_unassign() {
        let assign_event = make_event(
            EventType::Assign,
            EventData::Assign(AssignData {
                agent: "alice".into(),
                action: AssignAction::Assign,
                extra: BTreeMap::new(),
            }),
            "blake3:assign001",
        );
        let result = compensating_event(&assign_event, &[], "undoer", 2_000_000);
        assert!(result.is_ok());
        let comp = result.unwrap();
        assert_eq!(comp.event_type, EventType::Assign);
        if let EventData::Assign(d) = &comp.data {
            assert_eq!(d.agent, "alice");
            assert_eq!(d.action, AssignAction::Unassign);
        } else {
            panic!("expected Assign data");
        }
    }

    #[test]
    fn undo_unassign_flips_to_assign() {
        let unassign_event = make_event(
            EventType::Assign,
            EventData::Assign(AssignData {
                agent: "bob".into(),
                action: AssignAction::Unassign,
                extra: BTreeMap::new(),
            }),
            "blake3:unassign001",
        );
        let result = compensating_event(&unassign_event, &[], "undoer", 2_000_000);
        assert!(result.is_ok());
        let comp = result.unwrap();
        if let EventData::Assign(d) = &comp.data {
            assert_eq!(d.action, AssignAction::Assign);
        } else {
            panic!("expected Assign data");
        }
    }

    #[test]
    fn undo_link_emits_unlink() {
        let link_event = make_event(
            EventType::Link,
            EventData::Link(LinkData {
                target: "bn-other".into(),
                link_type: "blocks".into(),
                extra: BTreeMap::new(),
            }),
            "blake3:link001",
        );
        let result = compensating_event(&link_event, &[], "undoer", 2_000_000);
        assert!(result.is_ok());
        let comp = result.unwrap();
        assert_eq!(comp.event_type, EventType::Unlink);
        if let EventData::Unlink(d) = &comp.data {
            assert_eq!(d.target, "bn-other");
            assert_eq!(d.link_type.as_deref(), Some("blocks"));
        } else {
            panic!("expected Unlink data");
        }
    }

    #[test]
    fn undo_unlink_emits_link() {
        let unlink_event = make_event(
            EventType::Unlink,
            EventData::Unlink(UnlinkData {
                target: "bn-other".into(),
                link_type: Some("blocks".into()),
                extra: BTreeMap::new(),
            }),
            "blake3:unlink001",
        );
        let result = compensating_event(&unlink_event, &[], "undoer", 2_000_000);
        assert!(result.is_ok());
        let comp = result.unwrap();
        assert_eq!(comp.event_type, EventType::Link);
        if let EventData::Link(d) = &comp.data {
            assert_eq!(d.target, "bn-other");
            assert_eq!(d.link_type, "blocks");
        } else {
            panic!("expected Link data");
        }
    }

    #[test]
    fn undo_move_returns_to_prior_state() {
        let create_event = minimal_create();
        let move_to_doing = make_event(
            EventType::Move,
            EventData::Move(MoveData {
                state: State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
            "blake3:move001",
        );
        // Now undo the move-to-doing; prior events include create
        let prior = vec![&create_event];
        let result = compensating_event(&move_to_doing, &prior, "undoer", 2_000_000);
        assert!(result.is_ok());
        let comp = result.unwrap();
        assert_eq!(comp.event_type, EventType::Move);
        if let EventData::Move(d) = &comp.data {
            assert_eq!(d.state, State::Open); // initial state from create
        } else {
            panic!("expected Move data");
        }
    }

    #[test]
    fn undo_move_falls_back_to_open_with_no_prior() {
        let move_event = make_event(
            EventType::Move,
            EventData::Move(MoveData {
                state: State::Done,
                reason: None,
                extra: BTreeMap::new(),
            }),
            "blake3:move002",
        );
        let result = compensating_event(&move_event, &[], "undoer", 2_000_000);
        assert!(result.is_ok());
        let comp = result.unwrap();
        if let EventData::Move(d) = &comp.data {
            assert_eq!(d.state, State::Open);
        } else {
            panic!("expected Move data");
        }
    }

    #[test]
    fn undo_update_finds_prior_value() {
        let create_event = minimal_create();
        let update_event = make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "title".into(),
                value: serde_json::Value::String("New title".into()),
                extra: BTreeMap::new(),
            }),
            "blake3:update001",
        );
        let prior = vec![&create_event];
        let result = compensating_event(&update_event, &prior, "undoer", 2_000_000);
        assert!(result.is_ok());
        let comp = result.unwrap();
        if let EventData::Update(d) = &comp.data {
            assert_eq!(d.field, "title");
            assert_eq!(d.value, serde_json::Value::String("Test item".into()));
        } else {
            panic!("expected Update data");
        }
    }

    #[test]
    fn undo_update_no_prior_returns_error() {
        let update_event = make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "title".into(),
                value: serde_json::Value::String("New".into()),
                extra: BTreeMap::new(),
            }),
            "blake3:update002",
        );
        let result = compensating_event(&update_event, &[], "undoer", 2_000_000);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), UndoError::NoPriorState(_)));
    }

    #[test]
    fn undo_delete_reconstructs_create() {
        let create_event = minimal_create();
        let delete_event = make_event(
            EventType::Delete,
            EventData::Delete(DeleteData {
                reason: Some("accident".into()),
                extra: BTreeMap::new(),
            }),
            "blake3:delete001",
        );
        let prior = vec![&create_event];
        let result = compensating_event(&delete_event, &prior, "undoer", 2_000_000);
        assert!(result.is_ok());
        let comp = result.unwrap();
        assert_eq!(comp.event_type, EventType::Create);
        if let EventData::Create(d) = &comp.data {
            assert_eq!(d.title, "Test item");
        } else {
            panic!("expected Create data");
        }
    }

    #[test]
    fn undo_delete_no_prior_create_returns_error() {
        let delete_event = make_event(
            EventType::Delete,
            EventData::Delete(DeleteData {
                reason: None,
                extra: BTreeMap::new(),
            }),
            "blake3:delete002",
        );
        let result = compensating_event(&delete_event, &[], "undoer", 2_000_000);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), UndoError::NoPriorState(_)));
    }

    #[test]
    fn undo_comment_is_grow_only() {
        let comment_event = make_event(
            EventType::Comment,
            EventData::Comment(CommentData {
                body: "A comment".into(),
                extra: BTreeMap::new(),
            }),
            "blake3:comment001",
        );
        let result = compensating_event(&comment_event, &[], "undoer", 2_000_000);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            UndoError::GrowOnly(EventType::Comment)
        ));
    }

    #[test]
    fn undo_redact_is_grow_only() {
        let redact_event = make_event(
            EventType::Redact,
            EventData::Redact(crate::event::data::RedactData {
                target_hash: "blake3:xyz".into(),
                reason: "test".into(),
                extra: BTreeMap::new(),
            }),
            "blake3:redact001",
        );
        let result = compensating_event(&redact_event, &[], "undoer", 2_000_000);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            UndoError::GrowOnly(EventType::Redact)
        ));
    }

    #[test]
    fn undo_snapshot_is_grow_only() {
        let snap_event = make_event(
            EventType::Snapshot,
            EventData::Snapshot(crate::event::data::SnapshotData {
                state: serde_json::json!({}),
                extra: BTreeMap::new(),
            }),
            "blake3:snap001",
        );
        let result = compensating_event(&snap_event, &[], "undoer", 2_000_000);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            UndoError::GrowOnly(EventType::Snapshot)
        ));
    }

    #[test]
    fn compensating_event_references_original_in_parents() {
        let create_event = minimal_create();
        let comp = compensating_event(&create_event, &[], "undoer", 2_000_000).unwrap();
        assert_eq!(comp.parents, vec!["blake3:create001"]);
    }

    #[test]
    fn compensating_event_uses_current_agent_and_timestamp() {
        let create_event = minimal_create();
        let comp = compensating_event(&create_event, &[], "new-agent", 9_999_999).unwrap();
        assert_eq!(comp.agent, "new-agent");
        assert_eq!(comp.wall_ts_us, 9_999_999);
    }

    #[test]
    fn undo_update_uses_most_recent_prior_value() {
        let create_event = minimal_create();
        let update1 = make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "title".into(),
                value: serde_json::Value::String("Second title".into()),
                extra: BTreeMap::new(),
            }),
            "blake3:upd1",
        );
        let update2 = make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "title".into(),
                value: serde_json::Value::String("Third title".into()),
                extra: BTreeMap::new(),
            }),
            "blake3:upd2",
        );
        // Undo update2; prior = [create, update1]
        let prior = vec![&create_event, &update1];
        let result = compensating_event(&update2, &prior, "undoer", 2_000_000).unwrap();
        if let EventData::Update(d) = &result.data {
            assert_eq!(d.value, serde_json::Value::String("Second title".into()));
        } else {
            panic!("expected Update");
        }
    }
}
