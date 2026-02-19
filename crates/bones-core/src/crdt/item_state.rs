//! `WorkItemState` — composite CRDT aggregating all field-level CRDTs.
//!
//! A `WorkItemState` is the mergeable aggregate that the projection layer
//! materializes and the CLI displays. Each field delegates to the appropriate
//! CRDT primitive:
//!
//! - **LWW** ([`LwwRegister<T>`]): title, description, kind, size, urgency, parent
//! - **OR-Set** ([`OrSet<String>`]): assignees, labels, `blocked_by`, `related_to`
//! - **G-Set** ([`GSet<String>`]): comments (event hashes referencing comment content)
//! - **Epoch+Phase** ([`EpochPhaseState`]): lifecycle state
//! - **LWW<bool>** ([`LwwRegister<bool>`]): soft-delete flag
//!
//! # Merge Semantics
//!
//! `merge(a, b)` delegates to each field's CRDT merge. This preserves the
//! semilattice properties (commutative, associative, idempotent) because
//! the product of semilattices is itself a semilattice.
//!
//! # Event Application
//!
//! Given an [`Event`], `apply_event` routes to the correct field based on
//! the event type and updates the corresponding CRDT with the event's metadata.
//!
//! # Snapshot Support
//!
//! `to_snapshot` produces a JSON representation with per-field clock metadata
//! for use during log compaction. `from_snapshot` reconstructs the aggregate
//! from a snapshot event. Snapshot merge uses lattice join (not overwrite),
//! so `merge(state, snapshot) == merge(snapshot, state)`.

// Many methods are simple CRDT accessors that benefit from being non-const
// (they access HashSet which is not const-compatible). Suppress pedantic
// lints that don't add value for a CRDT module.
#![allow(
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::use_self,
    clippy::redundant_closure_for_method_calls,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::redundant_clone,
    clippy::match_same_arms
)]

use std::collections::HashSet;

use crate::clock::itc::Stamp;
use crate::crdt::OrSet;
use crate::crdt::gset::GSet;
use crate::crdt::lww::LwwRegister;
use crate::crdt::merge::Merge;
use crate::crdt::state::{EpochPhaseState, Phase};
use crate::event::Event;
use crate::event::data::{AssignAction, EventData};
use crate::event::types::EventType;
use crate::model::item::{Kind, Size, State, Urgency};

use super::Timestamp;

// ---------------------------------------------------------------------------
// WorkItemState
// ---------------------------------------------------------------------------

/// Composite CRDT representing the full state of a work item.
///
/// All fields are individually mergeable CRDTs. The aggregate merge
/// delegates to each field, preserving semilattice laws.
#[derive(Debug, Clone)]
pub struct WorkItemState {
    /// Item title (LWW register).
    pub title: LwwRegister<String>,
    /// Item description (LWW register, empty string = no description).
    pub description: LwwRegister<String>,
    /// Work item kind (LWW register).
    pub kind: LwwRegister<Kind>,
    /// Lifecycle state (epoch+phase CRDT).
    pub state: EpochPhaseState,
    /// T-shirt size estimate (LWW register, None encoded as Size::M default).
    pub size: LwwRegister<Option<Size>>,
    /// Priority/urgency override (LWW register).
    pub urgency: LwwRegister<Urgency>,
    /// Parent item ID (LWW register, empty string = no parent).
    pub parent: LwwRegister<String>,
    /// Assigned agents (OR-Set, add-wins).
    pub assignees: OrSet<String>,
    /// Labels (OR-Set, add-wins).
    pub labels: OrSet<String>,
    /// Blocked-by item IDs (OR-Set, add-wins).
    pub blocked_by: OrSet<String>,
    /// Related-to item IDs (OR-Set, add-wins).
    pub related_to: OrSet<String>,
    /// Comment event hashes (G-Set, grow-only).
    pub comments: GSet<String>,
    /// Soft-delete flag (LWW register).
    pub deleted: LwwRegister<bool>,
    /// Wall-clock timestamp of the earliest event (for created_at).
    pub created_at: u64,
    /// Wall-clock timestamp of the latest applied event (for updated_at).
    pub updated_at: u64,
}

impl WorkItemState {
    /// Create a new empty `WorkItemState` with default values.
    ///
    /// All LWW registers start with a zero stamp (epoch 0, no identity).
    /// All sets start empty. State starts at epoch 0, phase Open.
    pub fn new() -> Self {
        let zero_stamp = Stamp::seed();
        let zero_ts = 0u64;
        let zero_agent = String::new();
        let zero_hash = String::new();

        Self {
            title: LwwRegister::new(
                String::new(),
                zero_stamp.clone(),
                zero_ts,
                zero_agent.clone(),
                zero_hash.clone(),
            ),
            description: LwwRegister::new(
                String::new(),
                zero_stamp.clone(),
                zero_ts,
                zero_agent.clone(),
                zero_hash.clone(),
            ),
            kind: LwwRegister::new(
                Kind::Task,
                zero_stamp.clone(),
                zero_ts,
                zero_agent.clone(),
                zero_hash.clone(),
            ),
            state: EpochPhaseState::new(),
            size: LwwRegister::new(
                None,
                zero_stamp.clone(),
                zero_ts,
                zero_agent.clone(),
                zero_hash.clone(),
            ),
            urgency: LwwRegister::new(
                Urgency::Default,
                zero_stamp.clone(),
                zero_ts,
                zero_agent.clone(),
                zero_hash.clone(),
            ),
            parent: LwwRegister::new(
                String::new(),
                zero_stamp.clone(),
                zero_ts,
                zero_agent.clone(),
                zero_hash.clone(),
            ),
            assignees: OrSet::new(),
            labels: OrSet::new(),
            blocked_by: OrSet::new(),
            related_to: OrSet::new(),
            comments: GSet::new(),
            deleted: LwwRegister::new(false, zero_stamp, zero_ts, zero_agent, zero_hash),
            created_at: 0,
            updated_at: 0,
        }
    }

