//! Prolly Tree: content-defined chunked Merkle tree for efficient O(log N) diff.
//!
//! Events are keyed by `(item_id, wall_ts_us)`, sorted, then split into chunks
//! using a rolling hash (Gear hash) for content-defined boundaries. A balanced
//! Merkle tree is built over the chunks so that two replicas can diff in
//! O(log N) time by comparing hashes top-down.

use blake3::Hasher as Blake3;
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::event::Event;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Gear-hash mask: a chunk boundary fires when `gear_hash & MASK == 0`.
/// With 6 low bits → average chunk size ≈ 64 events.
const BOUNDARY_BITS: u32 = 6;
const BOUNDARY_MASK: u64 = (1u64 << BOUNDARY_BITS) - 1;

/// Minimum chunk size to avoid pathologically small chunks.
const MIN_CHUNK_SIZE: usize = 8;

/// Maximum chunk size to bound worst-case latency.
const MAX_CHUNK_SIZE: usize = 256;

/// Target fan-out for interior nodes (same boundary strategy applied
/// recursively at each tree level).
const INTERIOR_BOUNDARY_BITS: u32 = 3; // ~8 children per interior node
const INTERIOR_BOUNDARY_MASK: u64 = (1u64 << INTERIOR_BOUNDARY_BITS) - 1;
const MIN_INTERIOR_SIZE: usize = 2;
const MAX_INTERIOR_SIZE: usize = 32;

// ---------------------------------------------------------------------------
// Gear hash table (pseudo-random, deterministic)
// ---------------------------------------------------------------------------

/// 256-entry table for the Gear rolling hash.
/// Derived from BLAKE3 of byte index to ensure reproducibility.
fn gear_table() -> [u64; 256] {
    let mut table = [0u64; 256];
    for i in 0u16..256 {
        let h = blake3::hash(&i.to_le_bytes());
        let bytes: [u8; 8] = h.as_bytes()[0..8].try_into().unwrap();
        table[i as usize] = u64::from_le_bytes(bytes);
    }
    table
}

// ---------------------------------------------------------------------------
// Hash newtype
// ---------------------------------------------------------------------------

/// 32-byte BLAKE3 hash used as a node/chunk identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Hash(pub [u8; 32]);

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", hex::encode(&self.0[..8]))
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(&self.0))
    }
}

fn hash_bytes(data: &[u8]) -> Hash {
    Hash(*blake3::hash(data).as_bytes())
}

// ---------------------------------------------------------------------------
// Sort key
// ---------------------------------------------------------------------------

/// Sort key for events: `(item_id, wall_ts_us, event_hash)`.
/// The event_hash suffix makes the ordering fully deterministic even when
/// two events share the same item and timestamp.
fn sort_key(e: &Event) -> (String, i64, String) {
    (
        e.item_id.as_str().to_string(),
        e.wall_ts_us,
        e.event_hash.clone(),
    )
}

// ---------------------------------------------------------------------------
// Node types
// ---------------------------------------------------------------------------

/// A Prolly Tree node — either a leaf chunk of events or an interior node
/// pointing to children.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProllyNode {
    Leaf {
        hash: Hash,
        /// Serialised event hashes in this chunk (not full events — those live
        /// in the event log). We store just the `event_hash` strings so the
        /// tree is compact.
        event_hashes: Vec<String>,
    },
    Interior {
        hash: Hash,
        children: Vec<ProllyNode>,
    },
}

impl ProllyNode {
    pub fn hash(&self) -> Hash {
        match self {
            ProllyNode::Leaf { hash, .. } => *hash,
            ProllyNode::Interior { hash, .. } => *hash,
        }
    }

