pub mod gset;
pub mod item_state;
pub mod lww;
pub mod merge;
pub mod orset;
pub mod state;
pub mod trace;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::hash::Hash;

/// Timestamp for Last-Write-Wins CRDT
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub struct Timestamp {
    pub wall: DateTime<Utc>,
    pub actor: u64,
    pub event_hash: u64,
    pub itc: u64, // Simplified ITC for now
}

/// Last-Write-Wins Register
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lww<T> {
    pub value: T,
    pub timestamp: Timestamp,
}

/// Grow-only Set
pub use crate::crdt::gset::GSet;

/// Observed-Remove Set (Add-Wins)
///
/// Sets serialize in sorted order, so equal sets give equal bytes (and equal
/// snapshot event hashes) on every replica (bn-1ed2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound(serialize = "T: Serialize + Ord", deserialize = "T: Deserialize<'de>"))]
pub struct OrSet<T: Hash + Eq> {
    #[serde(serialize_with = "serialize_sorted")]
    pub elements: HashSet<(T, Timestamp)>,
    #[serde(serialize_with = "serialize_sorted")]
    pub tombstone: HashSet<(T, Timestamp)>,
}

/// Serialize a `HashSet` in sorted order instead of hash iteration order.
pub(crate) fn serialize_sorted<T: Serialize + Ord, S: serde::Serializer>(
    set: &HashSet<T>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut sorted: Vec<&T> = set.iter().collect();
    sorted.sort();
    serializer.collect_seq(sorted)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Phase {
    Init,
    Propose,
    Commit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochPhase {
    pub epoch: u64,
    pub phase: Phase,
}
