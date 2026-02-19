//! Canonical SQLite projection schema for bones.
//!
//! The schema is normalized for queryability and deterministic replay:
//! - `items` keeps the latest aggregate fields for each work item
//! - edge tables (`item_labels`, `item_assignees`, `item_dependencies`) model
//!   multi-valued relationships
//! - `item_comments` and `event_redactions` preserve event-driven side effects
//! - `projection_meta` tracks replay cursor metadata for incremental rebuilds

/// Migration v1: core normalized tables plus projection metadata.
pub const MIGRATION_V1_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS items (
    item_id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT,
    kind TEXT NOT NULL CHECK (kind IN ('task', 'goal', 'bug')),
    state TEXT NOT NULL CHECK (state IN ('open', 'doing', 'done', 'archived')),
    urgency TEXT NOT NULL DEFAULT 'default' CHECK (urgency IN ('urgent', 'default', 'punt')),
    size TEXT CHECK (size IS NULL OR size IN ('xxs', 'xs', 's', 'm', 'l', 'xl', 'xxl')),
    parent_id TEXT REFERENCES items(item_id) ON DELETE SET NULL,
    compact_summary TEXT,
    snapshot_json TEXT,
    is_deleted INTEGER NOT NULL DEFAULT 0 CHECK (is_deleted IN (0, 1)),
    deleted_at_us INTEGER,
    search_labels TEXT NOT NULL DEFAULT '',
    created_at_us INTEGER NOT NULL,
    updated_at_us INTEGER NOT NULL,
    CHECK (item_id LIKE 'bn-%')
);

CREATE TABLE IF NOT EXISTS item_labels (
    item_id TEXT NOT NULL REFERENCES items(item_id) ON DELETE CASCADE,
    label TEXT NOT NULL CHECK (length(trim(label)) > 0),
    created_at_us INTEGER NOT NULL,
    PRIMARY KEY (item_id, label)
);

CREATE TABLE IF NOT EXISTS item_assignees (
    item_id TEXT NOT NULL REFERENCES items(item_id) ON DELETE CASCADE,
    agent TEXT NOT NULL CHECK (length(trim(agent)) > 0),
    created_at_us INTEGER NOT NULL,
    PRIMARY KEY (item_id, agent)
);

CREATE TABLE IF NOT EXISTS item_dependencies (
    item_id TEXT NOT NULL REFERENCES items(item_id) ON DELETE CASCADE,
    depends_on_item_id TEXT NOT NULL REFERENCES items(item_id) ON DELETE CASCADE,
    link_type TEXT NOT NULL CHECK (length(trim(link_type)) > 0),
    created_at_us INTEGER NOT NULL,
    PRIMARY KEY (item_id, depends_on_item_id, link_type),
    CHECK (item_id <> depends_on_item_id)
);

