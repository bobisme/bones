//! Centrality metrics for the dependency graph.
//!
//! # Overview
//!
//! This module implements advanced centrality metrics that operate on the
//! condensed (SCC-collapsed) DAG from [`crate::graph::normalize`]. Each
//! metric answers a different question about item importance:
//!
//! - **Betweenness centrality** (`betweenness`): Which items act as bridges
//!   or bottlenecks in the dependency graph?
//! - **HITS** (`hits`): Which items are authoritative (many things depend on
//!   them) vs hubs (they depend on many things)?
//! - **Eigenvector centrality** (`eigenvector`): Which items are connected
//!   to other high-centrality items?
//!
//! # Usage
//!
//! All metrics take a [`NormalizedGraph`] reference and return scores indexed
//! by the original item IDs (not condensed node indices). Items sharing an
//! SCC receive the same score (the SCC's score).
//!
//! ```rust,ignore
//! use bones_triage::graph::normalize::NormalizedGraph;
//! use bones_triage::metrics::betweenness::betweenness_centrality;
//! use bones_triage::metrics::hits::hits;
//! use bones_triage::metrics::eigenvector::eigenvector_centrality;
//!
//! let ng: NormalizedGraph = /* build graph */;
//!
//! let bc = betweenness_centrality(&ng);
//! let (hubs, authorities) = hits(&ng, 100, 1e-6);
//! let ev = eigenvector_centrality(&ng, 100, 1e-6);
//! ```

pub mod betweenness;
pub mod eigenvector;
pub mod hits;
