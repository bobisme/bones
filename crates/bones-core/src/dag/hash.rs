//! Content-addressed event hashing for the Merkle-DAG.
//!
//! This module provides hash verification and parent chain validation on top of
//! the core hash computation in [`crate::event::writer::compute_event_hash`].
//!
//! # Merkle-DAG Properties
//!
//! - Each event's hash covers its content AND its parent hashes.
//! - Modifying any event invalidates the hashes of all its descendants.
//! - Parent hashes are sorted lexicographically before hashing.
//! - The full BLAKE3 hash (64 hex chars) is used for maximum integrity.
//! - Hash format: `blake3:<lowercase hex>`.

use std::collections::HashMap;

use crate::event::Event;
use crate::event::writer::{WriteError, compute_event_hash};

// ---------------------------------------------------------------------------
// Machine-readable error codes
// ---------------------------------------------------------------------------

/// Machine-readable codes for [`HashError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashErrorCode {
    /// The stored hash on an event does not match its computed hash.
    HashMismatch,
    /// An event references a parent hash not found in the event set.
    UnknownParent,
    /// The hash could not be computed (e.g., serialization failure).
    ComputeFailure,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from DAG hash verification.
#[derive(Debug, thiserror::Error)]
pub enum HashError {
    /// The stored event hash does not match the recomputed hash.
    #[error("event hash mismatch: stored={stored} expected={expected}")]
    HashMismatch {
        /// The hash stored on the event (which is wrong).
        stored: String,
        /// The hash we computed from the event's fields (the correct value).
        expected: String,
    },

    /// A parent hash referenced by an event is not found in the event set.
    #[error("event {event_hash} references unknown parent {parent_hash}")]
    UnknownParent {
        /// The hash of the event that has the bad parent reference.
        event_hash: String,
        /// The parent hash that was not found in the event set.
        parent_hash: String,
    },