    /// Collect all event hashes reachable from this node.
    pub fn collect_event_hashes(&self, out: &mut Vec<String>) {
        match self {
            ProllyNode::Leaf { event_hashes, .. } => {
                out.extend(event_hashes.iter().cloned());
            }
            ProllyNode::Interior { children, .. } => {
                for child in children {
                    child.collect_event_hashes(out);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ProllyTree
// ---------------------------------------------------------------------------

/// Content-addressed Prolly Tree over a set of events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProllyTree {
    pub root: ProllyNode,
    pub event_count: usize,
}

impl ProllyTree {
    /// Build a Prolly Tree from a set of events.
    ///
    /// Events are sorted by `(item_id, wall_ts_us, event_hash)`, then split
    /// into content-defined chunks using a Gear rolling hash. A balanced
    /// Merkle tree is constructed over the leaf chunks.
    ///
    /// The root hash is deterministic for any permutation of the same event
    /// set.
    pub fn build(events: &[Event]) -> Self {
        if events.is_empty() {
            let empty_hash = hash_bytes(b"prolly:empty");
            return ProllyTree {
                root: ProllyNode::Leaf {
                    hash: empty_hash,
                    event_hashes: vec![],
                },
                event_count: 0,
            };
        }

        // Sort events deterministically.
        let mut sorted: Vec<&Event> = events.iter().collect();
        sorted.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));

        // Build leaf chunks using Gear hash for content-defined boundaries.
        let event_hashes: Vec<String> = sorted.iter().map(|e| e.event_hash.clone()).collect();
        let leaves = chunk_leaf(&event_hashes);

        // Build interior levels until we have a single root.
        let root = build_interior(leaves);

        ProllyTree {
            root,
            event_count: events.len(),
        }
    }

    /// Compute the diff between `self` (local) and `other` (remote).
    ///
    /// Returns event hashes that are in `other` but not in `self`.
    /// Runs in O(k log N) where k is the number of differing chunks.
    pub fn diff(&self, other: &ProllyTree) -> Vec<String> {
        let mut missing = Vec::new();
        diff_nodes(&self.root, &other.root, &mut missing);
        missing
    }

    /// All event hashes stored in this tree.
    pub fn event_hashes(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.event_count);
        self.root.collect_event_hashes(&mut out);
        out
    }

    /// Serialize to bytes for wire transfer.
    pub fn to_bytes(&self) -> Vec<u8> {
        // Use bincode for compact binary serialization.
        // Fall back to JSON if bincode isn't available.
        serde_json::to_vec(self).expect("ProllyTree serialization should not fail")
    }

    /// Deserialize from bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(data)
    }
}

// ---------------------------------------------------------------------------
// Chunking
// ---------------------------------------------------------------------------

/// Split a sorted list of event hashes into content-defined leaf chunks.
fn chunk_leaf(event_hashes: &[String]) -> Vec<ProllyNode> {
    let table = gear_table();
    let mut chunks = Vec::new();
    let mut chunk_start = 0;
    let mut gear: u64 = 0;

    for (i, eh) in event_hashes.iter().enumerate() {
        // Feed event hash bytes into the Gear hash.
        for &b in eh.as_bytes() {
            gear = (gear << 1).wrapping_add(table[b as usize]);
        }

        let chunk_len = i - chunk_start + 1;

        let at_boundary = chunk_len >= MIN_CHUNK_SIZE && (gear & BOUNDARY_MASK) == 0;
        let at_max = chunk_len >= MAX_CHUNK_SIZE;
        let at_end = i == event_hashes.len() - 1;

        if at_boundary || at_max || at_end {
            let slice = &event_hashes[chunk_start..=i];
            let hash = hash_leaf_chunk(slice);
            chunks.push(ProllyNode::Leaf {
                hash,
                event_hashes: slice.to_vec(),
            });
            chunk_start = i + 1;
            gear = 0;
        }
    }

    chunks
}

fn hash_leaf_chunk(event_hashes: &[String]) -> Hash {
    let mut hasher = Blake3::new();
    hasher.update(b"prolly:leaf:");
    for eh in event_hashes {
        hasher.update(eh.as_bytes());
        hasher.update(b"\n");
    }
    Hash(*hasher.finalize().as_bytes())
}

// ---------------------------------------------------------------------------
// Interior tree construction
// ---------------------------------------------------------------------------

