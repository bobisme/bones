//! `bn show` — display full details of a single work item.
//!
//! Supports partial ID resolution: "a7x" → "bn-a7x", and prefix matching
//! when an exact match is not found.

use crate::output::{
    CliError, OutputMode, pretty_kv, pretty_rule, pretty_section, render_error, render_mode,
};
use crate::validate;
use bones_core::db::query;
use chrono::{DateTime, Local, Utc};
use clap::Args;
use rusqlite::params;
use serde::Serialize;
use std::io::Write;

#[derive(Args, Debug)]
pub struct ShowArgs {
    /// Item ID to display. Supports partial IDs: "a7x" → "bn-a7x".
    pub id: String,
}

/// Full item detail as returned in JSON output.
#[derive(Debug, Serialize)]
pub struct ShowItem {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub kind: String,
    pub state: String,
    pub urgency: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    /// Items this item depends on (blockers).
    pub depends_on: Vec<String>,
    /// Items that depend on this item.
    pub dependents: Vec<String>,
    pub comments: Vec<ShowComment>,
    pub created_at_us: i64,
    pub updated_at_us: i64,
}

/// A single comment in the `show` output.
#[derive(Debug, Serialize)]
pub struct ShowComment {
    pub author: String,
    pub body: String,
    pub created_at_us: i64,
}

fn micros_to_local_datetime(us: i64) -> String {
    DateTime::<Utc>::from_timestamp_micros(us)
        .map(|ts| {
            ts.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| us.to_string())
}

fn timeline_comments(mut comments: Vec<query::QueryComment>) -> Vec<ShowComment> {
    comments.sort_by(|a, b| {
        a.created_at_us
            .cmp(&b.created_at_us)
            .then_with(|| a.comment_id.cmp(&b.comment_id))
    });

    comments
        .into_iter()
        .map(|c| ShowComment {
            author: c.author,
            body: c.body,
            created_at_us: c.created_at_us,
        })
        .collect()
}

/// Execute `bn show <id>`.
///
/// Resolves partial IDs before querying. Opens the projection database
/// gracefully; returns a clear error if the item is not found.
///
/// # Errors
///
/// Returns an error if the database query fails or output rendering fails.
pub fn run_show(
    args: &ShowArgs,
    output: OutputMode,
    project_root: &std::path::Path,
) -> anyhow::Result<()> {
    if let Err(e) = validate::validate_item_id(&args.id) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }

    let db_path = project_root.join(".bones/bones.db");

    // Gracefully handle missing / corrupt projection
    let conn = match query::try_open_projection(&db_path)? {
        Some(c) => c,
        None => {
            render_error(
                output,
                &CliError::with_details(
                    "projection database not found",
                    "run `bn admin rebuild` to initialize the projection",
                    "projection_missing",
                ),
            )?;
            anyhow::bail!("projection not found");
        }
    };

    // Resolve the ID (possibly partial)
    let resolved_id = match resolve_item_id(&conn, &args.id)? {
        Some(id) => id,
        None => {
            render_error(
                output,
                &CliError::with_details(
                    format!("item '{}' not found", args.id),
                    "use `bn list` to see available items",
                    "item_not_found",
                ),
            )?;
            anyhow::bail!("item '{}' not found", args.id);
        }
    };

    // Fetch item
    let item = match query::get_item(&conn, &resolved_id, false)? {
        Some(i) => i,
        None => {
            render_error(
                output,
                &CliError::with_details(
                    format!("item '{}' not found", resolved_id),
                    "the item may have been deleted; use `bn list --state done` to find closed items",
                    "item_not_found",
                ),
            )?;
            anyhow::bail!("item '{}' not found", resolved_id);
        }
    };

    // Fetch related data
    let labels: Vec<String> = query::get_labels(&conn, &resolved_id)?
        .into_iter()
        .map(|l| l.label)
        .collect();

    let assignees: Vec<String> = query::get_assignees(&conn, &resolved_id)?
        .into_iter()
        .map(|a| a.agent)
        .collect();

    let depends_on: Vec<String> = query::get_dependencies(&conn, &resolved_id)?
        .into_iter()
        .map(|d| d.depends_on_item_id)
        .collect();

    let dependents: Vec<String> = query::get_dependents(&conn, &resolved_id)?
        .into_iter()
        .map(|d| d.item_id)
        .collect();

    let comments = timeline_comments(query::get_comments(&conn, &resolved_id, None, None)?);

    let show_item = ShowItem {
        id: item.item_id.clone(),
        title: item.title.clone(),
        description: item.description.clone(),
        kind: item.kind.clone(),
        state: item.state.clone(),
        urgency: item.urgency.clone(),
        size: item.size.clone(),
        parent_id: item.parent_id.clone(),
        labels,
        assignees,
        depends_on,
        dependents,
        comments,
        created_at_us: item.created_at_us,
        updated_at_us: item.updated_at_us,
    };

    render_mode(
        output,
        &show_item,
        |item, w| render_show_text(item, w),
        |item, w| render_show_human(item, w),
    )
}

