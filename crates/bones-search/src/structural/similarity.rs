//! Structural similarity between work items.
//!
//! # Overview
//!
//! Structural similarity captures relatedness between two work items based on
//! shared **structural properties** — labels, direct dependency neighbours,
//! assignees, shared parent goal, and proximity inside the dependency graph —
//! rather than lexical text overlap.
//!
//! ## Usage
//!
//! ```rust,ignore
//! use std::collections::HashSet;
//! use bones_search::structural::{StructuralScore, structural_similarity, jaccard};
//! use petgraph::graph::DiGraph;
//! use rusqlite::Connection;
//!
//! let conn: Connection = /* open projection db */;
//! let graph: DiGraph<String, ()> = /* build from RawGraph */;
//!
//! let score = structural_similarity("bn-001", "bn-002", &conn, &graph)?;
//! println!("label_sim={:.3}  dep_sim={:.3}  graph_proximity={:.3}",
//!          score.label_sim, score.dep_sim, score.graph_proximity);
//! ```

#![allow(clippy::module_name_repetitions)]

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result};
use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};
use rusqlite::Connection;
use rusqlite::OptionalExtension as _;

// ---------------------------------------------------------------------------
// StructuralScore
// ---------------------------------------------------------------------------

/// Per-feature structural similarity breakdown between two items.
///
/// All scores are in `[0.0, 1.0]` range.  The individual fields are kept
/// separate so downstream consumers (e.g. RRF fusion) can weight them
/// independently and show per-feature explanations to users.
#[derive(Debug, Clone, PartialEq)]
pub struct StructuralScore {
    /// Jaccard similarity of the two items' label sets.
    pub label_sim: f32,
    /// Jaccard similarity of direct dependency-neighbour sets
    /// (union of out-neighbours and in-neighbours in the graph).
    pub dep_sim: f32,
    /// Jaccard similarity of assignee/agent sets.
    pub assignee_sim: f32,
    /// `1.0` if both items share the same non-null parent goal, `0.0`
    /// otherwise.  Could be extended to graded parent-chain overlap.
    pub parent_sim: f32,
    /// Graph proximity: `1.0 / (1.0 + shortest_path_distance)` using an
    /// undirected BFS up to [`MAX_HOPS`] hops.  Items unreachable within
    /// the hop limit, or not present in the graph, score `0.0`.
    pub graph_proximity: f32,
}

