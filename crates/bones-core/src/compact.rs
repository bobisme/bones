//! Lattice-based log compaction for the bones event log.
//!
//! Over time the event log grows. Compaction replaces event sequences for
//! completed items with a single `item.snapshot` event — the semilattice join
//! of all events for that item. Compaction is coordination-free: each replica
//! compacts independently and converges to identical state.
//!
//! # Snapshot Semantics
//!
//! **Snapshots are lattice elements, not regular updates.**
//!
//! - For every LWW field the snapshot carries the winning `(wall_ts,
//!   agent_id, event_hash, value)` tuple — not just the value. Snapshots
//!   written before bn-1dy8 also carry an ITC `stamp`, which is ignored.
//! - For OR-Sets and G-Sets the snapshot carries the full set state.
//! - Applying a snapshot uses `merge(state, snapshot_state)` — a field-wise
//!   lattice join, *not* "overwrite with snapshot clock".
//!
//! This is critical: if a snapshot used a single event clock for all fields,
//! it would incorrectly dominate concurrent events that were not observed at
//! compaction time, violating semantic preservation.
//!
//! # Redaction Interaction
//!
//! Snapshots check the redaction set before including field values. Compaction
//! must never reintroduce redacted content.
//!
//! # Audit Metadata
//!
//! Each snapshot carries `_compacted_from` (count of original events),
//! `_earliest_ts`, and `_latest_ts` timestamps for audit trail.

use std::collections::BTreeMap;
use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::crdt::OrSet;
use crate::crdt::gset::GSet;
use crate::crdt::item_state::{LinkKey, WorkItemState};
use crate::crdt::lww::LwwRegister;
use crate::crdt::state::{EpochPhaseState, Phase};
use crate::event::Event;
use crate::event::data::{EventData, SnapshotData};
use crate::event::types::EventType;
use crate::event::writer;
use crate::model::item::{Kind, Size, Urgency};
use crate::model::item_id::ItemId;

// ---------------------------------------------------------------------------
// Snapshot payload — per-field CRDT clocks
// ---------------------------------------------------------------------------

/// Serializable representation of a single LWW register with its clock.
///
/// Preserves the full tie-breaking chain for correct lattice merge. Unknown
/// keys are ignored, so a legacy `stamp` key still deserializes (bn-1dy8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LwwSnapshot<T> {
    pub value: T,
    pub wall_ts: u64,
    pub agent_id: String,
    pub event_hash: String,
}

impl<T: Clone> From<&LwwRegister<T>> for LwwSnapshot<T> {
    fn from(reg: &LwwRegister<T>) -> Self {
        Self {
            value: reg.value.clone(),
            wall_ts: reg.wall_ts,
            agent_id: reg.agent_id.clone(),
            event_hash: reg.event_hash.clone(),
        }
    }
}

impl<T: Clone> From<&LwwSnapshot<T>> for LwwRegister<T> {
    fn from(snap: &LwwSnapshot<T>) -> Self {
        Self {
            value: snap.value.clone(),
            wall_ts: snap.wall_ts,
            agent_id: snap.agent_id.clone(),
            event_hash: snap.event_hash.clone(),
        }
    }
}

impl LwwSnapshot<String> {
    /// `true` for a register that no event wrote: empty value, zero clock.
    const fn is_unwritten(&self) -> bool {
        self.value.is_empty()
            && self.wall_ts == 0
            && self.agent_id.is_empty()
            && self.event_hash.is_empty()
    }
}

impl Default for LwwSnapshot<String> {
    fn default() -> Self {
        Self {
            value: String::new(),
            wall_ts: 0,
            agent_id: String::new(),
            event_hash: String::new(),
        }
    }
}

/// `true` for an OR-Set with no adds and no removes.
fn orset_is_blank<T: std::hash::Hash + Eq>(set: &OrSet<T>) -> bool {
    set.elements.is_empty() && set.tombstone.is_empty()
}

/// Full snapshot payload encoding every CRDT field with its clock metadata.
///
/// This is the `state` JSON inside an `item.snapshot` event's [`SnapshotData`].
/// It preserves enough information for field-wise lattice join on merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotPayload {
    /// Item identifier.
    pub item_id: String,

    // -- LWW scalar fields with per-field clocks --
    pub title: LwwSnapshot<String>,
    pub description: LwwSnapshot<String>,
    pub kind: LwwSnapshot<Kind>,
    pub size: LwwSnapshot<Option<Size>>,
    pub urgency: LwwSnapshot<Urgency>,
    pub parent: LwwSnapshot<String>,
    pub deleted: LwwSnapshot<bool>,

    // -- Epoch+Phase lifecycle state --
    pub state: EpochPhaseState,

    // -- OR-Set fields (full state with elements and tombstones) --
    pub assignees: OrSet<String>,
    pub labels: OrSet<String>,
    pub blocked_by: OrSet<String>,
    pub related_to: OrSet<String>,
    /// Links with their raw link type (bn-t37g). Left out when blank, so a
    /// snapshot of an item without links keeps its bytes. Snapshots written
    /// before bn-t37g lack it: see [`WorkItemState::from_snapshot_payload`].
    #[serde(default, skip_serializing_if = "orset_is_blank")]
    pub links: OrSet<LinkKey>,

    // -- G-Set (grow-only comment hashes) --
    pub comments: GSet<String>,

    /// Summary from the latest compact event (bn-t37g). Left out when no
    /// compact event wrote it. Older snapshots carry the summary in
    /// `description` instead.
    #[serde(default, skip_serializing_if = "LwwSnapshot::is_unwritten")]
    pub compact_summary: LwwSnapshot<String>,

    // -- Timestamps --
    pub created_at: u64,
    pub updated_at: u64,

    // -- Audit metadata --
    /// Number of original events that were compacted into this snapshot.
    #[serde(rename = "_compacted_from")]
    pub compacted_from: usize,
    /// Wall-clock timestamp (microseconds) of the earliest original event.
    #[serde(rename = "_earliest_ts")]
    pub earliest_ts: i64,
    /// Wall-clock timestamp (microseconds) of the latest original event.
    #[serde(rename = "_latest_ts")]
    pub latest_ts: i64,
    /// Snapshot format: [`SNAPSHOT_FORMAT`] for snapshots this bn writes, 0
    /// (the default) for snapshots written before bn-t37g.
    #[serde(rename = "_format", default)]
    pub format: u32,
}