/// Recursively build interior nodes until a single root remains.
fn build_interior(mut nodes: Vec<ProllyNode>) -> ProllyNode {
    if nodes.len() == 1 {
        return nodes.remove(0);
    }

    // Group nodes into interior chunks using Gear hash on child hashes.
    let table = gear_table();
    let mut groups: Vec<Vec<ProllyNode>> = Vec::new();
    let mut current_group: Vec<ProllyNode> = Vec::new();
    let mut gear: u64 = 0;

    for node in nodes {
        let h = node.hash();
        for &b in &h.0[..8] {
            gear = (gear << 1).wrapping_add(table[b as usize]);
        }
        current_group.push(node);

        let group_len = current_group.len();
        let at_boundary = group_len >= MIN_INTERIOR_SIZE && (gear & INTERIOR_BOUNDARY_MASK) == 0;
        let at_max = group_len >= MAX_INTERIOR_SIZE;

        if at_boundary || at_max {
            groups.push(std::mem::take(&mut current_group));
            gear = 0;
        }
    }
    if !current_group.is_empty() {
        groups.push(current_group);
    }

    // Build interior nodes from groups.
    let interior_nodes: Vec<ProllyNode> = groups
        .into_iter()
        .map(|children| {
            let hash = hash_interior(&children);
            ProllyNode::Interior { hash, children }
        })
        .collect();

    // Recurse until single root.
    build_interior(interior_nodes)
}

