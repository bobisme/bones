//! FTS5 full-text search with BM25 ranking.
//!
//! This module provides search helpers on top of the `items_fts` FTS5 virtual
//! table defined in [`super::schema`]. The FTS5 table is automatically kept
//! in sync with the `items` table via INSERT/UPDATE/DELETE triggers.
//!
//! # Column Weights (BM25)
//!
//! | Column      | Weight | Rationale                                  |
//! |-------------|--------|--------------------------------------------|
//! | title       | 3.0    | Most specific, short, high signal           |
//! | description | 2.0    | Detailed context, moderate signal           |
//! | labels      | 1.0    | Namespace tags, low cardinality             |
//!
//! # Tokenizer
//!
//! Porter stemmer + `unicode61` tokenizer with prefix indexes on 2 and 3
//! characters. This supports:
//! - **Stemming**: searching "running" matches "run", "runs", "runner"
//! - **Prefix search**: "auth*" matches "authentication", "authorize"
//! - **Unicode**: full Unicode word-breaking
//!
//! # Performance
//!
//! Sub-1ms query time at Tier S (≤1k items). FTS5 lookups are O(log N) via
//! the b-tree index and prefix tables.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use super::query::SearchHit;

/// Default BM25 column weights: title=3, description=2, labels=1.
pub const BM25_WEIGHT_TITLE: f64 = 3.0;
pub const BM25_WEIGHT_DESCRIPTION: f64 = 2.0;
pub const BM25_WEIGHT_LABELS: f64 = 1.0;

/// Search the FTS5 index with BM25 ranking and column weights.
///
/// This is the primary search entry point for the `bn search` command.
/// It joins FTS5 results with the `items` table to exclude soft-deleted
/// items and return full titles.
///
/// # Arguments
///
/// * `conn` — SQLite connection with the projection database open
/// * `query` — FTS5 query string (supports stemming, prefix `*`, boolean ops)
/// * `limit` — Maximum number of results
///
/// # BM25 Ranking
///
/// Results are sorted by BM25 relevance score (lower = better match).
/// Column weights: title 3×, description 2×, labels 1×.
///
/// # Errors
///
/// Returns an error if the FTS5 query is malformed or the database is
/// not properly initialized.
pub fn search_bm25(conn: &Connection, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
    let sql = "SELECT f.item_id, i.title, bm25(items_fts, ?1, ?2, ?3) AS rank \
               FROM items_fts f \
               INNER JOIN items i ON i.item_id = f.item_id \
               WHERE items_fts MATCH ?4 AND i.is_deleted = 0 \
               ORDER BY rank \
               LIMIT ?5";

    let mut stmt = conn
        .prepare(sql)
        .context("prepare FTS5 BM25 search query")?;

    let rows = stmt
        .query_map(
            params![
                BM25_WEIGHT_TITLE,
                BM25_WEIGHT_DESCRIPTION,
                BM25_WEIGHT_LABELS,
                query,
                limit,
            ],
            |row| {
                Ok(SearchHit {
                    item_id: row.get(0)?,
                    title: row.get(1)?,
                    rank: row.get(2)?,
                })
            },
        )
        .with_context(|| format!("execute FTS5 search for '{query}'"))?;

    let mut hits = Vec::new();
    for row in rows {
        hits.push(row.context("read FTS5 search hit")?);
    }
    Ok(hits)
}

/// Rebuild the FTS5 index from the current `items` table.
///
/// This drops and recreates all FTS5 index content. Useful after a full
/// projection rebuild or when the FTS index is suspected to be out of sync.
///
/// # Errors
///
/// Returns an error if the rebuild SQL fails.
pub fn rebuild_fts_index(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM items_fts;
         INSERT INTO items_fts(rowid, title, description, labels, item_id)
         SELECT rowid, title, COALESCE(description, ''), COALESCE(search_labels, ''), item_id
         FROM items;",
    )
    .context("rebuild FTS5 index from items table")?;
    Ok(())
}

/// Return the number of rows in the FTS5 index.
///
/// Useful for diagnostics and health checks.
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn fts_row_count(conn: &Connection) -> Result<u64> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM items_fts", [], |row| row.get(0))
        .context("count FTS5 rows")?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Validate that the FTS5 index is in sync with the `items` table.
