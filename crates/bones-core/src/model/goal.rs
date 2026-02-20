use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

use crate::{
    config::ProjectConfig,
    db::query,
    error::ModelError,
    event::{Event, EventData, EventType, MoveData},
    model::{
        item::{State, WorkItemFields},
        item_id::ItemId,
    },
};

/// SQLite projection handle used by goal model helpers.
pub type Db = Connection;

/// Safety cap for containment-depth validation.
pub const MAX_CONTAINMENT_DEPTH: usize = 256;

const AUTO_CLOSE_REASON: &str = "all children complete";
const AUTO_REOPEN_REASON: &str = "child reopened";

/// Policy controlling goal auto-close and auto-reopen behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalPolicy {
    /// Auto-close a goal when all active children are done/archived.
    pub auto_close: bool,
    /// Auto-reopen a done goal when an open/doing child appears.
    pub auto_reopen: bool,
}

impl Default for GoalPolicy {
    fn default() -> Self {
        Self {
            auto_close: true,
            auto_reopen: true,
        }
    }
}

impl GoalPolicy {
    /// Map project-level config into a goal policy.
    ///
    /// `goals.auto_complete = true` enables both auto-close and auto-reopen.
    /// `goals.auto_complete = false` disables both.
    #[must_use]
    pub fn from_project_config(config: &ProjectConfig) -> Self {
        let enabled = config.goals.auto_complete;
        Self {
            auto_close: enabled,
            auto_reopen: enabled,
        }
    }

    /// Apply per-goal overrides on top of project defaults.
    #[must_use]
    pub fn apply_override(self, override_policy: GoalPolicyOverride) -> Self {
        Self {
            auto_close: override_policy.auto_close.unwrap_or(self.auto_close),
            auto_reopen: override_policy.auto_reopen.unwrap_or(self.auto_reopen),
        }
    }
}

/// Optional per-goal policy override values.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalPolicyOverride {
    /// Override for [`GoalPolicy::auto_close`].
    pub auto_close: Option<bool>,
    /// Override for [`GoalPolicy::auto_reopen`].
    pub auto_reopen: Option<bool>,
}

/// Progress summary for a goal's direct children.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GoalProgress {
    pub total_children: usize,
    pub done_count: usize,
    pub doing_count: usize,
    pub open_count: usize,
    pub archived_count: usize,
    /// Number of open/doing children with at least one unresolved blocker.
    pub blocked_count: usize,
}

impl GoalProgress {
    fn active_children(self) -> usize {
        self.open_count + self.doing_count
    }

    fn all_active_complete(self) -> bool {
        self.total_children > 0 && self.active_children() == 0
    }
}

/// Parse policy override labels from a goal's labels.
///
/// Supported labels:
/// - `goal:auto-close=<bool>`
/// - `goal:auto-reopen=<bool>`
/// - `goal:manual` (disables both)
/// - `goal:auto` (enables both)
#[must_use]
pub fn goal_policy_override_from_labels(labels: &[String]) -> GoalPolicyOverride {
    let mut override_policy = GoalPolicyOverride::default();

    for label in labels {
        let normalized = label.trim().to_ascii_lowercase();

        match normalized.as_str() {
            "goal:manual" => {
                override_policy.auto_close = Some(false);
                override_policy.auto_reopen = Some(false);
                continue;
            }
            "goal:auto" => {
                override_policy.auto_close = Some(true);
                override_policy.auto_reopen = Some(true);
                continue;
            }
            _ => {}
        }

        if let Some(value) = normalized.strip_prefix("goal:auto-close=") {
            if let Some(parsed) = parse_policy_bool(value) {
                override_policy.auto_close = Some(parsed);
            }
        }

        if let Some(value) = normalized.strip_prefix("goal:auto-reopen=") {
            if let Some(parsed) = parse_policy_bool(value) {
                override_policy.auto_reopen = Some(parsed);
            }
        }
    }

    override_policy
}

/// Parse policy override labels from a projection-level work item aggregate.
#[must_use]
pub fn goal_policy_override_from_fields(fields: &WorkItemFields) -> GoalPolicyOverride {
    goal_policy_override_from_labels(&fields.labels)
}

