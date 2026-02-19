//! Graph-level abstractions for work-item relationships.
//!
//! This module groups higher-level relational functions that operate across
//! multiple items using the SQLite projection database.
//!
//! ## Submodules
//!
//! - [`hierarchy`] — Parent-child containment model, goal progress, and
//!   reparenting validation.

pub mod hierarchy;
