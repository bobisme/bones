//! Per-column type definitions for the binary event cache.
//!
//! A `CacheColumns` struct holds one column of each type for a batch of
//! events. It is used as the intermediate representation between the
//! [`super::CacheHeader`] metadata and the encoded column bytes.
//!
//! See `docs/binary-cache-format.md` for the byte-level layout.

use crate::event::{Event, EventType};
use crate::model::item_id::ItemId;

/// The column index constants for this format version.
pub const COL_TIMESTAMPS: usize = 0;
pub const COL_AGENTS: usize = 1;
pub const COL_EVENT_TYPES: usize = 2;
pub const COL_ITEM_IDS: usize = 3;
pub const COL_PARENTS: usize = 4;
pub const COL_ITC: usize = 5;
pub const COL_VALUES: usize = 6;

/// Total number of columns in the cache format.
pub const COLUMN_COUNT: usize = 7;

/// All event columns for a batch of events, decomposed by type.
///
/// Each field holds one column of data (in event order). The `i`-th element
/// of each column corresponds to the `i`-th event in the batch.
///
/// # Construction
///
/// Build with [`CacheColumns::from_events`]. Consume with
/// [`CacheColumns::to_column_slices`] for encoding.
#[derive(Debug, Clone, Default)]
pub struct CacheColumns {
    /// Wall-clock timestamps in microseconds since Unix epoch.
    pub timestamps: Vec<i64>,

    /// Agent identifier strings.
    pub agents: Vec<String>,

    /// Event type discriminants.
    pub event_types: Vec<EventType>,

    /// Item ID strings.
    pub item_ids: Vec<String>,

    /// Parent hash lists — each element is a comma-joined string of parent
    /// hashes (empty string for root events).
    pub parents: Vec<String>,

    /// ITC stamp strings.
    pub itc: Vec<String>,

    /// JSON-serialised event payload strings.
    pub values: Vec<String>,
}

impl CacheColumns {
    /// Decompose a slice of events into parallel columns.
    ///
    /// # Errors
    ///
    /// Returns an error if any event's data fails to serialise to JSON.
    pub fn from_events(events: &[Event]) -> Result<Self, serde_json::Error> {
        let n = events.len();
        let mut cols = Self {
            timestamps: Vec::with_capacity(n),
            agents: Vec::with_capacity(n),
            event_types: Vec::with_capacity(n),
            item_ids: Vec::with_capacity(n),
            parents: Vec::with_capacity(n),
            itc: Vec::with_capacity(n),
            values: Vec::with_capacity(n),
        };

        for event in events {
            cols.timestamps.push(event.wall_ts_us);
            cols.agents.push(event.agent.clone());
            cols.event_types.push(event.event_type);
            cols.item_ids.push(event.item_id.as_str().to_string());
            cols.parents.push(event.parents.join(","));
            cols.itc.push(event.itc.clone());
            cols.values.push(serde_json::to_string(&event.data)?);
        }

        Ok(cols)
    }