/// Evaluate whether a goal should auto-close under default policy.
///
/// This returns a synthetic `item.move` event with `agent = "bones"` and
/// reason `"all children complete"` when a transition should be emitted.
///
/// Errors are intentionally swallowed in this convenience wrapper.
#[must_use]
pub fn check_auto_close(goal_id: &str, db: &Db) -> Option<Event> {
    check_auto_close_with_policy(goal_id, db, GoalPolicy::default())
        .ok()
        .flatten()
}

/// Evaluate whether a goal should auto-close under the provided project policy.
pub fn check_auto_close_with_policy(
    goal_id: &str,
    db: &Db,
    project_policy: GoalPolicy,
) -> Result<Option<Event>> {
    let goal = require_goal(db, goal_id)?;
    if !matches!(goal.state.as_str(), "open" | "doing") {
        return Ok(None);
    }

    let policy = resolve_policy(db, goal_id, project_policy)?;
    if !policy.auto_close {
        return Ok(None);
    }

    let progress = goal_progress(goal_id, db)?;
    if progress.all_active_complete() {
        return Ok(Some(system_move_event(
            goal_id,
            State::Done,
            AUTO_CLOSE_REASON,
        )));
    }

    Ok(None)
}

/// Evaluate whether a goal should auto-reopen under default policy.
///
/// This returns a synthetic `item.move` event with `agent = "bones"` and
/// reason `"child reopened"` when a transition should be emitted.
///
/// Errors are intentionally swallowed in this convenience wrapper.
#[must_use]
pub fn check_auto_reopen(goal_id: &str, db: &Db) -> Option<Event> {
    check_auto_reopen_with_policy(goal_id, db, GoalPolicy::default())
        .ok()
        .flatten()
}

/// Evaluate whether a goal should auto-reopen under the provided project policy.
pub fn check_auto_reopen_with_policy(
    goal_id: &str,
    db: &Db,
    project_policy: GoalPolicy,
) -> Result<Option<Event>> {
    let goal = require_goal(db, goal_id)?;
    if goal.state != "done" {
        return Ok(None);
    }

    let policy = resolve_policy(db, goal_id, project_policy)?;
    if !policy.auto_reopen {
        return Ok(None);
    }

    let progress = goal_progress(goal_id, db)?;
    if progress.active_children() > 0 {
        return Ok(Some(system_move_event(
            goal_id,
            State::Open,
            AUTO_REOPEN_REASON,
        )));
    }

    Ok(None)
}

/// Compute direct-child progress summary for a goal.
pub fn goal_progress(goal_id: &str, db: &Db) -> Result<GoalProgress> {
    require_goal(db, goal_id)?;

    let children = query::get_children(db, goal_id)
        .with_context(|| format!("load children for goal '{goal_id}'"))?;

    let mut progress = GoalProgress::default();
    for child in children {
        progress.total_children += 1;
        match child.state.as_str() {
            "done" => progress.done_count += 1,
            "doing" => progress.doing_count += 1,
            "open" => progress.open_count += 1,
            "archived" => progress.archived_count += 1,
            _ => {}
        }
    }

    progress.blocked_count = blocked_children_count(goal_id, db)?;

    Ok(progress)
}

/// Validate that adding `child_id` under `parent_id` will not create circular
/// containment, and that the ancestry depth stays under a safety threshold.
pub fn check_circular_containment(parent_id: &str, child_id: &str, db: &Db) -> Result<()> {
    check_circular_containment_with_limit(parent_id, child_id, db, MAX_CONTAINMENT_DEPTH)
}

fn check_circular_containment_with_limit(
    parent_id: &str,
    child_id: &str,
    db: &Db,
    max_depth: usize,
) -> Result<()> {
    if parent_id == child_id {
        return Err(ModelError::CircularContainment {
            cycle: vec![child_id.to_string(), child_id.to_string()],
        }
        .into());
    }

    let parent = require_item(db, parent_id)?;
    if parent.kind != "goal" {
        bail!(
            "item '{parent_id}' is not a goal (kind={}): only goals may contain children",
            parent.kind
        );
    }

    let _ = require_item(db, child_id)?;

    let mut lineage = vec![parent_id.to_string()];
    let mut visited: HashSet<String> = HashSet::from([child_id.to_string(), parent_id.to_string()]);
    let mut depth = 1usize;
    let mut current = parent.parent_id;

    while let Some(ancestor_id) = current {
        depth += 1;
        if depth > max_depth {
            bail!(
                "containment depth exceeds safety limit: depth={depth}, max={max_depth}, parent='{parent_id}', child='{child_id}'"
            );
        }

        lineage.push(ancestor_id.clone());

        if ancestor_id == child_id {
            let mut cycle = vec![child_id.to_string()];
            cycle.extend(lineage.iter().cloned());
            return Err(ModelError::CircularContainment { cycle }.into());
        }

        if !visited.insert(ancestor_id.clone()) {
            let cycle_start = lineage
                .iter()
                .position(|id| id == &ancestor_id)
                .unwrap_or(0);
            let mut cycle = lineage[cycle_start..].to_vec();
            cycle.push(ancestor_id);
            return Err(ModelError::CircularContainment { cycle }.into());
        }

        current = require_item(db, lineage.last().expect("lineage has ancestor"))?.parent_id;
    }

    Ok(())
}

