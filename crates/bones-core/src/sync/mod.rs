//! Synchronisation helpers for bones event shards.
//!
//! This module provides:
//!
//! - [`merge`] — logic for combining divergent `.events` shard files.
//! - [`prolly`] — content-defined Merkle tree for O(log N) event set diffing.
//! - [`protocol`] — transport-agnostic 3-round sync protocol built on prolly trees.
//!
//! The prolly tree and protocol modules are **library APIs** for external sync
//! tools. bones does not own transport — tools like `maw`, custom MCP servers,
//! or direct TCP/HTTP services implement [`protocol::SyncTransport`] and call
//! the sync functions.

pub mod merge;
pub mod prolly;
pub mod protocol;
