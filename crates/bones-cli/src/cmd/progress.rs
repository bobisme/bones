//! `bn progress <goal-id>` — goal completion status.
//!
//! Shows a focused goal-progress view with child tree and progress bars.
//! Distinct from `bn show` (full item detail) — this is a focused view
//! of how far a goal is from completion.

use std::io::Write;
use std::path::Path;

use bones_core::db::query::{self, QueryItem};
use clap::Args;
use serde::Serialize;

use crate::output::{CliError, OutputMode, render, render_error};

/// Arguments for `bn progress`.
#[derive(Args, Debug)]
pub struct ProgressArgs {
    /// Goal item ID to show progress for.
    pub id: String,
}

/// Per-child summary for progress display.
#[derive(Debug, Serialize)]
struct ChildProgress {
    id: String,
    title: String,
    state: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sub_progress: Option<Box<GoalProgressOutput>>,
}

/// Progress counts.
#[derive(Debug, Serialize)]
struct ProgressCounts {
    total: usize,
    done: usize,
    doing: usize,
    open: usize,
    blocked: usize,
    archived: usize,
}

/// Full progress output payload.
#[derive(Debug, Serialize)]
struct GoalProgressOutput {
    id: String,
    title: String,
    state: String,
    kind: String,
    progress: ProgressCounts,
    children: Vec<ChildProgress>,
}

/// Execute `bn progress`.
pub fn run_progress(
    args: &ProgressArgs,
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

    // Resolve the item ID (partial ID support).
    let item_id = resolve_item_id(&conn, &args.id)?;

    // Get the parent item.
    let parent = query::get_item(&conn, &item_id, false)?
        .ok_or_else(|| anyhow::anyhow!("Item not found: {}", args.id))?;

    // Build progress tree recursively.
    let progress = build_progress(&conn, &parent)?;

    render(output, &progress, |report, w| {
        render_progress_human(report, 0, w)
    })
}