fn resolve_policy(db: &Db, goal_id: &str, project_policy: GoalPolicy) -> Result<GoalPolicy> {
    let labels = query::get_labels(db, goal_id)
        .with_context(|| format!("load labels for goal '{goal_id}'"))?
        .into_iter()
        .map(|label| label.label)
        .collect::<Vec<_>>();

    Ok(project_policy.apply_override(goal_policy_override_from_labels(&labels)))
}

fn parse_policy_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" | "enabled" => Some(true),
        "0" | "false" | "off" | "no" | "disabled" => Some(false),
        _ => None,
    }
}

fn require_item(db: &Db, item_id: &str) -> Result<query::QueryItem> {
    query::get_item(db, item_id, false)
        .with_context(|| format!("load item '{item_id}'"))?
        .ok_or_else(|| {
            ModelError::ItemNotFound {
                item_id: item_id.to_string(),
            }
            .into()
        })
}

fn require_goal(db: &Db, goal_id: &str) -> Result<query::QueryItem> {
    let goal = require_item(db, goal_id)?;
    if goal.kind != "goal" {
        bail!(
            "item '{goal_id}' is not a goal (kind={}): goal policy applies only to goals",
            goal.kind
        );
    }
    Ok(goal)
}

fn blocked_children_count(goal_id: &str, db: &Db) -> Result<usize> {
    let blocked: i64 = db
        .query_row(
            "SELECT COUNT(DISTINCT child.item_id)
             FROM items child
             JOIN item_dependencies dep
               ON dep.item_id = child.item_id
              AND dep.link_type IN ('blocks', 'blocked_by')
             JOIN items blocker
               ON blocker.item_id = dep.depends_on_item_id
             WHERE child.parent_id = ?1
               AND child.is_deleted = 0
               AND child.state IN ('open', 'doing')
               AND blocker.is_deleted = 0
               AND blocker.state NOT IN ('done', 'archived')",
            params![goal_id],
            |row| row.get(0),
        )
        .with_context(|| format!("count blocked children for goal '{goal_id}'"))?;

    usize::try_from(blocked).context("blocked children count overflow")
}