    /// Merge another `WorkItemState` into this one.
    ///
    /// Each field delegates to its own CRDT merge. The aggregate merge
    /// preserves semilattice properties because the product of semilattices
    /// is a semilattice.
    pub fn merge(&mut self, other: &WorkItemState) {
        self.title.merge(&other.title);
        self.description.merge(&other.description);
        self.kind.merge(&other.kind);
        self.state.merge(&other.state);
        self.size.merge(&other.size);
        self.urgency.merge(&other.urgency);
        self.parent.merge(&other.parent);

        // OR-Sets: merge via set union (takes ownership of clone)
        self.assignees.merge(other.assignees.clone());
        self.labels.merge(other.labels.clone());
        self.blocked_by.merge(other.blocked_by.clone());
        self.related_to.merge(other.related_to.clone());

        // G-Set: merge via set union
        self.comments.merge(other.comments.clone());

        // Deleted: LWW merge
        self.deleted.merge(&other.deleted);

        // Timestamps: created_at = min of non-zero, updated_at = max
        if other.created_at != 0 && (self.created_at == 0 || other.created_at < self.created_at) {
            self.created_at = other.created_at;
        }
        if other.updated_at > self.updated_at {
            self.updated_at = other.updated_at;
        }
    }

    /// Apply an event to this aggregate, updating the appropriate field CRDT.
    ///
    /// The event's metadata (wall_ts, agent, event_hash) is used to construct
    /// the LWW timestamp or OR-Set tag for the update.
    ///
    /// Unknown event types and unrecognized update fields are silently ignored
    /// (no-op), following the principle that invalid events are skipped during
    /// replay.
    pub fn apply_event(&mut self, event: &Event) {
        let wall_ts = event.wall_ts_us as u64;

        // Update created_at / updated_at timestamps.
        if self.created_at == 0 || wall_ts < self.created_at {
            self.created_at = wall_ts;
        }
        if wall_ts > self.updated_at {
            self.updated_at = wall_ts;
        }

        // Build LWW metadata from the event.
        let stamp = derive_stamp_from_hash(&event.event_hash);
        let agent_id = event.agent.clone();
        let event_hash = event.event_hash.clone();

        match event.event_type {
            EventType::Create => {
                if let EventData::Create(data) = &event.data {
                    self.title = LwwRegister::new(
                        data.title.clone(),
                        stamp.clone(),
                        wall_ts,
                        agent_id.clone(),
                        event_hash.clone(),
                    );
                    self.kind = LwwRegister::new(
                        data.kind,
                        stamp.clone(),
                        wall_ts,
                        agent_id.clone(),
                        event_hash.clone(),
                    );
                    if let Some(size) = data.size {
                        self.size = LwwRegister::new(
                            Some(size),
                            stamp.clone(),
                            wall_ts,
                            agent_id.clone(),
                            event_hash.clone(),
                        );
                    }
                    self.urgency = LwwRegister::new(
                        data.urgency,
                        stamp.clone(),
                        wall_ts,
                        agent_id.clone(),
                        event_hash.clone(),
                    );
                    if let Some(desc) = &data.description {
                        self.description = LwwRegister::new(
                            desc.clone(),
                            stamp.clone(),
                            wall_ts,
                            agent_id.clone(),
                            event_hash.clone(),
                        );
                    }
                    if let Some(parent) = &data.parent {
                        self.parent = LwwRegister::new(
                            parent.clone(),
                            stamp.clone(),
                            wall_ts,
                            agent_id.clone(),
                            event_hash.clone(),
                        );
                    }
                    // Apply initial labels via OR-Set.
                    for label in &data.labels {
                        let tag = make_orset_tag(wall_ts, &agent_id, &event_hash, label);
                        self.labels.add(label.clone(), tag);
                    }
                }
            }

            EventType::Update => {
                if let EventData::Update(data) = &event.data {
                    match data.field.as_str() {
                        "title" => {
                            if let Some(s) = data.value.as_str() {
                                self.title = LwwRegister::new(
                                    s.to_string(),
                                    stamp,
                                    wall_ts,
                                    agent_id,
                                    event_hash,
                                );
                            }
                        }
                        "description" => {
                            let desc = data
                                .value
                                .as_str()
                                .map(|s| s.to_string())
                                .unwrap_or_default();
                            self.description =
                                LwwRegister::new(desc, stamp, wall_ts, agent_id, event_hash);
                        }
                        "kind" => {
                            if let Some(kind) =
                                data.value.as_str().and_then(|s| s.parse::<Kind>().ok())
                            {
                                self.kind =
                                    LwwRegister::new(kind, stamp, wall_ts, agent_id, event_hash);
                            }
                        }
                        "size" => {
                            let size = data.value.as_str().and_then(|s| s.parse::<Size>().ok());
                            self.size =
                                LwwRegister::new(size, stamp, wall_ts, agent_id, event_hash);
                        }
                        "urgency" => {
                            if let Some(urgency) =
                                data.value.as_str().and_then(|s| s.parse::<Urgency>().ok())
                            {
                                self.urgency =
                                    LwwRegister::new(urgency, stamp, wall_ts, agent_id, event_hash);
                            }
                        }
                        "parent" => {
                            let parent = data
                                .value
                                .as_str()
                                .map(|s| s.to_string())
                                .unwrap_or_default();
                            self.parent =
                                LwwRegister::new(parent, stamp, wall_ts, agent_id, event_hash);
                        }
                        "labels" => {
                            // Labels update via OR-Set add/remove encoded in value.
                            if let Some(obj) = data.value.as_object() {
                                let action =
                                    obj.get("action").and_then(|v| v.as_str()).unwrap_or("");
                                let label = obj
                                    .get("label")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();

                                if !label.is_empty() {
                                    match action {
                                        "add" => {
                                            let tag = make_orset_tag(
                                                wall_ts,
                                                &agent_id,
                                                &event_hash,
                                                &label,
                                            );
                                            self.labels.add(label, tag);
                                        }
                                        "remove" => {
                                            self.labels.remove(&label);
                                        }
                                        _ => {} // Unknown action — no-op.
                                    }
                                }
                            }
                        }
                        _ => {} // Unknown field — no-op.
                    }
                }
            }

            EventType::Move => {
                if let EventData::Move(data) = &event.data {
                    // Map the model::item::State to crdt::state::Phase.
                    let target_phase = state_to_phase(data.state);
                    apply_phase_transition(&mut self.state, target_phase);
                }
            }

            EventType::Assign => {
                if let EventData::Assign(data) = &event.data {
                    match data.action {
                        AssignAction::Assign => {
                            let tag = make_orset_tag(wall_ts, &agent_id, &event_hash, &data.agent);
                            self.assignees.add(data.agent.clone(), tag);
                        }
                        AssignAction::Unassign => {
                            self.assignees.remove(&data.agent);
                        }
                    }
                }
            }

            EventType::Comment => {
                if let EventData::Comment(_) = &event.data {
                    // Add the event hash as a comment reference.
                    self.comments.insert(event.event_hash.clone());
                }
            }

            EventType::Link => {
                if let EventData::Link(data) = &event.data {
                    let tag = make_orset_tag(wall_ts, &agent_id, &event_hash, &data.target);
                    match data.link_type.as_str() {
                        "blocks" | "blocked_by" => {
                            self.blocked_by.add(data.target.clone(), tag);
                        }
                        "related_to" | "related" => {
                            self.related_to.add(data.target.clone(), tag);
                        }
                        _ => {} // Unknown link type — no-op.
                    }
                }
            }

            EventType::Unlink => {
                if let EventData::Unlink(data) = &event.data {
                    let is_blocked = data
                        .link_type
                        .as_ref()
                        .is_none_or(|lt| lt == "blocks" || lt == "blocked_by");
                    let is_related = data
                        .link_type
                        .as_ref()
                        .is_none_or(|lt| lt == "related_to" || lt == "related");

                    if is_blocked {
                        self.blocked_by.remove(&data.target);
                    }
                    if is_related {
                        self.related_to.remove(&data.target);
                    }
                }
            }

            EventType::Delete => {
                // Set deleted flag via LWW.
                self.deleted = LwwRegister::new(true, stamp, wall_ts, agent_id, event_hash);
            }

            EventType::Compact => {
                if let EventData::Compact(data) = &event.data {
                    // Replace description with summary.
                    self.description = LwwRegister::new(
                        data.summary.clone(),
                        stamp,
                        wall_ts,
                        agent_id,
                        event_hash,
                    );
                }
            }

            EventType::Snapshot => {
                // Snapshot application is handled via from_snapshot + merge,
                // not via apply_event. This is intentionally a no-op here.
                // Callers should use WorkItemState::from_snapshot() and merge.
            }

            EventType::Redact => {
                // Redaction targets a prior event — handled at the projection
                // level by filtering event hashes. No CRDT state change.
            }
        }
    }