impl StructuralScore {
    /// Weighted average of all feature scores.
    ///
    /// Weights are uniform for now; callers may prefer their own aggregation.
    #[must_use]
    pub fn mean(&self) -> f32 {
        (self.label_sim + self.dep_sim + self.assignee_sim + self.parent_sim + self.graph_proximity)
            / 5.0
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum BFS hop distance considered for graph proximity.
/// Items further apart than this are scored `0.0`.
const MAX_HOPS: usize = 5;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Generic Jaccard similarity: `|A ∩ B| / |A ∪ B|`.
///
/// Returns `0.0` if both sets are empty (to avoid 0/0).
///
/// # Examples
///
/// ```
/// use std::collections::HashSet;
/// use bones_search::structural::jaccard;
///
/// let a: HashSet<&str> = ["x", "y", "z"].into_iter().collect();
/// let b: HashSet<&str> = ["y", "z", "w"].into_iter().collect();
/// let sim = jaccard(&a, &b);
/// // intersection = {"y","z"} (2), union = {"x","y","z","w"} (4)
/// assert!((sim - 0.5).abs() < 1e-6);
/// ```
#[must_use]
pub fn jaccard<T: Eq + std::hash::Hash>(a: &HashSet<T>, b: &HashSet<T>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count() as f32;
    let union_size = a.union(b).count() as f32;
    if union_size == 0.0 {
        0.0
    } else {
        intersection / union_size
    }
}

/// Compute structural similarity between two items.
///
/// Queries SQLite for item metadata (labels, assignees, parent goal) and uses
/// the pre-built dependency graph for neighbour-set and BFS-proximity
/// computations.
///
/// # Parameters
///
/// - `a`, `b` — item IDs (e.g. `"bn-001"`).
/// - `db` — SQLite connection to the bones projection database.
/// - `graph` — directed dependency graph where an edge `X → Y` means "X
///   blocks Y".  Typically built with `bones_triage::graph::build::RawGraph`.
///
/// # Errors
///
/// Returns an error if any SQLite query fails.
pub fn structural_similarity(
    a: &str,
    b: &str,
    db: &Connection,
    graph: &DiGraph<String, ()>,
) -> Result<StructuralScore> {
    // -----------------------------------------------------------------------
    // 1. Labels — query item_labels table
    // -----------------------------------------------------------------------
    let labels_a = fetch_labels(db, a).with_context(|| format!("fetch labels for {a}"))?;
    let labels_b = fetch_labels(db, b).with_context(|| format!("fetch labels for {b}"))?;
    let label_sim = jaccard(&labels_a, &labels_b);

    // -----------------------------------------------------------------------
    // 2. Assignees — query item_assignees table
    // -----------------------------------------------------------------------
    let assignees_a = fetch_assignees(db, a).with_context(|| format!("fetch assignees for {a}"))?;
    let assignees_b = fetch_assignees(db, b).with_context(|| format!("fetch assignees for {b}"))?;
    let assignee_sim = jaccard(&assignees_a, &assignees_b);

    // -----------------------------------------------------------------------
    // 3. Parent goal — query items.parent_id
    // -----------------------------------------------------------------------
    let parent_a = fetch_parent(db, a).with_context(|| format!("fetch parent for {a}"))?;
    let parent_b = fetch_parent(db, b).with_context(|| format!("fetch parent for {b}"))?;
    let parent_sim = match (parent_a.as_deref(), parent_b.as_deref()) {
        (Some(pa), Some(pb)) if pa == pb => 1.0_f32,
        _ => 0.0_f32,
    };

    // -----------------------------------------------------------------------
    // 4. Build node lookup from petgraph
    // -----------------------------------------------------------------------
    // O(n) scan — acceptable at the scale of bones; callers that need to
    // batch many similarity queries should pre-build the map and pass it in.
    let node_map: HashMap<&str, NodeIndex> = graph
        .node_indices()
        .filter_map(|idx| graph.node_weight(idx).map(|s| (s.as_str(), idx)))
        .collect();

    // -----------------------------------------------------------------------
    // 5. Dep-neighbour Jaccard — undirected direct neighbours in the graph
    // -----------------------------------------------------------------------
    let neighbours_a = direct_neighbours(graph, &node_map, a);
    let neighbours_b = direct_neighbours(graph, &node_map, b);
    let dep_sim = jaccard(&neighbours_a, &neighbours_b);

    // -----------------------------------------------------------------------
    // 6. Graph proximity — BFS in undirected view, up to MAX_HOPS
    // -----------------------------------------------------------------------
    let graph_proximity = match bfs_distance(graph, &node_map, a, b, MAX_HOPS) {
        Some(dist) => 1.0_f32 / (1.0_f32 + dist as f32),
        None => 0.0_f32,
    };

    Ok(StructuralScore {
        label_sim,
        dep_sim,
        assignee_sim,
        parent_sim,
        graph_proximity,
    })
}

// ---------------------------------------------------------------------------
// SQLite helpers
// ---------------------------------------------------------------------------

/// Fetch the label set for one item from `item_labels`.
fn fetch_labels(db: &Connection, item_id: &str) -> Result<HashSet<String>> {
    let mut stmt = db
        .prepare_cached(
            "SELECT label
             FROM item_labels
             WHERE item_id = ?1",
        )
        .context("prepare fetch_labels")?;

    let labels = stmt
        .query_map([item_id], |row| row.get::<_, String>(0))
        .context("execute fetch_labels")?
        .collect::<Result<HashSet<_>, _>>()
        .context("collect labels")?;

    Ok(labels)
}

/// Fetch the assignee set for one item from `item_assignees`.
fn fetch_assignees(db: &Connection, item_id: &str) -> Result<HashSet<String>> {
    let mut stmt = db
        .prepare_cached(
            "SELECT agent
             FROM item_assignees
             WHERE item_id = ?1",
        )
        .context("prepare fetch_assignees")?;

    let agents = stmt
        .query_map([item_id], |row| row.get::<_, String>(0))
        .context("execute fetch_assignees")?
        .collect::<Result<HashSet<_>, _>>()
        .context("collect assignees")?;

    Ok(agents)
}

/// Fetch the `parent_id` for one item from the `items` table.
/// Returns `None` if the item has no parent or is not found.
fn fetch_parent(db: &Connection, item_id: &str) -> Result<Option<String>> {
    let mut stmt = db
        .prepare_cached(
            "SELECT parent_id
             FROM items
             WHERE item_id = ?1 AND is_deleted = 0",
        )
        .context("prepare fetch_parent")?;

    let parent = stmt
        .query_row([item_id], |row| row.get::<_, Option<String>>(0))
        .optional()
        .context("execute fetch_parent")?
        .flatten(); // flatten Option<Option<String>> → Option<String>

    Ok(parent)
}

// ---------------------------------------------------------------------------
// Graph helpers
// ---------------------------------------------------------------------------

/// Collect the set of direct (undirected) neighbour IDs for one item.
///
/// Neighbour IDs are cloned from graph node weights and stored as `String`.
/// Items not present in the graph produce an empty set.
fn direct_neighbours<'g>(
    graph: &'g DiGraph<String, ()>,
    node_map: &HashMap<&str, NodeIndex>,
    item_id: &str,
) -> HashSet<&'g str> {
    let Some(&idx) = node_map.get(item_id) else {
        return HashSet::new();
    };