/// Format of the snapshots this bn writes.
///
/// Format 2 (bn-t37g) matches the projection: raw link types, a separate
/// compact summary, and creates that write absent fields. Format 3
/// (bn-18fs) reads update values by the rules of `model::field_value`
/// (malformed values and blank members are no write). `bn compact`
/// replaces a snapshot of an older format even when its events did not
/// change.
///
/// The projection merges the fields of a snapshot of this format or newer
/// only (`db::project`). An older snapshot can hold values that this bn
/// never writes from the same events, so it stays JSON only there. Its
/// source events stay in the log, so nothing is lost.
pub const SNAPSHOT_FORMAT: u32 = 3;

// ---------------------------------------------------------------------------
// WorkItemState ↔ SnapshotPayload conversion
// ---------------------------------------------------------------------------

impl WorkItemState {
    /// Serialize the full CRDT aggregate to a [`SnapshotPayload`].
    ///
    /// This captures per-field clock metadata needed for correct lattice
    /// merge when the snapshot is applied on another replica.
    #[must_use]
    pub fn to_snapshot_payload(
        &self,
        item_id: &str,
        compacted_from: usize,
        earliest_ts: i64,
        latest_ts: i64,
    ) -> SnapshotPayload {
        SnapshotPayload {
            item_id: item_id.to_string(),
            title: LwwSnapshot::from(&self.title),
            description: LwwSnapshot::from(&self.description),
            kind: LwwSnapshot::from(&self.kind),
            size: LwwSnapshot::from(&self.size),
            urgency: LwwSnapshot::from(&self.urgency),
            parent: LwwSnapshot::from(&self.parent),
            deleted: LwwSnapshot::from(&self.deleted),
            state: self.state.clone(),
            assignees: self.assignees.clone(),
            labels: self.labels.clone(),
            blocked_by: self.blocked_by.clone(),
            related_to: self.related_to.clone(),
            links: self.links.clone(),
            comments: self.comments.clone(),
            compact_summary: LwwSnapshot::from(&self.compact_summary),
            created_at: self.created_at,
            updated_at: self.updated_at,
            compacted_from,
            earliest_ts,
            latest_ts,
            format: SNAPSHOT_FORMAT,
        }
    }

    /// Reconstruct a [`WorkItemState`] from a [`SnapshotPayload`].
    ///
    /// The resulting state can be merged with other states via the normal
    /// `WorkItemState::merge` — this is how snapshots participate in the
    /// lattice.
    ///
    /// A snapshot written before bn-t37g has no `links`. Its `blocked_by`
    /// and `related_to` members then become `blocks` and `related_to` links,
    /// with the same tags.
    #[must_use]
    pub fn from_snapshot_payload(payload: &SnapshotPayload) -> Self {
        let links = if orset_is_blank(&payload.links) {
            legacy_links(&payload.blocked_by, &payload.related_to)
        } else {
            payload.links.clone()
        };
        let mut state = Self {
            title: LwwRegister::from(&payload.title),
            description: LwwRegister::from(&payload.description),
            kind: LwwRegister::from(&payload.kind),
            state: payload.state.clone(),
            size: LwwRegister::from(&payload.size),
            urgency: LwwRegister::from(&payload.urgency),
            parent: LwwRegister::from(&payload.parent),
            assignees: payload.assignees.clone(),
            labels: payload.labels.clone(),
            blocked_by: payload.blocked_by.clone(),
            related_to: payload.related_to.clone(),
            links,
            comments: payload.comments.clone(),
            deleted: LwwRegister::from(&payload.deleted),
            compact_summary: LwwRegister::from(&payload.compact_summary),
            created_at: payload.created_at,
            updated_at: payload.updated_at,
        };
        // The stored views are for older readers. Derive them from links.
        state.derive_link_views();
        state
    }
}

/// Links of a snapshot written before bn-t37g, from its link views.
///
/// This is lossy. The old format kept only the views, so the raw link type
/// is unknown: a `blocked_by` member becomes a `(target, "blocks")` link
/// and a `related_to` member a `(target, "related_to")` link, with the same
/// tags. The views derived from these links equal the stored views. But a
/// merge with state replayed from the events can hold both the guessed type
/// and the real one (e.g. `blocks` and `blocked_by`) for one target.
fn legacy_links(blocked_by: &OrSet<String>, related_to: &OrSet<String>) -> OrSet<LinkKey> {
    let mut links = OrSet::new();
    for (view, link_type) in [(blocked_by, "blocks"), (related_to, "related_to")] {
        let key = |target: &String| LinkKey {
            target: target.clone(),
            link_type: link_type.to_string(),
        };
        links.elements.extend(
            view.elements
                .iter()
                .map(|(target, tag)| (key(target), tag.clone())),
        );
        links.tombstone.extend(
            view.tombstone
                .iter()
                .map(|(target, tag)| (key(target), tag.clone())),
        );
    }
    links
}

// ---------------------------------------------------------------------------
// CompactionReport
// ---------------------------------------------------------------------------

/// Report from compacting eligible items.
#[derive(Debug, Clone, Serialize)]
pub struct CompactionReport {
    /// Number of items compacted.
    pub items_compacted: usize,
    /// Number of events replaced by snapshots.
    pub events_replaced: usize,
    /// Number of snapshot events created (one per compacted item).
    pub snapshots_created: usize,
    /// Items skipped (not eligible).
    pub items_skipped: usize,
}

// ---------------------------------------------------------------------------
// Core compaction functions
// ---------------------------------------------------------------------------

