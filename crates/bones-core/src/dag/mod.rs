//! Eg-Walker event DAG with causal parent tracking.
//!
//! This module implements the Merkle-DAG structure used for content-addressed
//! event storage and causal ordering in the bones event log.
//!
//! # DAG Properties
//!
//! - **Content-addressed identity**: every event is identified by a BLAKE3
//!   hash of its fields, including its parent hashes.
//! - **Causal ordering**: parent hash references encode happens-before
//!   relationships, independent of wall-clock time.
//! - **Merkle integrity**: modifying any event changes its hash and therefore
//!   invalidates all descendant hashes, enabling tamper detection.
//! - **Linear events** have one parent; **merge points** have two (or more).
//!   Root events have no parents.
//!
//! # Sub-modules
//!
//! - [`hash`]: Hash verification and parent chain validation.
//!   ([`verify_event_hash`], [`verify_chain`])
//! - [`graph`]: In-memory DAG structure with parent/descendant traversal.
//!   ([`EventDag`], [`DagNode`])
//! - [`lca`]: Lowest Common Ancestor finding for divergent branches.
//!   ([`find_lca`], [`find_all_lcas`])
//! - [`replay`]: Divergent-branch replay for CRDT state reconstruction.
//!   ([`replay_divergent`], [`replay_divergent_for_item`])
//!
//! # Related Modules
//!
//! - [`crate::event::writer::compute_event_hash`]: Computes the BLAKE3 hash
//!   for an event. Used internally by [`hash::verify_event_hash`] and
//!   [`hash::verify_chain`].

pub mod graph;
pub mod hash;
pub mod lca;
pub mod replay;
