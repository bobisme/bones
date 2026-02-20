//! `SQLite` query helpers for the projection database.
//!
//! Provides typed Rust structs and composable query functions for common
//! access patterns: list/filter items, get by ID, search FTS5, retrieve
//! dependencies, labels, assignees, and comments.
//!
//! All functions take a shared `&Connection` reference and return
//! `anyhow::Result<T>` with typed structs (never raw rows).

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params, params_from_iter};
use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// A projected work item row from the `items` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryItem {
    pub item_id: String,
    pub title: String,
    pub description: Option<String>,
    pub kind: String,
    pub state: String,
    pub urgency: String,
    pub size: Option<String>,
    pub parent_id: Option<String>,
    pub compact_summary: Option<String>,
    pub is_deleted: bool,
    pub deleted_at_us: Option<i64>,
    pub search_labels: String,
    pub created_at_us: i64,
    pub updated_at_us: i64,
}

/// A comment attached to a work item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryComment {
    pub comment_id: i64,
    pub item_id: String,
    pub event_hash: String,
    pub author: String,
    pub body: String,
    pub created_at_us: i64,
}

/// A dependency edge between two items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryDependency {
    pub item_id: String,
    pub depends_on_item_id: String,
    pub link_type: String,
    pub created_at_us: i64,
}

/// A label attached to an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryLabel {
    pub item_id: String,
    pub label: String,
    pub created_at_us: i64,
}

/// Global label inventory row with usage count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelCount {
    pub name: String,
    pub count: usize,
}

/// An assignee of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryAssignee {
    pub item_id: String,
    pub agent: String,
    pub created_at_us: i64,
}

/// An FTS5 search hit with BM25 relevance score.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub item_id: String,
    pub title: String,
    pub rank: f64,
}

/// Aggregate counters for project-level stats used by reporting commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectStats {
    /// Open items by state (excluding deleted).
    pub by_state: HashMap<String, usize>,
    /// Open items by kind (excluding deleted).
    pub by_kind: HashMap<String, usize>,
    /// Open items by urgency (excluding deleted).
    pub by_urgency: HashMap<String, usize>,
    /// Events by type from the projected event tracker (empty when unavailable).
    pub events_by_type: HashMap<String, usize>,
    /// Events by agent from the projected event tracker (empty when unavailable).
    pub events_by_agent: HashMap<String, usize>,
}

// ---------------------------------------------------------------------------
// Sorting
// ---------------------------------------------------------------------------

/// Sort order for item listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    /// Most recently created first.
    CreatedDesc,
    /// Oldest first.
    CreatedAsc,
    /// Most recently updated first.
    #[default]
    UpdatedDesc,
    /// Oldest update first.
    UpdatedAsc,
    /// Urgency order: urgent > default > punt, then `updated_at` DESC.
    Priority,
}

impl SortOrder {
    const fn sql_clause(self) -> &'static str {
        match self {
            Self::CreatedDesc => "ORDER BY created_at_us DESC, i.item_id ASC",
            Self::CreatedAsc => "ORDER BY created_at_us ASC, i.item_id ASC",
            Self::UpdatedDesc => "ORDER BY updated_at_us DESC, i.item_id ASC",
            Self::UpdatedAsc => "ORDER BY updated_at_us ASC, i.item_id ASC",
            Self::Priority => {
                "ORDER BY CASE urgency \
                 WHEN 'urgent' THEN 0 \
                 WHEN 'default' THEN 1 \
                 WHEN 'punt' THEN 2 \
                 END ASC, updated_at_us DESC, item_id ASC"
            }
        }
    }
}

impl fmt::Display for SortOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreatedDesc => f.write_str("created_desc"),
            Self::CreatedAsc => f.write_str("created_asc"),
            Self::UpdatedDesc => f.write_str("updated_desc"),
            Self::UpdatedAsc => f.write_str("updated_asc"),
            Self::Priority => f.write_str("priority"),
        }
    }
}