    /// Reconstruct events from parallel column data.
    ///
    /// All columns must have the same length. Parent hashes are split on
    /// commas; the empty string yields an empty parent list (root event).
    ///
    /// # Errors
    ///
    /// Returns an error string if:
    /// - Column lengths differ.
    /// - An item ID string is not a valid `ItemId`.
    /// - A value JSON string cannot be parsed as the event's data payload.
    pub fn into_events(self) -> Result<Vec<Event>, String> {
        let n = self.timestamps.len();
        let check_len = |name: &str, len: usize| {
            if len == n {
                Ok(())
            } else {
                Err(format!("column '{name}' length {len} != timestamps length {n}"))
            }
        };
        check_len("agents", self.agents.len())?;
        check_len("event_types", self.event_types.len())?;
        check_len("item_ids", self.item_ids.len())?;
        check_len("parents", self.parents.len())?;
        check_len("itc", self.itc.len())?;
        check_len("values", self.values.len())?;

        let mut events = Vec::with_capacity(n);

        for i in 0..n {
            let event_type = self.event_types[i];
            let item_id = ItemId::parse(&self.item_ids[i])
                .map_err(|e| format!("row {i} invalid item_id: {e}"))?;

            let parents: Vec<String> = if self.parents[i].is_empty() {
                vec![]
            } else {
                self.parents[i].split(',').map(str::to_string).collect()
            };

            let data = crate::event::EventData::deserialize_for(event_type, &self.values[i])
                .map_err(|e| format!("row {i} data parse error: {e}"))?;

            events.push(Event {
                wall_ts_us: self.timestamps[i],
                agent: self.agents[i].clone(),
                itc: self.itc[i].clone(),
                parents,
                event_type,
                item_id,
                data,
                // event_hash is not stored in the cache columns (derived from
                // content). Callers that need the hash must recompute it from
                // the TSJSON writer or preserve it separately.
                event_hash: String::new(),
            });
        }

        Ok(events)
    }

    /// Return the number of events (rows) in this column set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.timestamps.len()
    }

    /// Return `true` if there are no events in this column set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.timestamps.is_empty()
    }
}

/// A single row extracted from the column arrays.
///
/// Useful for inspecting individual events without rebuilding a full [`Event`].
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnRow {
    /// Wall-clock timestamp.
    pub wall_ts_us: i64,
    /// Agent identifier.
    pub agent: String,
    /// Event type discriminant.
    pub event_type: EventType,
    /// Item ID string.
    pub item_id: String,
    /// Comma-joined parent hashes (empty for root events).
    pub parents: String,
    /// ITC stamp.
    pub itc: String,
    /// JSON payload string.
    pub value: String,
}

impl CacheColumns {
    /// Extract a single row by index.
    ///
    /// Returns `None` if `index >= self.len()`.
    #[must_use]
    pub fn row(&self, index: usize) -> Option<ColumnRow> {
        if index >= self.len() {
            return None;
        }
        Some(ColumnRow {
            wall_ts_us: self.timestamps[index],
            agent: self.agents[index].clone(),
            event_type: self.event_types[index],
            item_id: self.item_ids[index].clone(),
            parents: self.parents[index].clone(),
            itc: self.itc[index].clone(),
            value: self.values[index].clone(),
        })
    }

    /// Return only the event types column (useful for count-by-type queries).
    #[must_use]
    pub fn event_types(&self) -> &[EventType] {
        &self.event_types
    }

    /// Return only the timestamps column (useful for range queries).
    #[must_use]
    pub fn timestamps(&self) -> &[i64] {
        &self.timestamps
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventData, EventType};
    use crate::event::data::{CreateData, MoveData, CommentData};
    use crate::model::item::{Kind, State, Urgency};
    use crate::model::item_id::ItemId;
    use std::collections::BTreeMap;