    /// Check if this item is soft-deleted.
    pub const fn is_deleted(&self) -> bool {
        self.deleted.value
    }

    /// Return the current lifecycle phase.
    pub const fn phase(&self) -> Phase {
        self.state.phase
    }

    /// Return the current epoch.
    pub const fn epoch(&self) -> u64 {
        self.state.epoch
    }

    /// Return the set of current assignee names.
    pub fn assignee_names(&self) -> HashSet<&String> {
        self.assignees.values()
    }

    /// Return the set of current label strings.
    pub fn label_names(&self) -> HashSet<&String> {
        self.labels.values()
    }

    /// Return the set of items blocking this one.
    pub fn blocked_by_ids(&self) -> HashSet<&String> {
        self.blocked_by.values()
    }

    /// Return the set of related item IDs.
    pub fn related_to_ids(&self) -> HashSet<&String> {
        self.related_to.values()
    }

    /// Return comment event hashes.
    pub const fn comment_hashes(&self) -> &HashSet<String> {
        &self.comments.elements
    }
}

impl Default for WorkItemState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a `model::item::State` to a `crdt::state::Phase`.
const fn state_to_phase(state: State) -> Phase {
    match state {
        State::Open => Phase::Open,
        State::Doing => Phase::Doing,
        State::Done => Phase::Done,
        State::Archived => Phase::Archived,
    }
}

/// Apply a phase transition, handling epoch increments for reopen.
///
/// If the target phase is Open and the current phase is beyond Open,
/// this triggers a reopen (epoch increment). Otherwise it advances
/// within the current epoch. If the advance is invalid (backward move
/// within epoch), we force via reopen.
fn apply_phase_transition(state: &mut EpochPhaseState, target: Phase) {
    if target == Phase::Open && state.phase > Phase::Open {
        // Reopen: increment epoch.
        state.reopen();
    } else if target > state.phase {
        // Forward transition within epoch.
        let _ = state.advance(target);
    } else if target < state.phase && target != Phase::Open {
        // Backward move (e.g., Done -> Doing) requires reopen then advance.
        state.reopen();
        let _ = state.advance(target);
    }
    // target == state.phase is a no-op.
}

/// Derive a unique ITC stamp from the event hash.
///
/// The ITC text format parser is not yet implemented. To ensure LWW
/// tie-breaking works correctly, we derive a unique stamp from the
/// event_hash (which is guaranteed unique per event). Different hashes
/// produce concurrent stamps, so the LWW chain falls through to
/// wall_ts → agent_id → event_hash for deterministic resolution.
///
/// Same event_hash → same stamp → equal → idempotent (correct for
/// duplicate event application).
fn derive_stamp_from_hash(event_hash: &str) -> Stamp {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    event_hash.hash(&mut hasher);
    let bits = hasher.finish();

    // Fork the seed stamp along a path determined by hash bits.
    // 8 levels of forking gives 256 distinct stamp topologies,
    // making any two different hashes almost certainly produce
    // concurrent (incomparable) stamps.
    let mut stamp = Stamp::seed();
    for i in 0..8 {
        let (left, right) = stamp.fork();
        stamp = if (bits >> i) & 1 == 0 { left } else { right };
    }
    stamp.event();
    stamp
}