/// Render full item details in human-readable format.
fn render_show_human(item: &ShowItem, w: &mut dyn Write) -> std::io::Result<()> {
    pretty_section(w, &format!("Item {}", item.id))?;
    writeln!(w, "{}", item.title)?;
    pretty_rule(w)?;
    pretty_kv(w, "kind", &item.kind)?;
    pretty_kv(w, "state", &item.state)?;
    pretty_kv(w, "urgency", &item.urgency)?;
    if let Some(ref size) = item.size {
        pretty_kv(w, "size", size)?;
    }
    if let Some(ref parent) = item.parent_id {
        pretty_kv(w, "parent", parent)?;
    }
    if !item.labels.is_empty() {
        pretty_kv(w, "labels", item.labels.join(", "))?;
    }
    if !item.assignees.is_empty() {
        pretty_kv(w, "assigned", item.assignees.join(", "))?;
    }
    if !item.depends_on.is_empty() {
        pretty_kv(w, "depends_on", item.depends_on.join(", "))?;
    }
    if !item.dependents.is_empty() {
        pretty_kv(w, "dependents", item.dependents.join(", "))?;
    }

    if let Some(ref desc) = item.description {
        writeln!(w)?;
        pretty_section(w, "Description")?;
        for line in desc.lines() {
            writeln!(w, "{line}")?;
        }
    }

    if !item.comments.is_empty() {
        writeln!(w)?;
        pretty_section(w, &format!("Comments ({})", item.comments.len()))?;
        for (i, comment) in item.comments.iter().enumerate() {
            if i > 0 {
                writeln!(w)?;
            }
            writeln!(
                w,
                "[{}] {}: {}",
                micros_to_local_datetime(comment.created_at_us),
                comment.author,
                comment.body
            )?;
        }
    }
    Ok(())
}

fn render_show_text(item: &ShowItem, w: &mut dyn Write) -> std::io::Result<()> {
    writeln!(w, "Item {}", item.id)?;
    writeln!(w, "{:-<72}", "")?;
    writeln!(w, "{}", item.title)?;
    writeln!(w, "kind:        {}", item.kind)?;
    writeln!(w, "state:       {}", item.state)?;
    writeln!(w, "urgency:     {}", item.urgency)?;
    if let Some(ref size) = item.size {
        writeln!(w, "size:        {}", size)?;
    }
    if let Some(ref parent) = item.parent_id {
        writeln!(w, "parent:      {}", parent)?;
    }
    if !item.labels.is_empty() {
        writeln!(w, "labels:      {}", item.labels.join(", "))?;
    }
    if !item.assignees.is_empty() {
        writeln!(w, "assignees:   {}", item.assignees.join(", "))?;
    }
    if !item.depends_on.is_empty() {
        writeln!(w, "depends_on:  {}", item.depends_on.join(", "))?;
    }
    if !item.dependents.is_empty() {
        writeln!(w, "dependents:  {}", item.dependents.join(", "))?;
    }
    if let Some(ref desc) = item.description {
        writeln!(w)?;
        writeln!(w, "Description")?;
        writeln!(w, "{:-<72}", "")?;
        for line in desc.lines() {
            writeln!(w, "{line}")?;
        }
    }

    if !item.comments.is_empty() {
        writeln!(w)?;
        writeln!(w, "Comments ({})", item.comments.len())?;
        writeln!(w, "{:-<72}", "")?;
        for (idx, comment) in item.comments.iter().enumerate() {
            if idx > 0 {
                writeln!(w)?;
            }
            writeln!(
                w,
                "[{}] {}: {}",
                micros_to_local_datetime(comment.created_at_us),
                comment.author,
                comment.body
            )?;
        }
    }
    Ok(())
}

