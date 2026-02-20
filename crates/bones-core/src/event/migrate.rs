//! Event format migration helpers.
//!
//! This module provides version-specific transforms to upgrade events parsed
//! from older shard formats into the current in-memory [`Event`] shape.

use anyhow::{Result, anyhow};

use crate::event::{CURRENT_VERSION, Event};

/// Raw event representation parsed from an older format version.
///
/// Today v1 and current share the same logical event schema, so this is an
/// alias. Future format versions may replace this with a distinct type.
pub type RawEvent = Event;

/// Apply version-specific transforms to an event parsed from an older format.
///
/// Returns the event upgraded to the current-version representation.
pub fn migrate_event(event: RawEvent, from_version: u32) -> Result<Event> {
    match from_version {
        1 => migrate_v1_to_current(event),
        v if v == CURRENT_VERSION => Ok(event),
        v => Err(anyhow!("unknown format version {v}")),
    }
}

/// V1 -> current migration.
///
/// Currently a no-op passthrough because v1 is the active format.
fn migrate_v1_to_current(event: RawEvent) -> Result<Event> {
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{CreateData, EventData, EventType};
    use crate::model::item::{Kind, Size, Urgency};
    use crate::model::item_id::ItemId;
    use std::collections::BTreeMap;

    fn sample_event() -> Event {
        Event {
            wall_ts_us: 1_700_000_000_000_000,
            agent: "agent-1".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-a1b"),
            data: EventData::Create(CreateData {
                title: "Sample".into(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: vec!["compatibility".into()],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: "blake3:deadbeef".into(),
        }
    }

    #[test]
    fn migrate_current_version_passthrough() {
        let event = sample_event();
        let migrated = migrate_event(event.clone(), CURRENT_VERSION).expect("migrate");
        assert_eq!(migrated, event);
    }

    #[test]
    fn migrate_v1_passthrough() {
        let event = sample_event();
        let migrated = migrate_event(event.clone(), 1).expect("migrate");
        assert_eq!(migrated, event);
    }

    #[test]
    fn migrate_unknown_version_errors() {
        let event = sample_event();
        let err = migrate_event(event, CURRENT_VERSION + 1).expect_err("must fail");
        assert!(err.to_string().contains("unknown format version"));
    }
}