fn system_move_event(goal_id: &str, target_state: State, reason: &str) -> Event {
    Event {
        wall_ts_us: 0,
        agent: "bones".to_string(),
        itc: "itc:auto-goal-policy".to_string(),
        parents: vec![],
        event_type: EventType::Move,
        item_id: ItemId::new_unchecked(goal_id.to_string()),
        data: EventData::Move(MoveData {
            state: target_state,
            reason: Some(reason.to_string()),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use rusqlite::Connection;

    fn test_db() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        conn
    }

    fn insert_item(
        conn: &Connection,
        id: &str,
        kind: &str,
        state: &str,
        parent_id: Option<&str>,
        created_at: i64,
    ) {
        conn.execute(
            "INSERT INTO items
             (item_id, title, kind, state, urgency, is_deleted, search_labels,
              parent_id, created_at_us, updated_at_us)
             VALUES (?1, ?2, ?3, ?4, 'default', 0, '', ?5, ?6, ?6)",
            params![
                id,
                format!("title {id}"),
                kind,
                state,
                parent_id,
                created_at
            ],
        )
        .expect("insert item");
    }

    fn insert_dependency(conn: &Connection, item_id: &str, depends_on_item_id: &str) {
        conn.execute(
            "INSERT INTO item_dependencies
             (item_id, depends_on_item_id, link_type, created_at_us)
             VALUES (?1, ?2, 'blocks', 1000)",
            params![item_id, depends_on_item_id],
        )
        .expect("insert dependency");
    }

    fn insert_label(conn: &Connection, item_id: &str, label: &str) {
        conn.execute(
            "INSERT INTO item_labels (item_id, label, created_at_us)
             VALUES (?1, ?2, 1000)",
            params![item_id, label],
        )
        .expect("insert label");
    }

    #[test]
    fn policy_defaults_and_project_mapping() {
        let defaults = GoalPolicy::default();
        assert!(defaults.auto_close);
        assert!(defaults.auto_reopen);

        let mut cfg = ProjectConfig::default();
        cfg.goals.auto_complete = false;
        let from_cfg = GoalPolicy::from_project_config(&cfg);
        assert!(!from_cfg.auto_close);
        assert!(!from_cfg.auto_reopen);
    }

    #[test]
    fn policy_override_from_labels_and_fields() {
        let labels = vec![
            "goal:auto-close=off".to_string(),
            "goal:auto-reopen=yes".to_string(),
        ];
        let override_policy = goal_policy_override_from_labels(&labels);
        assert_eq!(override_policy.auto_close, Some(false));
        assert_eq!(override_policy.auto_reopen, Some(true));

        let fields = WorkItemFields {
            labels,
            ..WorkItemFields::default()
        };
        let from_fields = goal_policy_override_from_fields(&fields);
        assert_eq!(from_fields, override_policy);
    }

    #[test]
    fn progress_counts_states_and_blocked_children() {
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "open", None, 1);

        insert_item(&conn, "bn-block-open", "task", "open", None, 2);
        insert_item(&conn, "bn-block-done", "task", "done", None, 3);

        insert_item(&conn, "bn-c-open", "task", "open", Some("bn-goal"), 10);
        insert_item(&conn, "bn-c-doing", "task", "doing", Some("bn-goal"), 11);
        insert_item(&conn, "bn-c-done", "task", "done", Some("bn-goal"), 12);
        insert_item(&conn, "bn-c-arch", "task", "archived", Some("bn-goal"), 13);

        // Open child blocked by an open blocker (counts as blocked).
        insert_dependency(&conn, "bn-c-open", "bn-block-open");
        // Doing child blocked by a done blocker (resolved; should not count).
        insert_dependency(&conn, "bn-c-doing", "bn-block-done");

        let progress = goal_progress("bn-goal", &conn).unwrap();
        assert_eq!(progress.total_children, 4);
        assert_eq!(progress.open_count, 1);
        assert_eq!(progress.doing_count, 1);
        assert_eq!(progress.done_count, 1);
        assert_eq!(progress.archived_count, 1);
        assert_eq!(progress.blocked_count, 1);
    }

    #[test]
    fn auto_close_emits_move_event_when_all_children_complete() {
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "open", None, 1);
        insert_item(&conn, "bn-c1", "task", "done", Some("bn-goal"), 2);
        insert_item(&conn, "bn-c2", "task", "archived", Some("bn-goal"), 3);

        let event = check_auto_close("bn-goal", &conn).expect("auto-close event");
        assert_eq!(event.agent, "bones");
        assert_eq!(event.event_type, EventType::Move);
        assert_eq!(event.item_id.as_str(), "bn-goal");

        let EventData::Move(data) = event.data else {
            panic!("expected move event");
        };
        assert_eq!(data.state, State::Done);
        assert_eq!(data.reason.as_deref(), Some(AUTO_CLOSE_REASON));
    }

    #[test]
    fn auto_close_is_order_independent() {
        let conn = test_db();
        insert_item(&conn, "bn-g1", "goal", "open", None, 1);
        insert_item(&conn, "bn-g2", "goal", "open", None, 2);

        insert_item(&conn, "bn-g1-a", "task", "done", Some("bn-g1"), 10);
        insert_item(&conn, "bn-g1-b", "task", "archived", Some("bn-g1"), 11);

        // Same effective states, inserted in opposite order.
        insert_item(&conn, "bn-g2-b", "task", "archived", Some("bn-g2"), 20);
        insert_item(&conn, "bn-g2-a", "task", "done", Some("bn-g2"), 21);

        let e1 = check_auto_close("bn-g1", &conn).expect("g1 auto-close");
        let e2 = check_auto_close("bn-g2", &conn).expect("g2 auto-close");

        let EventData::Move(d1) = e1.data else {
            panic!("expected move")
        };
        let EventData::Move(d2) = e2.data else {
            panic!("expected move")
        };

        assert_eq!(d1.state, d2.state);
        assert_eq!(d1.reason, d2.reason);
    }

    #[test]
    fn auto_close_respects_goal_override_label() {
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "open", None, 1);
        insert_item(&conn, "bn-c1", "task", "done", Some("bn-goal"), 2);
        insert_label(&conn, "bn-goal", "goal:auto-close=off");

        let event = check_auto_close("bn-goal", &conn);
        assert!(event.is_none());
    }

    #[test]
    fn auto_reopen_emits_move_event_when_done_goal_gets_active_child() {
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "done", None, 1);
        insert_item(&conn, "bn-c1", "task", "open", Some("bn-goal"), 2);

        let event = check_auto_reopen("bn-goal", &conn).expect("auto-reopen event");
        assert_eq!(event.agent, "bones");
        assert_eq!(event.event_type, EventType::Move);

        let EventData::Move(data) = event.data else {
            panic!("expected move event");
        };
        assert_eq!(data.state, State::Open);
        assert_eq!(data.reason.as_deref(), Some(AUTO_REOPEN_REASON));
    }

    #[test]
    fn auto_reopen_respects_project_policy() {
        let conn = test_db();
        insert_item(&conn, "bn-goal", "goal", "done", None, 1);
        insert_item(&conn, "bn-c1", "task", "open", Some("bn-goal"), 2);

        let disabled = GoalPolicy {
            auto_close: true,
            auto_reopen: false,
        };

        let event = check_auto_reopen_with_policy("bn-goal", &conn, disabled).unwrap();
        assert!(event.is_none());
    }

    #[test]
    fn circular_containment_detects_cycle() {
        let conn = test_db();
        insert_item(&conn, "bn-parent", "goal", "open", None, 1);
        insert_item(&conn, "bn-child", "goal", "open", Some("bn-parent"), 2);

        let err = check_circular_containment("bn-child", "bn-parent", &conn).unwrap_err();
        assert!(err.to_string().contains("cycle"));
    }

    #[test]
    fn circular_containment_rejects_non_goal_parent() {
        let conn = test_db();
        insert_item(&conn, "bn-task-parent", "task", "open", None, 1);
        insert_item(&conn, "bn-child", "task", "open", None, 2);

        let err = check_circular_containment("bn-task-parent", "bn-child", &conn).unwrap_err();
        assert!(err.to_string().contains("not a goal"));
    }

    #[test]
    fn circular_containment_enforces_depth_safety_limit() {
        let conn = test_db();
        insert_item(&conn, "bn-root", "goal", "open", None, 1);
        insert_item(&conn, "bn-g1", "goal", "open", Some("bn-root"), 2);
        insert_item(&conn, "bn-g2", "goal", "open", Some("bn-g1"), 3);
        insert_item(&conn, "bn-g3", "goal", "open", Some("bn-g2"), 4);
        insert_item(&conn, "bn-g4", "goal", "open", Some("bn-g3"), 5);
        insert_item(&conn, "bn-child", "task", "open", None, 6);

        let err = check_circular_containment_with_limit("bn-g4", "bn-child", &conn, 3)
            .expect_err("depth should exceed limit");
        assert!(err.to_string().contains("safety limit"));
    }

    #[test]
    fn progress_updates_after_reparenting_and_state_changes() {
        let conn = test_db();
        insert_item(&conn, "bn-ga", "goal", "open", None, 1);
        insert_item(&conn, "bn-gb", "goal", "open", None, 2);
        insert_item(&conn, "bn-task", "task", "open", Some("bn-ga"), 3);

        let p_a = goal_progress("bn-ga", &conn).unwrap();
        let p_b = goal_progress("bn-gb", &conn).unwrap();
        assert_eq!(p_a.total_children, 1);
        assert_eq!(p_a.open_count, 1);
        assert_eq!(p_b.total_children, 0);

        conn.execute(
            "UPDATE items
             SET parent_id = 'bn-gb', state = 'done', updated_at_us = 999
             WHERE item_id = 'bn-task'",
            [],
        )
        .unwrap();

        let p_a2 = goal_progress("bn-ga", &conn).unwrap();
        let p_b2 = goal_progress("bn-gb", &conn).unwrap();

        assert_eq!(p_a2.total_children, 0);
        assert_eq!(p_b2.total_children, 1);
        assert_eq!(p_b2.done_count, 1);
    }
}
