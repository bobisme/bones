//! Parent-child containment model and goal progress computation.
//!
//! This module provides higher-level query functions that operate on the
//! SQLite projection database to answer hierarchy questions:
//!
//! - Which items are children of a given goal?
//! - What is a goal's progress (done/total direct children)?
//! - What is a goal's recursive progress (rolling up through nested goals)?
//! - Is a reparenting operation valid?
//! - What is the full subtree of a given item?
//! - What are the ancestors of a given item?
//!
//! # Terminology
//!
//! - **Parent**: An item whose `parent_id` is null (root) or set to another item.
//! - **Goal**: An item with `kind = 'goal'`. Only goals may have children
//!   (reparenting to a non-goal is rejected).
//! - **Progress**: The ratio of done (or archived) children to total non-deleted
//!   children. Nested goals contribute their own progress recursively.
//!
//! # Cycle prevention
//!
//! `validate_reparent` checks that the proposed new parent is not a descendant
//! of the item being moved, preventing reference cycles.
//!
//! # Error handling
//!
//! All functions return [`HierarchyError`], which distinguishes between
//! domain errors (wrong kind, cycle detected, item not found) and database
//! errors.

#![allow(
    clippy::must_use_candidate,
    clippy::module_name_repetitions,
    clippy::doc_markdown,
)]

use anyhow::Context as AnyhowContext;
use rusqlite::Connection;
use std::collections::{HashSet, VecDeque};
use std::fmt;

use crate::db::query::{self, QueryItem};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Progress of a goal: how many children are done vs total.
///
/// A child is **done** when its state is `'done'` or `'archived'`.
/// In-progress children have state `'doing'`. Open children are counted
/// in `total` but not in `done` or `in_progress`.
///
/// For nested goals, `compute_nested_progress` includes the leaf-item counts
/// across the entire subtree, rolling up through intermediate goal nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalProgress {
    /// Number of children (or leaf items in subtree) in the done/archived state.
    pub done: u32,
    /// Number of children (or leaf items in subtree) in the doing state.
    pub in_progress: u32,
    /// Total non-deleted children (or leaf items in subtree).
    pub total: u32,
}

impl GoalProgress {
    /// Create a new zeroed `GoalProgress`.
    pub fn zero() -> Self {
        Self {
            done: 0,
            in_progress: 0,
            total: 0,
        }
    }

    /// Percentage of work completed, in the range `0.0..=100.0`.
    ///
    /// Returns `100.0` if total is 0 (vacuously complete).
    pub fn percent_complete(&self) -> f32 {
        if self.total == 0 {
            return 100.0;
        }
        (self.done as f32 / self.total as f32) * 100.0
    }

    /// Returns `true` if all children are done (or there are no children).
    pub fn is_complete(&self) -> bool {
        self.total == 0 || self.done == self.total
    }

    /// Number of children that are not yet done.
    pub fn remaining(&self) -> u32 {
        self.total.saturating_sub(self.done)
    }
}

impl fmt::Display for GoalProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{} ({:.0}%)",
            self.done,
            self.total,
            self.percent_complete()
        )
    }
}

/// Errors that can occur in hierarchy operations.
#[derive(Debug)]
pub enum HierarchyError {
    /// The target parent item is not of kind `goal`. Reparenting to non-goals
    /// is rejected.
    NotAGoal {
        item_id: String,
        actual_kind: String,
    },
    /// The requested item does not exist (or is soft-deleted).
    ItemNotFound(String),
    /// The reparenting would create a cycle (the proposed parent is a
    /// descendant of the item being moved).
    CycleDetected {
        item_id: String,
        proposed_parent: String,
    },
    /// An underlying database error.
    Db(anyhow::Error),
}

impl fmt::Display for HierarchyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAGoal {
                item_id,
                actual_kind,
            } => write!(
                f,
                "item '{item_id}' is not a goal (kind={actual_kind}): only goals may be parents"
            ),
            Self::ItemNotFound(id) => write!(f, "item not found: '{id}'"),
            Self::CycleDetected {
                item_id,
                proposed_parent,
            } => write!(
                f,
                "reparenting '{item_id}' under '{proposed_parent}' would create a cycle"
            ),
            Self::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for HierarchyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Self::Db(e) = self {
            Some(e.as_ref())
        } else {
            None
        }
    }
}