/// Resolve a (possibly partial) item ID to a full canonical item ID.
///
/// Resolution order:
/// 1. Exact match on `item_id`
/// 2. If the input lacks the "bn-" prefix, try "bn-{input}" exactly
/// 3. Prefix match on `item_id LIKE 'bn-{input}%'` (or `LIKE '{input}%'`)
///
/// Returns `None` when no match is found.
///
/// # Errors
///
/// Returns an error if the database query fails.
pub fn resolve_item_id(conn: &rusqlite::Connection, input: &str) -> anyhow::Result<Option<String>> {
    let input = input.trim();

    // 1. Exact match
    let exact: Option<String> = conn
        .query_row(
            "SELECT item_id FROM items WHERE item_id = ?1 AND is_deleted = 0 LIMIT 1",
            params![input],
            |row| row.get(0),
        )
        .ok();
    if exact.is_some() {
        return Ok(exact);
    }

    // 2. Try prefixing "bn-" if not already present
    if !input.starts_with("bn-") {
        let with_prefix = format!("bn-{input}");
        let exact2: Option<String> = conn
            .query_row(
                "SELECT item_id FROM items WHERE item_id = ?1 AND is_deleted = 0 LIMIT 1",
                params![with_prefix],
                |row| row.get(0),
            )
            .ok();
        if exact2.is_some() {
            return Ok(exact2);
        }

        // 3a. Prefix match on "bn-{input}%"
        let like_pattern = format!("bn-{input}%");
        if let Some(resolved) = resolve_prefix_match(conn, input, &like_pattern)? {
            return Ok(Some(resolved));
        }
    } else {
        // 3b. Prefix match on "{input}%" (already has "bn-")
        let like_pattern = format!("{input}%");
        if let Some(resolved) = resolve_prefix_match(conn, input, &like_pattern)? {
            return Ok(Some(resolved));
        }
    }

    Ok(None)
}