impl FromStr for SortOrder {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "created_desc" | "created-desc" | "newest" => Ok(Self::CreatedDesc),
            "created_asc" | "created-asc" | "oldest" => Ok(Self::CreatedAsc),
            "updated_desc" | "updated-desc" | "recent" => Ok(Self::UpdatedDesc),
            "updated_asc" | "updated-asc" | "stale" => Ok(Self::UpdatedAsc),
            "priority" | "triage" => Ok(Self::Priority),
            other => bail!(
                "unknown sort order '{other}': expected one of created_desc, created_asc, updated_desc, updated_asc, priority"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// Filter criteria for item listings.
///
/// All fields are optional. When multiple fields are set, they are combined
/// with AND semantics.
#[derive(Debug, Clone, Default)]
pub struct ItemFilter {
    /// Filter by lifecycle state (exact match).
    pub state: Option<String>,
    /// Filter by item kind (exact match).
    pub kind: Option<String>,
    /// Filter by urgency (exact match).
    pub urgency: Option<String>,
    /// Filter by label (item must have this label).
    pub label: Option<String>,
    /// Filter by assignee (item must be assigned to this agent).
    pub assignee: Option<String>,
    /// Filter by `parent_id` (exact match).
    pub parent_id: Option<String>,
    /// Include soft-deleted items (default: false).
    pub include_deleted: bool,
    /// Maximum number of results.
    pub limit: Option<u32>,
    /// Offset for pagination.
    pub offset: Option<u32>,
    /// Sort order.
    pub sort: SortOrder,
}

// ---------------------------------------------------------------------------
// Aggregate helper query APIs
// ---------------------------------------------------------------------------

/// Count projected items grouped by `state`, excluding deleted rows.
pub fn item_counts_by_state(conn: &Connection) -> Result<HashMap<String, usize>> {
    count_items_grouped(conn, "state")
}

/// Count projected items grouped by `kind`, excluding deleted rows.
pub fn item_counts_by_kind(conn: &Connection) -> Result<HashMap<String, usize>> {
    count_items_grouped(conn, "kind")
}

/// Count projected items grouped by `urgency`, excluding deleted rows.
pub fn item_counts_by_urgency(conn: &Connection) -> Result<HashMap<String, usize>> {
    count_items_grouped(conn, "urgency")
}

/// Count projected events by `event_type` from `projected_events`.
///
/// Returns an empty map when `projected_events` is not yet available.
pub fn event_counts_by_type(conn: &Connection) -> Result<HashMap<String, usize>> {
    count_grouped_events(conn, "event_type")
}

/// Count projected events by `agent` from `projected_events`.
///
/// Returns an empty map when `projected_events` is not yet available.
pub fn event_counts_by_agent(conn: &Connection) -> Result<HashMap<String, usize>> {
    count_grouped_events(conn, "agent")
}

// ---------------------------------------------------------------------------
// Core query functions
// ---------------------------------------------------------------------------

/// Fetch a single item by exact `item_id`.
///
/// Returns `None` if the item does not exist (or is soft-deleted unless
/// `include_deleted` is true).
///
/// # Errors
///
/// Returns an error if the database query fails.
pub fn get_item(
    conn: &Connection,
    item_id: &str,
    include_deleted: bool,
) -> Result<Option<QueryItem>> {
    let sql = if include_deleted {
        "SELECT item_id, title, description, kind, state, urgency, size, \
         parent_id, compact_summary, is_deleted, deleted_at_us, \
         search_labels, created_at_us, updated_at_us \
         FROM items WHERE item_id = ?1"
    } else {
        "SELECT item_id, title, description, kind, state, urgency, size, \
         parent_id, compact_summary, is_deleted, deleted_at_us, \
         search_labels, created_at_us, updated_at_us \
         FROM items WHERE item_id = ?1 AND is_deleted = 0"
    };

    let mut stmt = conn.prepare(sql).context("prepare get_item query")?;

    let result = stmt.query_row(params![item_id], row_to_query_item);

    match result {
        Ok(item) => Ok(Some(item)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e).context(format!("get_item for '{item_id}'")),
    }
}

/// List items matching the given filter criteria.
///
/// Returns items in the requested sort order, limited by `filter.limit`
/// and offset by `filter.offset`.
///
/// # Errors
///
/// Returns an error if the database query fails.
pub fn list_items(conn: &Connection, filter: &ItemFilter) -> Result<Vec<QueryItem>> {
    let mut conditions: Vec<String> = Vec::new();
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if !filter.include_deleted {
        conditions.push("i.is_deleted = 0".to_string());
    }

    if let Some(ref state) = filter.state {
        param_values.push(Box::new(state.clone()));
        conditions.push(format!("i.state = ?{}", param_values.len()));
    }

    if let Some(ref kind) = filter.kind {
        param_values.push(Box::new(kind.clone()));
        conditions.push(format!("i.kind = ?{}", param_values.len()));
    }

    if let Some(ref urgency) = filter.urgency {
        param_values.push(Box::new(urgency.clone()));
        conditions.push(format!("i.urgency = ?{}", param_values.len()));
    }

    if let Some(ref parent_id) = filter.parent_id {
        param_values.push(Box::new(parent_id.clone()));
        conditions.push(format!("i.parent_id = ?{}", param_values.len()));
    }

    // Label and assignee filters require JOINs
    let mut joins = String::new();
    if let Some(ref label) = filter.label {
        param_values.push(Box::new(label.clone()));
        let _ = write!(
            joins,
            " INNER JOIN item_labels il ON il.item_id = i.item_id AND il.label = ?{}",
            param_values.len()
        );
    }

    if let Some(ref assignee) = filter.assignee {
        param_values.push(Box::new(assignee.clone()));
        let _ = write!(
            joins,
            " INNER JOIN item_assignees ia ON ia.item_id = i.item_id AND ia.agent = ?{}",
            param_values.len()
        );
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };

    let sort_clause = filter.sort.sql_clause();

    let limit_clause = match (filter.limit, filter.offset) {
        (Some(limit), Some(offset)) => format!(" LIMIT {limit} OFFSET {offset}"),
        (Some(limit), None) => format!(" LIMIT {limit}"),
        (None, Some(offset)) => format!(" LIMIT -1 OFFSET {offset}"),
        (None, None) => String::new(),
    };

    let sql = format!(
        "SELECT i.item_id, i.title, i.description, i.kind, i.state, i.urgency, i.size, \
         i.parent_id, i.compact_summary, i.is_deleted, i.deleted_at_us, \
         i.search_labels, i.created_at_us, i.updated_at_us \
         FROM items i{joins}{where_clause} {sort_clause}{limit_clause}"
    );

    let mut stmt = conn
        .prepare(&sql)
        .with_context(|| format!("prepare list_items query: {sql}"))?;

    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(AsRef::as_ref).collect();

    let rows = stmt
        .query_map(params_from_iter(params_ref), row_to_query_item)
        .context("execute list_items query")?;

    let mut items = Vec::new();
    for row in rows {
        items.push(row.context("read list_items row")?);
    }
    Ok(items)
}

/// Search items via FTS5 full-text search with BM25 ranking.
///
/// Column weights: title 3×, description 2×, labels 1×.
///
/// Returns up to `limit` results sorted by BM25 relevance (best match first).
///
/// # Errors
///
/// Returns an error if the FTS5 query fails (e.g., syntax error in query).
pub fn search(conn: &Connection, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
    let sql = "SELECT f.item_id, i.title, bm25(items_fts, 3.0, 2.0, 1.0) AS rank \
               FROM items_fts f \
               INNER JOIN items i ON i.item_id = f.item_id \
               WHERE items_fts MATCH ?1 AND i.is_deleted = 0 \
               ORDER BY rank \
               LIMIT ?2";

    let mut stmt = conn.prepare(sql).context("prepare FTS5 search query")?;

    let rows = stmt
        .query_map(params![query, limit], |row| {
            Ok(SearchHit {
                item_id: row.get(0)?,
                title: row.get(1)?,
                rank: row.get(2)?,
            })
        })
        .with_context(|| format!("execute FTS5 search for '{query}'"))?;

    let mut hits = Vec::new();
    for row in rows {
        hits.push(row.context("read search hit")?);
    }
    Ok(hits)
}

/// Get all labels for an item.
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn get_labels(conn: &Connection, item_id: &str) -> Result<Vec<QueryLabel>> {
    let sql = "SELECT item_id, label, created_at_us \
               FROM item_labels WHERE item_id = ?1 \
               ORDER BY label";

    let mut stmt = conn.prepare(sql).context("prepare get_labels")?;
    let rows = stmt
        .query_map(params![item_id], |row| {
            Ok(QueryLabel {
                item_id: row.get(0)?,
                label: row.get(1)?,
                created_at_us: row.get(2)?,
            })
        })
        .context("execute get_labels")?;

    let mut labels = Vec::new();
    for row in rows {
        labels.push(row.context("read label row")?);
    }
    Ok(labels)
}

/// List global label usage counts across all items.
///
/// # Errors
///
/// Returns an error if the aggregate query fails.
pub fn list_labels(
    conn: &Connection,
    limit: Option<u32>,
    offset: Option<u32>,
) -> Result<Vec<LabelCount>> {
    let limit_clause = match (limit, offset) {
        (Some(limit), Some(offset)) => format!(" LIMIT {limit} OFFSET {offset}"),
        (Some(limit), None) => format!(" LIMIT {limit}"),
        (None, Some(offset)) => format!(" LIMIT -1 OFFSET {offset}"),
        (None, None) => String::new(),
    };

    let sql = format!(
        "SELECT label, COUNT(*) as count \
         FROM item_labels \
         GROUP BY label \
         ORDER BY count DESC, label ASC{limit_clause}"
    );

    let mut stmt = conn.prepare(&sql).context("prepare list_labels")?;
    let rows = stmt
        .query_map([], |row| {
            let count: i64 = row.get(1)?;
            Ok(LabelCount {
                name: row.get(0)?,
                count: usize::try_from(count).unwrap_or(usize::MAX),
            })
        })
        .context("execute list_labels")?;

    let mut labels = Vec::new();
    for row in rows {
        labels.push(row.context("read list_labels row")?);
    }
    Ok(labels)
}

/// Get all assignees for an item.
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn get_assignees(conn: &Connection, item_id: &str) -> Result<Vec<QueryAssignee>> {
    let sql = "SELECT item_id, agent, created_at_us \
               FROM item_assignees WHERE item_id = ?1 \
               ORDER BY agent";

    let mut stmt = conn.prepare(sql).context("prepare get_assignees")?;
    let rows = stmt
        .query_map(params![item_id], |row| {
            Ok(QueryAssignee {
                item_id: row.get(0)?,
                agent: row.get(1)?,
                created_at_us: row.get(2)?,
            })
        })
        .context("execute get_assignees")?;

    let mut assignees = Vec::new();
    for row in rows {
        assignees.push(row.context("read assignee row")?);
    }
    Ok(assignees)
}

/// Get all comments for an item, newest first.
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn get_comments(
    conn: &Connection,
    item_id: &str,
    limit: Option<u32>,
    offset: Option<u32>,
) -> Result<Vec<QueryComment>> {
    let limit_clause = match (limit, offset) {
        (Some(limit), Some(offset)) => format!(" LIMIT {limit} OFFSET {offset}"),
        (Some(limit), None) => format!(" LIMIT {limit}"),
        (None, Some(offset)) => format!(" LIMIT -1 OFFSET {offset}"),
        (None, None) => String::new(),
    };

    let sql = format!(
        "SELECT comment_id, item_id, event_hash, author, body, created_at_us \
         FROM item_comments WHERE item_id = ?1 \
         ORDER BY created_at_us DESC{limit_clause}"
    );

    let mut stmt = conn.prepare(&sql).context("prepare get_comments")?;
    let rows = stmt
        .query_map(params![item_id], |row| {
            Ok(QueryComment {
                comment_id: row.get(0)?,
                item_id: row.get(1)?,
                event_hash: row.get(2)?,
                author: row.get(3)?,
                body: row.get(4)?,
                created_at_us: row.get(5)?,
            })
        })
        .context("execute get_comments")?;

    let mut comments = Vec::new();
    for row in rows {
        comments.push(row.context("read comment row")?);
    }
    Ok(comments)
}

/// Get items that a given item depends on (its blockers).
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn get_dependencies(conn: &Connection, item_id: &str) -> Result<Vec<QueryDependency>> {
    let sql = "SELECT item_id, depends_on_item_id, link_type, created_at_us \
               FROM item_dependencies WHERE item_id = ?1 \
               ORDER BY depends_on_item_id";

    let mut stmt = conn.prepare(sql).context("prepare get_dependencies")?;
    let rows = stmt
        .query_map(params![item_id], |row| {
            Ok(QueryDependency {
                item_id: row.get(0)?,
                depends_on_item_id: row.get(1)?,
                link_type: row.get(2)?,
                created_at_us: row.get(3)?,
            })
        })
        .context("execute get_dependencies")?;

    let mut deps = Vec::new();
    for row in rows {
        deps.push(row.context("read dependency row")?);
    }
    Ok(deps)
}

/// Get items that depend on the given item (its dependents / reverse deps).
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn get_dependents(conn: &Connection, item_id: &str) -> Result<Vec<QueryDependency>> {
    let sql = "SELECT item_id, depends_on_item_id, link_type, created_at_us \
               FROM item_dependencies WHERE depends_on_item_id = ?1 \
               ORDER BY item_id";

    let mut stmt = conn.prepare(sql).context("prepare get_dependents")?;
    let rows = stmt
        .query_map(params![item_id], |row| {
            Ok(QueryDependency {
                item_id: row.get(0)?,
                depends_on_item_id: row.get(1)?,
                link_type: row.get(2)?,
                created_at_us: row.get(3)?,
            })
        })
        .context("execute get_dependents")?;

    let mut deps = Vec::new();
    for row in rows {
        deps.push(row.context("read dependent row")?);
    }
    Ok(deps)
}

/// Get child items of the given parent item.
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn get_children(conn: &Connection, parent_id: &str) -> Result<Vec<QueryItem>> {
    let sql = "SELECT item_id, title, description, kind, state, urgency, size, \
               parent_id, compact_summary, is_deleted, deleted_at_us, \
               search_labels, created_at_us, updated_at_us \
               FROM items WHERE parent_id = ?1 AND is_deleted = 0 \
               ORDER BY created_at_us ASC";

    let mut stmt = conn.prepare(sql).context("prepare get_children")?;
    let rows = stmt
        .query_map(params![parent_id], row_to_query_item)
        .context("execute get_children")?;

    let mut children = Vec::new();
    for row in rows {
        children.push(row.context("read child row")?);
    }
    Ok(children)
}

/// Count items matching the given filter criteria.
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn count_items(conn: &Connection, filter: &ItemFilter) -> Result<u64> {
    let mut conditions: Vec<String> = Vec::new();
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if !filter.include_deleted {
        conditions.push("i.is_deleted = 0".to_string());
    }

    if let Some(ref state) = filter.state {
        param_values.push(Box::new(state.clone()));
        conditions.push(format!("i.state = ?{}", param_values.len()));
    }

    if let Some(ref kind) = filter.kind {
        param_values.push(Box::new(kind.clone()));
        conditions.push(format!("i.kind = ?{}", param_values.len()));
    }

    if let Some(ref urgency) = filter.urgency {
        param_values.push(Box::new(urgency.clone()));
        conditions.push(format!("i.urgency = ?{}", param_values.len()));
    }

    if let Some(ref parent_id) = filter.parent_id {
        param_values.push(Box::new(parent_id.clone()));
        conditions.push(format!("i.parent_id = ?{}", param_values.len()));
    }

    let mut joins = String::new();
    if let Some(ref label) = filter.label {
        param_values.push(Box::new(label.clone()));
        let _ = write!(
            joins,
            " INNER JOIN item_labels il ON il.item_id = i.item_id AND il.label = ?{}",
            param_values.len()
        );
    }

    if let Some(ref assignee) = filter.assignee {
        param_values.push(Box::new(assignee.clone()));
        let _ = write!(
            joins,
            " INNER JOIN item_assignees ia ON ia.item_id = i.item_id AND ia.agent = ?{}",
            param_values.len()
        );
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };

    let sql = format!("SELECT COUNT(*) FROM items i{joins}{where_clause}");

    let mut stmt = conn
        .prepare(&sql)
        .with_context(|| format!("prepare count_items: {sql}"))?;

    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(AsRef::as_ref).collect();

    let count: i64 = stmt
        .query_row(params_from_iter(params_ref), |row| row.get(0))
        .context("execute count_items")?;

    Ok(u64::try_from(count).unwrap_or(0))
}

/// Check if an item exists in the projection.
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn item_exists(conn: &Connection, item_id: &str) -> Result<bool> {
    let sql = "SELECT EXISTS(SELECT 1 FROM items WHERE item_id = ?1)";
    let exists: bool = conn
        .query_row(sql, params![item_id], |row| row.get(0))
        .context("check item_exists")?;
    Ok(exists)
}

/// Get the projection cursor metadata (last replay offset and hash).
///
/// Returns `(last_event_offset, last_event_hash)`.
///
/// # Errors
///
/// Returns an error if the query fails or no `projection_meta` row exists.
pub fn get_projection_cursor(conn: &Connection) -> Result<(i64, Option<String>)> {
    let sql = "SELECT last_event_offset, last_event_hash FROM projection_meta WHERE id = 1";
    conn.query_row(sql, [], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
    })
    .context("read projection cursor")
}

/// Update the projection cursor after replaying events.
///
/// # Errors
///
/// Returns an error if the update fails.
pub fn update_projection_cursor(
    conn: &Connection,
    offset: i64,
    event_hash: Option<&str>,
) -> Result<()> {
    let now_us = chrono::Utc::now().timestamp_micros();
    conn.execute(
        "UPDATE projection_meta SET last_event_offset = ?1, last_event_hash = ?2, \
         last_rebuild_at_us = ?3 WHERE id = 1",
        params![offset, event_hash, now_us],
    )
    .context("update projection cursor")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn count_items_grouped(conn: &Connection, column: &str) -> Result<HashMap<String, usize>> {
    let sql =
        format!("SELECT {column}, COUNT(*) FROM items WHERE is_deleted = 0 GROUP BY {column}");
    let mut stmt = conn
        .prepare(&sql)
        .context("prepare aggregate count query")?;
    let rows = stmt.query_map([], |row| {
        let key: String = row.get(0)?;
        let count: i64 = row.get(1)?;
        Ok((key, usize::try_from(count).unwrap_or(usize::MAX)))
    })?;

    let mut counts = HashMap::new();
    for row in rows {
        let (key, count) = row.context("read aggregate count")?;
        counts.insert(key, count);
    }

    Ok(counts)
}

fn count_grouped_events(conn: &Connection, group_by: &str) -> Result<HashMap<String, usize>> {
    if !table_exists(conn, "projected_events")? {
        return Ok(HashMap::new());
    }

    if !table_has_column(conn, "projected_events", group_by)? {
        return Ok(HashMap::new());
    }

    let sql = format!(
        "SELECT {group_by}, COUNT(*) FROM projected_events WHERE {group_by} IS NOT NULL GROUP BY {group_by}"
    );
    let mut stmt = conn
        .prepare(&sql)
        .context("prepare projected event aggregate query")?;
    let rows = stmt.query_map([], |row| {
        let key: String = row.get(0)?;
        let count: i64 = row.get(1)?;
        Ok((key, usize::try_from(count).unwrap_or(usize::MAX)))
    })?;

    let mut counts = HashMap::new();
    for row in rows {
        let (key, count) = row.context("read projected event aggregate")?;
        counts.insert(key, count);
    }

    Ok(counts)
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )
        .context("check table exists")?;
    Ok(exists)
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .context("prepare table_info pragma")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;

    for row in rows {
        let name = row.context("read table_info column")?;
        if name == column {
            return Ok(true);
        }
    }

    Ok(false)
}

fn row_to_query_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueryItem> {
    Ok(QueryItem {
        item_id: row.get(0)?,
        title: row.get(1)?,
        description: row.get(2)?,
        kind: row.get(3)?,
        state: row.get(4)?,
        urgency: row.get(5)?,
        size: row.get(6)?,
        parent_id: row.get(7)?,
        compact_summary: row.get(8)?,
        is_deleted: row.get::<_, i64>(9)? != 0,
        deleted_at_us: row.get(10)?,
        search_labels: row.get(11)?,
        created_at_us: row.get(12)?,
        updated_at_us: row.get(13)?,
    })
}

// ---------------------------------------------------------------------------
// Graceful recovery
// ---------------------------------------------------------------------------

/// Attempt to open the projection database with graceful recovery.
///
/// If the database file is missing or corrupt, returns `Ok(None)` instead
/// of an error. The caller can then trigger a full rebuild.
///
/// # Errors
///
/// Returns an error only for unexpected I/O failures (not missing/corrupt DB).
pub fn try_open_projection(path: &std::path::Path) -> Result<Option<Connection>> {
    if !path.exists() {
        return Ok(None);
    }

    match super::open_projection(path) {
        Ok(conn) => {
            // Quick integrity check — verify projection_meta is readable
            if get_projection_cursor(&conn).is_ok() {
                Ok(Some(conn))
            } else {
                tracing::warn!(
                    path = %path.display(),
                    "projection database corrupt, needs rebuild"
                );
                Ok(None)
            }
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "failed to open projection database, needs rebuild"
            );
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{migrations, open_projection};
    use rusqlite::{Connection, params};

    /// Create an in-memory migrated database with test data.
    fn test_db() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        conn
    }

    /// Insert a test item with minimal required fields.
    fn insert_item(conn: &Connection, id: &str, title: &str, state: &str, urgency: &str) {
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, \
             is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES (?1, ?2, 'task', ?3, ?4, 0, '', ?5, ?6)",
            params![id, title, state, urgency, 1000_i64, 2000_i64],
        )
        .expect("insert item");
    }