    fn make_create_event(ts: i64, agent: &str, item: &str, title: &str) -> Event {
        Event {
            wall_ts_us: ts,
            agent: agent.to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item),
            data: EventData::Create(CreateData {
                title: title.to_string(),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:{ts:016x}"),
        }
    }

    fn make_move_event(ts: i64, agent: &str, item: &str, parent_hash: &str) -> Event {
        Event {
            wall_ts_us: ts,
            agent: agent.to_string(),
            itc: "itc:AQ.1".to_string(),
            parents: vec![parent_hash.to_string()],
            event_type: EventType::Move,
            item_id: ItemId::new_unchecked(item),
            data: EventData::Move(MoveData {
                state: State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:move{ts:012x}"),
        }
    }

    fn make_comment_event(ts: i64, agent: &str, item: &str, body: &str) -> Event {
        Event {
            wall_ts_us: ts,
            agent: agent.to_string(),
            itc: "itc:Bg".to_string(),
            parents: vec![],
            event_type: EventType::Comment,
            item_id: ItemId::new_unchecked(item),
            data: EventData::Comment(CommentData {
                body: body.to_string(),
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:cmt{ts:013x}"),
        }
    }

    // === Column count constants ==========================================

    #[test]
    fn column_count_is_seven() {
        assert_eq!(COLUMN_COUNT, 7);
    }

    #[test]
    fn column_indices_are_distinct() {
        let indices = [
            COL_TIMESTAMPS,
            COL_AGENTS,
            COL_EVENT_TYPES,
            COL_ITEM_IDS,
            COL_PARENTS,
            COL_ITC,
            COL_VALUES,
        ];
        let set: std::collections::HashSet<_> = indices.iter().copied().collect();
        assert_eq!(set.len(), COLUMN_COUNT, "column indices must be distinct");
    }

    // === CacheColumns::from_events ========================================

    #[test]
    fn from_events_empty() {
        let cols = CacheColumns::from_events(&[]).unwrap();
        assert!(cols.is_empty());
        assert_eq!(cols.len(), 0);
    }

    #[test]
    fn from_events_single_create() {
        let event = make_create_event(1_700_000_000_000, "agent-a", "bn-a7x", "Do a thing");
        let cols = CacheColumns::from_events(std::slice::from_ref(&event)).unwrap();
        assert_eq!(cols.len(), 1);
        assert_eq!(cols.timestamps[0], 1_700_000_000_000);
        assert_eq!(cols.agents[0], "agent-a");
        assert_eq!(cols.event_types[0], EventType::Create);
        assert_eq!(cols.item_ids[0], "bn-a7x");
        assert_eq!(cols.parents[0], "");
        assert_eq!(cols.itc[0], "itc:AQ");
        assert!(cols.values[0].contains("Do a thing"));
    }

    #[test]
    fn from_events_parents_joined_with_comma() {
        let mut event = make_create_event(1_000, "a", "bn-a7x", "T");
        event.parents = vec!["blake3:aaa".to_string(), "blake3:bbb".to_string()];
        let cols = CacheColumns::from_events(std::slice::from_ref(&event)).unwrap();
        assert_eq!(cols.parents[0], "blake3:aaa,blake3:bbb");
    }

    #[test]
    fn from_events_multiple() {
        let events = vec![
            make_create_event(1_000, "alice", "bn-a7x", "Task A"),
            make_move_event(2_000, "bob", "bn-a7x", "blake3:abc"),
            make_comment_event(3_000, "alice", "bn-a7x", "Look at this"),
        ];
        let cols = CacheColumns::from_events(&events).unwrap();
        assert_eq!(cols.len(), 3);
        assert_eq!(cols.timestamps, vec![1_000, 2_000, 3_000]);
        assert_eq!(cols.agents, vec!["alice", "bob", "alice"]);
        assert_eq!(
            cols.event_types,
            vec![EventType::Create, EventType::Move, EventType::Comment]
        );
        assert_eq!(cols.item_ids, vec!["bn-a7x", "bn-a7x", "bn-a7x"]);
        assert_eq!(cols.parents[0], "");
        assert_eq!(cols.parents[1], "blake3:abc");
        assert_eq!(cols.parents[2], "");
    }

    // === CacheColumns::into_events ========================================

    #[test]
    fn into_events_empty() {
        let cols = CacheColumns::default();
        let events = cols.into_events().unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn into_events_roundtrip_single() {
        let event = make_create_event(1_700_000_000_000, "agent-a", "bn-a7x", "Do a thing");
        let cols = CacheColumns::from_events(std::slice::from_ref(&event)).unwrap();
        let mut reconstructed = cols.into_events().unwrap();
        assert_eq!(reconstructed.len(), 1);
        let rec = &mut reconstructed[0];
        // event_hash is not stored in cache columns — zero it for comparison
        rec.event_hash = event.event_hash.clone();
        assert_eq!(*rec, event);
    }

    #[test]
    fn into_events_roundtrip_multiple() {
        let events = vec![
            make_create_event(1_000, "alice", "bn-a7x", "Task A"),
            make_move_event(2_000, "bob", "bn-a7x", "blake3:abc"),
            make_comment_event(3_000, "alice", "bn-a7x", "Look at this"),
        ];
        let cols = CacheColumns::from_events(&events).unwrap();
        let mut reconstructed = cols.into_events().unwrap();
        assert_eq!(reconstructed.len(), events.len());
        for (i, (rec, orig)) in reconstructed.iter_mut().zip(events.iter()).enumerate() {
            rec.event_hash = orig.event_hash.clone();
            assert_eq!(rec, orig, "mismatch at row {i}");
        }
    }

    #[test]
    fn into_events_empty_parents_becomes_vec() {
        let event = make_create_event(1_000, "alice", "bn-a7x", "Task");
        let cols = CacheColumns::from_events(std::slice::from_ref(&event)).unwrap();
        let reconstructed = cols.into_events().unwrap();
        assert!(reconstructed[0].parents.is_empty());
    }

    #[test]
    fn into_events_multi_parent() {
        let mut event = make_create_event(1_000, "alice", "bn-a7x", "Task");
        event.parents = vec!["blake3:aaa".to_string(), "blake3:bbb".to_string()];
        let cols = CacheColumns::from_events(std::slice::from_ref(&event)).unwrap();
        let reconstructed = cols.into_events().unwrap();
        assert_eq!(
            reconstructed[0].parents,
            vec!["blake3:aaa".to_string(), "blake3:bbb".to_string()]
        );
    }

    #[test]
    fn into_events_column_length_mismatch_is_error() {
        let mut cols = CacheColumns::default();
        cols.timestamps = vec![1, 2];
        cols.agents = vec!["a".to_string()]; // wrong length
        cols.event_types = vec![EventType::Create, EventType::Create];
        cols.item_ids = vec!["bn-a7x".to_string(), "bn-a7x".to_string()];
        cols.parents = vec![String::new(), String::new()];
        cols.itc = vec!["itc:AQ".to_string(), "itc:AQ".to_string()];
        cols.values = vec![
            r#"{"title":"T","kind":"task"}"#.to_string(),
            r#"{"title":"T","kind":"task"}"#.to_string(),
        ];
        assert!(cols.into_events().is_err());
    }

    // === Row accessor =====================================================

    #[test]
    fn row_returns_correct_fields() {
        let events = vec![
            make_create_event(1_000, "alice", "bn-a7x", "Task A"),
            make_move_event(2_000, "bob", "bn-b8y", "blake3:ref"),
        ];
        let cols = CacheColumns::from_events(&events).unwrap();
        let row = cols.row(1).unwrap();
        assert_eq!(row.wall_ts_us, 2_000);
        assert_eq!(row.agent, "bob");
        assert_eq!(row.event_type, EventType::Move);
        assert_eq!(row.item_id, "bn-b8y");
        assert_eq!(row.parents, "blake3:ref");
    }

    #[test]
    fn row_out_of_bounds_returns_none() {
        let cols = CacheColumns::default();
        assert!(cols.row(0).is_none());
    }

    // === Column projections ==============================================

    #[test]
    fn event_types_projection() {
        let events = vec![
            make_create_event(1_000, "alice", "bn-a7x", "Task"),
            make_move_event(2_000, "bob", "bn-a7x", "blake3:abc"),
        ];
        let cols = CacheColumns::from_events(&events).unwrap();
        assert_eq!(
            cols.event_types(),
            &[EventType::Create, EventType::Move]
        );
    }

    #[test]
    fn timestamps_projection() {
        let events = vec![
            make_create_event(100, "a", "bn-a7x", "T"),
            make_create_event(200, "a", "bn-b8y", "U"),
        ];
        let cols = CacheColumns::from_events(&events).unwrap();
        assert_eq!(cols.timestamps(), &[100, 200]);
    }
}
