//! Structural similarity between work items.
//!
//! This module computes similarity scores based on shared structural
//! properties: labels, dependencies, assignees, parent goal, and
//! proximity in the dependency graph.

mod similarity;

pub use similarity::{StructuralScore, jaccard, structural_similarity};