CREATE TABLE IF NOT EXISTS item_comments (
    comment_id INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id TEXT NOT NULL REFERENCES items(item_id) ON DELETE CASCADE,
    event_hash TEXT NOT NULL UNIQUE,
    author TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at_us INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS event_redactions (
    target_event_hash TEXT PRIMARY KEY,
    item_id TEXT NOT NULL REFERENCES items(item_id) ON DELETE CASCADE,
    reason TEXT NOT NULL,
    redacted_by TEXT NOT NULL,
    redacted_at_us INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS projection_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    schema_version INTEGER NOT NULL,
    last_event_offset INTEGER NOT NULL DEFAULT 0,
    last_event_hash TEXT,
    last_rebuild_at_us INTEGER NOT NULL DEFAULT 0
);

INSERT OR IGNORE INTO projection_meta (
    id,
    schema_version,
    last_event_offset,
    last_event_hash,
    last_rebuild_at_us
) VALUES (1, 1, 0, NULL, 0);
"#;

/// Migration v2: read-path indexes and FTS5 table/triggers.
pub const MIGRATION_V2_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_items_state_urgency_updated
    ON items(state, urgency, updated_at_us DESC);

CREATE INDEX IF NOT EXISTS idx_items_kind_state
    ON items(kind, state);

CREATE INDEX IF NOT EXISTS idx_items_parent
    ON items(parent_id);

CREATE INDEX IF NOT EXISTS idx_items_deleted_updated
    ON items(is_deleted, updated_at_us DESC);

CREATE INDEX IF NOT EXISTS idx_item_labels_label
    ON item_labels(label, item_id);

CREATE INDEX IF NOT EXISTS idx_item_assignees_agent
    ON item_assignees(agent, item_id);

CREATE INDEX IF NOT EXISTS idx_item_dependencies_target_type
    ON item_dependencies(depends_on_item_id, link_type, item_id);

CREATE INDEX IF NOT EXISTS idx_item_comments_item_created
    ON item_comments(item_id, created_at_us DESC);

CREATE INDEX IF NOT EXISTS idx_event_redactions_item
    ON event_redactions(item_id);

CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(
    title,
    description,
    labels,
    item_id UNINDEXED,
    tokenize='porter unicode61',
    prefix='2 3'
);

CREATE TRIGGER IF NOT EXISTS items_ai
AFTER INSERT ON items
BEGIN
    INSERT INTO items_fts(rowid, title, description, labels, item_id)
    VALUES (
        new.rowid,
        new.title,
        COALESCE(new.description, ''),
        COALESCE(new.search_labels, ''),
        new.item_id
    );
END;

CREATE TRIGGER IF NOT EXISTS items_au
AFTER UPDATE ON items
BEGIN
    INSERT INTO items_fts(items_fts, rowid, title, description, labels, item_id)
    VALUES (
        'delete',
        old.rowid,
        old.title,
        COALESCE(old.description, ''),
        COALESCE(old.search_labels, ''),
        old.item_id
    );

    INSERT INTO items_fts(rowid, title, description, labels, item_id)
    VALUES (
        new.rowid,
        new.title,
        COALESCE(new.description, ''),
        COALESCE(new.search_labels, ''),
        new.item_id
    );
END;

CREATE TRIGGER IF NOT EXISTS items_ad
AFTER DELETE ON items
BEGIN
    INSERT INTO items_fts(items_fts, rowid, title, description, labels, item_id)
    VALUES (
        'delete',
        old.rowid,
        old.title,
        COALESCE(old.description, ''),
        COALESCE(old.search_labels, ''),
        old.item_id
    );
END;

DELETE FROM items_fts;
INSERT INTO items_fts(rowid, title, description, labels, item_id)
SELECT
    rowid,
    title,
    COALESCE(description, ''),
    COALESCE(search_labels, ''),
    item_id
FROM items;

UPDATE projection_meta
SET schema_version = 2
WHERE id = 1;
"#;

/// Indexes expected by list/filter/triage query paths.
pub const REQUIRED_INDEXES: &[&str] = &[
    "idx_items_state_urgency_updated",
    "idx_items_kind_state",
    "idx_items_parent",
    "idx_items_deleted_updated",
    "idx_item_labels_label",
    "idx_item_assignees_agent",
    "idx_item_dependencies_target_type",
    "idx_item_comments_item_created",
    "idx_event_redactions_item",
];

#[cfg(test)]
mod tests {
    use crate::db::migrations;
    use rusqlite::{Connection, params};

    fn seeded_conn() -> rusqlite::Result<Connection> {
        let mut conn = Connection::open_in_memory()?;
        migrations::migrate(&mut conn)?;

        for idx in 0..36_u32 {
            let item_id = format!("bn-{idx:03x}");
            let title = if idx % 4 == 0 {
                format!("Auth timeout regression {idx}")
            } else {
                format!("General maintenance {idx}")
            };
            let description = if idx % 4 == 0 {
                "Authentication retries fail after 30 seconds".to_string()
            } else {
                "Routine maintenance item".to_string()
            };
            let state = if idx % 2 == 0 { "open" } else { "doing" };
            let urgency = if idx % 3 == 0 { "urgent" } else { "default" };
            let labels = if idx % 4 == 0 { "auth backend" } else { "ops" };

            conn.execute(
                "INSERT INTO items (
                    item_id,
                    title,
                    description,
                    kind,
                    state,
                    urgency,
                    is_deleted,
                    search_labels,
                    created_at_us,
                    updated_at_us
                 ) VALUES (?1, ?2, ?3, 'task', ?4, ?5, 0, ?6, ?7, ?8)",
                params![
                    item_id,
                    title,
                    description,
                    state,
                    urgency,
                    labels,
                    i64::from(idx),
                    i64::from(idx) + 1_000
                ],
            )?;

            if idx % 4 == 0 {
                conn.execute(
                    "INSERT INTO item_labels (item_id, label, created_at_us)
                     VALUES (?1, 'backend', ?2)",
                    params![format!("bn-{idx:03x}"), i64::from(idx)],
                )?;
            }

            if idx % 5 == 0 {
                conn.execute(
                    "INSERT INTO item_assignees (item_id, agent, created_at_us)
                     VALUES (?1, 'bones-dev/0/keen-engine', ?2)",
                    params![format!("bn-{idx:03x}"), i64::from(idx)],
                )?;
            }
        }

        conn.execute(
            "INSERT INTO item_dependencies (item_id, depends_on_item_id, link_type, created_at_us)
             VALUES ('bn-006', 'bn-000', 'blocks', 10)",
            [],
        )?;
        conn.execute(
            "INSERT INTO item_dependencies (item_id, depends_on_item_id, link_type, created_at_us)
             VALUES ('bn-00a', 'bn-000', 'blocks', 11)",
            [],
        )?;

        Ok(conn)
    }

    fn query_plan_details(conn: &Connection, sql: &str) -> rusqlite::Result<Vec<String>> {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        stmt.query_map([], |row| row.get::<_, String>(3))?
            .collect::<Result<Vec<_>, _>>()
    }

    #[test]
    fn query_plan_uses_triage_index() -> rusqlite::Result<()> {
        let conn = seeded_conn()?;
        let details = query_plan_details(
            &conn,
            "SELECT item_id
             FROM items
             WHERE state = 'open' AND urgency = 'urgent'
             ORDER BY updated_at_us DESC
             LIMIT 20",
        )?;

        assert!(
            details
                .iter()
                .any(|detail| detail.contains("idx_items_state_urgency_updated")),
            "expected triage index in plan, got: {details:?}"
        );

        Ok(())
    }

    #[test]
    fn query_plan_uses_label_lookup_index() -> rusqlite::Result<()> {
        let conn = seeded_conn()?;
        let details = query_plan_details(
            &conn,
            "SELECT item_id
             FROM item_labels
             WHERE label = 'backend'
             ORDER BY item_id",
        )?;

        assert!(
            details
                .iter()
                .any(|detail| detail.contains("idx_item_labels_label")),
            "expected label index in plan, got: {details:?}"
        );

        Ok(())
    }

    #[test]
    fn query_plan_uses_reverse_dependency_index() -> rusqlite::Result<()> {
        let conn = seeded_conn()?;
        let details = query_plan_details(
            &conn,
            "SELECT item_id
             FROM item_dependencies
             WHERE depends_on_item_id = 'bn-000' AND link_type = 'blocks'",
        )?;

        assert!(
            details
                .iter()
                .any(|detail| detail.contains("idx_item_dependencies_target_type")),
            "expected dependency index in plan, got: {details:?}"
        );

        Ok(())
    }

    #[test]
    fn fts_supports_weighted_bm25_queries() -> rusqlite::Result<()> {
        let conn = seeded_conn()?;
        let mut stmt = conn.prepare(
            "SELECT item_id
             FROM items_fts
             WHERE items_fts MATCH 'auth'
             ORDER BY bm25(items_fts, 3.0, 2.0, 1.0)
             LIMIT 5",
        )?;

        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;

        assert!(
            !rows.is_empty(),
            "expected at least one lexical hit from items_fts"
        );

        Ok(())
    }
}