    // Collect out-neighbours (items this item blocks) and in-neighbours
    // (items that block this item) for a symmetric neighbour set.
    graph
        .neighbors_directed(idx, Direction::Outgoing)
        .chain(graph.neighbors_directed(idx, Direction::Incoming))
        .filter_map(|n_idx| graph.node_weight(n_idx).map(String::as_str))
        .collect()
}

/// BFS shortest-path distance in the **undirected** view of `graph`.
///
/// Explores both outgoing and incoming edges at each step so that directionality
/// does not obscure structural proximity.  Stops after `max_hops` hops.
///
/// Returns `Some(0)` when `from == to`.
/// Returns `None` when no path exists within `max_hops`.
fn bfs_distance(
    graph: &DiGraph<String, ()>,
    node_map: &HashMap<&str, NodeIndex>,
    from: &str,
    to: &str,
    max_hops: usize,
) -> Option<usize> {
    // Items not in the graph are unreachable.
    let &start = node_map.get(from)?;
    let &end = node_map.get(to)?;

    if start == end {
        return Some(0);
    }

    let mut visited: HashSet<NodeIndex> = HashSet::new();
    let mut queue: VecDeque<(NodeIndex, usize)> = VecDeque::new();

    visited.insert(start);
    queue.push_back((start, 0));

    while let Some((current, dist)) = queue.pop_front() {
        if dist >= max_hops {
            // No point exploring further — any neighbours would exceed the limit.
            continue;
        }

        // Undirected BFS: follow edges in both directions.
        let next_dist = dist + 1;
        for neighbour in graph
            .neighbors_directed(current, Direction::Outgoing)
            .chain(graph.neighbors_directed(current, Direction::Incoming))
        {
            if neighbour == end {
                return Some(next_dist);
            }
            if visited.insert(neighbour) {
                queue.push_back((neighbour, next_dist));
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db::migrations;
    use petgraph::graph::DiGraph;
    use rusqlite::params;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn setup_db() -> rusqlite::Connection {
        let mut conn = rusqlite::Connection::open_in_memory().expect("in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        conn
    }

    fn insert_item(conn: &rusqlite::Connection, item_id: &str) {
        conn.execute(
            "INSERT INTO items (
                item_id, title, kind, state, urgency, is_deleted,
                created_at_us, updated_at_us
             ) VALUES (?1, ?1, 'task', 'open', 'default', 0, 1000, 1000)",
            params![item_id],
        )
        .expect("insert item");
    }

    fn insert_item_with_parent(conn: &rusqlite::Connection, item_id: &str, parent_id: &str) {
        conn.execute(
            "INSERT INTO items (
                item_id, title, kind, state, urgency, is_deleted,
                parent_id, created_at_us, updated_at_us
             ) VALUES (?1, ?1, 'task', 'open', 'default', 0, ?2, 1000, 1000)",
            params![item_id, parent_id],
        )
        .expect("insert item with parent");
    }

    fn insert_label(conn: &rusqlite::Connection, item_id: &str, label: &str) {
        conn.execute(
            "INSERT INTO item_labels (item_id, label, created_at_us) VALUES (?1, ?2, 1000)",
            params![item_id, label],
        )
        .expect("insert label");
    }

    fn insert_assignee(conn: &rusqlite::Connection, item_id: &str, agent: &str) {
        conn.execute(
            "INSERT INTO item_assignees (item_id, agent, created_at_us) VALUES (?1, ?2, 1000)",
            params![item_id, agent],
        )
        .expect("insert assignee");
    }

    fn insert_dep(conn: &rusqlite::Connection, blocked: &str, blocker: &str) {
        conn.execute(
            "INSERT INTO item_dependencies (item_id, depends_on_item_id, link_type, created_at_us)
             VALUES (?1, ?2, 'blocks', 1000)",
            params![blocked, blocker],
        )
        .expect("insert dep");
    }

    fn empty_graph() -> DiGraph<String, ()> {
        DiGraph::new()
    }

    /// Build a graph from an edge list `(blocker, blocked)`.
    fn graph_from_edges(edges: &[(&str, &str)]) -> DiGraph<String, ()> {
        let mut graph = DiGraph::new();
        let mut node_map: HashMap<String, NodeIndex> = HashMap::new();

        for &(blocker, blocked) in edges {
            let blocker_idx = *node_map
                .entry(blocker.to_owned())
                .or_insert_with(|| graph.add_node(blocker.to_owned()));
            let blocked_idx = *node_map
                .entry(blocked.to_owned())
                .or_insert_with(|| graph.add_node(blocked.to_owned()));
            if !graph.contains_edge(blocker_idx, blocked_idx) {
                graph.add_edge(blocker_idx, blocked_idx, ());
            }
        }
        graph
    }

    // -----------------------------------------------------------------------
    // jaccard()
    // -----------------------------------------------------------------------

    #[test]
    fn jaccard_empty_sets_returns_zero() {
        let a: HashSet<&str> = HashSet::new();
        let b: HashSet<&str> = HashSet::new();
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    #[test]
    fn jaccard_identical_sets_returns_one() {
        let a: HashSet<&str> = ["x", "y", "z"].into_iter().collect();
        let b: HashSet<&str> = ["x", "y", "z"].into_iter().collect();
        assert!((jaccard(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn jaccard_disjoint_sets_returns_zero() {
        let a: HashSet<&str> = ["x"].into_iter().collect();
        let b: HashSet<&str> = ["y"].into_iter().collect();
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    #[test]
    fn jaccard_partial_overlap() {
        // |{y,z}| / |{x,y,z,w}| = 2/4 = 0.5
        let a: HashSet<&str> = ["x", "y", "z"].into_iter().collect();
        let b: HashSet<&str> = ["y", "z", "w"].into_iter().collect();
        assert!((jaccard(&a, &b) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn jaccard_one_empty_set_returns_zero() {
        let a: HashSet<&str> = ["x", "y"].into_iter().collect();
        let b: HashSet<&str> = HashSet::new();
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    // -----------------------------------------------------------------------
    // label_sim
    // -----------------------------------------------------------------------

    #[test]
    fn label_sim_shared_labels() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");
        insert_label(&conn, "bn-001", "auth");
        insert_label(&conn, "bn-001", "backend");
        insert_label(&conn, "bn-002", "auth");
        insert_label(&conn, "bn-002", "frontend");

        let score =
            structural_similarity("bn-001", "bn-002", &conn, &empty_graph()).expect("score");
        // intersection={auth}(1), union={auth,backend,frontend}(3) → 1/3
        assert!(
            (score.label_sim - (1.0_f32 / 3.0)).abs() < 1e-6,
            "{score:?}"
        );
    }

    #[test]
    fn label_sim_no_labels_is_zero() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");

        let score =
            structural_similarity("bn-001", "bn-002", &conn, &empty_graph()).expect("score");
        assert_eq!(score.label_sim, 0.0);
    }

    #[test]
    fn label_sim_identical_labels_is_one() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");
        insert_label(&conn, "bn-001", "auth");
        insert_label(&conn, "bn-002", "auth");

        let score =
            structural_similarity("bn-001", "bn-002", &conn, &empty_graph()).expect("score");
        assert!((score.label_sim - 1.0).abs() < 1e-6);
    }

    // -----------------------------------------------------------------------
    // assignee_sim
    // -----------------------------------------------------------------------

    #[test]
    fn assignee_sim_shared_agent() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");
        insert_assignee(&conn, "bn-001", "alice");
        insert_assignee(&conn, "bn-002", "alice");
        insert_assignee(&conn, "bn-002", "bob");

        let score =
            structural_similarity("bn-001", "bn-002", &conn, &empty_graph()).expect("score");
        // intersection={alice}(1), union={alice,bob}(2) → 0.5
        assert!((score.assignee_sim - 0.5).abs() < 1e-6);
    }

    #[test]
    fn assignee_sim_no_assignees_is_zero() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");

        let score =
            structural_similarity("bn-001", "bn-002", &conn, &empty_graph()).expect("score");
        assert_eq!(score.assignee_sim, 0.0);
    }

    // -----------------------------------------------------------------------
    // parent_sim
    // -----------------------------------------------------------------------

    #[test]
    fn parent_sim_shared_parent_is_one() {
        let conn = setup_db();
        insert_item(&conn, "bn-goal");
        insert_item_with_parent(&conn, "bn-001", "bn-goal");
        insert_item_with_parent(&conn, "bn-002", "bn-goal");

        let score =
            structural_similarity("bn-001", "bn-002", &conn, &empty_graph()).expect("score");
        assert!((score.parent_sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn parent_sim_different_parents_is_zero() {
        let conn = setup_db();
        insert_item(&conn, "bn-goal-a");
        insert_item(&conn, "bn-goal-b");
        insert_item_with_parent(&conn, "bn-001", "bn-goal-a");
        insert_item_with_parent(&conn, "bn-002", "bn-goal-b");

        let score =
            structural_similarity("bn-001", "bn-002", &conn, &empty_graph()).expect("score");
        assert_eq!(score.parent_sim, 0.0);
    }

    #[test]
    fn parent_sim_no_parent_is_zero() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");

        let score =
            structural_similarity("bn-001", "bn-002", &conn, &empty_graph()).expect("score");
        assert_eq!(score.parent_sim, 0.0);
    }

    // -----------------------------------------------------------------------
    // dep_sim
    // -----------------------------------------------------------------------

    #[test]
    fn dep_sim_shared_dependency_neighbour() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");
        insert_item(&conn, "bn-common");
        // Both bn-001 and bn-002 block bn-common
        insert_dep(&conn, "bn-common", "bn-001");
        insert_dep(&conn, "bn-common", "bn-002");

        // Graph: bn-001 → bn-common, bn-002 → bn-common
        let graph = graph_from_edges(&[("bn-001", "bn-common"), ("bn-002", "bn-common")]);

        let score = structural_similarity("bn-001", "bn-002", &conn, &graph).expect("score");
        // neighbours(bn-001) = {bn-common}, neighbours(bn-002) = {bn-common}
        // intersection=1, union=1 → dep_sim=1.0
        assert!((score.dep_sim - 1.0).abs() < 1e-6, "{score:?}");
    }

    #[test]
    fn dep_sim_no_shared_neighbours_is_zero() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");
        insert_item(&conn, "bn-dep-a");
        insert_item(&conn, "bn-dep-b");
        insert_dep(&conn, "bn-dep-a", "bn-001");
        insert_dep(&conn, "bn-dep-b", "bn-002");

        let graph = graph_from_edges(&[("bn-001", "bn-dep-a"), ("bn-002", "bn-dep-b")]);

        let score = structural_similarity("bn-001", "bn-002", &conn, &graph).expect("score");
        assert_eq!(score.dep_sim, 0.0);
    }

    // -----------------------------------------------------------------------
    // graph_proximity
    // -----------------------------------------------------------------------

    #[test]
    fn graph_proximity_direct_edge_is_half() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");

        // Direct edge: bn-001 → bn-002, distance=1 → 1/(1+1)=0.5
        let graph = graph_from_edges(&[("bn-001", "bn-002")]);
        let score = structural_similarity("bn-001", "bn-002", &conn, &graph).expect("score");
        assert!((score.graph_proximity - 0.5).abs() < 1e-6, "{score:?}");
    }

    #[test]
    fn graph_proximity_two_hops() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");
        insert_item(&conn, "bn-mid");

        // bn-001 → bn-mid → bn-002, distance=2 → 1/(1+2) ≈ 0.333
        let graph = graph_from_edges(&[("bn-001", "bn-mid"), ("bn-mid", "bn-002")]);
        let score = structural_similarity("bn-001", "bn-002", &conn, &graph).expect("score");
        assert!(
            (score.graph_proximity - (1.0_f32 / 3.0)).abs() < 1e-6,
            "{score:?}"
        );
    }

    #[test]
    fn graph_proximity_beyond_max_hops_is_zero() {
        let conn = setup_db();
        // Chain longer than MAX_HOPS (5): a→b→c→d→e→f→g (6 hops a to g)
        let ids = [
            "bn-a01", "bn-a02", "bn-a03", "bn-a04", "bn-a05", "bn-a06", "bn-a07",
        ];
        for id in ids {
            insert_item(&conn, id);
        }
        let edges: Vec<(&str, &str)> = ids.windows(2).map(|w| (w[0], w[1])).collect();
        let graph = graph_from_edges(&edges);

        let score = structural_similarity("bn-a01", "bn-a07", &conn, &graph).expect("score");
        assert_eq!(
            score.graph_proximity, 0.0,
            "6-hop path should exceed MAX_HOPS=5"
        );
    }

    #[test]
    fn graph_proximity_disconnected_is_zero() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");

        let graph = graph_from_edges(&[]);
        // Nodes not in graph → proximity=0
        let score = structural_similarity("bn-001", "bn-002", &conn, &graph).expect("score");
        assert_eq!(score.graph_proximity, 0.0);
    }

    #[test]
    fn graph_proximity_reversed_edge_reachable() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        insert_item(&conn, "bn-002");

        // Edge goes bn-002 → bn-001 (reversed); BFS is undirected so still 1 hop
        let graph = graph_from_edges(&[("bn-002", "bn-001")]);
        let score = structural_similarity("bn-001", "bn-002", &conn, &graph).expect("score");
        assert!((score.graph_proximity - 0.5).abs() < 1e-6, "{score:?}");
    }

    // -----------------------------------------------------------------------
    // StructuralScore::mean()
    // -----------------------------------------------------------------------

    #[test]
    fn mean_is_arithmetic_average() {
        let s = StructuralScore {
            label_sim: 1.0,
            dep_sim: 0.5,
            assignee_sim: 0.0,
            parent_sim: 1.0,
            graph_proximity: 0.5,
        };
        assert!((s.mean() - 0.6).abs() < 1e-6);
    }

    // -----------------------------------------------------------------------
    // bfs_distance edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn bfs_same_item_is_distance_zero() {
        let conn = setup_db();
        insert_item(&conn, "bn-001");
        let graph = graph_from_edges(&[("bn-001", "bn-002")]);
        // Same item: score = 1/(1+0) = 1.0
        let score = structural_similarity("bn-001", "bn-001", &conn, &graph).expect("score");
        assert!((score.graph_proximity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn bfs_exactly_max_hops_is_reachable() {
        let conn = setup_db();
        // Chain of MAX_HOPS=5 hops: a→b→c→d→e→f (5 hops, a to f)
        let ids = ["bn-b01", "bn-b02", "bn-b03", "bn-b04", "bn-b05", "bn-b06"];
        for id in ids {
            insert_item(&conn, id);
        }
        let edges: Vec<(&str, &str)> = ids.windows(2).map(|w| (w[0], w[1])).collect();
        let graph = graph_from_edges(&edges);

        let score = structural_similarity("bn-b01", "bn-b06", &conn, &graph).expect("score");
        // distance=5 → 1/(1+5) ≈ 0.1667
        assert!(
            score.graph_proximity > 0.0,
            "5-hop path (=MAX_HOPS) should be reachable, got {score:?}"
        );
        assert!((score.graph_proximity - (1.0_f32 / 6.0)).abs() < 1e-5);
    }
}