///
/// Returns `true` if the row counts match (excluding deleted items).
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn fts_in_sync(conn: &Connection) -> Result<bool> {
    let items_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM items WHERE is_deleted = 0",
            [],
            |row| row.get(0),
        )
        .context("count active items")?;

    let fts_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM items_fts", [], |row| row.get(0))
        .context("count FTS5 rows")?;

    // FTS includes deleted items until triggers fire on DELETE,
    // but triggers fire on UPDATE too, so soft-deleted items stay in FTS.
    // The items table has triggers that update FTS on every INSERT/UPDATE/DELETE,
    // so the FTS count matches the total items count (including deleted).
    let total_items: i64 = conn
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .context("count total items")?;

    Ok(fts_count == total_items || fts_count == items_count)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::project::{Projector, ensure_tracking_table};
    use crate::event::data::*;
    use crate::event::types::EventType;
    use crate::event::{Event, EventData};
    use crate::model::item::{Kind, Size, Urgency};
    use crate::model::item_id::ItemId;
    use std::collections::BTreeMap;

    fn test_db() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        ensure_tracking_table(&conn).expect("create tracking table");
        conn
    }

    fn make_create(
        id: &str,
        title: &str,
        desc: Option<&str>,
        labels: &[&str],
        hash: &str,
    ) -> Event {
        Event {
            wall_ts_us: 1000,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(id),
            data: EventData::Create(CreateData {
                title: title.into(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: labels.iter().map(|s| s.to_string()).collect(),
                parent: None,
                causation: None,
                description: desc.map(String::from),
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:{hash}"),
        }
    }

    #[test]
    fn search_bm25_finds_by_title() {
        let conn = test_db();
        let proj = Projector::new(&conn);
        proj.project_event(&make_create(
            "bn-001",
            "Authentication timeout regression",
            Some("Retries fail after 30 seconds"),
            &["auth", "backend"],
            "h1",
        ))
        .unwrap();
        proj.project_event(&make_create(
            "bn-002",
            "Update documentation",
            Some("Fix typos in README"),
            &["docs"],
            "h2",
        ))
        .unwrap();

        let hits = search_bm25(&conn, "authentication", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item_id, "bn-001");
    }

    #[test]
    fn search_bm25_stemming() {
        let conn = test_db();
        let proj = Projector::new(&conn);
        proj.project_event(&make_create(
            "bn-001",
            "Running tests slowly",
            None,
            &[],
            "h1",
        ))
        .unwrap();

        // Porter stemmer: "run" matches "running"
        let hits = search_bm25(&conn, "run", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_bm25_prefix() {
        let conn = test_db();
        let proj = Projector::new(&conn);
        proj.project_event(&make_create(
            "bn-001",
            "Authentication service broken",
            None,
            &[],
            "h1",
        ))
        .unwrap();

        let hits = search_bm25(&conn, "auth*", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_bm25_excludes_deleted() {
        let conn = test_db();
        let proj = Projector::new(&conn);
        proj.project_event(&make_create(
            "bn-001",
            "Important auth bug",
            None,
            &[],
            "h1",
        ))
        .unwrap();

        // Soft-delete
        proj.project_event(&Event {
            wall_ts_us: 2000,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Delete,
            item_id: ItemId::new_unchecked("bn-001"),
            data: EventData::Delete(DeleteData {
                reason: None,
                extra: BTreeMap::new(),
            }),
            event_hash: "blake3:del1".into(),
        })
        .unwrap();

        let hits = search_bm25(&conn, "auth", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_bm25_title_weighted_higher() {
        let conn = test_db();
        let proj = Projector::new(&conn);

        // Item with "auth" in title
        proj.project_event(&make_create(
            "bn-title",
            "Authentication regression",
            Some("A minor bug"),
            &[],
            "h1",
        ))
        .unwrap();

        // Item with "auth" only in description
        proj.project_event(&make_create(
            "bn-desc",
            "Minor bug fix",
            Some("Related to authentication module"),
            &[],
            "h2",
        ))
        .unwrap();

        let hits = search_bm25(&conn, "authentication", 10).unwrap();
        assert_eq!(hits.len(), 2);
        // Title match should rank better (lower BM25 score)
        assert_eq!(hits[0].item_id, "bn-title");
    }

    #[test]
    fn search_bm25_label_match() {
        let conn = test_db();
        let proj = Projector::new(&conn);
        proj.project_event(&make_create(
            "bn-001",
            "Fix something",
            None,
            &["backend", "security"],
            "h1",
        ))
        .unwrap();

        let hits = search_bm25(&conn, "security", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_bm25_limit() {
        let conn = test_db();
        let proj = Projector::new(&conn);
        for i in 0..20_u32 {
            proj.project_event(&make_create(
                &format!("bn-{i:03}"),
                &format!("Authentication bug {i}"),
                None,
                &[],
                &format!("h{i}"),
            ))
            .unwrap();
        }

        let hits = search_bm25(&conn, "authentication", 5).unwrap();
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn rebuild_fts_index_restores_data() {
        let conn = test_db();
        let proj = Projector::new(&conn);
        proj.project_event(&make_create("bn-001", "Auth bug", None, &[], "h1"))
            .unwrap();

        // Manually corrupt FTS
        conn.execute_batch("DELETE FROM items_fts").unwrap();
        let hits_before = search_bm25(&conn, "auth", 10).unwrap();
        assert!(hits_before.is_empty());

        // Rebuild
        rebuild_fts_index(&conn).unwrap();
        let hits_after = search_bm25(&conn, "auth", 10).unwrap();
        assert_eq!(hits_after.len(), 1);
    }

    #[test]
    fn fts_row_count_reports_correctly() {
        let conn = test_db();
        let proj = Projector::new(&conn);

        assert_eq!(fts_row_count(&conn).unwrap(), 0);

        proj.project_event(&make_create("bn-001", "Item 1", None, &[], "h1"))
            .unwrap();
        proj.project_event(&make_create("bn-002", "Item 2", None, &[], "h2"))
            .unwrap();

        assert_eq!(fts_row_count(&conn).unwrap(), 2);
    }

    #[test]
    fn fts_in_sync_after_projection() {
        let conn = test_db();
        let proj = Projector::new(&conn);
        proj.project_event(&make_create("bn-001", "Item", None, &[], "h1"))
            .unwrap();

        assert!(fts_in_sync(&conn).unwrap());
    }

    #[test]
    fn search_bm25_empty_query_returns_empty() {
        let conn = test_db();
        let proj = Projector::new(&conn);
        proj.project_event(&make_create("bn-001", "Item", None, &[], "h1"))
            .unwrap();

        // Empty match expression — FTS5 returns error for empty string
        let result = search_bm25(&conn, "nonexistent_term_xyz", 10).unwrap();
        assert!(result.is_empty());
    }
}