/// The events a snapshot of an item covers: each event of the item except
/// snapshots, once per event hash, in their first log position.
///
/// A duplicate line (git union merges can make them) counts once, so every
/// replica makes the same snapshot. An earlier snapshot is not a source: it
/// only summarises events that are still in the log (bn-t37g).
#[must_use]
pub fn compaction_sources(events: &[Event]) -> Vec<Event> {
    let mut seen = HashSet::new();
    events
        .iter()
        .filter(|e| e.event_type != EventType::Snapshot && seen.insert(e.event_hash.as_str()))
        .cloned()
        .collect()
}

/// `true` when `events` already hold a snapshot of exactly `sources`, in
/// the current [`SNAPSHOT_FORMAT`].
///
/// A snapshot's parents are the sorted hashes of its sources, so a second
/// `bn compact` with no new events for the item makes no new snapshot. A
/// snapshot of an older format does not count, so it gets replaced once.
fn has_snapshot_of(events: &[Event], sources: &[Event]) -> bool {
    let mut hashes: Vec<&str> = sources.iter().map(|e| e.event_hash.as_str()).collect();
    hashes.sort_unstable();
    events.iter().any(|e| {
        e.event_type == EventType::Snapshot
            && e.parents.len() == hashes.len()
            && e.parents
                .iter()
                .map(String::as_str)
                .eq(hashes.iter().copied())
            && extract_snapshot_payload(e).is_ok_and(|p| p.format >= SNAPSHOT_FORMAT)
    })
}

