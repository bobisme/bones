//! Graph-level abstractions for work-item relationships.
//!
//! This module groups higher-level relational functions that operate across
//! multiple items using the SQLite projection database and CRDT state.
//!
//! ## Submodules
//!
//! - [`hierarchy`] — Parent-child containment model, goal progress, and
//!   reparenting validation.
//! - [`blocking`] — Blocking dependency graph and relates links.
//!
//! [`WorkItemState`]: crate::crdt::item_state::WorkItemState

pub mod blocking;
pub mod cycles;
pub mod hierarchy;
