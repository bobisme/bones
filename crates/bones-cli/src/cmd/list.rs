//! `bn list` — list work items with filtering.
//!
//! By default, shows all open items (state = "open"). Filters can be
//! combined. Supports both human-readable table and JSON array output.

use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;
use bones_core::db::query::{self, ItemFilter, SortOrder};
use clap::Args;
use serde::Serialize;
use std::io::Write;

#[derive(Args, Debug, Clone, Default)]
pub struct ListArgs {
    /// Filter by state: open, doing, done, archived.
    /// Default: open (when no other filters are set).
    #[arg(long)]
    pub state: Option<String>,

    /// Filter by kind: task, goal, bug.
    #[arg(short, long)]
    pub kind: Option<String>,

    /// Filter by label (may be repeated for AND semantics).
    #[arg(short, long)]
    pub label: Vec<String>,

    /// Filter by urgency: urgent, default, punt.
    #[arg(short = 'u', long)]
    pub urgency: Option<String>,

    /// Filter by parent item ID.
    #[arg(long)]
    pub parent: Option<String>,

    /// Filter by assignee agent name.
    #[arg(long)]
    pub assignee: Option<String>,

    /// Maximum number of items to show (0 = all).
    #[arg(short = 'n', long, default_value = "50")]
    pub limit: usize,

    /// Sort order: updated_desc, updated_asc, created_desc, created_asc, priority.
    #[arg(long, default_value = "updated_desc")]
    pub sort: String,
}

/// A single list row emitted in JSON output.
#[derive(Debug, Serialize)]
pub struct ListItem {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub state: String,
    pub urgency: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    pub updated_at_us: i64,
}

/// Execute `bn list`.
///
/// Opens the projection database, applies filter criteria, and renders
/// the result as a table (human mode) or JSON array (JSON mode).
///
/// Returns an empty result when the projection database has not yet been
/// built (rather than an error), so that `bn list` is safe to run in a
/// freshly initialized project.
///
/// # Errors
///
/// Returns an error if the database query fails or output rendering fails.
pub fn run_list(
    args: &ListArgs,
    output: OutputMode,
    project_root: &std::path::Path,
) -> anyhow::Result<()> {
    let db_path = project_root.join(".bones/bones.db");

    // Gracefully handle missing / corrupt projection
    let conn = match query::try_open_projection(&db_path)? {
        Some(c) => c,
        None => {
            let items: Vec<ListItem> = Vec::new();
            return render(output, &items, |_, w| {
                writeln!(w, "(projection not found — run `bn rebuild` to initialize)")
            });
        }
    };

    // Validate sort order
    let sort = match args.sort.parse::<SortOrder>() {
        Ok(s) => s,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(
                    format!("invalid --sort value: {e}"),
                    "valid values: updated_desc, updated_asc, created_desc, created_asc, priority",
                    "invalid_sort_order",
                ),
            )?;
            anyhow::bail!("invalid sort order: {e}");
        }
    };

    if let Some(ref state) = args.state {
        if let Err(e) = validate::validate_state(state) {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
        }
    }
    if let Some(ref kind) = args.kind {
        if let Err(e) = validate::validate_kind(kind) {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
        }
    }
    for label in &args.label {
        if let Err(e) = validate::validate_label(label) {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
        }
    }
    if let Some(ref parent) = args.parent {
        if let Err(e) = validate::validate_item_id(parent) {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
        }
    }

    // Default to showing open items unless any filter is explicitly set
    let has_any_filter = args.state.is_some()
        || args.kind.is_some()
        || !args.label.is_empty()
        || args.urgency.is_some()
        || args.parent.is_some();
    let state_filter = if has_any_filter {
        args.state.clone()
    } else {
        Some("open".to_string())
    };

    // ItemFilter supports a single label; we do AND post-filtering for extras
    let primary_label = args.label.first().cloned();

    let filter = ItemFilter {
        state: state_filter,
        kind: args.kind.clone(),
        urgency: args.urgency.clone(),
        label: primary_label,
        parent_id: args.parent.clone(),
        assignee: args.assignee.clone(),
        limit: if args.limit > 0 {
            Some(u32::try_from(args.limit).unwrap_or(u32::MAX))
        } else {
            None
        },
        sort,
        ..Default::default()
    };

    let mut raw = query::list_items(&conn, &filter)?;

    // AND-filter for additional labels beyond the first
    if args.label.len() > 1 {
        let extra_labels: Vec<&str> = args.label.iter().skip(1).map(String::as_str).collect();
        raw.retain(|item| {
            let labels = query::get_labels(&conn, &item.item_id).unwrap_or_default();
            let label_set: std::collections::HashSet<&str> =
                labels.iter().map(|l| l.label.as_str()).collect();
            extra_labels.iter().all(|&req| label_set.contains(req))
        });
    }

    // Enrich each raw item with its label set
    let items: anyhow::Result<Vec<ListItem>> = raw
        .iter()
        .map(|qi| {
            let labels = query::get_labels(&conn, &qi.item_id)
                .unwrap_or_default()
                .into_iter()
                .map(|l| l.label)
                .collect();
            Ok(ListItem {
                id: qi.item_id.clone(),
                title: qi.title.clone(),
                kind: qi.kind.clone(),
                state: qi.state.clone(),
                urgency: qi.urgency.clone(),
                size: qi.size.clone(),
                parent_id: qi.parent_id.clone(),
                labels,
                updated_at_us: qi.updated_at_us,
            })
        })
        .collect();
    let items = items?;

    render(output, &items, |items, w| render_list_human(items, w))
}

