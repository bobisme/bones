//! Dependency graph module for triage computation.
//!
//! # Overview
//!
//! This module constructs and normalizes a petgraph-based directed dependency
//! graph from the SQLite projection database. The graph feeds into all
//! centrality metrics and scheduling computations in the triage engine.
//!
//! ## Pipeline
//!
//! ```text
//! SQLite item_dependencies
//!        ↓  build::RawGraph::from_sqlite()
//! RawGraph (DiGraph with possible cycles)
//!        ↓  normalize::NormalizedGraph::from_raw()
//! NormalizedGraph
//!   ├─ condensed: SCCs collapsed to single nodes (DAG)
//!   └─ reduced:  transitively-reduced DAG
//!        ↓  stats::GraphStats::from_normalized()
//! GraphStats (density, component count, cycle count, …)
//! ```
//!
//! ## Cache Invalidation
//!
//! [`RawGraph::content_hash`] is a BLAKE3 hash of the edge set. Compare it
//! against a stored value to detect when the graph needs to be rebuilt.
//!
//! ## Typical Usage
//!
//! ```rust,ignore
//! use rusqlite::Connection;
//! use bones_triage::graph::{build::RawGraph, normalize::NormalizedGraph, stats::GraphStats};
//!
//! let conn: Connection = /* open projection db */;
//! let raw = RawGraph::from_sqlite(&conn)?;
//! let ng  = NormalizedGraph::from_raw(raw);
//! let stats = GraphStats::from_normalized(&ng);
//!
//! println!("nodes={} edges={} density={:.3} cycles={}",
//!     stats.node_count, stats.edge_count, stats.density, stats.cycle_count);
//! ```

pub mod build;
pub mod cycles;
pub mod normalize;
pub mod stats;

// Re-export primary types at module level for convenience.
pub use build::RawGraph;
pub use cycles::{find_all_cycles, would_create_cycle};
pub use normalize::{NormalizedGraph, SccNode};
pub use stats::GraphStats;