/// Compact all events for a single item into one `item.snapshot` event.
///
/// Replays the [`compaction_sources`] of `events` to produce final CRDT
/// state, then creates a single snapshot event encoding the full
/// `WorkItemState` with per-field clocks.
///
/// # Arguments
///
/// * `item_id` — The work item identifier.
/// * `events` — All events for this item, in causal/chronological order.
/// * `agent` — Agent identifier for the snapshot event.
/// * `redacted_hashes` — Set of event hashes that have been redacted.
///   If any source events are redacted, the compaction is skipped (returns None).
///
/// # Returns
///
/// `Some(Event)` with the snapshot, or `None` if the item has redacted events
/// (compaction must not reintroduce redacted content).
///
/// # Panics
///
/// Panics if the `SnapshotPayload` cannot be serialized to JSON. This should
/// never happen with valid CRDT state.
#[must_use]
pub fn compact_item<S: ::std::hash::BuildHasher>(
    item_id: &str,
    events: &[Event],
    agent: &str,
    redacted_hashes: &HashSet<String, S>,
) -> Option<Event> {
    // Check for redacted events — refuse to compact if any source events
    // are redacted, since the snapshot would reintroduce the content.
    for event in events {
        if redacted_hashes.contains(&event.event_hash) {
            return None;
        }
    }

    let events = compaction_sources(events);
    if events.is_empty() {
        return None;
    }

    // Replay all events to build the final CRDT state.
    let state = WorkItemState::from_events(&events);

    // Compute audit metadata.
    // order_ts, so the snapshot (latest + 1) sorts after every source event
    // even when some carry pre-epoch timestamps (bn-1ed2).
    let earliest_ts = events.iter().map(Event::order_ts).min().unwrap_or(0);
    let latest_ts = events.iter().map(Event::order_ts).max().unwrap_or(0);

    // Build the snapshot payload.
    let payload = state.to_snapshot_payload(item_id, events.len(), earliest_ts, latest_ts);

    // Serialize payload to JSON value for SnapshotData.
    let state_json =
        serde_json::to_value(&payload).expect("SnapshotPayload should always serialize");

    // Build the snapshot event.
    // Use the latest event's timestamp + 1µs for the snapshot,
    // and reference all leaf events as parents.
    let snapshot_ts = latest_ts + 1;
    let parents: Vec<String> = events
        .iter()
        .map(|e| e.event_hash.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let mut sorted_parents = parents;
    sorted_parents.sort();

    // The canonically latest event, not the last in log order, so the
    // snapshot is the same event on every replica (bn-1ed2).
    let itc = events
        .iter()
        .max_by(|a, b| {
            (a.order_ts(), &a.agent, &a.event_hash).cmp(&(b.order_ts(), &b.agent, &b.event_hash))
        })
        .map_or_else(
            || crate::event::ITC_PLACEHOLDER.to_string(),
            |e| e.itc.clone(),
        );

    let item_id_parsed = ItemId::new_unchecked(item_id);

    let mut snapshot_event = Event {
        wall_ts_us: snapshot_ts,
        agent: agent.to_string(),
        itc,
        parents: sorted_parents,
        event_type: EventType::Snapshot,
        item_id: item_id_parsed,
        data: EventData::Snapshot(SnapshotData {
            state: state_json,
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(), // Will be computed
    };

    // Compute and set the event hash.
    snapshot_event.event_hash =
        writer::compute_event_hash(&snapshot_event).expect("snapshot event should always hash");

    Some(snapshot_event)
}

/// Check if a work item is eligible for compaction.
///
/// An item is eligible if:
/// - Its lifecycle state is Done or Archived
/// - It has been in that terminal state for at least `min_age_days`
/// - It is not soft-deleted (deleted items are already lightweight)
///
/// # Arguments
///
/// * `state` — The current CRDT state of the item.
/// * `min_age_days` — Minimum days in done/archived before compaction.
/// * `now_us` — Current wall-clock time in microseconds.
#[must_use]
pub fn is_eligible(state: &WorkItemState, min_age_days: u32, now_us: i64) -> bool {
    // Must be in a terminal phase.
    let phase = state.phase();
    if phase != Phase::Done && phase != Phase::Archived {
        return false;
    }

    // Must not be soft-deleted.
    if state.is_deleted() {
        return false;
    }

    // Check age: updated_at represents when the item last changed state.
    let age_us = now_us.saturating_sub(state.updated_at.cast_signed());
    let min_age_us = i64::from(min_age_days) * 24 * 60 * 60 * 1_000_000;

    age_us >= min_age_us
}

/// Compact eligible items from a collection of events grouped by item ID.
///
/// # Arguments
///
/// * `events_by_item` — Map from `item_id` to all events for that item.
/// * `agent` — Agent identifier for snapshot events.
/// * `min_age_days` — Minimum days in done/archived before compaction.
/// * `now_us` — Current wall-clock time in microseconds.
/// * `redacted_hashes` — Set of event hashes that have been redacted.
///
/// # Returns
///
/// A tuple of (`snapshot_events`, report).
pub fn compact_items<S: ::std::hash::BuildHasher>(
    events_by_item: &BTreeMap<String, Vec<Event>>,
    agent: &str,
    min_age_days: u32,
    now_us: i64,
    redacted_hashes: &HashSet<String, S>,
) -> (Vec<Event>, CompactionReport) {
    let mut snapshots = Vec::new();
    let mut report = CompactionReport {
        items_compacted: 0,
        events_replaced: 0,
        snapshots_created: 0,
        items_skipped: 0,
    };

    for (item_id, events) in events_by_item {
        if events.is_empty() {
            report.items_skipped += 1;
            continue;
        }

        // Skip items with no events to compact, or with a snapshot of
        // exactly these events already (bn-t37g).
        let sources = compaction_sources(events);
        if sources.is_empty() || has_snapshot_of(events, &sources) {
            report.items_skipped += 1;
            continue;
        }

        // Replay to determine eligibility.
        let state = WorkItemState::from_events(&sources);

        if !is_eligible(&state, min_age_days, now_us) {
            report.items_skipped += 1;
            continue;
        }

        // Attempt compaction.
        match compact_item(item_id, events, agent, redacted_hashes) {
            Some(snapshot) => {
                report.items_compacted += 1;
                report.events_replaced += sources.len();
                report.snapshots_created += 1;
                snapshots.push(snapshot);
            }
            None => {
                // Redacted events prevented compaction.
                report.items_skipped += 1;
            }
        }
    }

    (snapshots, report)
}

/// Verify that compacted state matches uncompacted state.
///
/// Replays original events to produce a `WorkItemState`, then reconstructs
/// a `WorkItemState` from the snapshot event's payload and compares them
/// field by field.
///
/// # Returns
///
/// `Ok(true)` if the states match, `Ok(false)` if they diverge,
/// or `Err` if the snapshot event cannot be parsed.
///
/// # Errors
///
/// Returns an error if the snapshot event cannot be deserialized into a
/// `SnapshotPayload`.
pub fn verify_compaction(
    item_id: &str,
    original_events: &[Event],
    snapshot_event: &Event,
) -> Result<bool> {
    // Replay the events the snapshot covers.
    let original_state = WorkItemState::from_events(&compaction_sources(original_events));

    // Parse snapshot payload.
    let payload = extract_snapshot_payload(snapshot_event)
        .with_context(|| format!("parse snapshot for {item_id}"))?;

    // Reconstruct state from snapshot.
    let snapshot_state = WorkItemState::from_snapshot_payload(&payload);

    // Compare field by field.
    Ok(states_match(&original_state, &snapshot_state))
}

/// Verify lattice join property: merge(original, snapshot) == original.
///
/// If compaction is correct, the snapshot is the join of all events, so
/// merging the snapshot with the original state should produce identical
/// state (idempotency of join with self's join).
///
/// # Errors
///
/// Returns an error if the snapshot event cannot be deserialized.
pub fn verify_lattice_join(original_events: &[Event], snapshot_event: &Event) -> Result<bool> {
    // Build original state.
    let original_state = WorkItemState::from_events(&compaction_sources(original_events));

    // Build snapshot state.
    let payload = extract_snapshot_payload(snapshot_event)?;
    let snapshot_state = WorkItemState::from_snapshot_payload(&payload);

    // Merge snapshot into original (should be no-op since snapshot == join).
    let mut merged = original_state.clone();
    merged.merge(&snapshot_state);

    Ok(states_match(&original_state, &merged))
}

/// Extract and deserialize the [`SnapshotPayload`] from an `item.snapshot` event.
///
/// # Errors
///
/// Returns an error if the event is not a snapshot type or if the payload
/// cannot be deserialized.
pub fn extract_snapshot_payload(event: &Event) -> Result<SnapshotPayload> {
    if event.event_type != EventType::Snapshot {
        bail!("expected item.snapshot event, got {}", event.event_type);
    }

    let state_json = match &event.data {
        EventData::Snapshot(data) => &data.state,
        _ => bail!("event data is not Snapshot variant"),
    };

    let payload: SnapshotPayload = serde_json::from_value(state_json.clone())
        .context("deserialize SnapshotPayload from snapshot event")?;

    Ok(payload)
}

// ---------------------------------------------------------------------------
// Comparison helper
// ---------------------------------------------------------------------------

/// Compare two `WorkItemState` instances for semantic equality.
///
/// This checks all fields including CRDT metadata. It is used during
/// verification to ensure compaction is semantics-preserving.
fn states_match(a: &WorkItemState, b: &WorkItemState) -> bool {
    // LWW fields: compare value and clock metadata.
    a.title.value == b.title.value
        && a.title.wall_ts == b.title.wall_ts
        && a.title.agent_id == b.title.agent_id
        && a.title.event_hash == b.title.event_hash
        && a.description.value == b.description.value
        && a.description.wall_ts == b.description.wall_ts
        && a.kind.value == b.kind.value
        && a.kind.wall_ts == b.kind.wall_ts
        && a.size.value == b.size.value
        && a.size.wall_ts == b.size.wall_ts
        && a.urgency.value == b.urgency.value
        && a.urgency.wall_ts == b.urgency.wall_ts
        && a.parent.value == b.parent.value
        && a.parent.wall_ts == b.parent.wall_ts
        && a.deleted.value == b.deleted.value
        && a.deleted.wall_ts == b.deleted.wall_ts
        // EpochPhaseState
        && a.state == b.state
        // OR-Sets
        && a.assignees == b.assignees
        && a.labels == b.labels
        && a.blocked_by == b.blocked_by
        && a.related_to == b.related_to
        && a.links == b.links
        && a.compact_summary.value == b.compact_summary.value
        && a.compact_summary.wall_ts == b.compact_summary.wall_ts
        // G-Set
        && a.comments == b.comments
        // Timestamps
        && a.created_at == b.created_at
        && a.updated_at == b.updated_at
}

// ---------------------------------------------------------------------------
// Compaction policy configuration
// ---------------------------------------------------------------------------

/// Configuration for the compaction policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionPolicy {
    /// Minimum days an item must be in done/archived state before compaction.
    /// Default: 30.
    pub min_age_days: u32,

    /// Target lifecycle states eligible for compaction.
    /// Default: `["done", "archived"]`.
    pub target_states: Vec<String>,

    /// If true, perform a dry run (report what would be compacted, but don't
    /// write any snapshot events).
    pub dry_run: bool,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            min_age_days: 30,
            target_states: vec!["done".to_string(), "archived".to_string()],
            dry_run: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::data::*;
    use crate::model::item::{Kind, Size, State, Urgency};
    use std::collections::BTreeMap;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn make_event(
        event_type: EventType,
        data: EventData,
        wall_ts_us: i64,
        agent: &str,
        event_hash: &str,
        item_id: &str,
    ) -> Event {
        Event {
            wall_ts_us,
            agent: agent.to_string(),
            itc: crate::event::ITC_PLACEHOLDER.to_string(),
            parents: vec![],
            event_type,
            item_id: ItemId::new_unchecked(item_id),
            data,
            event_hash: event_hash.to_string(),
        }
    }

    fn create_event(title: &str, wall_ts: i64, agent: &str, hash: &str, item_id: &str) -> Event {
        make_event(
            EventType::Create,
            EventData::Create(CreateData {
                title: title.to_string(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: vec!["backend".to_string()],
                parent: None,
                causation: None,
                description: Some("A description".to_string()),
                extra: BTreeMap::new(),
            }),
            wall_ts,
            agent,
            hash,
            item_id,
        )
    }

    fn move_event(state: State, wall_ts: i64, agent: &str, hash: &str, item_id: &str) -> Event {
        make_event(
            EventType::Move,
            EventData::Move(MoveData {
                state,
                reason: None,
                extra: BTreeMap::new(),
            }),
            wall_ts,
            agent,
            hash,
            item_id,
        )
    }

    fn assign_event(
        target_agent: &str,
        wall_ts: i64,
        agent: &str,
        hash: &str,
        item_id: &str,
    ) -> Event {
        make_event(
            EventType::Assign,
            EventData::Assign(AssignData {
                agent: target_agent.to_string(),
                action: AssignAction::Assign,
                extra: BTreeMap::new(),
            }),
            wall_ts,
            agent,
            hash,
            item_id,
        )
    }

    fn comment_event(body: &str, wall_ts: i64, agent: &str, hash: &str, item_id: &str) -> Event {
        make_event(
            EventType::Comment,
            EventData::Comment(CommentData {
                body: body.to_string(),
                extra: BTreeMap::new(),
            }),
            wall_ts,
            agent,
            hash,
            item_id,
        )
    }

    fn update_title_event(
        title: &str,
        wall_ts: i64,
        agent: &str,
        hash: &str,
        item_id: &str,
    ) -> Event {
        make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "title".to_string(),
                value: serde_json::Value::String(title.to_string()),
                extra: BTreeMap::new(),
            }),
            wall_ts,
            agent,
            hash,
            item_id,
        )
    }

    fn sample_item_events(item_id: &str) -> Vec<Event> {
        vec![
            create_event("Fix auth retry", 1_000_000, "alice", "blake3:e1", item_id),
            assign_event("bob", 2_000_000, "alice", "blake3:e2", item_id),
            move_event(State::Doing, 3_000_000, "bob", "blake3:e3", item_id),
            comment_event("Found root cause", 4_000_000, "bob", "blake3:e4", item_id),
            update_title_event(
                "Fix auth retry logic",
                5_000_000,
                "bob",
                "blake3:e5",
                item_id,
            ),
            move_event(State::Done, 6_000_000, "bob", "blake3:e6", item_id),
        ]
    }

    fn label_event(action: &str, label: &str, wall_ts: i64, hash: &str, item_id: &str) -> Event {
        make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "labels".to_string(),
                value: serde_json::json!({"action": action, "label": label}),
                extra: BTreeMap::new(),
            }),
            wall_ts,
            "alice",
            hash,
            item_id,
        )
    }

    /// Every ordering of `items`, by Heap's algorithm.
    fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        fn heap<T: Clone>(k: usize, items: &mut Vec<T>, out: &mut Vec<Vec<T>>) {
            if k <= 1 {
                out.push(items.clone());
                return;
            }
            for i in 0..k {
                heap(k - 1, items, out);
                let j = if k % 2 == 0 { i } else { 0 };
                items.swap(j, k - 1);
            }
        }
        let mut out = Vec::new();
        heap(items.len(), &mut items.to_vec(), &mut out);
        out
    }

    #[test]
    fn snapshot_does_not_depend_on_log_order() {
        // A merged log is sorted by raw timestamp, which can differ from LWW
        // order; the snapshot must match what the projection shows for every
        // log order (bn-1ed2).
        let id = "bn-ord";
        let events = vec![
            create_event("Created", 1_000, "alice", "blake3:e1", id),
            label_event("add", "frontend", 1_500, "blake3:e2", id),
            update_title_event("Older", 2_000, "bob", "blake3:e3", id),
            label_event("remove", "backend", 2_500, "blake3:e4", id),
            update_title_event("Newest", 3_000, "bob", "blake3:e5", id),
            move_event(State::Done, 4_000, "bob", "blake3:e6", id),
        ];
        // Distinct itc strings, so the test sees which event the snapshot's itc
        // comes from.
        let events: Vec<Event> = events
            .into_iter()
            .enumerate()
            .map(|(i, mut e)| {
                e.itc = format!("itc:test{i}");
                e
            })
            .collect();
        let redacted = HashSet::new();
        let mut first: Option<serde_json::Value> = None;
        let mut first_hash: Option<String> = None;
        for order in permutations(&events) {
            let mut snapshot = compact_item(id, &order, "compactor", &redacted).unwrap();
            // The written event, bytes and hash, is the same for every order.
            crate::event::writer::write_event(&mut snapshot).unwrap();
            match &first_hash {
                None => first_hash = Some(snapshot.event_hash.clone()),
                Some(expected) => assert_eq!(&snapshot.event_hash, expected),
            }
            let payload = extract_snapshot_payload(&snapshot).unwrap();
            assert_eq!(payload.title.value, "Newest");
            assert!(payload.labels.contains(&"frontend".to_string()));
            assert!(!payload.labels.contains(&"backend".to_string()));
            let json = serde_json::to_value(&payload).unwrap();
            match &first {
                None => first = Some(json),
                Some(expected) => assert_eq!(&json, expected),
            }
        }
    }

    // -----------------------------------------------------------------------
    // compact_item
    // -----------------------------------------------------------------------

    #[test]
    fn compact_item_produces_snapshot() {
        let events = sample_item_events("bn-test1");
        let redacted = HashSet::new();

        let snapshot = compact_item("bn-test1", &events, "compactor", &redacted)
            .expect("should produce snapshot");

        assert_eq!(snapshot.event_type, EventType::Snapshot);
        assert_eq!(snapshot.item_id.as_str(), "bn-test1");
        assert_eq!(snapshot.agent, "compactor");
        assert!(snapshot.event_hash.starts_with("blake3:"));
    }

    #[test]
    fn compact_item_empty_events_returns_none() {
        let redacted = HashSet::new();
        assert!(compact_item("bn-test1", &[], "compactor", &redacted).is_none());
    }

    #[test]
    fn compact_item_redacted_events_returns_none() {
        let events = sample_item_events("bn-test1");
        let mut redacted = HashSet::new();
        redacted.insert("blake3:e3".to_string());

        assert!(compact_item("bn-test1", &events, "compactor", &redacted).is_none());
    }

    #[test]
    fn compact_item_snapshot_payload_has_audit_metadata() {
        let events = sample_item_events("bn-test1");
        let redacted = HashSet::new();

        let snapshot = compact_item("bn-test1", &events, "compactor", &redacted).unwrap();
        let payload = extract_snapshot_payload(&snapshot).unwrap();

        assert_eq!(payload.compacted_from, 6);
        assert_eq!(payload.earliest_ts, 1_000_000);
        assert_eq!(payload.latest_ts, 6_000_000);
    }

    #[test]
    fn compact_item_snapshot_preserves_state() {
        let events = sample_item_events("bn-test1");
        let redacted = HashSet::new();

        let snapshot = compact_item("bn-test1", &events, "compactor", &redacted).unwrap();
        let payload = extract_snapshot_payload(&snapshot).unwrap();

        assert_eq!(payload.title.value, "Fix auth retry logic");
        assert_eq!(payload.kind.value, Kind::Task);
        assert_eq!(payload.size.value, Some(Size::M));
        assert_eq!(payload.state.phase, Phase::Done);
        assert_eq!(payload.description.value, "A description");
        assert!(!payload.deleted.value);
    }

    // -----------------------------------------------------------------------
    // verify_compaction
    // -----------------------------------------------------------------------

    #[test]
    fn verify_compaction_matches() {
        let events = sample_item_events("bn-test1");
        let redacted = HashSet::new();

        let snapshot = compact_item("bn-test1", &events, "compactor", &redacted).unwrap();
        let matches = verify_compaction("bn-test1", &events, &snapshot).unwrap();

        assert!(matches, "compacted state should match replayed state");
    }

    #[test]
    fn verify_lattice_join_holds() {
        let events = sample_item_events("bn-test1");
        let redacted = HashSet::new();

        let snapshot = compact_item("bn-test1", &events, "compactor", &redacted).unwrap();
        let holds = verify_lattice_join(&events, &snapshot).unwrap();

        assert!(holds, "merge(original, snapshot) should equal original");
    }

    // -----------------------------------------------------------------------
    // SnapshotPayload roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_payload_serde_roundtrip() {
        let events = sample_item_events("bn-test1");
        let redacted = HashSet::new();

        let snapshot = compact_item("bn-test1", &events, "compactor", &redacted).unwrap();
        let payload = extract_snapshot_payload(&snapshot).unwrap();

        // Serialize to JSON and back.
        let json = serde_json::to_string(&payload).expect("serialize");
        let roundtripped: SnapshotPayload = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(roundtripped.item_id, payload.item_id);
        assert_eq!(roundtripped.title.value, payload.title.value);
        assert_eq!(roundtripped.compacted_from, payload.compacted_from);
    }

    /// Snapshots written before bn-1dy8 carry an ITC `stamp` on every LWW
    /// field. They must still deserialize, and the stamp is ignored.
    #[test]
    fn snapshot_payload_accepts_legacy_stamp_key() {
        let events = sample_item_events("bn-test1");
        let redacted = HashSet::new();
        let snapshot = compact_item("bn-test1", &events, "compactor", &redacted).unwrap();
        let payload = extract_snapshot_payload(&snapshot).unwrap();

        let mut json = serde_json::to_value(&payload).expect("serialize");
        let legacy_stamp = serde_json::json!({
            "id": "One",
            "event": { "Branch": [1, { "Leaf": 0 }, { "Leaf": 2 }] }
        });
        for field in [
            "title",
            "description",
            "kind",
            "size",
            "urgency",
            "parent",
            "deleted",
        ] {
            json[field]
                .as_object_mut()
                .expect("LWW field is an object")
                .insert("stamp".to_string(), legacy_stamp.clone());
        }
        assert!(json["title"].get("stamp").is_some());

        let legacy: SnapshotPayload = serde_json::from_value(json).expect("legacy deserialize");
        assert_eq!(legacy.title, payload.title);
        assert_eq!(legacy.deleted, payload.deleted);
        assert_eq!(
            serde_json::to_value(&legacy).unwrap(),
            serde_json::to_value(&payload).unwrap()
        );
    }

    #[test]
    fn from_snapshot_payload_roundtrips_state() {
        let events = sample_item_events("bn-test1");

        // Build original state.
        let mut original = WorkItemState::new();
        for event in &events {
            original.apply_event(event);
        }

        // Convert to payload and back.
        let payload = original.to_snapshot_payload("bn-test1", events.len(), 1_000_000, 6_000_000);
        let reconstructed = WorkItemState::from_snapshot_payload(&payload);

        assert!(states_match(&original, &reconstructed));
    }

    // -----------------------------------------------------------------------
    // is_eligible
    // -----------------------------------------------------------------------

    #[test]
    fn eligible_done_item_old_enough() {
        let events = sample_item_events("bn-test1");
        let mut state = WorkItemState::new();
        for event in &events {
            state.apply_event(event);
        }

        // 31 days after the last event.
        let now = 6_000_000 + 31 * 24 * 60 * 60 * 1_000_000;
        assert!(is_eligible(&state, 30, now));
    }

    #[test]
    fn not_eligible_done_item_too_new() {
        let events = sample_item_events("bn-test1");
        let mut state = WorkItemState::new();
        for event in &events {
            state.apply_event(event);
        }

        // Only 10 days after.
        let now = 6_000_000 + 10 * 24 * 60 * 60 * 1_000_000;
        assert!(!is_eligible(&state, 30, now));
    }

    #[test]
    fn not_eligible_open_item() {
        let events = vec![create_event(
            "Title",
            1_000_000,
            "alice",
            "blake3:c1",
            "bn-test1",
        )];
        let mut state = WorkItemState::new();
        for event in &events {
            state.apply_event(event);
        }

        let now = 1_000_000 + 365 * 24 * 60 * 60 * 1_000_000;
        assert!(!is_eligible(&state, 30, now));
    }

    #[test]
    fn not_eligible_deleted_item() {
        let events = vec![
            create_event("Title", 1_000_000, "alice", "blake3:c1", "bn-test1"),
            move_event(State::Done, 2_000_000, "alice", "blake3:m1", "bn-test1"),
            make_event(
                EventType::Delete,
                EventData::Delete(DeleteData {
                    reason: Some("dup".to_string()),
                    extra: BTreeMap::new(),
                }),
                3_000_000,
                "alice",
                "blake3:d1",
                "bn-test1",
            ),
        ];
        let mut state = WorkItemState::new();
        for event in &events {
            state.apply_event(event);
        }

        let now = 3_000_000 + 365 * 24 * 60 * 60 * 1_000_000;
        assert!(!is_eligible(&state, 30, now));
    }

    // -----------------------------------------------------------------------
    // compact_items batch
    // -----------------------------------------------------------------------

    #[test]
    fn compact_items_batch() {
        let item1_events = sample_item_events("bn-test1");
        let item2_events = vec![create_event(
            "Open item",
            1_000_000,
            "alice",
            "blake3:o1",
            "bn-test2",
        )];

        let mut events_by_item = BTreeMap::new();
        events_by_item.insert("bn-test1".to_string(), item1_events);
        events_by_item.insert("bn-test2".to_string(), item2_events);

        let now = 6_000_000 + 31 * 24 * 60 * 60 * 1_000_000;
        let redacted = HashSet::new();

        let (snapshots, report) = compact_items(&events_by_item, "compactor", 30, now, &redacted);

        assert_eq!(snapshots.len(), 1);
        assert_eq!(report.items_compacted, 1);
        assert_eq!(report.events_replaced, 6);
        assert_eq!(report.snapshots_created, 1);
        assert_eq!(report.items_skipped, 1);
    }

    #[test]
    fn compact_items_skips_already_compacted() {
        // Create an item that's already a single snapshot.
        let events = sample_item_events("bn-test1");
        let redacted = HashSet::new();
        let snapshot = compact_item("bn-test1", &events, "compactor", &redacted).unwrap();

        let mut events_by_item = BTreeMap::new();
        events_by_item.insert("bn-test1".to_string(), vec![snapshot]);

        let now = 6_000_000 + 365 * 24 * 60 * 60 * 1_000_000;
        let (snapshots, report) = compact_items(&events_by_item, "compactor", 30, now, &redacted);

        assert_eq!(snapshots.len(), 0);
        assert_eq!(report.items_skipped, 1);
    }

    #[test]
    fn duplicate_line_does_not_change_snapshot() {
        // A union merge can write one event twice. The snapshot must be the
        // same event as on a replica without the duplicate (bn-t37g).
        let events = sample_item_events("bn-test1");
        let mut with_dup = events.clone();
        with_dup.insert(2, events[1].clone());
        let redacted = HashSet::new();

        let plain = compact_item("bn-test1", &events, "compactor", &redacted).unwrap();
        let dup = compact_item("bn-test1", &with_dup, "compactor", &redacted).unwrap();
        assert_eq!(dup.event_hash, plain.event_hash);
        assert_eq!(extract_snapshot_payload(&dup).unwrap().compacted_from, 6);

        let now = 6_000_000 + 365 * 24 * 60 * 60 * 1_000_000;
        let by_item = BTreeMap::from([("bn-test1".to_string(), with_dup)]);
        let (_, report) = compact_items(&by_item, "compactor", 30, now, &redacted);
        assert_eq!(report.events_replaced, 6);
    }

    #[test]
    fn second_compaction_makes_no_new_snapshot() {
        // `bn compact` appends the snapshot and keeps the events. A second
        // run over events + snapshot must not make another snapshot
        // (bn-t37g).
        let events = sample_item_events("bn-test1");
        let redacted = HashSet::new();
        let now = 6_000_000 + 365 * 24 * 60 * 60 * 1_000_000;

        let by_item = BTreeMap::from([("bn-test1".to_string(), events.clone())]);
        let (first, _) = compact_items(&by_item, "compactor", 30, now, &redacted);
        assert_eq!(first.len(), 1);

        let mut log = events.clone();
        log.push(first[0].clone());
        let by_item = BTreeMap::from([("bn-test1".to_string(), log.clone())]);
        let (second, report) = compact_items(&by_item, "compactor", 30, now, &redacted);
        assert!(second.is_empty(), "second run made {second:?}");
        assert_eq!(report.items_skipped, 1);

        // A snapshot of the same events in an older format is replaced.
        let mut old = first[0].clone();
        if let EventData::Snapshot(data) = &mut old.data {
            data.state.as_object_mut().unwrap().remove("_format");
        }
        let mut old_log = events.clone();
        old_log.push(old);
        let by_item = BTreeMap::from([("bn-test1".to_string(), old_log)]);
        let (redo, _) = compact_items(&by_item, "compactor", 30, now, &redacted);
        assert_eq!(redo.len(), 1, "an old-format snapshot must be replaced");
        assert_eq!(redo[0].event_hash, first[0].event_hash);

        // So is a snapshot of format 2, from before the field_value rules
        // (bn-18fs).
        let mut format_2 = first[0].clone();
        if let EventData::Snapshot(data) = &mut format_2.data {
            data.state["_format"] = serde_json::json!(2);
        }
        let mut format_2_log = events.clone();
        format_2_log.push(format_2);
        let by_item = BTreeMap::from([("bn-test1".to_string(), format_2_log)]);
        let (redo, _) = compact_items(&by_item, "compactor", 30, now, &redacted);
        assert_eq!(redo.len(), 1, "a format 2 snapshot must be replaced");
        assert_eq!(redo[0].event_hash, first[0].event_hash);

        // The earlier snapshot is not a source: with a new event, the new
        // snapshot covers the events only, and verifies against the log.
        log.push(comment_event(
            "late note",
            7_000_000,
            "bob",
            "blake3:e7",
            "bn-test1",
        ));
        let by_item = BTreeMap::from([("bn-test1".to_string(), log.clone())]);
        let (third, _) = compact_items(&by_item, "compactor", 30, now, &redacted);
        assert_eq!(third.len(), 1);
        let payload = extract_snapshot_payload(&third[0]).unwrap();
        assert_eq!(payload.compacted_from, 7);
        assert_eq!(payload.latest_ts, 7_000_000);
        assert!(!third[0].parents.contains(&first[0].event_hash));
        assert!(verify_compaction("bn-test1", &log, &third[0]).unwrap());
    }

    #[test]
    fn snapshot_without_links_or_summary_keeps_its_keys() {
        // New keys are left out when blank, so snapshots of items without
        // links or compact events keep the bytes they had (bn-t37g).
        let events = sample_item_events("bn-test1");
        let snapshot = compact_item("bn-test1", &events, "compactor", &HashSet::new()).unwrap();
        let EventData::Snapshot(data) = &snapshot.data else {
            panic!("not a snapshot");
        };
        let obj = data.state.as_object().unwrap();
        assert!(!obj.contains_key("links"));
        assert!(!obj.contains_key("compact_summary"));
    }

    #[test]
    fn legacy_snapshot_links_come_from_views() {
        // A snapshot written before bn-t37g has only blocked_by and
        // related_to. They become "blocks" and "related_to" links.
        let mut state = WorkItemState::new();
        for (target, link_type, hash) in [
            ("bn-b", "blocks", "blake3:l1"),
            ("bn-r", "related_to", "blake3:l2"),
        ] {
            state.apply_event(&make_event(
                EventType::Link,
                EventData::Link(LinkData {
                    target: target.to_string(),
                    link_type: link_type.to_string(),
                    extra: BTreeMap::new(),
                }),
                1_000,
                "alice",
                hash,
                "bn-test1",
            ));
        }
        let mut json =
            serde_json::to_value(state.to_snapshot_payload("bn-test1", 2, 1_000, 1_000)).unwrap();
        let obj = json.as_object_mut().unwrap();
        assert!(obj.remove("links").is_some());
        let legacy: SnapshotPayload = serde_json::from_value(json).expect("legacy deserialize");
        assert!(orset_is_blank(&legacy.links));

        let restored = WorkItemState::from_snapshot_payload(&legacy);
        let links: HashSet<(String, String)> = restored
            .link_keys()
            .into_iter()
            .map(|k| (k.target.clone(), k.link_type.clone()))
            .collect();
        let expected: HashSet<(String, String)> = [
            ("bn-b".to_string(), "blocks".to_string()),
            ("bn-r".to_string(), "related_to".to_string()),
        ]
        .into_iter()
        .collect();
        assert_eq!(links, expected);
        assert_eq!(restored.compact_summary.value, "");
    }

    // -----------------------------------------------------------------------
    // Semilattice property: merge(state, snapshot) == state
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_merge_is_idempotent_with_original() {
        let events = sample_item_events("bn-test1");

        // Build original state.
        let mut original = WorkItemState::new();
        for event in &events {
            original.apply_event(event);
        }

        // Build snapshot state from payload.
        let payload = original.to_snapshot_payload("bn-test1", events.len(), 1_000_000, 6_000_000);
        let snapshot_state = WorkItemState::from_snapshot_payload(&payload);

        // Merge snapshot into original.
        let mut merged = original.clone();
        merged.merge(&snapshot_state);

        assert!(
            states_match(&original, &merged),
            "merge(original, snapshot) should equal original"
        );
    }

    #[test]
    fn snapshot_merge_commutative() {
        let events = sample_item_events("bn-test1");

        let mut original = WorkItemState::new();
        for event in &events {
            original.apply_event(event);
        }

        let payload = original.to_snapshot_payload("bn-test1", events.len(), 1_000_000, 6_000_000);
        let snapshot_state = WorkItemState::from_snapshot_payload(&payload);

        // merge(original, snapshot)
        let mut ab = original.clone();
        ab.merge(&snapshot_state);

        // merge(snapshot, original)
        let mut ba = snapshot_state.clone();
        ba.merge(&original);

        assert!(
            states_match(&ab, &ba),
            "snapshot merge should be commutative"
        );
    }

    // -----------------------------------------------------------------------
    // CompactionPolicy
    // -----------------------------------------------------------------------

    #[test]
    fn compaction_policy_defaults() {
        let policy = CompactionPolicy::default();
        assert_eq!(policy.min_age_days, 30);
        assert_eq!(
            policy.target_states,
            vec!["done".to_string(), "archived".to_string()]
        );
        assert!(!policy.dry_run);
    }
}
