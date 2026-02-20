//! Fusion of multiple search signals into a composite duplicate risk score.
//!
//! This module combines results from lexical (FTS5), semantic (KNN), and structural
//! similarity signals using Reciprocal Rank Fusion (RRF) to produce a final duplicate
//! risk classification.

pub mod hybrid;
pub mod scoring;

pub use hybrid::{HybridSearchResult, hybrid_search};
pub use scoring::{
    DupCandidate, DuplicateRisk, SearchConfig, build_dup_candidates, classify_risk, rrf_fuse,
};
