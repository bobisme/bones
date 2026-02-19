//! Graph construction from SQLite projection database.
//!
//! # Overview
//!
//! This module queries the `item_dependencies` table in the SQLite projection
//! database and builds a [`petgraph`] directed graph suitable for triage
//! computations (centrality metrics, cycle detection, scheduling analysis).
//!
//! ## Edge Direction
//!
//! An edge `A → B` in the graph means "A **blocks** B" — A must be completed
//! before B can start. This matches the `item_dependencies` schema where:
//!
//! ```sql
//! item_id         -- the dependent (blocked item)
//! depends_on_item_id  -- the dependency (the blocker)
//! ```
//!
//! So for each row `(item_id=B, depends_on_item_id=A, link_type='blocks')`
//! we insert edge `A → B`.
//!
//! ## Cache Invalidation
//!
//! The graph is associated with a content hash of the edge set (BLAKE3 of
//! the sorted edge list). Callers can compare the hash against a stored
//! value to avoid rebuilding the graph on every access.
//!
//! ## Only Blocking Edges
//!
//! Only `link_type = 'blocks'` edges are included in the dependency graph.
//! Informational `related_to` links are excluded.

#![allow(clippy::module_name_repetitions)]

use std::collections::HashMap;

use anyhow::{Context, Result};
use petgraph::graph::{DiGraph, NodeIndex};
use rusqlite::Connection;
use tracing::instrument;

// ---------------------------------------------------------------------------
// RawGraph
// ---------------------------------------------------------------------------

/// A directed dependency graph built from SQLite.
///
/// Nodes are item IDs (strings). An edge `A → B` means "A blocks B".
///
/// `RawGraph` preserves all edges as stored in the projection database,
/// including any cycles that may exist due to concurrent or inconsistent
/// link events. Use [`crate::graph::normalize`] to condense SCCs and
/// optionally reduce transitive edges.
#[derive(Debug)]
pub struct RawGraph {
    /// Directed graph: nodes = item IDs, edges = blocking relationships.
    pub graph: DiGraph<String, ()>,
    /// Mapping from item ID to petgraph `NodeIndex`.
    pub node_map: HashMap<String, NodeIndex>,
    /// BLAKE3 content hash of the edge set used for cache invalidation.
    pub content_hash: String,
}

impl RawGraph {
    /// Build a [`RawGraph`] by querying `item_dependencies` in `conn`.
    ///
    /// Only rows where `link_type IN ('blocks', 'blocked_by')` are used.
    /// All non-deleted items from the `items` table are included as nodes
    /// (even those with no dependencies) so downstream metrics see the
    /// full node set.
    ///
    /// The content hash is derived from the sorted list of `(blocker, blocked)`
    /// pairs, so it changes only when edges change.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite query fails.
    #[instrument(skip(conn))]
    pub fn from_sqlite(conn: &Connection) -> Result<Self> {
        // Step 1: load all non-deleted item IDs as graph nodes.
        let item_ids = load_item_ids(conn)?;

        let mut graph = DiGraph::<String, ()>::new();
        let mut node_map: HashMap<String, NodeIndex> = HashMap::with_capacity(item_ids.len());

        for id in item_ids {
            let idx = graph.add_node(id.clone());
            node_map.insert(id, idx);
        }

        // Step 2: load blocking edges and add them to the graph.
        let edges = load_blocking_edges(conn)?;

        // Compute content hash before mutating graph.
        let content_hash = compute_edge_hash(&edges);

        for (blocker, blocked) in edges {
            // If either endpoint is not already a node, add it.
            // This handles references to items not in the items table
            // (e.g., deleted items still referenced in dependencies).
            let blocker_idx = *node_map.entry(blocker.clone()).or_insert_with(|| {
                graph.add_node(blocker.clone())
            });
            let blocked_idx = *node_map.entry(blocked.clone()).or_insert_with(|| {
                graph.add_node(blocked.clone())
            });

            // Avoid duplicate edges (petgraph allows them by default).
            if !graph.contains_edge(blocker_idx, blocked_idx) {
                graph.add_edge(blocker_idx, blocked_idx, ());
            }
        }

        Ok(Self {
            graph,
            node_map,
            content_hash,
        })
    }

    /// Return the number of nodes (items) in the graph.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Return the number of edges (blocking relationships) in the graph.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    /// Look up the `NodeIndex` for an item ID.
    #[must_use]
    pub fn node_index(&self, item_id: &str) -> Option<NodeIndex> {
        self.node_map.get(item_id).copied()
    }