/// Render the item list as a human-readable table.
fn render_list_human(items: &[ListItem], w: &mut dyn Write) -> std::io::Result<()> {
    if items.is_empty() {
        return writeln!(w, "(no items)");
    }

    writeln!(
        w,
        "{:<22}  {:<6}  {:<10}  {:<8}  {}",
        "ID", "KIND", "STATE", "URGENCY", "TITLE"
    )?;
    writeln!(w, "{}", "-".repeat(72))?;

    for item in items {
        let labels_suffix = if item.labels.is_empty() {
            String::new()
        } else {
            format!("  [{}]", item.labels.join(", "))
        };

        let max_title_len = 40usize;
        let title = if item.title.len() > max_title_len {
            format!("{}…", &item.title[..max_title_len.saturating_sub(1)])
        } else {
            item.title.clone()
        };

        writeln!(
            w,
            "{:<22}  {:<6}  {:<10}  {:<8}  {}{}",
            item.id, item.kind, item.state, item.urgency, title, labels_suffix
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputMode;
    use bones_core::db::migrations;
    use rusqlite::Connection;
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // Arg parsing
    // -----------------------------------------------------------------------

    #[test]
    fn list_args_defaults() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: ListArgs,
        }
        let w = Wrapper::parse_from(["test"]);
        assert!(w.args.state.is_none());
        assert!(w.args.kind.is_none());
        assert!(w.args.label.is_empty());
        assert!(w.args.urgency.is_none());
        assert!(w.args.parent.is_none());
        assert_eq!(w.args.limit, 50);
        assert_eq!(w.args.sort, "updated_desc");
    }

    #[test]
    fn list_args_all_flags() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: ListArgs,
        }
        let w = Wrapper::parse_from([
            "test",
            "--state",
            "doing",
            "--kind",
            "bug",
            "--label",
            "backend",
            "--label",
            "urgent",
            "--urgency",
            "urgent",
            "--parent",
            "bn-abc",
            "-n",
            "10",
            "--sort",
            "priority",
        ]);
        assert_eq!(w.args.state.as_deref(), Some("doing"));
        assert_eq!(w.args.kind.as_deref(), Some("bug"));
        assert_eq!(w.args.label, vec!["backend", "urgent"]);
        assert_eq!(w.args.urgency.as_deref(), Some("urgent"));
        assert_eq!(w.args.parent.as_deref(), Some("bn-abc"));
        assert_eq!(w.args.limit, 10);
        assert_eq!(w.args.sort, "priority");
    }

    // -----------------------------------------------------------------------
    // render_list_human
    // -----------------------------------------------------------------------

    #[test]
    fn render_list_human_empty() {
        let mut buf = Vec::new();
        render_list_human(&[], &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("(no items)"));
    }

    #[test]
    fn render_list_human_shows_header_and_row() {
        let items = vec![ListItem {
            id: "bn-abc".into(),
            title: "Fix auth".into(),
            kind: "task".into(),
            state: "open".into(),
            urgency: "urgent".into(),
            size: None,
            parent_id: None,
            labels: vec!["backend".into()],
            updated_at_us: 1000,
        }];
        let mut buf = Vec::new();
        render_list_human(&items, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("ID"));
        assert!(out.contains("bn-abc"));
        assert!(out.contains("Fix auth"));
        assert!(out.contains("backend"));
    }

    #[test]
    fn render_list_human_truncates_long_title() {
        let long_title = "A".repeat(60);
        let items = vec![ListItem {
            id: "bn-x".into(),
            title: long_title.clone(),
            kind: "task".into(),
            state: "open".into(),
            urgency: "default".into(),
            size: None,
            parent_id: None,
            labels: vec![],
            updated_at_us: 1000,
        }];
        let mut buf = Vec::new();
        render_list_human(&items, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        // Title should be truncated (not all 60 chars)
        assert!(!out.contains(&long_title));
        assert!(out.contains('…'));
    }

    // -----------------------------------------------------------------------
    // Integration: run_list against an in-memory DB via temp file
    // -----------------------------------------------------------------------

    fn setup_test_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(&bones_dir).unwrap();
        let db_path = bones_dir.join("bones.db");

        let mut conn = Connection::open(&db_path).expect("open db");
        migrations::migrate(&mut conn).expect("migrate");

        // Insert test items
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, \
             search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-001', 'Open task', 'task', 'open', 'default', 0, '', 1000, 2000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, \
             search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-002', 'Doing bug', 'bug', 'doing', 'urgent', 0, '', 1001, 2001)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, \
             search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-003', 'Done goal', 'goal', 'done', 'punt', 0, '', 1002, 2002)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO item_labels (item_id, label, created_at_us) VALUES ('bn-001', 'auth', 100)",
            [],
        )
        .unwrap();

        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test]
    fn run_list_defaults_to_open_items() {
        let (_dir, root) = setup_test_db();
        let args = ListArgs {
            state: None,
            kind: None,
            label: vec![],
            urgency: None,
            parent: None,
            assignee: None,
            limit: 50,
            sort: "updated_desc".into(),
        };
        // Should not error and should list only open items
        run_list(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_list_with_state_filter() {
        let (_dir, root) = setup_test_db();
        let args = ListArgs {
            state: Some("doing".into()),
            kind: None,
            label: vec![],
            urgency: None,
            parent: None,
            assignee: None,
            limit: 50,
            sort: "updated_desc".into(),
        };
        run_list(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_list_empty_returns_no_items_message() {
        let (_dir, root) = setup_test_db();
        let args = ListArgs {
            state: Some("archived".into()),
            kind: None,
            label: vec![],
            urgency: None,
            parent: None,
            assignee: None,
            limit: 50,
            sort: "updated_desc".into(),
        };
        // No archived items → should succeed with empty result
        run_list(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_list_json_output_is_array() {
        let (_dir, root) = setup_test_db();
        let args = ListArgs {
            state: None,
            kind: None,
            label: vec![],
            urgency: None,
            parent: None,
            assignee: None,
            limit: 50,
            sort: "updated_desc".into(),
        };
        // JSON mode should not panic
        run_list(&args, OutputMode::Json, &root).unwrap();
    }

    #[test]
    fn run_list_missing_projection_returns_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No .bones/bones.db — should return gracefully
        let args = ListArgs {
            state: None,
            kind: None,
            label: vec![],
            urgency: None,
            parent: None,
            assignee: None,
            limit: 50,
            sort: "updated_desc".into(),
        };
        run_list(&args, OutputMode::Human, dir.path()).unwrap();
    }

    #[test]
    fn run_list_filter_by_kind() {
        let (_dir, root) = setup_test_db();
        let args = ListArgs {
            state: Some("doing".into()),
            kind: Some("bug".into()),
            label: vec![],
            urgency: None,
            parent: None,
            assignee: None,
            limit: 50,
            sort: "updated_desc".into(),
        };
        run_list(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_list_invalid_sort_returns_error() {
        let (_dir, root) = setup_test_db();
        let args = ListArgs {
            state: None,
            kind: None,
            label: vec![],
            urgency: None,
            parent: None,
            assignee: None,
            limit: 50,
            sort: "bogus_sort".into(),
        };
        assert!(run_list(&args, OutputMode::Human, &root).is_err());
    }

    #[test]
    fn list_item_json_serializable() {
        let item = ListItem {
            id: "bn-001".into(),
            title: "Test item".into(),
            kind: "task".into(),
            state: "open".into(),
            urgency: "default".into(),
            size: None,
            parent_id: None,
            labels: vec!["auth".into()],
            updated_at_us: 1000,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("bn-001"));
        assert!(json.contains("auth"));
        // size and parent_id should be omitted when None
        assert!(!json.contains("size"));
        assert!(!json.contains("parent_id"));
    }
}
