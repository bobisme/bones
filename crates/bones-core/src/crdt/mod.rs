pub mod gset;
pub mod item_state;
pub mod lww;
pub mod merge;
pub mod orset;
pub mod state;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrSet<T: Hash + Eq> {
    pub elements: HashSet<(T, Timestamp)>,
    pub tombstone: HashSet<(T, Timestamp)>,
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
