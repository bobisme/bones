//! Synchronisation helpers for bones event shards.
//!
//! This module provides merge logic for combining divergent `.events` shard
//! files produced by concurrent agents or git branches.

pub mod merge;