    fn insert_item_full(
        conn: &Connection,
        id: &str,
        title: &str,
        desc: Option<&str>,
        kind: &str,
        state: &str,
        urgency: &str,
        parent_id: Option<&str>,
        labels: &str,
        created: i64,
        updated: i64,
    ) {
        conn.execute(
            "INSERT INTO items (item_id, title, description, kind, state, urgency, \
             parent_id, is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?10)",
            params![
                id, title, desc, kind, state, urgency, parent_id, labels, created, updated
            ],
        )
        .expect("insert full item");
    }

    fn insert_label(conn: &Connection, item_id: &str, label: &str) {
        conn.execute(
            "INSERT INTO item_labels (item_id, label, created_at_us) VALUES (?1, ?2, 100)",
            params![item_id, label],
        )
        .expect("insert label");
    }

    fn insert_assignee(conn: &Connection, item_id: &str, agent: &str) {
        conn.execute(
            "INSERT INTO item_assignees (item_id, agent, created_at_us) VALUES (?1, ?2, 100)",
            params![item_id, agent],
        )
        .expect("insert assignee");
    }

    fn _insert_comment(conn: &Connection, item_id: &str, hash: &str, author: &str, body: &str) {
        conn.execute(
            "INSERT INTO item_comments (item_id, event_hash, author, body, created_at_us) \
             VALUES (?1, ?2, ?3, ?4, 100)",
            params![item_id, hash, author, body],
        )
        .expect("insert comment");
    }