impl From<anyhow::Error> for HierarchyError {
    fn from(e: anyhow::Error) -> Self {
        Self::Db(e)
    }
}

// ---------------------------------------------------------------------------
// Core functions
// ---------------------------------------------------------------------------

/// Compute the **direct** progress of a goal: counts its immediate children.
///
/// A child state of `done` or `archived` increments `done`.
/// A child state of `doing` increments `in_progress`.
/// All non-deleted children are counted in `total`.
///
/// # Errors
///
/// Returns [`HierarchyError::ItemNotFound`] if `goal_id` does not exist,
/// [`HierarchyError::NotAGoal`] if the item is not of kind `goal`, or
/// [`HierarchyError::Db`] for database failures.
pub fn compute_direct_progress(
    conn: &Connection,
    goal_id: &str,
) -> Result<GoalProgress, HierarchyError> {
    // Verify the item exists and is a goal.
    let goal = require_goal(conn, goal_id)?;
    let _ = goal; // just confirming it exists

    let children = query::get_children(conn, goal_id)
        .with_context(|| format!("get_children for '{goal_id}'"))?;

    Ok(tally_progress(&children))
}

/// Compute the **nested** progress of a goal, rolling up through the entire
/// subtree.
///
/// For a goal hierarchy like:
/// ```text
/// Goal A
///   ├── Task X (done)
///   ├── Goal B
///   │   ├── Task Y (done)
///   │   └── Task Z (open)
///   └── Task W (doing)
/// ```
///
/// The nested progress for Goal A counts leaf items (tasks) across the whole
/// tree: `done=2, in_progress=1, total=4`.
///
/// Intermediate goal nodes themselves are **not** counted as progress items
/// — only their leaf children are. This means a goal with no children
/// contributes zero to the total.
///
/// # Errors
///
/// Returns [`HierarchyError::ItemNotFound`] if `goal_id` does not exist,
/// [`HierarchyError::NotAGoal`] if the item is not of kind `goal`, or
/// [`HierarchyError::Db`] for database failures.
pub fn compute_nested_progress(
    conn: &Connection,
    goal_id: &str,
) -> Result<GoalProgress, HierarchyError> {
    // Verify the item exists and is a goal.
    require_goal(conn, goal_id)?;

    let mut total_progress = GoalProgress::zero();
    accumulate_progress(conn, goal_id, &mut total_progress, &mut HashSet::new())?;

    Ok(total_progress)
}

/// Get all item IDs in the subtree rooted at `root_id`, including `root_id`
/// itself.
///
/// Uses BFS traversal. Cycles are detected and skipped (though they should
/// not occur in a validated tree).
///
/// Returns items in BFS order (root first, then breadth-by-breadth).
///
/// # Errors
///
/// Returns [`HierarchyError::Db`] for database failures.
pub fn get_subtree_ids(
    conn: &Connection,
    root_id: &str,
) -> Result<Vec<String>, HierarchyError> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut result: Vec<String> = Vec::new();

    queue.push_back(root_id.to_string());

    while let Some(current_id) = queue.pop_front() {
        if !visited.insert(current_id.clone()) {
            continue; // already visited — skip (cycle guard)
        }
        result.push(current_id.clone());

        let children = query::get_children(conn, &current_id)
            .with_context(|| format!("get_children for '{current_id}'"))?;

        for child in children {
            if !visited.contains(&child.item_id) {
                queue.push_back(child.item_id);
            }
        }
    }

    Ok(result)
}

