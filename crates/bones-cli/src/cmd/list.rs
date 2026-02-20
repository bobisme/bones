//! `bn list` — list work items with filtering.
//!
//! By default, shows all open items (state = "open"). Filters can be
//! combined. Supports both human-readable table and JSON output.

use crate::output::{CliError, OutputMode, render, render_error, render_mode};
use crate::validate;
use bones_core::db::query::{self, ItemFilter, QueryItem, SortOrder};
use bones_core::model::item::Urgency;
use clap::Args;
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::io::Write;
use std::str::FromStr;

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

    /// Filter to items updated at or after this datetime.
    ///
    /// Accepts RFC3339 (2026-02-20T12:00:00Z), epoch seconds, or epoch microseconds.
    #[arg(long)]
    pub since: Option<String>,

    /// Filter to items updated at or before this datetime.
    ///
    /// Accepts RFC3339 (2026-02-20T12:00:00Z), epoch seconds, or epoch microseconds.
    #[arg(long)]
    pub until: Option<String>,

    /// Maximum number of items to show (0 = all).
    #[arg(short = 'n', long, default_value = "50")]
    pub limit: usize,

    /// Pagination offset.
    #[arg(long, default_value = "0")]
    pub offset: usize,

    /// Sort order: priority, created, updated, state.
    ///
    /// Legacy values are also accepted: created_desc, created_asc,
    /// updated_desc, updated_asc.
    #[arg(long, default_value = "updated")]
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

/// JSON response payload for list results.
#[derive(Debug, Serialize)]
struct ListResponse {
    items: Vec<ListItem>,
    total: usize,
    limit: usize,
    offset: usize,
    has_more: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ListSort {
    Priority,
    CreatedAsc,
    CreatedDesc,
    UpdatedAsc,
    #[default]
    UpdatedDesc,
    State,
}

impl FromStr for ListSort {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "priority" | "triage" => Ok(Self::Priority),
            "created" | "created_asc" | "created-asc" | "oldest" => Ok(Self::CreatedAsc),
            "created_desc" | "created-desc" | "newest" => Ok(Self::CreatedDesc),
            "updated" | "updated_desc" | "updated-desc" | "recent" => Ok(Self::UpdatedDesc),
            "updated_asc" | "updated-asc" | "stale" => Ok(Self::UpdatedAsc),
            "state" => Ok(Self::State),
            other => anyhow::bail!(
                "unknown sort order '{other}': expected one of priority, created, updated, state"
            ),
        }
    }
}