    fn insert_dependency(conn: &Connection, item_id: &str, depends_on: &str) {
        conn.execute(
            "INSERT INTO item_dependencies (item_id, depends_on_item_id, link_type, created_at_us) \
             VALUES (?1, ?2, 'blocks', 100)",
            params![item_id, depends_on],
        )
        .expect("insert dependency");
    }

    // -----------------------------------------------------------------------
    // get_item tests
    // -----------------------------------------------------------------------

    #[test]
    fn get_item_found() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Fix auth timeout", "open", "urgent");

        let item = get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.item_id, "bn-001");
        assert_eq!(item.title, "Fix auth timeout");
        assert_eq!(item.state, "open");
        assert_eq!(item.urgency, "urgent");
        assert!(!item.is_deleted);
    }

    #[test]
    fn get_item_not_found() {
        let conn = test_db();
        let item = get_item(&conn, "bn-nonexistent", false).unwrap();
        assert!(item.is_none());
    }

    #[test]
    fn get_item_excludes_deleted() {
        let conn = test_db();
        insert_item(&conn, "bn-del", "Deleted item", "open", "default");
        conn.execute(
            "UPDATE items SET is_deleted = 1, deleted_at_us = 3000 WHERE item_id = 'bn-del'",
            [],
        )
        .unwrap();

        assert!(get_item(&conn, "bn-del", false).unwrap().is_none());
        let item = get_item(&conn, "bn-del", true).unwrap().unwrap();
        assert!(item.is_deleted);
        assert_eq!(item.deleted_at_us, Some(3000));
    }

    // -----------------------------------------------------------------------
    // list_items tests
    // -----------------------------------------------------------------------

    #[test]
    fn list_items_no_filter() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "First", "open", "default");
        insert_item(&conn, "bn-002", "Second", "doing", "urgent");

        let items = list_items(&conn, &ItemFilter::default()).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn list_items_filter_by_state() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Open item", "open", "default");
        insert_item(&conn, "bn-002", "Doing item", "doing", "default");

        let filter = ItemFilter {
            state: Some("open".to_string()),
            ..Default::default()
        };
        let items = list_items(&conn, &filter).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, "bn-001");
    }

    #[test]
    fn list_items_filter_by_kind() {
        let conn = test_db();
        insert_item_full(
            &conn, "bn-001", "A bug", None, "bug", "open", "default", None, "", 100, 200,
        );
        insert_item_full(
            &conn, "bn-002", "A task", None, "task", "open", "default", None, "", 100, 200,
        );

        let filter = ItemFilter {
            kind: Some("bug".to_string()),
            ..Default::default()
        };
        let items = list_items(&conn, &filter).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, "bn-001");
    }

    #[test]
    fn list_items_filter_by_urgency() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Urgent item", "open", "urgent");
        insert_item(&conn, "bn-002", "Default item", "open", "default");
        insert_item(&conn, "bn-003", "Punt item", "open", "punt");

        let filter = ItemFilter {
            urgency: Some("urgent".to_string()),
            ..Default::default()
        };
        let items = list_items(&conn, &filter).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, "bn-001");
    }

    #[test]
    fn list_items_filter_by_label() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Backend bug", "open", "default");
        insert_item(&conn, "bn-002", "Frontend bug", "open", "default");
        insert_label(&conn, "bn-001", "backend");
        insert_label(&conn, "bn-002", "frontend");

        let filter = ItemFilter {
            label: Some("backend".to_string()),
            ..Default::default()
        };
        let items = list_items(&conn, &filter).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, "bn-001");
    }

    #[test]
    fn list_items_filter_by_assignee() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Alice's task", "open", "default");
        insert_item(&conn, "bn-002", "Bob's task", "open", "default");
        insert_assignee(&conn, "bn-001", "alice");
        insert_assignee(&conn, "bn-002", "bob");

        let filter = ItemFilter {
            assignee: Some("alice".to_string()),
            ..Default::default()
        };
        let items = list_items(&conn, &filter).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, "bn-001");
    }

    #[test]
    fn list_items_filter_by_parent() {
        let conn = test_db();
        insert_item(&conn, "bn-parent", "Parent", "open", "default");
        insert_item_full(
            &conn,
            "bn-child1",
            "Child 1",
            None,
            "task",
            "open",
            "default",
            Some("bn-parent"),
            "",
            100,
            200,
        );
        insert_item_full(
            &conn,
            "bn-child2",
            "Child 2",
            None,
            "task",
            "open",
            "default",
            Some("bn-parent"),
            "",
            101,
            201,
        );
        insert_item(&conn, "bn-other", "Other", "open", "default");

        let filter = ItemFilter {
            parent_id: Some("bn-parent".to_string()),
            ..Default::default()
        };
        let items = list_items(&conn, &filter).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn list_items_combined_filters() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Urgent open", "open", "urgent");
        insert_item(&conn, "bn-002", "Default open", "open", "default");
        insert_item(&conn, "bn-003", "Urgent doing", "doing", "urgent");

        let filter = ItemFilter {
            state: Some("open".to_string()),
            urgency: Some("urgent".to_string()),
            ..Default::default()
        };
        let items = list_items(&conn, &filter).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, "bn-001");
    }

    #[test]
    fn list_items_with_limit_and_offset() {
        let conn = test_db();
        for i in 0..10 {
            insert_item_full(
                &conn,
                &format!("bn-{i:03}"),
                &format!("Item {i}"),
                None,
                "task",
                "open",
                "default",
                None,
                "",
                i * 100,
                i * 100 + 50,
            );
        }

        let filter = ItemFilter {
            limit: Some(3),
            sort: SortOrder::CreatedAsc,
            ..Default::default()
        };
        let items = list_items(&conn, &filter).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].item_id, "bn-000");

        let filter2 = ItemFilter {
            limit: Some(3),
            offset: Some(3),
            sort: SortOrder::CreatedAsc,
            ..Default::default()
        };
        let items2 = list_items(&conn, &filter2).unwrap();
        assert_eq!(items2.len(), 3);
        assert_eq!(items2[0].item_id, "bn-003");
    }

    #[test]
    fn list_items_stable_tie_breaks_use_item_id() {
        let conn = test_db();

        // Same timestamps force ORDER BY tie-break behavior.
        insert_item_full(
            &conn, "bn-010", "Ten", None, "task", "open", "default", None, "", 100, 200,
        );
        insert_item_full(
            &conn, "bn-002", "Two", None, "task", "open", "default", None, "", 100, 200,
        );
        insert_item_full(
            &conn, "bn-001", "One", None, "task", "open", "default", None, "", 100, 200,
        );

        let asc = ItemFilter {
            sort: SortOrder::CreatedAsc,
            ..Default::default()
        };
        let asc_items = list_items(&conn, &asc).unwrap();
        assert_eq!(asc_items[0].item_id, "bn-001");
        assert_eq!(asc_items[1].item_id, "bn-002");
        assert_eq!(asc_items[2].item_id, "bn-010");

        let desc = ItemFilter {
            sort: SortOrder::UpdatedDesc,
            ..Default::default()
        };
        let desc_items = list_items(&conn, &desc).unwrap();
        assert_eq!(desc_items[0].item_id, "bn-001");
        assert_eq!(desc_items[1].item_id, "bn-002");
        assert_eq!(desc_items[2].item_id, "bn-010");
    }

    #[test]
    fn list_items_excludes_deleted() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Active", "open", "default");
        insert_item(&conn, "bn-002", "Deleted", "open", "default");
        conn.execute(
            "UPDATE items SET is_deleted = 1 WHERE item_id = 'bn-002'",
            [],
        )
        .unwrap();

        let items = list_items(&conn, &ItemFilter::default()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, "bn-001");

        let filter = ItemFilter {
            include_deleted: true,
            ..Default::default()
        };
        let items_with_deleted = list_items(&conn, &filter).unwrap();
        assert_eq!(items_with_deleted.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Sort order tests
    // -----------------------------------------------------------------------

    #[test]
    fn list_items_priority_sort() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Default", "open", "default");
        insert_item(&conn, "bn-002", "Urgent", "open", "urgent");
        insert_item(&conn, "bn-003", "Punt", "open", "punt");

        let filter = ItemFilter {
            sort: SortOrder::Priority,
            ..Default::default()
        };
        let items = list_items(&conn, &filter).unwrap();
        assert_eq!(items[0].urgency, "urgent");
        assert_eq!(items[1].urgency, "default");
        assert_eq!(items[2].urgency, "punt");
    }

    #[test]
    fn sort_order_parse_roundtrip() {
        for order in [
            SortOrder::CreatedDesc,
            SortOrder::CreatedAsc,
            SortOrder::UpdatedDesc,
            SortOrder::UpdatedAsc,
            SortOrder::Priority,
        ] {
            let s = order.to_string();
            let parsed: SortOrder = s.parse().unwrap();
            assert_eq!(order, parsed);
        }
    }

    #[test]
    fn sort_order_parse_aliases() {
        assert_eq!(
            "newest".parse::<SortOrder>().unwrap(),
            SortOrder::CreatedDesc
        );
        assert_eq!(
            "oldest".parse::<SortOrder>().unwrap(),
            SortOrder::CreatedAsc
        );
        assert_eq!(
            "recent".parse::<SortOrder>().unwrap(),
            SortOrder::UpdatedDesc
        );
        assert_eq!("stale".parse::<SortOrder>().unwrap(), SortOrder::UpdatedAsc);
        assert_eq!("triage".parse::<SortOrder>().unwrap(), SortOrder::Priority);
    }

    // -----------------------------------------------------------------------
    // Search tests
    // -----------------------------------------------------------------------

    #[test]
    fn search_fts5_finds_by_title() {
        let conn = test_db();
        insert_item_full(
            &conn,
            "bn-001",
            "Authentication timeout regression",
            Some("Retries fail after 30 seconds"),
            "task",
            "open",
            "urgent",
            None,
            "auth backend",
            100,
            200,
        );
        insert_item_full(
            &conn,
            "bn-002",
            "Update documentation",
            Some("Fix typos in README"),
            "task",
            "open",
            "default",
            None,
            "docs",
            101,
            201,
        );

        let hits = search(&conn, "authentication", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item_id, "bn-001");
    }

    #[test]
    fn search_fts5_stemming() {
        let conn = test_db();
        insert_item_full(
            &conn,
            "bn-001",
            "Running tests slowly",
            None,
            "task",
            "open",
            "default",
            None,
            "",
            100,
            200,
        );

        // Porter stemmer: "run" should match "running"
        let hits = search(&conn, "run", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item_id, "bn-001");
    }

    #[test]
    fn search_fts5_prefix() {
        let conn = test_db();
        insert_item_full(
            &conn,
            "bn-001",
            "Authentication service broken",
            None,
            "task",
            "open",
            "default",
            None,
            "",
            100,
            200,
        );

        // Prefix search: "auth*" matches "authentication"
        let hits = search(&conn, "auth*", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_fts5_excludes_deleted() {
        let conn = test_db();
        insert_item_full(
            &conn,
            "bn-001",
            "Important authentication bug",
            None,
            "task",
            "open",
            "default",
            None,
            "",
            100,
            200,
        );
        conn.execute(
            "UPDATE items SET is_deleted = 1 WHERE item_id = 'bn-001'",
            [],
        )
        .unwrap();

        let hits = search(&conn, "authentication", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_fts5_limit() {
        let conn = test_db();
        for i in 0..20 {
            insert_item_full(
                &conn,
                &format!("bn-{i:03}"),
                &format!("Authentication bug {i}"),
                None,
                "task",
                "open",
                "default",
                None,
                "",
                i * 100,
                i * 100 + 50,
            );
        }

        let hits = search(&conn, "authentication", 5).unwrap();
        assert_eq!(hits.len(), 5);
    }

    // -----------------------------------------------------------------------
    // Label / Assignee / Comment / Dependency tests
    // -----------------------------------------------------------------------

    #[test]
    fn get_labels_returns_sorted() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Item", "open", "default");
        insert_label(&conn, "bn-001", "zulu");
        insert_label(&conn, "bn-001", "alpha");
        insert_label(&conn, "bn-001", "mike");

        let labels = get_labels(&conn, "bn-001").unwrap();
        assert_eq!(labels.len(), 3);
        assert_eq!(labels[0].label, "alpha");
        assert_eq!(labels[1].label, "mike");
        assert_eq!(labels[2].label, "zulu");
    }

    #[test]
    fn list_labels_returns_counts() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Item 1", "open", "default");
        insert_item(&conn, "bn-002", "Item 2", "open", "default");
        insert_item(&conn, "bn-003", "Item 3", "open", "default");

        insert_label(&conn, "bn-001", "area:backend");
        insert_label(&conn, "bn-002", "area:backend");
        insert_label(&conn, "bn-003", "type:bug");

        let labels = list_labels(&conn, None, None).unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].name, "area:backend");
        assert_eq!(labels[0].count, 2);
        assert_eq!(labels[1].name, "type:bug");
        assert_eq!(labels[1].count, 1);
    }

    #[test]
    fn get_assignees_returns_sorted() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Item", "open", "default");
        insert_assignee(&conn, "bn-001", "charlie");
        insert_assignee(&conn, "bn-001", "alice");
        insert_assignee(&conn, "bn-001", "bob");

        let assignees = get_assignees(&conn, "bn-001").unwrap();
        assert_eq!(assignees.len(), 3);
        assert_eq!(assignees[0].agent, "alice");
        assert_eq!(assignees[1].agent, "bob");
        assert_eq!(assignees[2].agent, "charlie");
    }

    #[test]
    fn get_comments_newest_first() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Item", "open", "default");
        conn.execute(
            "INSERT INTO item_comments (item_id, event_hash, author, body, created_at_us) \
             VALUES ('bn-001', 'hash1', 'alice', 'First comment', 100)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO item_comments (item_id, event_hash, author, body, created_at_us) \
             VALUES ('bn-001', 'hash2', 'bob', 'Second comment', 200)",
            [],
        )
        .unwrap();

        let comments = get_comments(&conn, "bn-001", None, None).unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].body, "Second comment");
        assert_eq!(comments[1].body, "First comment");
    }

    #[test]
    fn get_dependencies_and_dependents() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Blocker", "open", "default");
        insert_item(&conn, "bn-002", "Blocked", "open", "default");
        insert_item(&conn, "bn-003", "Also blocked", "open", "default");
        insert_dependency(&conn, "bn-002", "bn-001");
        insert_dependency(&conn, "bn-003", "bn-001");

        // bn-002 depends on bn-001
        let deps = get_dependencies(&conn, "bn-002").unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].depends_on_item_id, "bn-001");

        // bn-001 has two dependents
        let dependents = get_dependents(&conn, "bn-001").unwrap();
        assert_eq!(dependents.len(), 2);
    }

    #[test]
    fn get_children_returns_ordered() {
        let conn = test_db();
        insert_item(&conn, "bn-parent", "Parent", "open", "default");
        insert_item_full(
            &conn,
            "bn-child2",
            "Second child",
            None,
            "task",
            "open",
            "default",
            Some("bn-parent"),
            "",
            200,
            200,
        );
        insert_item_full(
            &conn,
            "bn-child1",
            "First child",
            None,
            "task",
            "open",
            "default",
            Some("bn-parent"),
            "",
            100,
            100,
        );

        let children = get_children(&conn, "bn-parent").unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].item_id, "bn-child1");
        assert_eq!(children[1].item_id, "bn-child2");
    }

    // -----------------------------------------------------------------------
    // count_items / item_exists
    // -----------------------------------------------------------------------

    #[test]
    fn count_items_with_filter() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Open 1", "open", "default");
        insert_item(&conn, "bn-002", "Open 2", "open", "default");
        insert_item(&conn, "bn-003", "Doing 1", "doing", "default");

        let filter = ItemFilter {
            state: Some("open".to_string()),
            ..Default::default()
        };
        assert_eq!(count_items(&conn, &filter).unwrap(), 2);
        assert_eq!(count_items(&conn, &ItemFilter::default()).unwrap(), 3);
    }

    #[test]
    fn item_exists_works() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Exists", "open", "default");

        assert!(item_exists(&conn, "bn-001").unwrap());
        assert!(!item_exists(&conn, "bn-nope").unwrap());
    }

    // -----------------------------------------------------------------------
    // Projection cursor
    // -----------------------------------------------------------------------

    #[test]
    fn projection_cursor_roundtrip() {
        let conn = test_db();

        let (offset, hash) = get_projection_cursor(&conn).unwrap();
        assert_eq!(offset, 0);
        assert!(hash.is_none());

        update_projection_cursor(&conn, 42, Some("abc123")).unwrap();

        let (offset, hash) = get_projection_cursor(&conn).unwrap();
        assert_eq!(offset, 42);
        assert_eq!(hash.as_deref(), Some("abc123"));
    }

    // -----------------------------------------------------------------------
    // Graceful recovery
    // -----------------------------------------------------------------------

    #[test]
    fn try_open_projection_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nonexistent.db");
        let result = try_open_projection(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn try_open_projection_valid_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        // Create a valid DB
        let _conn = open_projection(&path).unwrap();
        drop(_conn);

        let conn = try_open_projection(&path).unwrap();
        assert!(conn.is_some());
    }

    #[test]
    fn try_open_projection_corrupt_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("corrupt.db");
        std::fs::write(&path, b"this is not a sqlite database").unwrap();

        let result = try_open_projection(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn item_counts_by_state_groups_non_deleted_rows_only() {
        let conn = test_db();
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-001', 'Open item', 'task', 'open', 'default', 0, '', 1000, 1000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-002', 'Deleted item', 'task', 'done', 'default', 1, '', 1000, 1000)",
            [],
        )
        .unwrap();

        let by_state = item_counts_by_state(&conn).unwrap();
        assert_eq!(by_state.get("open").copied().unwrap_or(0), 1);
        assert!(!by_state.contains_key("deleted"));
    }

    #[test]
    fn item_counts_by_kind_and_urgency_include_expected_groups() {
        let conn = test_db();
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-001', 'Bug item', 'bug', 'open', 'urgent', 0, '', 1000, 1000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-002', 'Task item', 'task', 'open', 'default', 0, '', 1000, 1000)",
            [],
        )
        .unwrap();

        let by_kind = item_counts_by_kind(&conn).unwrap();
        let by_urgency = item_counts_by_urgency(&conn).unwrap();
        assert_eq!(by_kind.get("bug").copied().unwrap_or(0), 1);
        assert_eq!(by_urgency.get("urgent").copied().unwrap_or(0), 1);
        assert_eq!(by_urgency.get("default").copied().unwrap_or(0), 1);
    }

    #[test]
    fn event_counts_from_projected_events_are_counted_by_type_and_agent() {
        let conn = test_db();
        ensure_tracking_table_for_query_tests(&conn);

        conn.execute(
            "INSERT INTO projected_events (event_hash, item_id, event_type, projected_at_us, agent) \
             VALUES ('blake3:a', 'bn-001', 'item.create', 1, 'alice')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO projected_events (event_hash, item_id, event_type, projected_at_us, agent) \
             VALUES ('blake3:b', 'bn-002', 'item.update', 2, 'bob')",
            [],
        )
        .unwrap();

        let by_type = event_counts_by_type(&conn).unwrap();
        let by_agent = event_counts_by_agent(&conn).unwrap();
        assert_eq!(by_type.get("item.create").copied().unwrap_or(0), 1);
        assert_eq!(by_type.get("item.update").copied().unwrap_or(0), 1);
        assert_eq!(by_agent.get("alice").copied().unwrap_or(0), 1);
        assert_eq!(by_agent.get("bob").copied().unwrap_or(0), 1);
    }

    #[test]
    fn get_comments_paginated() {
        let conn = test_db();
        insert_item(&conn, "bn-001", "Item", "open", "default");
        for i in 0..5 {
            conn.execute(
                "INSERT INTO item_comments (item_id, event_hash, author, body, created_at_us) \
                 VALUES (?1, ?2, 'alice', ?3, ?4)",
                params!["bn-001", format!("hash{i}"), format!("Comment {i}"), 100 + i as i64],
            )
            .unwrap();
        }

        // Newest first order: Comment 4 (104), Comment 3 (103), ...

        let page1 = get_comments(&conn, "bn-001", Some(2), None).unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0].body, "Comment 4");
        assert_eq!(page1[1].body, "Comment 3");

        let page2 = get_comments(&conn, "bn-001", Some(2), Some(2)).unwrap();
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[0].body, "Comment 2");
        assert_eq!(page2[1].body, "Comment 1");
    }

    fn ensure_tracking_table_for_query_tests(conn: &Connection) {
        let sql = "CREATE TABLE IF NOT EXISTS projected_events (
            event_hash TEXT PRIMARY KEY,
            item_id TEXT NOT NULL,
            event_type TEXT NOT NULL,
            projected_at_us INTEGER NOT NULL,
            agent TEXT NOT NULL DEFAULT ''
        );";
        conn.execute(sql, []).expect("create projected_events");
    }
}
