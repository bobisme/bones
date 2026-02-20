//! `bn status` — quick agent/human orientation.
//!
//! Like `git status` for work management: shows what you're working on,
//! what's assigned to you, and project-level counts. Designed to be the
//! first command after crash/restart.

use std::io::Write;
use std::path::Path;

use bones_core::db::query::{self, ItemFilter, QueryItem};
use clap::Args;
use serde::Serialize;

use crate::agent;
use crate::output::{CliError, OutputMode, render, render_error};

/// Arguments for `bn status`.
#[derive(Args, Debug, Default)]
pub struct StatusArgs {}

/// Project-level status counts.
#[derive(Debug, Serialize)]
struct ProjectCounts {
    open: u64,
    doing: u64,
    done: u64,
    archived: u64,
    blocked: u64,
}

/// A summary item for the assigned-to-you section.
#[derive(Debug, Serialize)]
struct AssignedItem {
    id: String,
    title: String,
    state: String,
    urgency: String,
}

/// Full status output payload.
#[derive(Debug, Serialize)]
struct StatusOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    assigned: Vec<AssignedItem>,
    project: ProjectCounts,
}

/// Execute `bn status`.
pub fn run_status(
    _args: &StatusArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let db_path = project_root.join(".bones/bones.db");
    let conn = match query::try_open_projection(&db_path)? {
        Some(conn) => conn,
        None => {
            render_error(
                output,
                &CliError::with_details(
                    "projection database not found",
                    "run `bn rebuild` to initialize the projection",
                    "projection_missing",
                ),
            )?;
            anyhow::bail!("projection not found");
        }
    };

    // Try to resolve agent identity (optional — status works without it).
    let resolved_agent = agent::resolve_agent(agent_flag);

    // Get items assigned to the agent (if we have an agent identity).
    let assigned = if let Some(ref agent_id) = resolved_agent {
        let filter = ItemFilter {
            state: None,
            kind: None,
            label: None,
            urgency: None,
            parent_id: None,
            assignee: Some(agent_id.clone()),
            include_deleted: false,
            limit: None,
            offset: None,
            sort: Default::default(),
        };
        let items = query::list_items(&conn, &filter)?;
        items
            .into_iter()
            .filter(|it| it.state != "done" && it.state != "archived")
            .map(|it| AssignedItem {
                id: it.item_id,
                title: it.title,
                state: it.state,
                urgency: it.urgency,
            })
            .collect()
    } else {
        Vec::new()
    };

    // Count items by state.
    let count_state = |state: &str| -> u64 {
        let filter = ItemFilter {
            state: Some(state.to_string()),
            kind: None,
            label: None,
            urgency: None,
            parent_id: None,
            assignee: None,
            include_deleted: false,
            limit: None,
            offset: None,
            sort: Default::default(),
        };
        query::count_items(&conn, &filter).unwrap_or(0)
    };

    let open_count = count_state("open");
    let doing_count = count_state("doing");
    let done_count = count_state("done");
    let archived_count = count_state("archived");

    // Count blocked items (those with unresolved blocking deps).
    let blocked_count = count_blocked_items(&conn);

    let payload = StatusOutput {
        agent: resolved_agent,
        assigned,
        project: ProjectCounts {
            open: open_count,
            doing: doing_count,
            done: done_count,
            archived: archived_count,
            blocked: blocked_count,
        },
    };

    render(output, &payload, |report, w| render_status_human(report, w))
}

/// Count items that are blocked by at least one open/doing dependency.
fn count_blocked_items(conn: &rusqlite::Connection) -> u64 {
    // item_dependencies: item_id depends_on depends_on_item_id
    // link_type = 'blocks' means depends_on_item_id blocks item_id
    let sql = r#"
        SELECT COUNT(DISTINCT d.item_id)
        FROM item_dependencies d
        JOIN items blocker ON blocker.item_id = d.depends_on_item_id
        JOIN items blocked ON blocked.item_id = d.item_id
        WHERE d.link_type = 'blocks'
          AND blocker.state NOT IN ('done', 'archived')
          AND blocker.is_deleted = 0
          AND blocked.state NOT IN ('done', 'archived')
          AND blocked.is_deleted = 0
    "#;

    conn.query_row(sql, [], |row| row.get::<_, u64>(0))
        .unwrap_or(0)
}