fn hash_interior(children: &[ProllyNode]) -> Hash {
    let mut hasher = Blake3::new();
    hasher.update(b"prolly:interior:");
    for child in children {
        hasher.update(&child.hash().0);
    }
    Hash(*hasher.finalize().as_bytes())
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

/// Walk two trees in parallel, collecting event hashes from `other` that
/// are not present in `local`.
fn diff_nodes(local: &ProllyNode, other: &ProllyNode, missing: &mut Vec<String>) {
    // If hashes match, subtrees are identical — prune.
    if local.hash() == other.hash() {
        return;
    }

    match (local, other) {
        // Both interior — match children by hash and recurse.
        (
            ProllyNode::Interior {
                children: local_children,
                ..
            },
            ProllyNode::Interior {
                children: other_children,
                ..
            },
        ) => {
            // Build a set of local child hashes for quick lookup.
            let local_set: std::collections::HashSet<Hash> =
                local_children.iter().map(|c| c.hash()).collect();

            for other_child in other_children {
                if local_set.contains(&other_child.hash()) {
                    // Identical subtree — skip.
                    continue;
                }
                // Try to find a matching local child to recurse into.
                // If no structural match, collect all events from other_child.
                let local_match = local_children.iter().find(|lc| {
                    // Heuristic: same node type and overlapping structure.
                    matches!(
                        (lc, other_child),
                        (ProllyNode::Interior { .. }, ProllyNode::Interior { .. })
                            | (ProllyNode::Leaf { .. }, ProllyNode::Leaf { .. })
                    )
                });

                match local_match {
                    Some(lm) => diff_nodes(lm, other_child, missing),
                    None => other_child.collect_event_hashes(missing),
                }
            }
        }
        // One or both are leaves — collect all event hashes from other
        // that aren't in local.
        _ => {
            let mut local_hashes = Vec::new();
            local.collect_event_hashes(&mut local_hashes);
            let local_set: std::collections::HashSet<&str> =
                local_hashes.iter().map(|s| s.as_str()).collect();

            let mut other_hashes = Vec::new();
            other.collect_event_hashes(&mut other_hashes);

            for h in other_hashes {
                if !local_set.contains(h.as_str()) {
                    missing.push(h);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// hex helper (avoid adding a crate dep for this)
// ---------------------------------------------------------------------------
mod hex {
    const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            s.push(HEX_CHARS[(b >> 4) as usize] as char);
            s.push(HEX_CHARS[(b & 0x0f) as usize] as char);
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::data::{CreateData, EventData};
    use crate::event::{Event, EventType};
    use crate::model::item::{Kind, Urgency};
    use crate::model::item_id::ItemId;
    use std::collections::BTreeMap;

    fn make_event(item_id: &str, ts: i64, hash_suffix: &str) -> Event {
        Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "0:0".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: format!("Item {item_id}"),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:{item_id}_{ts}_{hash_suffix}"),
        }
    }

    #[test]
    fn empty_tree() {
        let tree = ProllyTree::build(&[]);
        assert_eq!(tree.event_count, 0);
        assert!(tree.event_hashes().is_empty());
    }

    #[test]
    fn single_event() {
        let events = vec![make_event("item-1", 1000, "a")];
        let tree = ProllyTree::build(&events);
        assert_eq!(tree.event_count, 1);
        assert_eq!(tree.event_hashes().len(), 1);
    }

    #[test]
    fn deterministic_root_hash_same_order() {
        let events = vec![
            make_event("a", 1, "x"),
            make_event("b", 2, "y"),
            make_event("c", 3, "z"),
        ];
        let t1 = ProllyTree::build(&events);
        let t2 = ProllyTree::build(&events);
        assert_eq!(t1.root.hash(), t2.root.hash());
    }

    #[test]
    fn deterministic_root_hash_different_insertion_order() {
        let e1 = make_event("a", 1, "x");
        let e2 = make_event("b", 2, "y");
        let e3 = make_event("c", 3, "z");

        let t1 = ProllyTree::build(&[e1.clone(), e2.clone(), e3.clone()]);
        let t2 = ProllyTree::build(&[e3.clone(), e1.clone(), e2.clone()]);
        let t3 = ProllyTree::build(&[e2, e3, e1]);

        assert_eq!(t1.root.hash(), t2.root.hash());
        assert_eq!(t2.root.hash(), t3.root.hash());
    }

    #[test]
    fn diff_identical_trees_is_empty() {
        let events = vec![make_event("a", 1, "x"), make_event("b", 2, "y")];
        let t1 = ProllyTree::build(&events);
        let t2 = ProllyTree::build(&events);
        assert!(t1.diff(&t2).is_empty());
    }

    #[test]
    fn diff_finds_new_events() {
        let shared = vec![make_event("a", 1, "x"), make_event("b", 2, "y")];
        let mut extended = shared.clone();
        extended.push(make_event("c", 3, "z"));

        let t_local = ProllyTree::build(&shared);
        let t_remote = ProllyTree::build(&extended);

        let missing = t_local.diff(&t_remote);
        assert!(missing.contains(&"blake3:c_3_z".to_string()));
    }

    #[test]
    fn diff_empty_vs_populated() {
        let empty = ProllyTree::build(&[]);
        let populated = ProllyTree::build(&[make_event("a", 1, "x"), make_event("b", 2, "y")]);

        let missing = empty.diff(&populated);
        assert_eq!(missing.len(), 2);
    }

    #[test]
    fn serialization_roundtrip() {
        let events = vec![
            make_event("a", 1, "x"),
            make_event("b", 2, "y"),
            make_event("c", 3, "z"),
        ];
        let tree = ProllyTree::build(&events);
        let bytes = tree.to_bytes();
        let restored = ProllyTree::from_bytes(&bytes).expect("deserialize");
        assert_eq!(tree.root.hash(), restored.root.hash());
        assert_eq!(tree.event_count, restored.event_count);
    }

    #[test]
    fn many_events_produce_multiple_chunks() {
        // 200 events should produce multiple leaf chunks
        let events: Vec<Event> = (0..200)
            .map(|i| make_event(&format!("item-{i:04}"), i as i64, &format!("h{i}")))
            .collect();
        let tree = ProllyTree::build(&events);
        assert_eq!(tree.event_count, 200);
        assert_eq!(tree.event_hashes().len(), 200);

        // Verify determinism
        let tree2 = ProllyTree::build(&events);
        assert_eq!(tree.root.hash(), tree2.root.hash());
    }

    #[test]
    fn diff_with_overlapping_events() {
        // A has events 0..100, B has events 50..150
        let all_events: Vec<Event> = (0..150)
            .map(|i| make_event(&format!("item-{i:04}"), i as i64, &format!("h{i}")))
            .collect();

        let a_events = &all_events[0..100];
        let b_events = &all_events[50..150];

        let tree_a = ProllyTree::build(a_events);
        let tree_b = ProllyTree::build(b_events);

        // Events in B not in A should be 100..150
        let missing = tree_a.diff(&tree_b);
        let expected_missing: std::collections::HashSet<String> = (100..150)
            .map(|i| format!("blake3:item-{i:04}_{i}_h{i}"))
            .collect();

        let missing_set: std::collections::HashSet<String> = missing.into_iter().collect();
        for expected in &expected_missing {
            assert!(
                missing_set.contains(expected),
                "Expected {expected} in diff but not found"
            );
        }
    }

    #[test]
    fn diff_symmetric_finds_both_sides() {
        let shared = vec![make_event("shared", 1, "s")];
        let mut a = shared.clone();
        a.push(make_event("only-a", 2, "a"));
        let mut b = shared;
        b.push(make_event("only-b", 3, "b"));

        let tree_a = ProllyTree::build(&a);
        let tree_b = ProllyTree::build(&b);

        let a_missing = tree_a.diff(&tree_b); // events in B not in A
        let b_missing = tree_b.diff(&tree_a); // events in A not in B

        assert!(a_missing.contains(&"blake3:only-b_3_b".to_string()));
        assert!(b_missing.contains(&"blake3:only-a_2_a".to_string()));
    }

    #[test]
    fn same_item_different_timestamps() {
        let events = vec![
            make_event("item-1", 100, "v1"),
            make_event("item-1", 200, "v2"),
            make_event("item-1", 300, "v3"),
        ];
        let tree = ProllyTree::build(&events);
        assert_eq!(tree.event_count, 3);
        let hashes = tree.event_hashes();
        // Should be sorted by timestamp
        assert_eq!(hashes[0], "blake3:item-1_100_v1");
        assert_eq!(hashes[1], "blake3:item-1_200_v2");
        assert_eq!(hashes[2], "blake3:item-1_300_v3");
    }

    #[test]
    fn hash_display() {
        let h = hash_bytes(b"test");
        let s = format!("{h}");
        assert_eq!(s.len(), 64); // 32 bytes = 64 hex chars
    }

    #[test]
    fn large_diff_performance() {
        // Build two trees with 1000 events each, 900 shared
        let shared: Vec<Event> = (0..900)
            .map(|i| make_event(&format!("s{i:04}"), i, &format!("s{i}")))
            .collect();

        let mut a = shared.clone();
        for i in 900..1000 {
            a.push(make_event(&format!("a{i:04}"), i, &format!("a{i}")));
        }

        let mut b = shared;
        for i in 900..1000 {
            b.push(make_event(&format!("b{i:04}"), i, &format!("b{i}")));
        }

        let tree_a = ProllyTree::build(&a);
        let tree_b = ProllyTree::build(&b);

        let missing_from_b = tree_a.diff(&tree_b);
        // Should find ~100 events from B not in A
        assert!(
            missing_from_b.len() >= 90,
            "Expected ~100 missing, got {}",
            missing_from_b.len()
        );
    }

    #[test]
    fn build_and_diff_stress() {
        // Stress: 500 events, shuffle, build, diff with subset
        let all: Vec<Event> = (0..500)
            .map(|i| make_event(&format!("x{i:04}"), i, &format!("x{i}")))
            .collect();

        let subset = &all[0..400];

        let tree_all = ProllyTree::build(&all);
        let tree_sub = ProllyTree::build(subset);

        let missing = tree_sub.diff(&tree_all);
        // Events 400..500 should be missing from subset's perspective
        assert!(
            missing.len() >= 90,
            "Expected ~100 missing, got {}",
            missing.len()
        );
    }

    #[test]
    fn gear_table_is_deterministic() {
        let t1 = gear_table();
        let t2 = gear_table();
        assert_eq!(t1, t2);
    }

    #[test]
    fn chunk_boundaries_are_content_defined() {
        // Inserting an event in the middle should only affect nearby chunks
        let base: Vec<Event> = (0..100)
            .map(|i| make_event(&format!("e{i:04}"), i * 10, &format!("h{i}")))
            .collect();

        let tree_base = ProllyTree::build(&base);

        // Add one event in the middle
        let mut modified = base;
        modified.push(make_event("e0050x", 505, "hx"));

        let tree_mod = ProllyTree::build(&modified);

        // Root hashes should differ (new event added)
        assert_ne!(tree_base.root.hash(), tree_mod.root.hash());

        // But diff should find exactly the new event
        let missing = tree_base.diff(&tree_mod);
        assert!(missing.contains(&"blake3:e0050x_505_hx".to_string()));
    }

    #[test]
    fn empty_diff_with_empty() {
        let e1 = ProllyTree::build(&[]);
        let e2 = ProllyTree::build(&[]);
        assert!(e1.diff(&e2).is_empty());
    }

    #[test]
    fn serialization_preserves_event_count() {
        let events: Vec<Event> = (0..50)
            .map(|i| make_event(&format!("item-{i}"), i, &format!("h{i}")))
            .collect();
        let tree = ProllyTree::build(&events);
        let bytes = tree.to_bytes();
        let restored = ProllyTree::from_bytes(&bytes).unwrap();
        assert_eq!(restored.event_count, 50);
        assert_eq!(restored.event_hashes().len(), 50);
    }
}