/// Get the ancestor chain of an item, from immediate parent up to root.
///
/// Returns an empty vec if the item has no parent. The first element is
/// the immediate parent, and the last is the root ancestor.
///
/// Cycles are detected and cause an early return (the chain is truncated
/// at the point of the repeat).
///
/// # Errors
///
/// Returns [`HierarchyError::ItemNotFound`] if `item_id` does not exist,
/// or [`HierarchyError::Db`] for database failures.
pub fn get_ancestors(
    conn: &Connection,
    item_id: &str,
) -> Result<Vec<QueryItem>, HierarchyError> {
    // Verify the item exists.
    let start = query::get_item(conn, item_id, false)
        .with_context(|| format!("get_item '{item_id}'"))?
        .ok_or_else(|| HierarchyError::ItemNotFound(item_id.to_string()))?;

    let mut ancestors: Vec<QueryItem> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start.item_id.clone());

    let mut current_parent_id = start.parent_id;

    while let Some(parent_id) = current_parent_id {
        if parent_id.is_empty() {
            break;
        }
        if !visited.insert(parent_id.clone()) {
            break; // cycle guard
        }
        let parent = query::get_item(conn, &parent_id, false)
            .with_context(|| format!("get_item '{parent_id}'"))?
            .ok_or_else(|| HierarchyError::ItemNotFound(parent_id.clone()))?;

        current_parent_id = parent.parent_id.clone();
        ancestors.push(parent);
    }

    Ok(ancestors)
}