    /// Failed to compute the hash of an event.
    #[error("failed to compute event hash: {0}")]
    Compute(#[from] WriteError),
}

impl HashError {
    /// Return the machine-readable error code for this error.
    #[must_use]
    pub fn code(&self) -> HashErrorCode {
        match self {
            HashError::HashMismatch { .. } => HashErrorCode::HashMismatch,
            HashError::UnknownParent { .. } => HashErrorCode::UnknownParent,
            HashError::Compute(_) => HashErrorCode::ComputeFailure,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Verify that an event's stored hash matches the hash of its content.
///
/// The hash is recomputed from the event's fields 1–7 (wall_ts_us, agent,
/// itc, parents, event_type, item_id, data) using BLAKE3, and compared
/// against the `event_hash` field stored on the event.
///
/// Returns `true` if the stored hash is valid, `false` otherwise.
///
/// # Errors
///
/// Returns [`HashError::Compute`] if the hash cannot be computed (e.g.,
/// data serialization failure).
pub fn verify_event_hash(event: &Event) -> Result<bool, HashError> {
    let expected = compute_event_hash(event)?;
    Ok(event.event_hash == expected)
}

/// Verify the Merkle-DAG integrity of a collection of events.
///
/// This function performs two checks for each event in the collection:
///
/// 1. **Hash integrity**: the event's stored `event_hash` matches the BLAKE3
///    hash computed from its fields. If any ancestor was modified, its hash
///    will no longer match, causing this check to fail.
///
/// 2. **Parent resolution**: every parent hash referenced by an event must
///    appear as another event's `event_hash` in the provided collection.
///
/// Together, these checks enforce the Merkle property: modifying any event
/// invalidates the hashes of all its descendants (because a descendant's hash
/// covers its `parents` field, which encodes ancestor hashes).
///
/// Events may be provided in any order; all hashes are collected into a lookup
/// table before validation begins.
///
/// # Errors
///
/// Returns the first [`HashError`] encountered. The order in which events
/// are checked is not guaranteed.
pub fn verify_chain(events: &[&Event]) -> Result<(), HashError> {
    // Build a lookup table of all known event hashes in this collection.
    let known: HashMap<&str, ()> = events.iter().map(|e| (e.event_hash.as_str(), ())).collect();

    for event in events {
        // 1. Verify each event's stored hash matches its computed hash.
        let expected = compute_event_hash(event)?;
        if event.event_hash != expected {
            return Err(HashError::HashMismatch {
                stored: event.event_hash.clone(),
                expected,
            });
        }

        // 2. Verify every parent reference resolves to a known event.
        for parent in &event.parents {
            if !known.contains_key(parent.as_str()) {
                return Err(HashError::UnknownParent {
                    event_hash: event.event_hash.clone(),
                    parent_hash: parent.clone(),
                });
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::event::Event;
    use crate::event::data::{CreateData, EventData, MoveData};
    use crate::event::types::EventType;
    use crate::event::writer::write_event;
    use crate::model::item::{Kind, State, Urgency};
    use crate::model::item_id::ItemId;

    // -------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------

    /// Build a root event with the given timestamp and compute its hash.
    fn make_root(wall_ts_us: i64) -> Event {
        let mut event = Event {
            wall_ts_us,
            agent: "agent-a".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-a1b2"),
            data: EventData::Create(CreateData {
                title: "Root event".into(),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: "blake3:placeholder".into(),
        };
        // Compute and stamp the correct hash.
        write_event(&mut event).expect("write_event should not fail");
        event
    }

    /// Build a child event that references `parent_hash`, then compute its hash.
    fn make_child(wall_ts_us: i64, parent_hash: &str) -> Event {
        let mut event = Event {
            wall_ts_us,
            agent: "agent-b".into(),
            itc: "itc:AQ.1".into(),
            parents: vec![parent_hash.to_owned()],
            event_type: EventType::Move,
            item_id: ItemId::new_unchecked("bn-a1b2"),
            data: EventData::Move(MoveData {
                state: State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
            event_hash: "blake3:placeholder".into(),
        };
        write_event(&mut event).expect("write_event should not fail");
        event
    }

    // -------------------------------------------------------------------
    // verify_event_hash
    // -------------------------------------------------------------------

    #[test]
    fn test_verify_event_hash_valid() {
        let event = make_root(1_000_000);
        let result = verify_event_hash(&event).expect("should compute");
        assert!(result, "freshly written event should have valid hash");
    }

    #[test]
    fn test_verify_event_hash_tampered_content() {
        let mut event = make_root(1_000_000);
        // Tamper: change the timestamp but leave the stored hash alone.
        event.wall_ts_us += 1;
        let result = verify_event_hash(&event).expect("should compute");
        assert!(!result, "tampered event should fail hash check");
    }

    #[test]
    fn test_verify_event_hash_tampered_agent() {
        let mut event = make_root(1_000_000);
        event.agent = "evil-agent".into();
        let result = verify_event_hash(&event).expect("should compute");
        assert!(!result, "event with modified agent should fail hash check");
    }

    #[test]
    fn test_verify_event_hash_tampered_parents() {
        let root = make_root(1_000_000);
        let mut child = make_child(2_000_000, &root.event_hash);
        // Tamper: change the parent reference while leaving the stored hash alone.
        child.parents[0] = "blake3:forged_parent_hash".into();
        let result = verify_event_hash(&child).expect("should compute");
        assert!(!result, "event with forged parent should fail hash check");
    }

    #[test]
    fn test_verify_event_hash_deterministic() {
        let event = make_root(42_000_000);
        // Calling verify multiple times on the same event should always agree.
        let r1 = verify_event_hash(&event).expect("first call");
        let r2 = verify_event_hash(&event).expect("second call");
        assert_eq!(r1, r2, "verify_event_hash must be deterministic");
        assert!(r1, "should be valid");
    }

    // -------------------------------------------------------------------
    // verify_chain
    // -------------------------------------------------------------------

    #[test]
    fn test_verify_chain_single_root_event() {
        let root = make_root(1_000_000);
        verify_chain(&[&root]).expect("single valid root event should pass");
    }

    #[test]
    fn test_verify_chain_linear_chain() {
        let root = make_root(1_000_000);
        let child = make_child(2_000_000, &root.event_hash);
        let grandchild = make_child(3_000_000, &child.event_hash);

        verify_chain(&[&root, &child, &grandchild]).expect("valid 3-event chain should pass");
    }

    #[test]
    fn test_verify_chain_order_independent() {
        // Provide events in reverse order — should still pass.
        let root = make_root(1_000_000);
        let child = make_child(2_000_000, &root.event_hash);

        verify_chain(&[&child, &root]).expect("order should not matter for verify_chain");
    }

    #[test]
    fn test_verify_chain_tampered_root_detected() {
        let mut root = make_root(1_000_000);
        let child = make_child(2_000_000, &root.event_hash);

        // Tamper root content (leave its stored hash unchanged).
        root.wall_ts_us += 999;

        let err =
            verify_chain(&[&root, &child]).expect_err("tampered root should cause chain failure");
        assert_eq!(err.code(), HashErrorCode::HashMismatch);
    }

    #[test]
    fn test_verify_chain_tampered_child_detected() {
        let root = make_root(1_000_000);
        let mut child = make_child(2_000_000, &root.event_hash);

        // Tamper child content (leave its stored hash unchanged).
        child.agent = "impersonator".into();

        let err =
            verify_chain(&[&root, &child]).expect_err("tampered child should cause chain failure");
        assert_eq!(err.code(), HashErrorCode::HashMismatch);
    }

    #[test]
    fn test_verify_chain_unknown_parent_detected() {
        let child = make_child(2_000_000, "blake3:nonexistent_parent_hash");
        // Note: child.event_hash is valid for child's own fields; only its
        // parent reference is unresolvable.

        let err =
            verify_chain(&[&child]).expect_err("unresolvable parent should cause chain failure");
        assert_eq!(err.code(), HashErrorCode::UnknownParent);
    }

    #[test]
    fn test_verify_chain_merkle_cascade_property() {
        // Create a 3-event chain: root → child → grandchild.
        let root = make_root(1_000_000);
        let child = make_child(2_000_000, &root.event_hash);
        let grandchild = make_child(3_000_000, &child.event_hash);

        // Chain is valid before tampering.
        verify_chain(&[&root, &child, &grandchild]).expect("chain is initially valid");

        // Now simulate an ancestor modification:
        // Modify root and correctly recompute its hash (as an attacker would).
        let mut modified_root = root.clone();
        modified_root.wall_ts_us += 1;
        let new_root_hash = compute_event_hash(&modified_root).expect("hash compute");
        modified_root.event_hash = new_root_hash.clone();

        // modified_root now has a valid-looking hash, but child still references
        // the OLD root hash. The Merkle property means this breaks the chain:
        // child.parents[0] == old root hash, which no longer exists in the set.
        let err = verify_chain(&[&modified_root, &child, &grandchild])
            .expect_err("Merkle cascade: ancestor modification breaks descendants");
        // Either the child's parent is now unresolvable (old root hash gone)
        // or the root's hash mismatches — both demonstrate the Merkle property.
        assert!(
            matches!(
                err.code(),
                HashErrorCode::UnknownParent | HashErrorCode::HashMismatch
            ),
            "expected Merkle violation error, got: {:?}",
            err
        );
    }

    #[test]
    fn test_verify_chain_empty_slice() {
        // Empty slice should pass trivially.
        verify_chain(&[]).expect("empty slice should be valid");
    }

    #[test]
    fn test_verify_chain_merge_event_two_parents() {
        // Test a DAG with a merge point (two parents).
        let root_a = make_root(1_000_000);
        let root_b = make_root(1_100_000); // different timestamp → different hash

        // Build a merge event with two parents (sorted lexicographically).
        let mut parents = vec![root_a.event_hash.clone(), root_b.event_hash.clone()];
        parents.sort();

        let mut merge_event = Event {
            wall_ts_us: 2_000_000,
            agent: "agent-c".into(),
            itc: "itc:AQ.2".into(),
            parents,
            event_type: EventType::Move,
            item_id: ItemId::new_unchecked("bn-a1b2"),
            data: EventData::Move(MoveData {
                state: State::Done,
                reason: Some("merged".into()),
                extra: BTreeMap::new(),
            }),
            event_hash: "blake3:placeholder".into(),
        };
        write_event(&mut merge_event).expect("write merge event");

        verify_chain(&[&root_a, &root_b, &merge_event])
            .expect("DAG with merge point should be valid");
    }

    // -------------------------------------------------------------------
    // HashError machine-readable codes
    // -------------------------------------------------------------------

    #[test]
    fn test_hash_error_code_mismatch() {
        let err = HashError::HashMismatch {
            stored: "blake3:wrong".into(),
            expected: "blake3:right".into(),
        };
        assert_eq!(err.code(), HashErrorCode::HashMismatch);
    }

    #[test]
    fn test_hash_error_code_unknown_parent() {
        let err = HashError::UnknownParent {
            event_hash: "blake3:child".into(),
            parent_hash: "blake3:missing".into(),
        };
        assert_eq!(err.code(), HashErrorCode::UnknownParent);
    }
}