/// Resolve a possibly-partial item ID to a full item ID.
fn resolve_item_id(conn: &rusqlite::Connection, partial: &str) -> anyhow::Result<String> {
    // Try exact match first.
    if query::get_item(conn, partial, false)?.is_some() {
        return Ok(partial.to_string());
    }

    // Try with "bn-" prefix.
    let with_prefix = format!("bn-{partial}");
    if query::get_item(conn, &with_prefix, false)?.is_some() {
        return Ok(with_prefix);
    }

    // Try prefix match.
    let sql = "SELECT item_id FROM items WHERE item_id LIKE ? AND is_deleted = 0";
    let pattern = format!("{partial}%");
    let mut stmt = conn.prepare(sql)?;
    let mut matches: Vec<String> = stmt
        .query_map([&pattern], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();

    if matches.is_empty() {
        // Also try with bn- prefix.
        let pattern2 = format!("bn-{partial}%");
        matches = stmt
            .query_map([&pattern2], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
    }

    match matches.len() {
        0 => anyhow::bail!("No item found matching '{partial}'"),
        1 => Ok(matches.remove(0)),
        n => anyhow::bail!("Ambiguous ID '{partial}': matches {n} items"),
    }
}

/// Build a progress tree for an item and its children.
fn build_progress(
    conn: &rusqlite::Connection,
    item: &QueryItem,
) -> anyhow::Result<GoalProgressOutput> {
    let children = query::get_children(conn, &item.item_id)?;

    let mut child_entries = Vec::new();
    let mut counts = ProgressCounts {
        total: children.len(),
        done: 0,
        doing: 0,
        open: 0,
        blocked: 0,
        archived: 0,
    };

    for child in &children {
        // Count by state.
        match child.state.as_str() {
            "done" => counts.done += 1,
            "doing" => counts.doing += 1,
            "open" => counts.open += 1,
            "archived" => counts.archived += 1,
            _ => counts.open += 1,
        }

        // Check if this child is blocked.
        let deps = query::get_dependencies(conn, &child.item_id)?;
        let is_blocked = deps.iter().any(|dep| {
            dep.link_type == "blocks"
                && query::get_item(conn, &dep.depends_on_item_id, false)
                    .ok()
                    .flatten()
                    .is_some_and(|blocker| blocker.state != "done" && blocker.state != "archived")
        });

        if is_blocked && child.state != "done" && child.state != "archived" {
            counts.blocked += 1;
        }

        // Recursively get sub-progress for goals.
        let sub_progress = if child.kind == "goal" {
            let sub = build_progress(conn, child)?;
            Some(Box::new(sub))
        } else {
            None
        };

        child_entries.push(ChildProgress {
            id: child.item_id.clone(),
            title: child.title.clone(),
            state: child.state.clone(),
            kind: child.kind.clone(),
            sub_progress,
        });
    }

    Ok(GoalProgressOutput {
        id: item.item_id.clone(),
        title: item.title.clone(),
        state: item.state.clone(),
        kind: item.kind.clone(),
        progress: counts,
        children: child_entries,
    })
}

fn render_progress_human(
    report: &GoalProgressOutput,
    indent: usize,
    w: &mut dyn Write,
) -> std::io::Result<()> {
    let prefix = "  ".repeat(indent);

    // Title line.
    let kind_tag = if report.kind == "goal" {
        "[goal"
    } else {
        "[task"
    };
    writeln!(w, "{prefix}{} {kind_tag}, {}]", report.title, report.state)?;

    if report.progress.total == 0 {
        writeln!(w, "{prefix}  (no children)")?;
        return Ok(());
    }

    // Progress bar.
    let total = report.progress.total;
    let done = report.progress.done;
    let fraction = if total > 0 {
        done as f64 / total as f64
    } else {
        0.0
    };
    let bar_width = 16;
    let filled = (fraction * bar_width as f64).round() as usize;
    let empty = bar_width - filled;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty));
    let pct = (fraction * 100.0).round() as usize;
    writeln!(w, "{prefix}  Progress: {done}/{total} ({pct}%) {bar}")?;

    // Children.
    for child in &report.children {
        let state_indicator = match child.state.as_str() {
            "done" => "done ",
            "doing" => "doing",
            "open" => "open ",
            "archived" => "arch ",
            _ => &child.state,
        };
        writeln!(
            w,
            "{prefix}  {state_indicator} {}  {}",
            child.id, child.title
        )?;

        // Render sub-goal progress.
        if let Some(ref sub) = child.sub_progress {
            render_progress_human(sub, indent + 2, w)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_progress_basic() {
        let report = GoalProgressOutput {
            id: "bn-p1".to_string(),
            title: "Phase 1: Auth Migration".to_string(),
            state: "open".to_string(),
            kind: "goal".to_string(),
            progress: ProgressCounts {
                total: 3,
                done: 1,
                doing: 1,
                open: 1,
                blocked: 0,
                archived: 0,
            },
            children: vec![
                ChildProgress {
                    id: "bn-a1".to_string(),
                    title: "Implement JWT rotation".to_string(),
                    state: "done".to_string(),
                    kind: "task".to_string(),
                    sub_progress: None,
                },
                ChildProgress {
                    id: "bn-b2".to_string(),
                    title: "Update OIDC provider".to_string(),
                    state: "doing".to_string(),
                    kind: "task".to_string(),
                    sub_progress: None,
                },
                ChildProgress {
                    id: "bn-c3".to_string(),
                    title: "Write migration runbook".to_string(),
                    state: "open".to_string(),
                    kind: "task".to_string(),
                    sub_progress: None,
                },
            ],
        };

        let mut out = Vec::new();
        render_progress_human(&report, 0, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");

        assert!(rendered.contains("Phase 1: Auth Migration"));
        assert!(rendered.contains("[goal, open]"));
        assert!(rendered.contains("1/3 (33%)"));
        assert!(rendered.contains("done  bn-a1  Implement JWT rotation"));
        assert!(rendered.contains("doing bn-b2  Update OIDC provider"));
        assert!(rendered.contains("open  bn-c3  Write migration runbook"));
    }

    #[test]
    fn render_progress_empty_children() {
        let report = GoalProgressOutput {
            id: "bn-g1".to_string(),
            title: "Empty goal".to_string(),
            state: "open".to_string(),
            kind: "goal".to_string(),
            progress: ProgressCounts {
                total: 0,
                done: 0,
                doing: 0,
                open: 0,
                blocked: 0,
                archived: 0,
            },
            children: vec![],
        };

        let mut out = Vec::new();
        render_progress_human(&report, 0, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");

        assert!(rendered.contains("(no children)"));
    }

    #[test]
    fn render_progress_all_done() {
        let report = GoalProgressOutput {
            id: "bn-g2".to_string(),
            title: "Complete goal".to_string(),
            state: "done".to_string(),
            kind: "goal".to_string(),
            progress: ProgressCounts {
                total: 2,
                done: 2,
                doing: 0,
                open: 0,
                blocked: 0,
                archived: 0,
            },
            children: vec![
                ChildProgress {
                    id: "bn-x1".to_string(),
                    title: "Task X".to_string(),
                    state: "done".to_string(),
                    kind: "task".to_string(),
                    sub_progress: None,
                },
                ChildProgress {
                    id: "bn-x2".to_string(),
                    title: "Task Y".to_string(),
                    state: "done".to_string(),
                    kind: "task".to_string(),
                    sub_progress: None,
                },
            ],
        };

        let mut out = Vec::new();
        render_progress_human(&report, 0, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");

        assert!(rendered.contains("2/2 (100%)"));
        assert!(rendered.contains("████████████████"));
    }

    #[test]
    fn render_progress_nested_goals() {
        let sub_goal = GoalProgressOutput {
            id: "bn-sub".to_string(),
            title: "Sub-goal".to_string(),
            state: "doing".to_string(),
            kind: "goal".to_string(),
            progress: ProgressCounts {
                total: 1,
                done: 0,
                doing: 1,
                open: 0,
                blocked: 0,
                archived: 0,
            },
            children: vec![ChildProgress {
                id: "bn-sub1".to_string(),
                title: "Sub task".to_string(),
                state: "doing".to_string(),
                kind: "task".to_string(),
                sub_progress: None,
            }],
        };

        let report = GoalProgressOutput {
            id: "bn-top".to_string(),
            title: "Top goal".to_string(),
            state: "open".to_string(),
            kind: "goal".to_string(),
            progress: ProgressCounts {
                total: 1,
                done: 0,
                doing: 1,
                open: 0,
                blocked: 0,
                archived: 0,
            },
            children: vec![ChildProgress {
                id: "bn-sub".to_string(),
                title: "Sub-goal".to_string(),
                state: "doing".to_string(),
                kind: "goal".to_string(),
                sub_progress: Some(Box::new(sub_goal)),
            }],
        };

        let mut out = Vec::new();
        render_progress_human(&report, 0, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");

        assert!(rendered.contains("Top goal"));
        assert!(rendered.contains("Sub-goal"));
        assert!(rendered.contains("Sub task"));
    }

    #[test]
    fn progress_counts_serialize() {
        let counts = ProgressCounts {
            total: 5,
            done: 2,
            doing: 1,
            open: 1,
            blocked: 1,
            archived: 0,
        };
        let json = serde_json::to_string(&counts).expect("serialize");
        assert!(json.contains("\"total\":5"));
        assert!(json.contains("\"done\":2"));
    }

    #[test]
    fn progress_smoke_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(&bones_dir).expect("create .bones");
        let db_path = bones_dir.join("bones.db");

        let mut conn = rusqlite::Connection::open(&db_path).expect("open db");
        bones_core::db::migrations::migrate(&mut conn).expect("migrate");

        // Insert a goal with children.
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-goal', 'My Goal', 'goal', 'open', 'default', 0, '', 1000, 1000)",
            [],
        ).expect("insert goal");
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, parent_id, created_at_us, updated_at_us) \
             VALUES ('bn-t1', 'Task 1', 'task', 'done', 'default', 0, '', 'bn-goal', 1000, 1000)",
            [],
        ).expect("insert t1");
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, parent_id, created_at_us, updated_at_us) \
             VALUES ('bn-t2', 'Task 2', 'task', 'open', 'default', 0, '', 'bn-goal', 1000, 1000)",
            [],
        ).expect("insert t2");

        drop(conn);

        let args = ProgressArgs {
            id: "bn-goal".to_string(),
        };
        let result = run_progress(&args, OutputMode::Json, dir.path());
        assert!(result.is_ok());
    }
}