/// Execute `bn list`.
///
/// Opens the projection database, applies filter criteria, and renders
/// the result as a table (human mode) or JSON object (JSON mode).
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
            if output.is_json() {
                let response = ListResponse {
                    items: Vec::new(),
                    total: 0,
                    limit: effective_limit(args.limit, 0, args.offset),
                    offset: args.offset,
                    has_more: false,
                };
                return render(output, &response, |_, _| Ok(()));
            }

            let items: Vec<ListItem> = Vec::new();
            return render_mode(
                output,
                &items,
                |_, w| writeln!(w, "advice  projection-missing  run `bn admin rebuild`"),
                |_, w| {
                    writeln!(
                        w,
                        "(projection not found — run `bn admin rebuild` to initialize)"
                    )
                },
            );
        }
    };

    // Validate sort order
    let sort = match args.sort.parse::<ListSort>() {
        Ok(s) => s,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(
                    format!("invalid --sort value: {e}"),
                    "valid values: priority, created, updated, state",
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
    if let Some(ref urgency) = args.urgency {
        if urgency.parse::<Urgency>().is_err() {
            render_error(
                output,
                &CliError::with_details(
                    format!("invalid urgency '{urgency}'"),
                    "use --urgency urgent|default|punt",
                    "invalid_urgency",
                ),
            )?;
            anyhow::bail!("invalid urgency");
        }
    }
    if let Some(ref parent) = args.parent {
        if let Err(e) = validate::validate_item_id(parent) {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
        }
    }
    if let Some(ref assignee) = args.assignee {
        if let Err(e) = validate::validate_agent(assignee) {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
        }
    }

    let since_us = match args.since.as_deref() {
        Some(raw) => match parse_datetime_to_micros(raw) {
            Some(value) => Some(value),
            None => {
                render_error(
                    output,
                    &CliError::with_details(
                        format!("invalid --since value '{raw}'"),
                        "use RFC3339, epoch seconds, or epoch microseconds",
                        "invalid_datetime",
                    ),
                )?;
                anyhow::bail!("invalid --since value");
            }
        },
        None => None,
    };

    let until_us = match args.until.as_deref() {
        Some(raw) => match parse_datetime_to_micros(raw) {
            Some(value) => Some(value),
            None => {
                render_error(
                    output,
                    &CliError::with_details(
                        format!("invalid --until value '{raw}'"),
                        "use RFC3339, epoch seconds, or epoch microseconds",
                        "invalid_datetime",
                    ),
                )?;
                anyhow::bail!("invalid --until value");
            }
        },
        None => None,
    };

    if let (Some(since), Some(until)) = (since_us, until_us)
        && since > until
    {
        render_error(
            output,
            &CliError::with_details(
                "invalid date range: --since must be <= --until",
                "swap the values or remove one bound",
                "invalid_date_range",
            ),
        )?;
        anyhow::bail!("invalid date range");
    }

    let response = build_list_response(&conn, args, sort, since_us, until_us)?;

    if output.is_json() {
        return render(output, &response, |_, _| Ok(()));
    }

    render_mode(
        output,
        &response.items,
        |items, w| render_list_text(items, w),
        |items, w| render_list_human(items, w),
    )
}

fn build_list_response(
    conn: &rusqlite::Connection,
    args: &ListArgs,
    sort: ListSort,
    since_us: Option<i64>,
    until_us: Option<i64>,
) -> anyhow::Result<ListResponse> {
    // Default to showing open items unless any filter is explicitly set.
    // Pagination/sort alone should not disable this default behavior.
    let has_any_filter = args.state.is_some()
        || args.kind.is_some()
        || !args.label.is_empty()
        || args.urgency.is_some()
        || args.parent.is_some()
        || args.assignee.is_some()
        || since_us.is_some()
        || until_us.is_some();

    let state_filter = if has_any_filter {
        args.state.clone()
    } else {
        Some("open".to_string())
    };

    // Fetch an unpaginated set first, then apply deterministic sort + pagination
    // in Rust so metadata remains consistent even with composite label filters.
    let filter = ItemFilter {
        state: state_filter,
        kind: args.kind.clone(),
        urgency: args.urgency.clone(),
        label: args.label.first().cloned(),
        parent_id: args.parent.clone(),
        assignee: args.assignee.clone(),
        limit: None,
        offset: None,
        sort: SortOrder::UpdatedDesc,
        ..Default::default()
    };

    let mut raw = query::list_items(conn, &filter)?;

    // AND-filter labels when multiple --label values are supplied.
    if !args.label.is_empty() {
        let required_labels: HashSet<&str> = args.label.iter().map(String::as_str).collect();
        raw.retain(|item| {
            let labels = query::get_labels(conn, &item.item_id).unwrap_or_default();
            let present: HashSet<&str> = labels.iter().map(|l| l.label.as_str()).collect();
            required_labels.iter().all(|label| present.contains(label))
        });
    }

    if let Some(since) = since_us {
        raw.retain(|item| item.updated_at_us >= since);
    }
    if let Some(until) = until_us {
        raw.retain(|item| item.updated_at_us <= until);
    }

    sort_query_items(&mut raw, sort);

    let total = raw.len();
    let start = args.offset.min(total);
    let end = if args.limit == 0 {
        total
    } else {
        start.saturating_add(args.limit).min(total)
    };
    let has_more = args.limit > 0 && end < total;

    let items: anyhow::Result<Vec<ListItem>> = raw[start..end]
        .iter()
        .map(|qi| {
            let labels = query::get_labels(conn, &qi.item_id)
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

    Ok(ListResponse {
        items: items?,
        total,
        limit: effective_limit(args.limit, total, args.offset),
        offset: args.offset,
        has_more,
    })
}

fn sort_query_items(items: &mut [QueryItem], sort: ListSort) {
    items.sort_by(|a, b| compare_query_items(a, b, sort));
}

fn compare_query_items(a: &QueryItem, b: &QueryItem, sort: ListSort) -> Ordering {
    match sort {
        ListSort::Priority => urgency_rank(&a.urgency)
            .cmp(&urgency_rank(&b.urgency))
            .then_with(|| b.updated_at_us.cmp(&a.updated_at_us))
            .then_with(|| a.item_id.cmp(&b.item_id)),
        ListSort::State => state_rank(&a.state)
            .cmp(&state_rank(&b.state))
            .then_with(|| b.updated_at_us.cmp(&a.updated_at_us))
            .then_with(|| a.item_id.cmp(&b.item_id)),
        ListSort::CreatedAsc => a
            .created_at_us
            .cmp(&b.created_at_us)
            .then_with(|| a.item_id.cmp(&b.item_id)),
        ListSort::CreatedDesc => b
            .created_at_us
            .cmp(&a.created_at_us)
            .then_with(|| a.item_id.cmp(&b.item_id)),
        ListSort::UpdatedAsc => a
            .updated_at_us
            .cmp(&b.updated_at_us)
            .then_with(|| a.item_id.cmp(&b.item_id)),
        ListSort::UpdatedDesc => b
            .updated_at_us
            .cmp(&a.updated_at_us)
            .then_with(|| a.item_id.cmp(&b.item_id)),
    }
}

fn urgency_rank(value: &str) -> u8 {
    match value {
        "urgent" => 0,
        "default" => 1,
        "punt" => 2,
        _ => 3,
    }
}

fn state_rank(value: &str) -> u8 {
    match value {
        "open" => 0,
        "doing" => 1,
        "done" => 2,
        "archived" => 3,
        _ => 4,
    }
}

fn parse_datetime_to_micros(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    if let Ok(num) = raw.parse::<i64>() {
        if num > 1_000_000_000_000 {
            return Some(num);
        }
        return Some(num.saturating_mul(1_000_000));
    }

    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp_micros())
}

fn effective_limit(limit: usize, total: usize, offset: usize) -> usize {
    if limit == 0 {
        total.saturating_sub(offset)
    } else {
        limit
    }
}

/// Render the item list as a human-readable table.
fn render_list_human(items: &[ListItem], w: &mut dyn Write) -> std::io::Result<()> {
    if items.is_empty() {
        writeln!(w, "No items found.")?;
        return writeln!(w, "Use `bn create --title \"...\"` to add a new item");
    }

    writeln!(w, "Items: {}", items.len())?;
    writeln!(w, "{:-<72}", "")?;

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

fn render_list_text(items: &[ListItem], w: &mut dyn Write) -> std::io::Result<()> {
    if items.is_empty() {
        writeln!(w, "advice  no-items  bn create --title \"...\"")?;
        return Ok(());
    }

    for item in items {
        let labels = if item.labels.is_empty() {
            String::new()
        } else {
            format!("  labels={}", item.labels.join(","))
        };
        writeln!(
            w,
            "{}  {}  {}  {}  {}{}",
            item.id, item.kind, item.state, item.urgency, item.title, labels
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
    use clap::Parser;
    use rusqlite::Connection;
    use std::path::PathBuf;

    fn default_args() -> ListArgs {
        ListArgs {
            state: None,
            kind: None,
            label: vec![],
            urgency: None,
            parent: None,
            assignee: None,
            since: None,
            until: None,
            limit: 50,
            offset: 0,
            sort: "updated".into(),
        }
    }

    // -----------------------------------------------------------------------
    // Arg parsing
    // -----------------------------------------------------------------------

    #[test]
    fn list_args_defaults() {
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
        assert!(w.args.assignee.is_none());
        assert!(w.args.since.is_none());
        assert!(w.args.until.is_none());
        assert_eq!(w.args.limit, 50);
        assert_eq!(w.args.offset, 0);
        assert_eq!(w.args.sort, "updated");
    }

    #[test]
    fn list_args_all_flags() {
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
            "--assignee",
            "alice",
            "--since",
            "2026-02-20T00:00:00Z",
            "--until",
            "2026-02-21T00:00:00Z",
            "-n",
            "10",
            "--offset",
            "5",
            "--sort",
            "priority",
        ]);
        assert_eq!(w.args.state.as_deref(), Some("doing"));
        assert_eq!(w.args.kind.as_deref(), Some("bug"));
        assert_eq!(w.args.label, vec!["backend", "urgent"]);
        assert_eq!(w.args.urgency.as_deref(), Some("urgent"));
        assert_eq!(w.args.parent.as_deref(), Some("bn-abc"));
        assert_eq!(w.args.assignee.as_deref(), Some("alice"));
        assert_eq!(w.args.since.as_deref(), Some("2026-02-20T00:00:00Z"));
        assert_eq!(w.args.until.as_deref(), Some("2026-02-21T00:00:00Z"));
        assert_eq!(w.args.limit, 10);
        assert_eq!(w.args.offset, 5);
        assert_eq!(w.args.sort, "priority");
    }

    // -----------------------------------------------------------------------
    // Sort and datetime helpers
    // -----------------------------------------------------------------------

    #[test]
    fn list_sort_parses_new_and_legacy_values() {
        assert_eq!("priority".parse::<ListSort>().unwrap(), ListSort::Priority);
        assert_eq!("created".parse::<ListSort>().unwrap(), ListSort::CreatedAsc);
        assert_eq!(
            "created_desc".parse::<ListSort>().unwrap(),
            ListSort::CreatedDesc
        );
        assert_eq!(
            "updated".parse::<ListSort>().unwrap(),
            ListSort::UpdatedDesc
        );
        assert_eq!(
            "updated_asc".parse::<ListSort>().unwrap(),
            ListSort::UpdatedAsc
        );
        assert_eq!("state".parse::<ListSort>().unwrap(), ListSort::State);
    }

    #[test]
    fn parse_datetime_accepts_seconds_micros_and_rfc3339() {
        assert_eq!(
            parse_datetime_to_micros("1700000000"),
            Some(1_700_000_000_000_000)
        );
        assert_eq!(
            parse_datetime_to_micros("1700000000000000"),
            Some(1_700_000_000_000_000)
        );
        assert!(parse_datetime_to_micros("2026-02-20T00:00:00Z").is_some());
        assert!(parse_datetime_to_micros("not-a-date").is_none());
    }

    #[test]
    fn sort_tie_breaks_by_id_for_stability() {
        let mut items = vec![
            QueryItem {
                item_id: "bn-zzz".into(),
                title: "A".into(),
                description: None,
                kind: "task".into(),
                state: "open".into(),
                urgency: "default".into(),
                size: None,
                parent_id: None,
                compact_summary: None,
                is_deleted: false,
                deleted_at_us: None,
                search_labels: "".into(),
                created_at_us: 100,
                updated_at_us: 200,
            },
            QueryItem {
                item_id: "bn-aaa".into(),
                title: "B".into(),
                description: None,
                kind: "task".into(),
                state: "open".into(),
                urgency: "default".into(),
                size: None,
                parent_id: None,
                compact_summary: None,
                is_deleted: false,
                deleted_at_us: None,
                search_labels: "".into(),
                created_at_us: 100,
                updated_at_us: 200,
            },
        ];

        sort_query_items(&mut items, ListSort::UpdatedDesc);
        assert_eq!(items[0].item_id, "bn-aaa");
        assert_eq!(items[1].item_id, "bn-zzz");
    }

    // -----------------------------------------------------------------------
    // render_list_human
    // -----------------------------------------------------------------------

    #[test]
    fn render_list_human_empty() {
        let mut buf = Vec::new();
        render_list_human(&[], &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("No items found"));
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
    // Integration: run_list / build_list_response against a temp DB
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
    fn build_list_response_includes_pagination_metadata() {
        let (_dir, root) = setup_test_db();
        let db_path = root.join(".bones/bones.db");
        let conn = Connection::open(db_path).unwrap();

        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, \
             search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-004', 'Open extra 1', 'task', 'open', 'default', 0, '', 1003, 2003)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, \
             search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-005', 'Open extra 2', 'task', 'open', 'default', 0, '', 1004, 2004)",
            [],
        )
        .unwrap();

        let mut args = default_args();
        args.limit = 2;
        args.offset = 1;
        args.sort = "created".into();

        let response = build_list_response(&conn, &args, ListSort::CreatedAsc, None, None).unwrap();
        // Default filter is state=open; we inserted 3 open rows total.
        assert_eq!(response.total, 3);
        assert_eq!(response.limit, 2);
        assert_eq!(response.offset, 1);
        assert_eq!(response.items.len(), 2);
        assert!(!response.has_more);
    }

    #[test]
    fn build_list_response_supports_since_until() {
        let (_dir, root) = setup_test_db();
        let db_path = root.join(".bones/bones.db");
        let conn = Connection::open(db_path).unwrap();

        let mut args = default_args();
        args.state = Some("doing".into());

        // bn-002 has updated_at_us=2001
        let response =
            build_list_response(&conn, &args, ListSort::UpdatedDesc, Some(2000), Some(2001))
                .unwrap();
        assert_eq!(response.total, 1);
        assert_eq!(response.items[0].id, "bn-002");

        let response_none =
            build_list_response(&conn, &args, ListSort::UpdatedDesc, Some(2002), None).unwrap();
        assert_eq!(response_none.total, 0);
    }

    #[test]
    fn run_list_defaults_to_open_items() {
        let (_dir, root) = setup_test_db();
        let args = default_args();
        // Should not error and should list only open items by default
        run_list(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_list_with_state_filter() {
        let (_dir, root) = setup_test_db();
        let mut args = default_args();
        args.state = Some("doing".into());
        run_list(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_list_empty_returns_no_items_message() {
        let (_dir, root) = setup_test_db();
        let mut args = default_args();
        args.state = Some("archived".into());
        // No archived items → should succeed with empty result
        run_list(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_list_json_output_is_object() {
        let (_dir, root) = setup_test_db();
        let args = default_args();
        // JSON mode should not panic
        run_list(&args, OutputMode::Json, &root).unwrap();
    }

    #[test]
    fn run_list_missing_projection_returns_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No .bones/bones.db — should return gracefully
        let args = default_args();
        run_list(&args, OutputMode::Human, dir.path()).unwrap();
    }

    #[test]
    fn run_list_filter_by_kind() {
        let (_dir, root) = setup_test_db();
        let mut args = default_args();
        args.state = Some("doing".into());
        args.kind = Some("bug".into());
        run_list(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_list_invalid_sort_returns_error() {
        let (_dir, root) = setup_test_db();
        let mut args = default_args();
        args.sort = "bogus_sort".into();
        assert!(run_list(&args, OutputMode::Human, &root).is_err());
    }

    #[test]
    fn list_json_serialization_includes_metadata() {
        let response = ListResponse {
            items: vec![ListItem {
                id: "bn-001".into(),
                title: "Test item".into(),
                kind: "task".into(),
                state: "open".into(),
                urgency: "default".into(),
                size: None,
                parent_id: None,
                labels: vec!["auth".into()],
                updated_at_us: 1000,
            }],
            total: 1,
            limit: 50,
            offset: 0,
            has_more: false,
        };

        let json = serde_json::to_value(&response).unwrap();
        assert!(json["items"].is_array());
        assert_eq!(json["total"], 1);
        assert_eq!(json["limit"], 50);
        assert_eq!(json["offset"], 0);
        assert_eq!(json["has_more"], false);
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