fn render_status_human(report: &StatusOutput, w: &mut dyn Write) -> std::io::Result<()> {
    // Agent identity section.
    if let Some(ref agent) = report.agent {
        writeln!(w, "Agent: {agent}")?;
    } else {
        writeln!(
            w,
            "Agent: (none — set --agent, BONES_AGENT, AGENT, or USER)"
        )?;
    }

    // Assigned items section.
    if report.assigned.is_empty() {
        if report.agent.is_some() {
            writeln!(w, "Assigned to you: 0 items")?;
        }
    } else {
        writeln!(w, "Assigned to you: {} items", report.assigned.len())?;
        for item in &report.assigned {
            let state_icon = match item.state.as_str() {
                "doing" => "doing",
                "open" => "open ",
                _ => &item.state,
            };
            writeln!(w, "  {state_icon} {}  {}", item.id, item.title)?;
        }
    }

    // Blocked items.
    if report.project.blocked > 0 {
        writeln!(w)?;
        writeln!(
            w,
            "Blocked: {} items waiting on dependencies",
            report.project.blocked
        )?;
    }

    // Project summary.
    writeln!(w)?;
    writeln!(
        w,
        "Project: {} open, {} doing, {} done, {} archived",
        report.project.open, report.project.doing, report.project.done, report.project.archived,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db::migrations;
    use rusqlite::Connection;

    fn setup_db() -> (tempfile::TempDir, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(&bones_dir).expect("create .bones");
        let db_path = bones_dir.join("bones.db");
        let mut conn = Connection::open(&db_path).expect("open db");
        migrations::migrate(&mut conn).expect("migrate");
        (dir, conn)
    }

    #[test]
    fn status_empty_project() {
        let (dir, _conn) = setup_db();
        let args = StatusArgs {};
        let result = run_status(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok());
    }

    #[test]
    fn status_with_items() {
        let (dir, conn) = setup_db();

        // Insert some items.
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-001', 'Task A', 'task', 'doing', 'default', 0, '', 1000, 1000)",
            [],
        ).expect("insert");
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-002', 'Task B', 'task', 'open', 'default', 0, '', 1000, 1000)",
            [],
        ).expect("insert");
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-003', 'Task C', 'task', 'done', 'default', 0, '', 1000, 1000)",
            [],
        ).expect("insert");

        // Assign bn-001 to test-agent.
        conn.execute(
            "INSERT INTO item_assignees (item_id, agent, created_at_us) VALUES ('bn-001', 'test-agent', 1000)",
            [],
        ).expect("insert assignee");

        drop(conn);

        let args = StatusArgs {};
        let result = run_status(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok());
    }

    #[test]
    fn status_human_render() {
        let report = StatusOutput {
            agent: Some("alice".to_string()),
            assigned: vec![
                AssignedItem {
                    id: "bn-a1".to_string(),
                    title: "Fix auth retry".to_string(),
                    state: "doing".to_string(),
                    urgency: "default".to_string(),
                },
                AssignedItem {
                    id: "bn-b2".to_string(),
                    title: "Write docs".to_string(),
                    state: "open".to_string(),
                    urgency: "default".to_string(),
                },
            ],
            project: ProjectCounts {
                open: 47,
                doing: 12,
                done: 89,
                archived: 15,
                blocked: 3,
            },
        };

        let mut out = Vec::new();
        render_status_human(&report, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");

        assert!(rendered.contains("Agent: alice"));
        assert!(rendered.contains("Assigned to you: 2 items"));
        assert!(rendered.contains("doing bn-a1  Fix auth retry"));
        assert!(rendered.contains("open  bn-b2  Write docs"));
        assert!(rendered.contains("Blocked: 3 items"));
        assert!(rendered.contains("47 open, 12 doing, 89 done, 15 archived"));
    }

    #[test]
    fn status_no_agent() {
        let report = StatusOutput {
            agent: None,
            assigned: vec![],
            project: ProjectCounts {
                open: 10,
                doing: 2,
                done: 5,
                archived: 0,
                blocked: 0,
            },
        };

        let mut out = Vec::new();
        render_status_human(&report, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("Agent: (none"));
    }

    #[test]
    fn count_blocked_items_empty_db() {
        let (dir, conn) = setup_db();
        assert_eq!(count_blocked_items(&conn), 0);
    }

    #[test]
    fn count_blocked_items_with_deps() {
        let (_dir, conn) = setup_db();

        // Insert items: A blocks B, A is open, B is open → B is blocked.
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-a', 'Blocker', 'task', 'open', 'default', 0, '', 1000, 1000)",
            [],
        ).expect("insert");
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-b', 'Blocked', 'task', 'open', 'default', 0, '', 1000, 1000)",
            [],
        ).expect("insert");
        conn.execute(
            "INSERT INTO item_dependencies (item_id, depends_on_item_id, link_type, created_at_us) \
             VALUES ('bn-b', 'bn-a', 'blocks', 1000)",
            [],
        )
        .expect("insert dep");

        assert_eq!(count_blocked_items(&conn), 1);
    }
}