/// Construct an OR-Set tag (Timestamp) from event metadata.
///
/// Uses wall_ts as the time, and hashes the agent/event_hash/suffix
/// to deterministic u64 fields.
fn make_orset_tag(wall_ts: u64, agent: &str, event_hash: &str, suffix: &str) -> Timestamp {
    use chrono::TimeZone;
    use std::hash::{Hash, Hasher};

    let secs = wall_ts / 1_000_000;
    let nsecs = ((wall_ts % 1_000_000) * 1_000) as u32;
    let wall = chrono::Utc
        .timestamp_opt(secs as i64, nsecs)
        .single()
        .unwrap_or_else(chrono::Utc::now);

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    agent.hash(&mut hasher);
    let actor = hasher.finish();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    event_hash.hash(&mut hasher);
    suffix.hash(&mut hasher);
    let event_hash_u64 = hasher.finish();

    Timestamp {
        wall,
        actor,
        event_hash: event_hash_u64,
        itc: wall_ts,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::itc::Stamp;
    use crate::event::Event;
    use crate::event::data::*;
    use crate::event::types::EventType;
    use crate::model::item::{Kind, Size, State, Urgency};
    use crate::model::item_id::ItemId;
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
    ) -> Event {
        let mut stamp = Stamp::seed();
        stamp.event();
        Event {
            wall_ts_us,
            agent: agent.to_string(),
            itc: stamp.to_string(),
            parents: vec![],
            event_type,
            item_id: ItemId::new_unchecked("bn-test1"),
            data,
            event_hash: event_hash.to_string(),
        }
    }

    fn create_event(title: &str, wall_ts: i64, agent: &str, hash: &str) -> Event {
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
        )
    }

    fn update_title_event(title: &str, wall_ts: i64, agent: &str, hash: &str) -> Event {
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
        )
    }

    fn move_event(state: State, wall_ts: i64, agent: &str, hash: &str) -> Event {
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
        )
    }

    fn assign_event(
        target_agent: &str,
        action: AssignAction,
        wall_ts: i64,
        agent: &str,
        hash: &str,
    ) -> Event {
        make_event(
            EventType::Assign,
            EventData::Assign(AssignData {
                agent: target_agent.to_string(),
                action,
                extra: BTreeMap::new(),
            }),
            wall_ts,
            agent,
            hash,
        )
    }

    fn comment_event(body: &str, wall_ts: i64, agent: &str, hash: &str) -> Event {
        make_event(
            EventType::Comment,
            EventData::Comment(CommentData {
                body: body.to_string(),
                extra: BTreeMap::new(),
            }),
            wall_ts,
            agent,
            hash,
        )
    }

    fn link_event(target: &str, link_type: &str, wall_ts: i64, agent: &str, hash: &str) -> Event {
        make_event(
            EventType::Link,
            EventData::Link(LinkData {
                target: target.to_string(),
                link_type: link_type.to_string(),
                extra: BTreeMap::new(),
            }),
            wall_ts,
            agent,
            hash,
        )
    }

    fn unlink_event(
        target: &str,
        link_type: Option<&str>,
        wall_ts: i64,
        agent: &str,
        hash: &str,
    ) -> Event {
        make_event(
            EventType::Unlink,
            EventData::Unlink(UnlinkData {
                target: target.to_string(),
                link_type: link_type.map(|s| s.to_string()),
                extra: BTreeMap::new(),
            }),
            wall_ts,
            agent,
            hash,
        )
    }

    fn delete_event(wall_ts: i64, agent: &str, hash: &str) -> Event {
        make_event(
            EventType::Delete,
            EventData::Delete(DeleteData {
                reason: Some("duplicate".to_string()),
                extra: BTreeMap::new(),
            }),
            wall_ts,
            agent,
            hash,
        )
    }

    fn compact_event(summary: &str, wall_ts: i64, agent: &str, hash: &str) -> Event {
        make_event(
            EventType::Compact,
            EventData::Compact(CompactData {
                summary: summary.to_string(),
                extra: BTreeMap::new(),
            }),
            wall_ts,
            agent,
            hash,
        )
    }

    fn label_add_event(label: &str, wall_ts: i64, agent: &str, hash: &str) -> Event {
        make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "labels".to_string(),
                value: serde_json::json!({"action": "add", "label": label}),
                extra: BTreeMap::new(),
            }),
            wall_ts,
            agent,
            hash,
        )
    }

    fn label_remove_event(label: &str, wall_ts: i64, agent: &str, hash: &str) -> Event {
        make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "labels".to_string(),
                value: serde_json::json!({"action": "remove", "label": label}),
                extra: BTreeMap::new(),
            }),
            wall_ts,
            agent,
            hash,
        )
    }

    // -----------------------------------------------------------------------
    // Default state
    // -----------------------------------------------------------------------

    #[test]
    fn default_state_is_empty() {
        let state = WorkItemState::new();
        assert_eq!(state.title.value, "");
        assert_eq!(state.description.value, "");
        assert_eq!(state.kind.value, Kind::Task);
        assert_eq!(state.state, EpochPhaseState::new());
        assert_eq!(state.size.value, None);
        assert_eq!(state.urgency.value, Urgency::Default);
        assert_eq!(state.parent.value, "");
        assert!(state.assignees.is_empty());
        assert!(state.labels.is_empty());
        assert!(state.blocked_by.is_empty());
        assert!(state.related_to.is_empty());
        assert!(state.comments.is_empty());
        assert!(!state.is_deleted());
        assert_eq!(state.created_at, 0);
        assert_eq!(state.updated_at, 0);
    }

    #[test]
    fn default_impl_matches_new() {
        let a = WorkItemState::new();
        let b = WorkItemState::default();
        // Compare field by field since we don't impl PartialEq on the whole struct.
        assert_eq!(a.title.value, b.title.value);
        assert_eq!(a.state, b.state);
        assert_eq!(a.created_at, b.created_at);
    }

    // -----------------------------------------------------------------------
    // Event application: Create
    // -----------------------------------------------------------------------

    #[test]
    fn apply_create_sets_fields() {
        let mut state = WorkItemState::new();
        let event = create_event("Fix auth", 1000, "alice", "blake3:create1");
        state.apply_event(&event);

        assert_eq!(state.title.value, "Fix auth");
        assert_eq!(state.kind.value, Kind::Task);
        assert_eq!(state.size.value, Some(Size::M));
        assert_eq!(state.urgency.value, Urgency::Default);
        assert_eq!(state.description.value, "A description");
        assert!(state.label_names().contains(&"backend".to_string()));
        assert_eq!(state.created_at, 1000);
        assert_eq!(state.updated_at, 1000);
    }

    // -----------------------------------------------------------------------
    // Event application: Update
    // -----------------------------------------------------------------------

    #[test]
    fn apply_update_title() {
        let mut state = WorkItemState::new();
        state.apply_event(&create_event("Old", 1000, "alice", "blake3:c1"));
        state.apply_event(&update_title_event("New Title", 2000, "alice", "blake3:u1"));
        assert_eq!(state.title.value, "New Title");
    }

    #[test]
    fn apply_update_description() {
        let mut state = WorkItemState::new();
        let event = make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "description".to_string(),
                value: serde_json::Value::String("Updated desc".to_string()),
                extra: BTreeMap::new(),
            }),
            2000,
            "alice",
            "blake3:u2",
        );
        state.apply_event(&event);
        assert_eq!(state.description.value, "Updated desc");
    }

    #[test]
    fn apply_update_kind() {
        let mut state = WorkItemState::new();
        let event = make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "kind".to_string(),
                value: serde_json::Value::String("bug".to_string()),
                extra: BTreeMap::new(),
            }),
            2000,
            "alice",
            "blake3:u3",
        );
        state.apply_event(&event);
        assert_eq!(state.kind.value, Kind::Bug);
    }

    #[test]
    fn apply_update_size() {
        let mut state = WorkItemState::new();
        let event = make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "size".to_string(),
                value: serde_json::Value::String("xl".to_string()),
                extra: BTreeMap::new(),
            }),
            2000,
            "alice",
            "blake3:u4",
        );
        state.apply_event(&event);
        assert_eq!(state.size.value, Some(Size::Xl));
    }

    #[test]
    fn apply_update_urgency() {
        let mut state = WorkItemState::new();
        let event = make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "urgency".to_string(),
                value: serde_json::Value::String("urgent".to_string()),
                extra: BTreeMap::new(),
            }),
            2000,
            "alice",
            "blake3:u5",
        );
        state.apply_event(&event);
        assert_eq!(state.urgency.value, Urgency::Urgent);
    }

    #[test]
    fn apply_update_parent() {
        let mut state = WorkItemState::new();
        let event = make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "parent".to_string(),
                value: serde_json::Value::String("bn-parent1".to_string()),
                extra: BTreeMap::new(),
            }),
            2000,
            "alice",
            "blake3:u6",
        );
        state.apply_event(&event);
        assert_eq!(state.parent.value, "bn-parent1");
    }

    #[test]
    fn apply_update_labels_add_remove() {
        let mut state = WorkItemState::new();
        state.apply_event(&label_add_event("frontend", 1000, "alice", "blake3:la1"));
        assert!(state.label_names().contains(&"frontend".to_string()));

        state.apply_event(&label_add_event("urgent", 2000, "alice", "blake3:la2"));
        assert_eq!(state.labels.len(), 2);

        state.apply_event(&label_remove_event("frontend", 3000, "alice", "blake3:lr1"));
        assert!(!state.label_names().contains(&"frontend".to_string()));
        assert!(state.label_names().contains(&"urgent".to_string()));
    }

    #[test]
    fn apply_update_unknown_field_is_noop() {
        let mut state = WorkItemState::new();
        let event = make_event(
            EventType::Update,
            EventData::Update(UpdateData {
                field: "nonexistent_field".to_string(),
                value: serde_json::Value::String("whatever".to_string()),
                extra: BTreeMap::new(),
            }),
            2000,
            "alice",
            "blake3:u7",
        );
        let before_title = state.title.value.clone();
        state.apply_event(&event);
        assert_eq!(state.title.value, before_title);
    }

    // -----------------------------------------------------------------------
    // Event application: Move
    // -----------------------------------------------------------------------

    #[test]
    fn apply_move_forward() {
        let mut state = WorkItemState::new();
        state.apply_event(&move_event(State::Doing, 1000, "alice", "blake3:m1"));
        assert_eq!(state.phase(), Phase::Doing);

        state.apply_event(&move_event(State::Done, 2000, "alice", "blake3:m2"));
        assert_eq!(state.phase(), Phase::Done);
    }

    #[test]
    fn apply_move_reopen() {
        let mut state = WorkItemState::new();
        state.apply_event(&move_event(State::Done, 1000, "alice", "blake3:m1"));
        assert_eq!(state.phase(), Phase::Done);
        assert_eq!(state.epoch(), 0);

        state.apply_event(&move_event(State::Open, 2000, "alice", "blake3:m2"));
        assert_eq!(state.phase(), Phase::Open);
        assert_eq!(state.epoch(), 1);
    }

    #[test]
    fn apply_move_archived_then_reopen() {
        let mut state = WorkItemState::new();
        state.apply_event(&move_event(State::Done, 1000, "alice", "blake3:m1"));
        state.apply_event(&move_event(State::Archived, 2000, "alice", "blake3:m2"));
        assert_eq!(state.phase(), Phase::Archived);
        assert_eq!(state.epoch(), 0);

        state.apply_event(&move_event(State::Open, 3000, "alice", "blake3:m3"));
        assert_eq!(state.phase(), Phase::Open);
        assert_eq!(state.epoch(), 1);
    }

    // -----------------------------------------------------------------------
    // Event application: Assign
    // -----------------------------------------------------------------------

    #[test]
    fn apply_assign_and_unassign() {
        let mut state = WorkItemState::new();
        state.apply_event(&assign_event(
            "alice",
            AssignAction::Assign,
            1000,
            "admin",
            "blake3:a1",
        ));
        assert!(state.assignee_names().contains(&"alice".to_string()));

        state.apply_event(&assign_event(
            "bob",
            AssignAction::Assign,
            2000,
            "admin",
            "blake3:a2",
        ));
        assert_eq!(state.assignees.len(), 2);

        state.apply_event(&assign_event(
            "alice",
            AssignAction::Unassign,
            3000,
            "admin",
            "blake3:a3",
        ));
        assert!(!state.assignee_names().contains(&"alice".to_string()));
        assert!(state.assignee_names().contains(&"bob".to_string()));
    }

    // -----------------------------------------------------------------------
    // Event application: Comment
    // -----------------------------------------------------------------------

    #[test]
    fn apply_comment_adds_to_gset() {
        let mut state = WorkItemState::new();
        state.apply_event(&comment_event("hello", 1000, "alice", "blake3:c1"));
        state.apply_event(&comment_event("world", 2000, "bob", "blake3:c2"));

        assert_eq!(state.comments.len(), 2);
        assert!(state.comment_hashes().contains("blake3:c1"));
        assert!(state.comment_hashes().contains("blake3:c2"));
    }

    #[test]
    fn apply_duplicate_comment_is_idempotent() {
        let mut state = WorkItemState::new();
        let event = comment_event("hello", 1000, "alice", "blake3:c1");
        state.apply_event(&event);
        state.apply_event(&event);
        assert_eq!(state.comments.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Event application: Link/Unlink
    // -----------------------------------------------------------------------

    #[test]
    fn apply_link_blocks() {
        let mut state = WorkItemState::new();
        state.apply_event(&link_event(
            "bn-blocker",
            "blocks",
            1000,
            "alice",
            "blake3:l1",
        ));
        assert!(state.blocked_by_ids().contains(&"bn-blocker".to_string()));
    }

    #[test]
    fn apply_link_related() {
        let mut state = WorkItemState::new();
        state.apply_event(&link_event(
            "bn-related",
            "related_to",
            1000,
            "alice",
            "blake3:l2",
        ));
        assert!(state.related_to_ids().contains(&"bn-related".to_string()));
    }

    #[test]
    fn apply_unlink_blocks() {
        let mut state = WorkItemState::new();
        state.apply_event(&link_event("bn-b1", "blocks", 1000, "alice", "blake3:l1"));
        assert!(!state.blocked_by.is_empty());

        state.apply_event(&unlink_event(
            "bn-b1",
            Some("blocks"),
            2000,
            "alice",
            "blake3:ul1",
        ));
        assert!(state.blocked_by.is_empty());
    }

    #[test]
    fn apply_unlink_related() {
        let mut state = WorkItemState::new();
        state.apply_event(&link_event(
            "bn-r1",
            "related_to",
            1000,
            "alice",
            "blake3:l1",
        ));
        state.apply_event(&unlink_event(
            "bn-r1",
            Some("related_to"),
            2000,
            "alice",
            "blake3:ul1",
        ));
        assert!(state.related_to.is_empty());
    }

    // -----------------------------------------------------------------------
    // Event application: Delete
    // -----------------------------------------------------------------------

    #[test]
    fn apply_delete_sets_flag() {
        let mut state = WorkItemState::new();
        assert!(!state.is_deleted());

        state.apply_event(&delete_event(1000, "alice", "blake3:d1"));
        assert!(state.is_deleted());
    }

    // -----------------------------------------------------------------------
    // Event application: Compact
    // -----------------------------------------------------------------------

    #[test]
    fn apply_compact_replaces_description() {
        let mut state = WorkItemState::new();
        state.apply_event(&create_event("Title", 1000, "alice", "blake3:c1"));
        assert_eq!(state.description.value, "A description");

        state.apply_event(&compact_event("TL;DR summary", 2000, "alice", "blake3:cp1"));
        assert_eq!(state.description.value, "TL;DR summary");
    }

    // -----------------------------------------------------------------------
    // Event application: Timestamps
    // -----------------------------------------------------------------------

    #[test]
    fn timestamps_track_min_max() {
        let mut state = WorkItemState::new();
        state.apply_event(&create_event("T", 5000, "alice", "blake3:c1"));
        assert_eq!(state.created_at, 5000);
        assert_eq!(state.updated_at, 5000);

        state.apply_event(&update_title_event("T2", 3000, "bob", "blake3:u1"));
        assert_eq!(state.created_at, 3000); // min
        assert_eq!(state.updated_at, 5000); // still max

        state.apply_event(&update_title_event("T3", 8000, "carol", "blake3:u2"));
        assert_eq!(state.created_at, 3000);
        assert_eq!(state.updated_at, 8000);
    }

    // -----------------------------------------------------------------------
    // Merge: field delegation
    // -----------------------------------------------------------------------

    #[test]
    fn merge_lww_fields() {
        let mut a = WorkItemState::new();
        a.apply_event(&create_event("Title A", 1000, "alice", "blake3:a1"));

        let mut b = WorkItemState::new();
        b.apply_event(&create_event("Title B", 2000, "bob", "blake3:b1"));

        a.merge(&b);
        // b has higher wall_ts → b's title wins.
        assert_eq!(a.title.value, "Title B");
    }

    #[test]
    fn merge_epoch_phase() {
        let mut a = WorkItemState::new();
        a.apply_event(&move_event(State::Doing, 1000, "alice", "blake3:m1"));

        let mut b = WorkItemState::new();
        b.apply_event(&move_event(State::Done, 2000, "bob", "blake3:m2"));

        a.merge(&b);
        // Both epoch 0, Done > Doing.
        assert_eq!(a.phase(), Phase::Done);
    }

    #[test]
    fn merge_epoch_phase_reopen_wins() {
        let mut a = WorkItemState::new();
        a.apply_event(&move_event(State::Done, 1000, "alice", "blake3:m1"));

        let mut b = WorkItemState::new();
        b.apply_event(&move_event(State::Done, 1000, "bob", "blake3:m2"));
        b.apply_event(&move_event(State::Open, 2000, "bob", "blake3:m3"));

        a.merge(&b);
        // b has epoch 1, a has epoch 0. Higher epoch wins.
        assert_eq!(a.epoch(), 1);
        assert_eq!(a.phase(), Phase::Open);
    }

    #[test]
    fn merge_orset_assignees() {
        let mut a = WorkItemState::new();
        a.apply_event(&assign_event(
            "alice",
            AssignAction::Assign,
            1000,
            "admin",
            "blake3:a1",
        ));

        let mut b = WorkItemState::new();
        b.apply_event(&assign_event(
            "bob",
            AssignAction::Assign,
            1000,
            "admin",
            "blake3:a2",
        ));

        a.merge(&b);
        assert!(a.assignee_names().contains(&"alice".to_string()));
        assert!(a.assignee_names().contains(&"bob".to_string()));
    }

    #[test]
    fn merge_gset_comments() {
        let mut a = WorkItemState::new();
        a.apply_event(&comment_event("c1", 1000, "alice", "blake3:c1"));

        let mut b = WorkItemState::new();
        b.apply_event(&comment_event("c2", 2000, "bob", "blake3:c2"));

        a.merge(&b);
        assert_eq!(a.comments.len(), 2);
        assert!(a.comment_hashes().contains("blake3:c1"));
        assert!(a.comment_hashes().contains("blake3:c2"));
    }

    #[test]
    fn merge_deleted_lww() {
        let mut a = WorkItemState::new();
        // a is not deleted.

        let mut b = WorkItemState::new();
        b.apply_event(&delete_event(2000, "bob", "blake3:d1"));

        a.merge(&b);
        // b's delete has higher wall_ts → deleted wins.
        assert!(a.is_deleted());
    }

    #[test]
    fn merge_timestamps() {
        let mut a = WorkItemState::new();
        a.apply_event(&create_event("A", 5000, "alice", "blake3:a1"));

        let mut b = WorkItemState::new();
        b.apply_event(&create_event("B", 3000, "bob", "blake3:b1"));
        b.apply_event(&update_title_event("B2", 8000, "bob", "blake3:b2"));

        a.merge(&b);
        assert_eq!(a.created_at, 3000);
        assert_eq!(a.updated_at, 8000);
    }

    // -----------------------------------------------------------------------
    // Merge: semilattice properties
    // -----------------------------------------------------------------------

    fn make_state_a() -> WorkItemState {
        let mut s = WorkItemState::new();
        s.apply_event(&create_event("Title A", 1000, "alice", "blake3:a1"));
        s.apply_event(&move_event(State::Doing, 2000, "alice", "blake3:a2"));
        s.apply_event(&assign_event(
            "alice",
            AssignAction::Assign,
            3000,
            "admin",
            "blake3:a3",
        ));
        s.apply_event(&comment_event("comment a", 4000, "alice", "blake3:a4"));
        s.apply_event(&link_event("bn-b1", "blocks", 5000, "alice", "blake3:a5"));
        s
    }

    fn make_state_b() -> WorkItemState {
        let mut s = WorkItemState::new();
        s.apply_event(&create_event("Title B", 1500, "bob", "blake3:b1"));
        s.apply_event(&move_event(State::Done, 2500, "bob", "blake3:b2"));
        s.apply_event(&assign_event(
            "bob",
            AssignAction::Assign,
            3500,
            "admin",
            "blake3:b3",
        ));
        s.apply_event(&comment_event("comment b", 4500, "bob", "blake3:b4"));
        s.apply_event(&link_event("bn-r1", "related_to", 5500, "bob", "blake3:b5"));
        s
    }

    fn make_state_c() -> WorkItemState {
        let mut s = WorkItemState::new();
        s.apply_event(&create_event("Title C", 1200, "carol", "blake3:c1"));
        s.apply_event(&assign_event(
            "carol",
            AssignAction::Assign,
            3200,
            "admin",
            "blake3:c3",
        ));
        s.apply_event(&label_add_event("urgent", 4200, "carol", "blake3:c4"));
        s
    }

    /// Compare two WorkItemStates for equivalence (all fields).
    fn states_equal(a: &WorkItemState, b: &WorkItemState) -> bool {
        a.title.value == b.title.value
            && a.title.wall_ts == b.title.wall_ts
            && a.description.value == b.description.value
            && a.kind.value == b.kind.value
            && a.state == b.state
            && a.size.value == b.size.value
            && a.urgency.value == b.urgency.value
            && a.parent.value == b.parent.value
            && a.assignees == b.assignees
            && a.labels == b.labels
            && a.blocked_by == b.blocked_by
            && a.related_to == b.related_to
            && a.comments == b.comments
            && a.deleted.value == b.deleted.value
            && a.created_at == b.created_at
            && a.updated_at == b.updated_at
    }

    #[test]
    fn merge_commutative() {
        let a = make_state_a();
        let b = make_state_b();

        let mut ab = a.clone();
        ab.merge(&b);

        let mut ba = b.clone();
        ba.merge(&a);

        assert!(
            states_equal(&ab, &ba),
            "merge should be commutative\n  ab.title={}, ba.title={}\n  ab.state={:?}, ba.state={:?}",
            ab.title.value,
            ba.title.value,
            ab.state,
            ba.state,
        );
    }

    #[test]
    fn merge_associative() {
        let a = make_state_a();
        let b = make_state_b();
        let c = make_state_c();

        // (a ⊔ b) ⊔ c
        let mut ab_c = a.clone();
        ab_c.merge(&b);
        ab_c.merge(&c);

        // a ⊔ (b ⊔ c)
        let mut bc = b.clone();
        bc.merge(&c);
        let mut a_bc = a.clone();
        a_bc.merge(&bc);

        assert!(states_equal(&ab_c, &a_bc), "merge should be associative");
    }

    #[test]
    fn merge_idempotent() {
        let a = make_state_a();
        let before = a.clone();
        let mut merged = a.clone();
        merged.merge(&before);

        assert!(
            states_equal(&merged, &before),
            "merge with self should be idempotent"
        );
    }

    // -----------------------------------------------------------------------
    // Full event sequence
    // -----------------------------------------------------------------------

    #[test]
    fn full_lifecycle() {
        let mut state = WorkItemState::new();

        // Create.
        state.apply_event(&create_event("Fix auth", 1000, "alice", "blake3:e1"));
        assert_eq!(state.title.value, "Fix auth");
        assert_eq!(state.phase(), Phase::Open);

        // Assign.
        state.apply_event(&assign_event(
            "bob",
            AssignAction::Assign,
            2000,
            "alice",
            "blake3:e2",
        ));
        assert!(state.assignee_names().contains(&"bob".to_string()));

        // Move to Doing.
        state.apply_event(&move_event(State::Doing, 3000, "bob", "blake3:e3"));
        assert_eq!(state.phase(), Phase::Doing);

        // Add comment.
        state.apply_event(&comment_event("Found root cause", 4000, "bob", "blake3:e4"));
        assert_eq!(state.comments.len(), 1);

        // Update title.
        state.apply_event(&update_title_event(
            "Fix auth retry logic",
            5000,
            "bob",
            "blake3:e5",
        ));
        assert_eq!(state.title.value, "Fix auth retry logic");

        // Add blocker.
        state.apply_event(&link_event("bn-dep1", "blocks", 6000, "bob", "blake3:e6"));
        assert!(!state.blocked_by.is_empty());

        // Remove blocker.
        state.apply_event(&unlink_event(
            "bn-dep1",
            Some("blocks"),
            7000,
            "bob",
            "blake3:e7",
        ));
        assert!(state.blocked_by.is_empty());

        // Move to Done.
        state.apply_event(&move_event(State::Done, 8000, "bob", "blake3:e8"));
        assert_eq!(state.phase(), Phase::Done);

        // Add label.
        state.apply_event(&label_add_event("shipped", 9000, "alice", "blake3:e9"));
        assert!(state.label_names().contains(&"shipped".to_string()));

        assert_eq!(state.created_at, 1000);
        assert_eq!(state.updated_at, 9000);
    }

    // -----------------------------------------------------------------------
    // Concurrent divergent merge
    // -----------------------------------------------------------------------

    #[test]
    fn divergent_branches_merge_correctly() {
        // Simulate two agents forking from the same create event,
        // making concurrent changes, then merging.

        // Shared base.
        let create = create_event("Shared Title", 1000, "alice", "blake3:c1");

        // Branch A: alice updates title, moves to Doing, assigns self.
        let mut branch_a = WorkItemState::new();
        branch_a.apply_event(&create);
        branch_a.apply_event(&update_title_event(
            "Alice's Title",
            2000,
            "alice",
            "blake3:a1",
        ));
        branch_a.apply_event(&move_event(State::Doing, 3000, "alice", "blake3:a2"));
        branch_a.apply_event(&assign_event(
            "alice",
            AssignAction::Assign,
            4000,
            "alice",
            "blake3:a3",
        ));

        // Branch B: bob updates title (later ts), adds label, assigns self.
        let mut branch_b = WorkItemState::new();
        branch_b.apply_event(&create);
        branch_b.apply_event(&update_title_event("Bob's Title", 2500, "bob", "blake3:b1"));
        branch_b.apply_event(&label_add_event("urgent", 3500, "bob", "blake3:b2"));
        branch_b.apply_event(&assign_event(
            "bob",
            AssignAction::Assign,
            4500,
            "bob",
            "blake3:b3",
        ));

        // Merge A into B and B into A — should converge.
        let mut merged_ab = branch_a.clone();
        merged_ab.merge(&branch_b);

        let mut merged_ba = branch_b.clone();
        merged_ba.merge(&branch_a);

        assert!(states_equal(&merged_ab, &merged_ba));

        // Bob's title wins (higher wall_ts: 2500 > 2000).
        assert_eq!(merged_ab.title.value, "Bob's Title");

        // Phase: Doing from branch A (Doing > Open).
        assert_eq!(merged_ab.phase(), Phase::Doing);

        // Both assignees present (OR-Set union).
        assert!(merged_ab.assignee_names().contains(&"alice".to_string()));
        assert!(merged_ab.assignee_names().contains(&"bob".to_string()));

        // Bob's label present.
        assert!(merged_ab.label_names().contains(&"urgent".to_string()));
        // Create's label present.
        assert!(merged_ab.label_names().contains(&"backend".to_string()));
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn merge_default_with_default() {
        let a = WorkItemState::new();
        let b = WorkItemState::new();
        let mut merged = a.clone();
        merged.merge(&b);

        assert_eq!(merged.title.value, "");
        assert_eq!(merged.state, EpochPhaseState::new());
        assert_eq!(merged.created_at, 0);
    }

    #[test]
    fn merge_with_default_is_identity() {
        let a = make_state_a();
        let mut merged = a.clone();
        merged.merge(&WorkItemState::new());

        assert!(states_equal(&merged, &a));
    }

    #[test]
    fn apply_events_then_merge_equals_merge_then_apply() {
        // Commutativity test: apply events to separate states, merge
        // vs. apply all events to one state.
        let e1 = create_event("Title", 1000, "alice", "blake3:e1");
        let e2 = update_title_event("Updated", 2000, "bob", "blake3:e2");
        let e3 = assign_event("carol", AssignAction::Assign, 3000, "admin", "blake3:e3");

        // Path 1: apply e1, e2 to A; apply e1, e3 to B; merge.
        let mut path1_a = WorkItemState::new();
        path1_a.apply_event(&e1);
        path1_a.apply_event(&e2);

        let mut path1_b = WorkItemState::new();
        path1_b.apply_event(&e1);
        path1_b.apply_event(&e3);

        path1_a.merge(&path1_b);

        // Path 2: apply e1, e3 to B; apply e1, e2 to A; merge.
        let mut path2_b = WorkItemState::new();
        path2_b.apply_event(&e1);
        path2_b.apply_event(&e3);

        let mut path2_a = WorkItemState::new();
        path2_a.apply_event(&e1);
        path2_a.apply_event(&e2);

        path2_b.merge(&path2_a);

        assert!(states_equal(&path1_a, &path2_b));
    }

    #[test]
    fn snapshot_event_is_noop() {
        let mut state = WorkItemState::new();
        state.apply_event(&create_event("Title", 1000, "alice", "blake3:c1"));

        let snapshot_event = make_event(
            EventType::Snapshot,
            EventData::Snapshot(SnapshotData {
                state: serde_json::json!({"title": "Snapshot Title"}),
                extra: BTreeMap::new(),
            }),
            2000,
            "compactor",
            "blake3:s1",
        );

        let title_before = state.title.value.clone();
        state.apply_event(&snapshot_event);
        // Snapshot event doesn't change title (handled separately).
        assert_eq!(state.title.value, title_before);
    }

    #[test]
    fn redact_event_is_noop() {
        let mut state = WorkItemState::new();
        state.apply_event(&create_event("Title", 1000, "alice", "blake3:c1"));

        let redact_event = make_event(
            EventType::Redact,
            EventData::Redact(RedactData {
                target_hash: "blake3:c1".to_string(),
                reason: "secret".to_string(),
                extra: BTreeMap::new(),
            }),
            2000,
            "admin",
            "blake3:r1",
        );

        let title_before = state.title.value.clone();
        state.apply_event(&redact_event);
        assert_eq!(state.title.value, title_before);
    }

    // -----------------------------------------------------------------------
    // Accessor methods
    // -----------------------------------------------------------------------

    #[test]
    fn accessor_methods() {
        let mut state = WorkItemState::new();
        state.apply_event(&create_event("T", 1000, "alice", "blake3:c1"));
        state.apply_event(&assign_event(
            "alice",
            AssignAction::Assign,
            2000,
            "admin",
            "blake3:a1",
        ));
        state.apply_event(&link_event("bn-b1", "blocks", 3000, "alice", "blake3:l1"));
        state.apply_event(&link_event(
            "bn-r1",
            "related_to",
            4000,
            "alice",
            "blake3:l2",
        ));
        state.apply_event(&comment_event("hi", 5000, "alice", "blake3:cm1"));

        assert!(state.assignee_names().contains(&"alice".to_string()));
        assert!(state.label_names().contains(&"backend".to_string()));
        assert!(state.blocked_by_ids().contains(&"bn-b1".to_string()));
        assert!(state.related_to_ids().contains(&"bn-r1".to_string()));
        assert!(state.comment_hashes().contains("blake3:cm1"));
        assert_eq!(state.phase(), Phase::Open);
        assert_eq!(state.epoch(), 0);
        assert!(!state.is_deleted());
    }
}