fn resolve_prefix_match(
    conn: &rusqlite::Connection,
    input: &str,
    like_pattern: &str,
) -> anyhow::Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT item_id FROM items
         WHERE item_id LIKE ?1 AND is_deleted = 0
         ORDER BY item_id
         LIMIT 6",
    )?;

    let rows = stmt.query_map(params![like_pattern], |row| row.get::<_, String>(0))?;
    let mut matches = Vec::new();
    for row in rows {
        matches.push(row?);
    }

    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => {
            anyhow::bail!(
                "ambiguous item ID prefix '{}'; matches: {}",
                input,
                matches.join(", ")
            )
        }
    }
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
    fn show_args_parses_id() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: ShowArgs,
        }
        let w = Wrapper::parse_from(["test", "item-123"]);
        assert_eq!(w.args.id, "item-123");
    }

    // -----------------------------------------------------------------------
    // render_show_human
    // -----------------------------------------------------------------------

    fn make_show_item() -> ShowItem {
        ShowItem {
            id: "bn-abc".into(),
            title: "Fix authentication timeout".into(),
            description: Some("The auth service times out after 30s.".into()),
            kind: "bug".into(),
            state: "doing".into(),
            urgency: "urgent".into(),
            size: Some("m".into()),
            parent_id: Some("bn-parent".into()),
            labels: vec!["backend".into(), "auth".into()],
            assignees: vec!["alice".into()],
            depends_on: vec!["bn-001".into()],
            dependents: vec!["bn-002".into()],
            comments: vec![ShowComment {
                author: "alice".into(),
                body: "Looking into it.".into(),
                created_at_us: 1000,
            }],
            created_at_us: 500,
            updated_at_us: 2000,
        }
    }

    #[test]
    fn render_show_human_includes_all_fields() {
        let item = make_show_item();
        let mut buf = Vec::new();
        render_show_human(&item, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();

        assert!(out.contains("bn-abc"), "missing id");
        assert!(out.contains("Fix authentication timeout"), "missing title");
        assert!(out.contains("bug"), "missing kind");
        assert!(out.contains("doing"), "missing state");
        assert!(out.contains("urgent"), "missing urgency");
        assert!(out.contains("m"), "missing size");
        assert!(out.contains("bn-parent"), "missing parent");
        assert!(out.contains("backend"), "missing label");
        assert!(out.contains("alice"), "missing assignee");
        assert!(out.contains("bn-001"), "missing depends_on");
        assert!(out.contains("bn-002"), "missing dependent");
        assert!(out.contains("Looking into it"), "missing comment");
        assert!(out.contains("The auth service"), "missing description");
    }

    #[test]
    fn render_show_human_without_optional_fields() {
        let item = ShowItem {
            id: "bn-min".into(),
            title: "Minimal item".into(),
            description: None,
            kind: "task".into(),
            state: "open".into(),
            urgency: "default".into(),
            size: None,
            parent_id: None,
            labels: vec![],
            assignees: vec![],
            depends_on: vec![],
            dependents: vec![],
            comments: vec![],
            created_at_us: 100,
            updated_at_us: 200,
        };
        let mut buf = Vec::new();
        render_show_human(&item, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("bn-min"));
        assert!(out.contains("Minimal item"));
        // Optional fields should be absent
        assert!(!out.contains("parent"));
        assert!(!out.contains("labels"));
    }

    #[test]
    fn timeline_comments_sorted_oldest_first() {
        let input = vec![
            query::QueryComment {
                comment_id: 2,
                item_id: "bn-1".into(),
                event_hash: "blake3:b".into(),
                author: "bob".into(),
                body: "second".into(),
                created_at_us: 2_000,
            },
            query::QueryComment {
                comment_id: 1,
                item_id: "bn-1".into(),
                event_hash: "blake3:a".into(),
                author: "alice".into(),
                body: "first".into(),
                created_at_us: 1_000,
            },
        ];

        let out = timeline_comments(input);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].author, "alice");
        assert_eq!(out[1].author, "bob");
    }

    #[test]
    fn render_show_text_comments_include_timestamp() {
        let item = make_show_item();
        let mut buf = Vec::new();
        render_show_text(&item, &mut buf).expect("render text");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(out.contains("Comments (1)"));
        assert!(out.contains("] alice: Looking into it."));
    }

    // -----------------------------------------------------------------------
    // resolve_item_id
    // -----------------------------------------------------------------------

    fn test_db_with_item(item_id: &str) -> Connection {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, \
             search_labels, created_at_us, updated_at_us) \
             VALUES (?1, 'Test', 'task', 'open', 'default', 0, '', 100, 200)",
            params![item_id],
        )
        .expect("insert item");
        conn
    }

    #[test]
    fn resolve_exact_id() {
        let conn = test_db_with_item("bn-abc123");
        assert_eq!(
            resolve_item_id(&conn, "bn-abc123").unwrap(),
            Some("bn-abc123".into())
        );
    }

    #[test]
    fn resolve_without_bn_prefix() {
        let conn = test_db_with_item("bn-abc123");
        assert_eq!(
            resolve_item_id(&conn, "abc123").unwrap(),
            Some("bn-abc123".into())
        );
    }

    #[test]
    fn resolve_prefix_match() {
        let conn = test_db_with_item("bn-abc123");
        // "abc" → "bn-abc123" via prefix match
        assert_eq!(
            resolve_item_id(&conn, "abc").unwrap(),
            Some("bn-abc123".into())
        );
    }

    #[test]
    fn resolve_prefix_match_with_bn_prefix() {
        let conn = test_db_with_item("bn-abc123");
        // "bn-abc" → "bn-abc123" via prefix match
        assert_eq!(
            resolve_item_id(&conn, "bn-abc").unwrap(),
            Some("bn-abc123".into())
        );
    }

    #[test]
    fn resolve_not_found() {
        let conn = test_db_with_item("bn-abc123");
        assert!(resolve_item_id(&conn, "xyz999").unwrap().is_none());
    }

    #[test]
    fn resolve_prefix_rejects_ambiguous_matches() {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, created_at_us, updated_at_us)\
             VALUES ('bn-100', 'A', 'task', 'open', 'default', 0, '', 100, 200)",
            [],
        )
        .expect("insert first item");
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, created_at_us, updated_at_us)\
             VALUES ('bn-101', 'B', 'task', 'open', 'default', 0, '', 100, 200)",
            [],
        )
        .expect("insert second item");

        let err = resolve_item_id(&conn, "bn-10").expect_err("prefix should be ambiguous");
        assert!(err.to_string().contains("ambiguous item ID prefix"));
    }

    #[test]
    fn resolve_skips_deleted() {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, \
             search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-del1', 'Deleted', 'task', 'open', 'default', 1, '', 100, 200)",
            [],
        )
        .expect("insert deleted item");
        assert!(resolve_item_id(&conn, "del1").unwrap().is_none());
    }

    // -----------------------------------------------------------------------
    // Integration: run_show against temp DB
    // -----------------------------------------------------------------------

    fn setup_test_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(&bones_dir).unwrap();
        let db_path = bones_dir.join("bones.db");

        let mut conn = Connection::open(&db_path).expect("open db");
        migrations::migrate(&mut conn).expect("migrate");

        conn.execute(
            "INSERT INTO items (item_id, title, description, kind, state, urgency, \
             is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-xyz789', 'Auth bug', 'Details here.', 'bug', 'open', 'urgent', \
             0, 'backend', 500, 1000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO item_labels (item_id, label, created_at_us) VALUES ('bn-xyz789', 'backend', 100)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO item_comments (item_id, event_hash, author, body, created_at_us) \
             VALUES ('bn-xyz789', 'blake3:hash1', 'alice', 'Investigating.', 200)",
            [],
        )
        .unwrap();

        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test]
    fn run_show_exact_id() {
        let (_dir, root) = setup_test_db();
        let args = ShowArgs {
            id: "bn-xyz789".into(),
        };
        run_show(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_show_partial_id() {
        let (_dir, root) = setup_test_db();
        // "xyz789" → "bn-xyz789"
        let args = ShowArgs {
            id: "xyz789".into(),
        };
        run_show(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_show_prefix_partial_id() {
        let (_dir, root) = setup_test_db();
        // "xyz" → prefix match → "bn-xyz789"
        let args = ShowArgs { id: "xyz".into() };
        run_show(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_show_json_output() {
        let (_dir, root) = setup_test_db();
        let args = ShowArgs {
            id: "bn-xyz789".into(),
        };
        run_show(&args, OutputMode::Json, &root).unwrap();
    }

    #[test]
    fn run_show_not_found_returns_error() {
        let (_dir, root) = setup_test_db();
        let args = ShowArgs {
            id: "nonexistent".into(),
        };
        assert!(run_show(&args, OutputMode::Human, &root).is_err());
    }

    #[test]
    fn run_show_missing_projection_returns_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = ShowArgs {
            id: "bn-001".into(),
        };
        assert!(run_show(&args, OutputMode::Human, dir.path()).is_err());
    }

    #[test]
    fn show_item_json_serializable() {
        let item = ShowItem {
            id: "bn-test".into(),
            title: "Test".into(),
            description: Some("Desc".into()),
            kind: "task".into(),
            state: "open".into(),
            urgency: "default".into(),
            size: None,
            parent_id: None,
            labels: vec!["auth".into()],
            assignees: vec!["alice".into()],
            depends_on: vec!["bn-001".into()],
            dependents: vec![],
            comments: vec![ShowComment {
                author: "alice".into(),
                body: "LGTM".into(),
                created_at_us: 1000,
            }],
            created_at_us: 500,
            updated_at_us: 1000,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("bn-test"));
        assert!(json.contains("auth"));
        assert!(json.contains("LGTM"));
        // size and parent_id omitted
        assert!(!json.contains("\"size\""));
        assert!(!json.contains("\"parent_id\""));
    }
}