/// Validate that reparenting `item_id` under `new_parent_id` is allowed.
///
/// This checks:
/// 1. Both items exist.
/// 2. The new parent is of kind `goal`.
/// 3. The move would not create a cycle (i.e., `new_parent_id` is not a
///    descendant of `item_id`).
///
/// On success returns `Ok(())`. On failure returns the appropriate
/// [`HierarchyError`] variant.
///
/// # Errors
///
/// Returns [`HierarchyError::ItemNotFound`] if either item does not exist,
/// [`HierarchyError::NotAGoal`] if the new parent is not a goal, or
/// [`HierarchyError::CycleDetected`] if the move would create a cycle.
pub fn validate_reparent(
    conn: &Connection,
    item_id: &str,
    new_parent_id: &str,
) -> Result<(), HierarchyError> {
    // 1. Verify both items exist.
    if query::get_item(conn, item_id, false)
        .with_context(|| format!("get_item '{item_id}'"))?
        .is_none()
    {
        return Err(HierarchyError::ItemNotFound(item_id.to_string()));
    }

    // 2. Verify the proposed parent is a goal.
    require_goal(conn, new_parent_id)?;

    // 3. Check for cycles: new_parent_id must not be in the subtree of item_id.
    let subtree = get_subtree_ids(conn, item_id)?;
    if subtree.contains(&new_parent_id.to_string()) {
        return Err(HierarchyError::CycleDetected {
            item_id: item_id.to_string(),
            proposed_parent: new_parent_id.to_string(),
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Look up `item_id` and return it if it is of kind `goal`.
///
/// Returns `HierarchyError::ItemNotFound` if not found,
/// `HierarchyError::NotAGoal` if found but not a goal.
fn require_goal(
    conn: &Connection,
    item_id: &str,
) -> Result<QueryItem, HierarchyError> {
    let item = query::get_item(conn, item_id, false)
        .with_context(|| format!("get_item '{item_id}'"))?
        .ok_or_else(|| HierarchyError::ItemNotFound(item_id.to_string()))?;

    if item.kind != "goal" {
        return Err(HierarchyError::NotAGoal {
            item_id: item_id.to_string(),
            actual_kind: item.kind.clone(),
        });
    }

    Ok(item)
}

/// Tally a list of items into a `GoalProgress`.
fn tally_progress(items: &[QueryItem]) -> GoalProgress {
    let mut progress = GoalProgress::zero();
    for item in items {
        progress.total += 1;
        match item.state.as_str() {
            "done" | "archived" => progress.done += 1,
            "doing" => progress.in_progress += 1,
            _ => {} // "open" — counted in total but not done/in_progress
        }
    }
    progress
}

/// Recursively accumulate leaf-item progress from a subtree.
///
/// Intermediate goal nodes are not counted; only leaf items (non-goal items
/// or goal items with no children) contribute to `accumulator`.
///
/// `visited` prevents infinite loops in malformed trees.
fn accumulate_progress(
    conn: &Connection,
    current_id: &str,
    accumulator: &mut GoalProgress,
    visited: &mut HashSet<String>,
) -> Result<(), HierarchyError> {
    if !visited.insert(current_id.to_string()) {
        return Ok(()); // cycle guard
    }

    let children = query::get_children(conn, current_id)
        .with_context(|| format!("get_children for '{current_id}'"))?;

    if children.is_empty() {
        // Leaf goal with no children — contributes nothing to progress.
        return Ok(());
    }

    for child in &children {
        if child.kind == "goal" {
            // Recurse into nested goal.
            accumulate_progress(conn, &child.item_id, accumulator, visited)?;
        } else {
            // Leaf item — tally directly.
            accumulator.total += 1;
            match child.state.as_str() {
                "done" | "archived" => accumulator.done += 1,
                "doing" => accumulator.in_progress += 1,
                _ => {}
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{migrations, open_projection};
    use rusqlite::{Connection, params};

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn test_db() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        conn
    }

    /// Insert an item with minimal required fields.
    fn insert_item(
        conn: &Connection,
        id: &str,
        kind: &str,
        state: &str,
        parent_id: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO items \
             (item_id, title, kind, state, urgency, is_deleted, search_labels, \
              parent_id, created_at_us, updated_at_us) \
             VALUES (?1, ?2, ?3, ?4, 'default', 0, '', ?5, 1000, 2000)",
            params![id, format!("Title for {id}"), kind, state, parent_id],
        )
        .expect("insert item");
    }

    // -----------------------------------------------------------------------
    // GoalProgress: unit tests for the type itself
    // -----------------------------------------------------------------------

    #[test]
    fn goal_progress_zero() {
        let p = GoalProgress::zero();
        assert_eq!(p.done, 0);
        assert_eq!(p.in_progress, 0);
        assert_eq!(p.total, 0);
    }

    #[test]
    fn goal_progress_percent_zero_total_is_100() {
        let p = GoalProgress::zero();
        assert_eq!(p.percent_complete(), 100.0);
        assert!(p.is_complete());
    }

    #[test]
    fn goal_progress_percent_half() {
        let p = GoalProgress {
            done: 1,
            in_progress: 0,
            total: 2,
        };
        assert!((p.percent_complete() - 50.0).abs() < f32::EPSILON);
        assert!(!p.is_complete());
    }

    #[test]
    fn goal_progress_all_done() {
        let p = GoalProgress {
            done: 5,
            in_progress: 0,
            total: 5,
        };
        assert_eq!(p.percent_complete(), 100.0);
        assert!(p.is_complete());
    }

    #[test]
    fn goal_progress_remaining() {
        let p = GoalProgress {
            done: 3,
            in_progress: 1,
            total: 5,
        };
        assert_eq!(p.remaining(), 2);
    }

    #[test]
    fn goal_progress_display() {
        let p = GoalProgress {
            done: 2,
            in_progress: 1,
            total: 4,
        };
        let s = p.to_string();
        assert!(s.contains("2/4"), "display: {s}");
        assert!(s.contains("50%"), "display: {s}");
    }

    // -----------------------------------------------------------------------
    // HierarchyError: display
    // -----------------------------------------------------------------------

    #[test]
    fn hierarchy_error_display_not_a_goal() {
        let e = HierarchyError::NotAGoal {
            item_id: "bn-001".to_string(),
            actual_kind: "task".to_string(),
        };
        assert!(e.to_string().contains("bn-001"));
        assert!(e.to_string().contains("task"));
    }

    #[test]
    fn hierarchy_error_display_not_found() {
        let e = HierarchyError::ItemNotFound("bn-missing".to_string());
        assert!(e.to_string().contains("bn-missing"));
    }

    #[test]
    fn hierarchy_error_display_cycle() {
        let e = HierarchyError::CycleDetected {
            item_id: "bn-001".to_string(),
            proposed_parent: "bn-002".to_string(),
        };
        let s = e.to_string();
        assert!(s.contains("bn-001"));
        assert!(s.contains("bn-002"));
        assert!(s.contains("cycle"));
    }

    // -----------------------------------------------------------------------
    // compute_direct_progress
    // -----------------------------------------------------------------------

    #[test]
    fn direct_progress_empty_goal() {
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "open", None);

        let p = compute_direct_progress(&conn, "bn-goal").unwrap();
        assert_eq!(p.total, 0);
        assert_eq!(p.done, 0);
        assert!(p.is_complete()); // vacuously complete
    }

    #[test]
    fn direct_progress_all_open() {
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "open", None);
        insert_item(&conn, "bn-c1", "task", "open", Some("bn-goal"));
        insert_item(&conn, "bn-c2", "task", "open", Some("bn-goal"));

        let p = compute_direct_progress(&conn, "bn-goal").unwrap();
        assert_eq!(p.total, 2);
        assert_eq!(p.done, 0);
        assert_eq!(p.in_progress, 0);
        assert!(!p.is_complete());
    }

    #[test]
    fn direct_progress_mixed_states() {
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "open", None);
        insert_item(&conn, "bn-c1", "task", "done", Some("bn-goal"));
        insert_item(&conn, "bn-c2", "task", "doing", Some("bn-goal"));
        insert_item(&conn, "bn-c3", "task", "open", Some("bn-goal"));
        insert_item(&conn, "bn-c4", "task", "archived", Some("bn-goal"));

        let p = compute_direct_progress(&conn, "bn-goal").unwrap();
        assert_eq!(p.total, 4);
        assert_eq!(p.done, 2); // done + archived
        assert_eq!(p.in_progress, 1);
        assert!(!p.is_complete());
    }

    #[test]
    fn direct_progress_all_done() {
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "open", None);
        insert_item(&conn, "bn-c1", "task", "done", Some("bn-goal"));
        insert_item(&conn, "bn-c2", "task", "done", Some("bn-goal"));

        let p = compute_direct_progress(&conn, "bn-goal").unwrap();
        assert_eq!(p.total, 2);
        assert_eq!(p.done, 2);
        assert!(p.is_complete());
    }

    #[test]
    fn direct_progress_not_found_returns_error() {
        let conn = test_db();
        let err = compute_direct_progress(&conn, "bn-missing").unwrap_err();
        assert!(matches!(err, HierarchyError::ItemNotFound(_)));
    }

    #[test]
    fn direct_progress_not_a_goal_returns_error() {
        let conn = test_db();
        insert_item(&conn, "bn-task", "task", "open", None);

        let err = compute_direct_progress(&conn, "bn-task").unwrap_err();
        assert!(matches!(
            err,
            HierarchyError::NotAGoal { actual_kind, .. } if actual_kind == "task"
        ));
    }

    #[test]
    fn direct_progress_excludes_deleted_children() {
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "open", None);
        insert_item(&conn, "bn-c1", "task", "done", Some("bn-goal"));
        // Insert a deleted child manually
        conn.execute(
            "INSERT INTO items \
             (item_id, title, kind, state, urgency, is_deleted, search_labels, \
              parent_id, created_at_us, updated_at_us) \
             VALUES ('bn-del', 'Deleted', 'task', 'open', 'default', 1, '', 'bn-goal', 1000, 2000)",
            [],
        )
        .unwrap();

        let p = compute_direct_progress(&conn, "bn-goal").unwrap();
        assert_eq!(p.total, 1); // deleted child excluded by get_children
        assert_eq!(p.done, 1);
    }

    // -----------------------------------------------------------------------
    // compute_nested_progress
    // -----------------------------------------------------------------------

    #[test]
    fn nested_progress_flat_goal() {
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "open", None);
        insert_item(&conn, "bn-c1", "task", "done", Some("bn-goal"));
        insert_item(&conn, "bn-c2", "task", "open", Some("bn-goal"));

        let p = compute_nested_progress(&conn, "bn-goal").unwrap();
        assert_eq!(p.total, 2);
        assert_eq!(p.done, 1);
    }

    #[test]
    fn nested_progress_rolls_up_through_subgoals() {
        // Goal A
        //   ├── Task X (done)
        //   ├── Goal B
        //   │   ├── Task Y (done)
        //   │   └── Task Z (open)
        //   └── Task W (doing)
        let conn = test_db();
        insert_item(&conn, "bn-ga", "goal", "open", None);
        insert_item(&conn, "bn-tx", "task", "done", Some("bn-ga"));
        insert_item(&conn, "bn-gb", "goal", "open", Some("bn-ga"));
        insert_item(&conn, "bn-ty", "task", "done", Some("bn-gb"));
        insert_item(&conn, "bn-tz", "task", "open", Some("bn-gb"));
        insert_item(&conn, "bn-tw", "task", "doing", Some("bn-ga"));

        let p = compute_nested_progress(&conn, "bn-ga").unwrap();
        assert_eq!(p.total, 4, "leaf items: tx, ty, tz, tw");
        assert_eq!(p.done, 2, "tx and ty are done");
        assert_eq!(p.in_progress, 1, "tw is doing");
    }

    #[test]
    fn nested_progress_deeply_nested() {
        // G1 -> G2 -> G3 -> Task (done)
        let conn = test_db();
        insert_item(&conn, "bn-g1", "goal", "open", None);
        insert_item(&conn, "bn-g2", "goal", "open", Some("bn-g1"));
        insert_item(&conn, "bn-g3", "goal", "open", Some("bn-g2"));
        insert_item(&conn, "bn-t1", "task", "done", Some("bn-g3"));

        let p = compute_nested_progress(&conn, "bn-g1").unwrap();
        assert_eq!(p.total, 1);
        assert_eq!(p.done, 1);
        assert!(p.is_complete());
    }

    #[test]
    fn nested_progress_empty_subgoal_contributes_nothing() {
        // Goal has one task child and one empty subgoal child
        let conn = test_db();
        insert_item(&conn, "bn-ga", "goal", "open", None);
        insert_item(&conn, "bn-t1", "task", "done", Some("bn-ga"));
        insert_item(&conn, "bn-gb", "goal", "open", Some("bn-ga")); // empty

        let p = compute_nested_progress(&conn, "bn-ga").unwrap();
        assert_eq!(p.total, 1, "only the task counts");
        assert_eq!(p.done, 1);
    }

    #[test]
    fn nested_progress_not_found() {
        let conn = test_db();
        let err = compute_nested_progress(&conn, "bn-missing").unwrap_err();
        assert!(matches!(err, HierarchyError::ItemNotFound(_)));
    }

    #[test]
    fn nested_progress_not_a_goal() {
        let conn = test_db();
        insert_item(&conn, "bn-task", "task", "open", None);
        let err = compute_nested_progress(&conn, "bn-task").unwrap_err();
        assert!(matches!(err, HierarchyError::NotAGoal { .. }));
    }

    // -----------------------------------------------------------------------
    // get_subtree_ids
    // -----------------------------------------------------------------------

    #[test]
    fn subtree_single_node() {
        let conn = test_db();
        insert_item(&conn, "bn-root", "goal", "open", None);

        let ids = get_subtree_ids(&conn, "bn-root").unwrap();
        assert_eq!(ids, vec!["bn-root"]);
    }

    #[test]
    fn subtree_with_children() {
        let conn = test_db();
        insert_item(&conn, "bn-root", "goal", "open", None);
        insert_item(&conn, "bn-c1", "task", "open", Some("bn-root"));
        insert_item(&conn, "bn-c2", "task", "done", Some("bn-root"));

        let ids = get_subtree_ids(&conn, "bn-root").unwrap();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&"bn-root".to_string()));
        assert!(ids.contains(&"bn-c1".to_string()));
        assert!(ids.contains(&"bn-c2".to_string()));
    }

    #[test]
    fn subtree_bfs_order_root_first() {
        // Root -> C1, C2 -> G3 (under C1)
        let conn = test_db();
        insert_item(&conn, "bn-root", "goal", "open", None);
        insert_item(&conn, "bn-c1", "goal", "open", Some("bn-root"));
        insert_item(&conn, "bn-c2", "task", "open", Some("bn-root"));
        insert_item(&conn, "bn-gc1", "task", "done", Some("bn-c1"));

        let ids = get_subtree_ids(&conn, "bn-root").unwrap();
        assert_eq!(ids[0], "bn-root"); // root is always first
        assert_eq!(ids.len(), 4);
    }

    // -----------------------------------------------------------------------
    // get_ancestors
    // -----------------------------------------------------------------------

    #[test]
    fn ancestors_no_parent() {
        let conn = test_db();
        insert_item(&conn, "bn-root", "goal", "open", None);

        let ancestors = get_ancestors(&conn, "bn-root").unwrap();
        assert!(ancestors.is_empty());
    }

    #[test]
    fn ancestors_one_level() {
        let conn = test_db();
        insert_item(&conn, "bn-parent", "goal", "open", None);
        insert_item(&conn, "bn-child", "task", "open", Some("bn-parent"));

        let ancestors = get_ancestors(&conn, "bn-child").unwrap();
        assert_eq!(ancestors.len(), 1);
        assert_eq!(ancestors[0].item_id, "bn-parent");
    }

    #[test]
    fn ancestors_three_levels() {
        let conn = test_db();
        insert_item(&conn, "bn-g1", "goal", "open", None);
        insert_item(&conn, "bn-g2", "goal", "open", Some("bn-g1"));
        insert_item(&conn, "bn-t1", "task", "open", Some("bn-g2"));

        let ancestors = get_ancestors(&conn, "bn-t1").unwrap();
        assert_eq!(ancestors.len(), 2);
        assert_eq!(ancestors[0].item_id, "bn-g2");
        assert_eq!(ancestors[1].item_id, "bn-g1");
    }

    #[test]
    fn ancestors_not_found() {
        let conn = test_db();
        let err = get_ancestors(&conn, "bn-missing").unwrap_err();
        assert!(matches!(err, HierarchyError::ItemNotFound(_)));
    }

    // -----------------------------------------------------------------------
    // validate_reparent
    // -----------------------------------------------------------------------

    #[test]
    fn validate_reparent_ok() {
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "open", None);
        insert_item(&conn, "bn-task", "task", "open", None);

        assert!(validate_reparent(&conn, "bn-task", "bn-goal").is_ok());
    }

    #[test]
    fn validate_reparent_to_non_goal_rejected() {
        let conn = test_db();
        insert_item(&conn, "bn-task1", "task", "open", None);
        insert_item(&conn, "bn-task2", "task", "open", None);

        let err = validate_reparent(&conn, "bn-task1", "bn-task2").unwrap_err();
        assert!(matches!(err, HierarchyError::NotAGoal { .. }));
    }

    #[test]
    fn validate_reparent_item_not_found() {
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "open", None);

        let err = validate_reparent(&conn, "bn-missing", "bn-goal").unwrap_err();
        assert!(matches!(err, HierarchyError::ItemNotFound(_)));
    }

    #[test]
    fn validate_reparent_parent_not_found() {
        let conn = test_db();
        insert_item(&conn, "bn-task", "task", "open", None);

        let err = validate_reparent(&conn, "bn-task", "bn-missing").unwrap_err();
        assert!(matches!(err, HierarchyError::ItemNotFound(_)));
    }

    #[test]
    fn validate_reparent_detects_cycle_direct() {
        // Moving bn-goal under one of its own children
        let conn = test_db();
        insert_item(&conn, "bn-parent", "goal", "open", None);
        insert_item(&conn, "bn-child", "goal", "open", Some("bn-parent"));

        // Trying to move bn-parent under bn-child would create a cycle
        let err = validate_reparent(&conn, "bn-parent", "bn-child").unwrap_err();
        assert!(matches!(err, HierarchyError::CycleDetected { .. }));
    }

    #[test]
    fn validate_reparent_detects_cycle_indirect() {
        // G1 -> G2 -> G3; try to move G1 under G3
        let conn = test_db();
        insert_item(&conn, "bn-g1", "goal", "open", None);
        insert_item(&conn, "bn-g2", "goal", "open", Some("bn-g1"));
        insert_item(&conn, "bn-g3", "goal", "open", Some("bn-g2"));

        let err = validate_reparent(&conn, "bn-g1", "bn-g3").unwrap_err();
        assert!(matches!(err, HierarchyError::CycleDetected { .. }));
    }

    #[test]
    fn validate_reparent_self_cycle() {
        // Reparenting an item under itself
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "open", None);

        let err = validate_reparent(&conn, "bn-goal", "bn-goal").unwrap_err();
        assert!(matches!(err, HierarchyError::CycleDetected { .. }));
    }

    #[test]
    fn validate_reparent_move_to_different_goal() {
        // Moving a task from one goal to another — should succeed
        let conn = test_db();
        insert_item(&conn, "bn-ga", "goal", "open", None);
        insert_item(&conn, "bn-gb", "goal", "open", None);
        insert_item(&conn, "bn-task", "task", "open", Some("bn-ga"));

        assert!(validate_reparent(&conn, "bn-task", "bn-gb").is_ok());
    }

    #[test]
    fn validate_reparent_bug_under_goal_ok() {
        // A bug item can be parented under a goal
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "open", None);
        insert_item(&conn, "bn-bug", "bug", "open", None);

        assert!(validate_reparent(&conn, "bn-bug", "bn-goal").is_ok());
    }
}