    /// Return the item ID label for a node.
    #[must_use]
    pub fn item_id(&self, idx: NodeIndex) -> Option<&str> {
        self.graph.node_weight(idx).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Load all non-deleted item IDs from the projection database.
fn load_item_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT item_id FROM items WHERE is_deleted = 0")
        .context("prepare item_ids query")?;

    let ids = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("execute item_ids query")?
        .collect::<Result<Vec<_>, _>>()
        .context("collect item_ids")?;

    Ok(ids)
}

/// Load blocking dependency edges from `item_dependencies`.
///
/// Returns `Vec<(blocker_id, blocked_id)>` where `blocker_id → blocked_id`.
fn load_blocking_edges(conn: &Connection) -> Result<Vec<(String, String)>> {
    // link_type 'blocks': depends_on_item_id blocks item_id
    // link_type 'blocked_by': item_id is blocked_by depends_on_item_id
    // Both map to the same edge direction: depends_on_item_id → item_id
    let mut stmt = conn
        .prepare(
            "SELECT depends_on_item_id, item_id
             FROM item_dependencies
             WHERE link_type IN ('blocks', 'blocked_by')
             ORDER BY depends_on_item_id, item_id",
        )
        .context("prepare blocking_edges query")?;

    let edges = stmt
        .query_map([], |row| {
            let blocker: String = row.get(0)?;
            let blocked: String = row.get(1)?;
            Ok((blocker, blocked))
        })
        .context("execute blocking_edges query")?
        .collect::<Result<Vec<_>, _>>()
        .context("collect blocking edges")?;

    Ok(edges)
}

/// Compute a BLAKE3 hash of the sorted edge list for cache invalidation.
fn compute_edge_hash(edges: &[(String, String)]) -> String {
    let mut hasher = blake3::Hasher::new();
    for (blocker, blocked) in edges {
        hasher.update(blocker.as_bytes());
        hasher.update(b"\x00");
        hasher.update(blocked.as_bytes());
        hasher.update(b"\x00");
    }
    format!("blake3:{}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db::migrations;
    use rusqlite::params;

    fn setup_db() -> rusqlite::Connection {
        let mut conn = rusqlite::Connection::open_in_memory().expect("in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        conn
    }

    fn insert_item(conn: &rusqlite::Connection, item_id: &str) {
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, created_at_us, updated_at_us)
             VALUES (?1, ?1, 'task', 'open', 'default', 0, 1000, 1000)",
            params![item_id],
        )
        .expect("insert item");
    }

    fn insert_dep(conn: &rusqlite::Connection, item_id: &str, depends_on: &str) {
        conn.execute(
            "INSERT INTO item_dependencies (item_id, depends_on_item_id, link_type, created_at_us)
             VALUES (?1, ?2, 'blocks', 1000)",
            params![item_id, depends_on],
        )
        .expect("insert dependency");
    }

    #[test]
    fn empty_db_produces_empty_graph() {
        let conn = setup_db();
        let graph = RawGraph::from_sqlite(&conn).expect("build graph");
        assert_eq!(graph.node_count(), 0);
        assert_eq!(graph.edge_count(), 0);
        // Hash of empty edge set is stable.
        assert!(graph.content_hash.starts_with("blake3:"));
    }

    #[test]
    fn items_without_deps_are_nodes_only() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");

        let graph = RawGraph::from_sqlite(&conn).expect("build graph");
        assert_eq!(graph.node_count(), 2);
        assert_eq!(graph.edge_count(), 0);
        assert!(graph.node_index("bn-001").is_some());
        assert!(graph.node_index("bn-002").is_some());
    }

    #[test]
    fn single_blocking_edge_direction() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");
        // bn-002 depends on bn-001 → bn-001 blocks bn-002
        insert_dep(&conn, "bn-002", "bn-001");

        let graph = RawGraph::from_sqlite(&conn).expect("build graph");
        assert_eq!(graph.edge_count(), 1);

        let a = graph.node_index("bn-001").expect("bn-001 node");
        let b = graph.node_index("bn-002").expect("bn-002 node");
        // Edge should go bn-001 → bn-002 (blocker → blocked)
        assert!(graph.graph.contains_edge(a, b), "expected bn-001 → bn-002");
        assert!(!graph.graph.contains_edge(b, a), "no reverse edge");
    }

    #[test]
    fn deleted_items_excluded_as_nodes() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, created_at_us, updated_at_us)
             VALUES ('bn-deleted', 'deleted', 'task', 'open', 'default', 1, 1000, 1000)",
            [],
        )
        .expect("insert deleted item");

        let graph = RawGraph::from_sqlite(&conn).expect("build graph");
        assert_eq!(graph.node_count(), 1);
        assert!(graph.node_index("bn-001").is_some());
        assert!(graph.node_index("bn-deleted").is_none());
    }

    #[test]
    fn duplicate_edges_not_added() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");
        insert_dep(&conn, "bn-002", "bn-001");

        // Add another dependency with different link_type (blocked_by)
        // pointing to the same logical edge — should still be 1 graph edge.
        conn.execute(
            "INSERT INTO item_dependencies (item_id, depends_on_item_id, link_type, created_at_us)
             VALUES ('bn-002', 'bn-001', 'blocked_by', 2000)",
            [],
        )
        .expect("insert blocked_by dep");

        let graph = RawGraph::from_sqlite(&conn).expect("build graph");
        // Both 'blocks' and 'blocked_by' map to same directed edge, deduplicated.
        assert_eq!(graph.edge_count(), 1);
    }

    #[test]
    fn content_hash_changes_with_edges() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");

        let empty_hash = RawGraph::from_sqlite(&conn)
            .expect("build graph")
            .content_hash;

        insert_dep(&conn, "bn-002", "bn-001");
        let with_edge_hash = RawGraph::from_sqlite(&conn)
            .expect("build graph")
            .content_hash;

        assert_ne!(empty_hash, with_edge_hash, "hash must change when edges added");
    }

    #[test]
    fn chain_of_deps() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");
        insert_item(&conn, "bn-003");
        insert_dep(&conn, "bn-002", "bn-001"); // bn-001 → bn-002
        insert_dep(&conn, "bn-003", "bn-002"); // bn-002 → bn-003

        let graph = RawGraph::from_sqlite(&conn).expect("build graph");
        assert_eq!(graph.node_count(), 3);
        assert_eq!(graph.edge_count(), 2);

        let n1 = graph.node_index("bn-001").unwrap();
        let n2 = graph.node_index("bn-002").unwrap();
        let n3 = graph.node_index("bn-003").unwrap();
        assert!(graph.graph.contains_edge(n1, n2));
        assert!(graph.graph.contains_edge(n2, n3));
    }
}
