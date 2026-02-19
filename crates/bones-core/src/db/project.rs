//! Event replay → SQLite projection pipeline.
//!
//! The [`Projector`] replays events from the TSJSON event log and upserts
//! the resulting state into the SQLite projection database. It handles all
//! 11 event types and supports both incremental (single-event) and full
//! rebuild modes.
//!
//! # Deduplication
//!
//! Events are deduplicated by `event_hash`. When a duplicate hash is
//! encountered (e.g. from git merge duplicating lines in shard files),
//! the event is silently skipped.
//!
//! # Cursor
//!
//! After projecting a batch, the caller can persist the byte offset and
//! last event hash via [`super::query::update_projection_cursor`] for
//! incremental replay on next startup.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::event::Event;
use crate::event::data::{AssignAction, EventData};
use crate::event::types::EventType;

// ---------------------------------------------------------------------------
// ProjectionStats
// ---------------------------------------------------------------------------

/// Statistics returned after a projection run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionStats {
    /// Number of events successfully projected.
    pub projected: usize,
    /// Number of duplicate events skipped.
    pub duplicates: usize,
    /// Number of events that caused errors (logged and skipped).
    pub errors: usize,
}

// ---------------------------------------------------------------------------
// Projector
// ---------------------------------------------------------------------------

/// Replays events into the SQLite projection.
///
/// Create a `Projector` with a connection, then call [`project_event`] for
/// each event or [`project_batch`] for a slice.
pub struct Projector<'conn> {
    conn: &'conn Connection,
}

impl<'conn> Projector<'conn> {
    /// Create a new projector backed by the given connection.
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(conn: &'conn Connection) -> Self {
        Self { conn }
    }

    /// Project a batch of events, returning aggregate statistics.
    ///
    /// Events are applied inside a single transaction for performance.
    /// Duplicate events (same `event_hash`) are silently skipped.
    ///
    /// # Errors
    ///
    /// Returns an error if the transaction fails to commit. Individual event
    /// projection errors are counted in `stats.errors` but do not abort the
    /// batch.
    pub fn project_batch(&self, events: &[Event]) -> Result<ProjectionStats> {
        let mut stats = ProjectionStats::default();

        // Use a savepoint so we can commit all-or-nothing
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .context("begin projection transaction")?;

        for event in events {
            match self.project_event_inner(event) {
                Ok(ProjectResult::Projected) => stats.projected += 1,
                Ok(ProjectResult::Duplicate) => stats.duplicates += 1,
                Err(e) => {
                    tracing::warn!(
                        event_hash = %event.event_hash,
                        event_type = %event.event_type,
                        item_id = %event.item_id,
                        error = %e,
                        "skipping event due to projection error"
                    );
                    stats.errors += 1;
                }
            }
        }

        self.conn
            .execute_batch("COMMIT")
            .context("commit projection transaction")?;

        Ok(stats)
    }

    /// Project a single event (outside of any managed transaction).
    ///
    /// Returns `true` if the event was projected, `false` if it was a
    /// duplicate.
    ///
    /// # Errors
    ///
    /// Returns an error if the projection fails.
    pub fn project_event(&self, event: &Event) -> Result<bool> {
        match self.project_event_inner(event)? {
            ProjectResult::Projected => Ok(true),
            ProjectResult::Duplicate => Ok(false),
        }
    }

    // -----------------------------------------------------------------------
    // Internal dispatch
    // -----------------------------------------------------------------------

    fn project_event_inner(&self, event: &Event) -> Result<ProjectResult> {
        // Dedup check: skip if event_hash already projected
        if self.is_event_projected(&event.event_hash)? {
            return Ok(ProjectResult::Duplicate);
        }

        match event.event_type {
            EventType::Create => self.project_create(event)?,
            EventType::Update => self.project_update(event)?,
            EventType::Move => self.project_move(event)?,
            EventType::Assign => self.project_assign(event)?,
            EventType::Comment => self.project_comment(event)?,
            EventType::Link => self.project_link(event)?,
            EventType::Unlink => self.project_unlink(event)?,
            EventType::Delete => self.project_delete(event)?,
            EventType::Compact => self.project_compact(event)?,
            EventType::Snapshot => self.project_snapshot(event)?,
            EventType::Redact => self.project_redact(event)?,
        }

        // Record that this event hash has been projected
        self.record_projected_hash(&event.event_hash, event)?;

        Ok(ProjectResult::Projected)
    }

    // -----------------------------------------------------------------------
    // Dedup tracking
    // -----------------------------------------------------------------------

    fn is_event_projected(&self, event_hash: &str) -> Result<bool> {
        // Check item_comments table for comment events (unique on event_hash)
        // and the event_redactions table for redact events.
        // For a general dedup check, we use projection_meta's tracking.
        // Simple approach: check if item_comments has this hash OR if
        // event_redactions has it as target. For general dedup, we use
        // a lightweight check in the projected_events tracking.
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM projected_events WHERE event_hash = ?1)",
                params![event_hash],
                |row| row.get(0),
            )
            .unwrap_or(false);
        Ok(exists)
    }

    fn record_projected_hash(&self, event_hash: &str, event: &Event) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO projected_events (event_hash, item_id, event_type, projected_at_us) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    event_hash,
                    event.item_id.as_str(),
                    event.event_type.as_str(),
                    event.wall_ts_us,
                ],
            )
            .context("record projected event hash")?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Event type handlers
    // -----------------------------------------------------------------------

    fn project_create(&self, event: &Event) -> Result<()> {
        let EventData::Create(ref data) = event.data else {
            anyhow::bail!("expected Create data for item.create event");
        };

        let labels_str = data.labels.join(" ");

        self.conn
            .execute(
                "INSERT OR IGNORE INTO items (
                    item_id, title, description, kind, state, urgency,
                    size, parent_id, is_deleted, search_labels,
                    created_at_us, updated_at_us
                ) VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?6, ?7, 0, ?8, ?9, ?10)",
                params![
                    event.item_id.as_str(),
                    data.title,
                    data.description,
                    data.kind.to_string(),
                    data.urgency.to_string(),
                    data.size.map(|s| s.to_string()),
                    data.parent.as_deref(),
                    labels_str,
                    event.wall_ts_us,
                    event.wall_ts_us,
                ],
            )
            .with_context(|| format!("project create for {}", event.item_id))?;

        // Insert initial labels
        for label in &data.labels {
            self.conn
                .execute(
                    "INSERT OR IGNORE INTO item_labels (item_id, label, created_at_us)
                     VALUES (?1, ?2, ?3)",
                    params![event.item_id.as_str(), label, event.wall_ts_us],
                )
                .with_context(|| format!("insert label '{label}' for {}", event.item_id))?;
        }

        Ok(())
    }

    fn project_update(&self, event: &Event) -> Result<()> {
        let EventData::Update(ref data) = event.data else {
            anyhow::bail!("expected Update data for item.update event");
        };

        self.ensure_item_exists(event)?;

        match data.field.as_str() {
            "title" => {
                let value = data.value.as_str().unwrap_or_default();
                self.conn.execute(
                    "UPDATE items SET title = ?1, updated_at_us = ?2 WHERE item_id = ?3",
                    params![value, event.wall_ts_us, event.item_id.as_str()],
                )?;
            }
            "description" => {
                let value = data.value.as_str().map(String::from);
                self.conn.execute(
                    "UPDATE items SET description = ?1, updated_at_us = ?2 WHERE item_id = ?3",
                    params![value, event.wall_ts_us, event.item_id.as_str()],
                )?;
            }
            "kind" => {
                let value = data.value.as_str().unwrap_or("task");
                self.conn.execute(
                    "UPDATE items SET kind = ?1, updated_at_us = ?2 WHERE item_id = ?3",
                    params![value, event.wall_ts_us, event.item_id.as_str()],
                )?;
            }
            "size" => {
                let value = data.value.as_str().map(String::from);
                self.conn.execute(
                    "UPDATE items SET size = ?1, updated_at_us = ?2 WHERE item_id = ?3",
                    params![value, event.wall_ts_us, event.item_id.as_str()],
                )?;
            }
            "urgency" => {
                let value = data.value.as_str().unwrap_or("default");
                self.conn.execute(
                    "UPDATE items SET urgency = ?1, updated_at_us = ?2 WHERE item_id = ?3",
                    params![value, event.wall_ts_us, event.item_id.as_str()],
                )?;
            }
            "parent" => {
                let value = data.value.as_str().map(String::from);
                self.conn.execute(
                    "UPDATE items SET parent_id = ?1, updated_at_us = ?2 WHERE item_id = ?3",
                    params![value, event.wall_ts_us, event.item_id.as_str()],
                )?;
            }
            "labels" => {
                // Labels update: replace entire label set
                if let Some(labels) = data.value.as_array() {
                    // Delete existing labels
                    self.conn.execute(
                        "DELETE FROM item_labels WHERE item_id = ?1",
                        params![event.item_id.as_str()],
                    )?;

                    let mut label_strings = Vec::new();
                    for label_val in labels {
                        if let Some(label) = label_val.as_str() {
                            self.conn.execute(
                                "INSERT OR IGNORE INTO item_labels (item_id, label, created_at_us)
                                 VALUES (?1, ?2, ?3)",
                                params![event.item_id.as_str(), label, event.wall_ts_us],
                            )?;
                            label_strings.push(label.to_string());
                        }
                    }

                    // Update search_labels
                    let search_labels = label_strings.join(" ");
                    self.conn.execute(
                        "UPDATE items SET search_labels = ?1, updated_at_us = ?2 WHERE item_id = ?3",
                        params![search_labels, event.wall_ts_us, event.item_id.as_str()],
                    )?;
                }
            }
            _ => {
                // Unknown field — just bump updated_at
                tracing::debug!(
                    field = %data.field,
                    item_id = %event.item_id,
                    "ignoring update for unknown field"
                );
                self.conn.execute(
                    "UPDATE items SET updated_at_us = ?1 WHERE item_id = ?2",
                    params![event.wall_ts_us, event.item_id.as_str()],
                )?;
            }
        }

        Ok(())
    }

    fn project_move(&self, event: &Event) -> Result<()> {
        let EventData::Move(ref data) = event.data else {
            anyhow::bail!("expected Move data for item.move event");
        };

        self.ensure_item_exists(event)?;

        self.conn
            .execute(
                "UPDATE items SET state = ?1, updated_at_us = ?2 WHERE item_id = ?3",
                params![
                    data.state.to_string(),
                    event.wall_ts_us,
                    event.item_id.as_str(),
                ],
            )
            .with_context(|| format!("project move for {}", event.item_id))?;

        Ok(())
    }

    fn project_assign(&self, event: &Event) -> Result<()> {
        let EventData::Assign(ref data) = event.data else {
            anyhow::bail!("expected Assign data for item.assign event");
        };

        self.ensure_item_exists(event)?;

        match data.action {
            AssignAction::Assign => {
                self.conn
                    .execute(
                        "INSERT OR IGNORE INTO item_assignees (item_id, agent, created_at_us)
                         VALUES (?1, ?2, ?3)",
                        params![event.item_id.as_str(), data.agent, event.wall_ts_us],
                    )
                    .with_context(|| format!("assign {} to {}", data.agent, event.item_id))?;
            }
            AssignAction::Unassign => {
                self.conn
                    .execute(
                        "DELETE FROM item_assignees WHERE item_id = ?1 AND agent = ?2",
                        params![event.item_id.as_str(), data.agent],
                    )
                    .with_context(|| format!("unassign {} from {}", data.agent, event.item_id))?;
            }
        }

        // Bump updated_at
        self.conn.execute(
            "UPDATE items SET updated_at_us = ?1 WHERE item_id = ?2",
            params![event.wall_ts_us, event.item_id.as_str()],
        )?;

        Ok(())
    }

    fn project_comment(&self, event: &Event) -> Result<()> {
        let EventData::Comment(ref data) = event.data else {
            anyhow::bail!("expected Comment data for item.comment event");
        };

        self.ensure_item_exists(event)?;

        self.conn
            .execute(
                "INSERT OR IGNORE INTO item_comments (item_id, event_hash, author, body, created_at_us)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    event.item_id.as_str(),
                    event.event_hash,
                    event.agent,
                    data.body,
                    event.wall_ts_us,
                ],
            )
            .with_context(|| format!("project comment for {}", event.item_id))?;

        // Bump updated_at
        self.conn.execute(
            "UPDATE items SET updated_at_us = ?1 WHERE item_id = ?2",
            params![event.wall_ts_us, event.item_id.as_str()],
        )?;

        Ok(())
    }

    fn project_link(&self, event: &Event) -> Result<()> {
        let EventData::Link(ref data) = event.data else {
            anyhow::bail!("expected Link data for item.link event");
        };

        self.ensure_item_exists(event)?;

        self.conn
            .execute(
                "INSERT OR IGNORE INTO item_dependencies (item_id, depends_on_item_id, link_type, created_at_us)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    event.item_id.as_str(),
                    data.target,
                    data.link_type,
                    event.wall_ts_us,
                ],
            )
            .with_context(|| {
                format!("project link {} -> {}", event.item_id, data.target)
            })?;

        // Bump updated_at
        self.conn.execute(
            "UPDATE items SET updated_at_us = ?1 WHERE item_id = ?2",
            params![event.wall_ts_us, event.item_id.as_str()],
        )?;

        Ok(())
    }

    fn project_unlink(&self, event: &Event) -> Result<()> {
        let EventData::Unlink(ref data) = event.data else {
            anyhow::bail!("expected Unlink data for item.unlink event");
        };

        self.ensure_item_exists(event)?;

        if let Some(ref link_type) = data.link_type {
            self.conn
                .execute(
                    "DELETE FROM item_dependencies \
                     WHERE item_id = ?1 AND depends_on_item_id = ?2 AND link_type = ?3",
                    params![event.item_id.as_str(), data.target, link_type],
                )
                .with_context(|| format!("unlink {} -/-> {}", event.item_id, data.target))?;
        } else {
            // No link_type: remove all links to target
            self.conn
                .execute(
                    "DELETE FROM item_dependencies \
                     WHERE item_id = ?1 AND depends_on_item_id = ?2",
                    params![event.item_id.as_str(), data.target],
                )
                .with_context(|| format!("unlink all {} -/-> {}", event.item_id, data.target))?;
        }

        // Bump updated_at
        self.conn.execute(
            "UPDATE items SET updated_at_us = ?1 WHERE item_id = ?2",
            params![event.wall_ts_us, event.item_id.as_str()],
        )?;

        Ok(())
    }

    fn project_delete(&self, event: &Event) -> Result<()> {
        self.ensure_item_exists(event)?;

        self.conn
            .execute(
                "UPDATE items SET is_deleted = 1, deleted_at_us = ?1, updated_at_us = ?1 \
                 WHERE item_id = ?2",
                params![event.wall_ts_us, event.item_id.as_str()],
            )
            .with_context(|| format!("project delete for {}", event.item_id))?;

        Ok(())
    }

    fn project_compact(&self, event: &Event) -> Result<()> {
        let EventData::Compact(ref data) = event.data else {
            anyhow::bail!("expected Compact data for item.compact event");
        };

        self.ensure_item_exists(event)?;

        self.conn
            .execute(
                "UPDATE items SET compact_summary = ?1, updated_at_us = ?2 WHERE item_id = ?3",
                params![data.summary, event.wall_ts_us, event.item_id.as_str()],
            )
            .with_context(|| format!("project compact for {}", event.item_id))?;

        Ok(())
    }

    fn project_snapshot(&self, event: &Event) -> Result<()> {
        let EventData::Snapshot(ref data) = event.data else {
            anyhow::bail!("expected Snapshot data for item.snapshot event");
        };

        self.ensure_item_exists(event)?;

        let json_str = serde_json::to_string(&data.state).context("serialize snapshot state")?;

        self.conn
            .execute(
                "UPDATE items SET snapshot_json = ?1, updated_at_us = ?2 WHERE item_id = ?3",
                params![json_str, event.wall_ts_us, event.item_id.as_str()],
            )
            .with_context(|| format!("project snapshot for {}", event.item_id))?;

        Ok(())
    }

    fn project_redact(&self, event: &Event) -> Result<()> {
        let EventData::Redact(ref data) = event.data else {
            anyhow::bail!("expected Redact data for item.redact event");
        };

        // Insert redaction record
        self.conn
            .execute(
                "INSERT OR IGNORE INTO event_redactions \
                 (target_event_hash, item_id, reason, redacted_by, redacted_at_us) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    data.target_hash,
                    event.item_id.as_str(),
                    data.reason,
                    event.agent,
                    event.wall_ts_us,
                ],
            )
            .with_context(|| {
                format!(
                    "project redact for {} targeting {}",
                    event.item_id, data.target_hash
                )
            })?;

        // Redact the comment body if the target hash is a comment event
        self.conn
            .execute(
                "UPDATE item_comments SET body = '[redacted]' WHERE event_hash = ?1",
                params![data.target_hash],
            )
            .context("redact comment body")?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Ensure the item exists in the projection. If not, create a placeholder
    /// row so that subsequent operations (UPDATE, foreign keys) succeed.
    ///
    /// This handles out-of-order event replay and missing create events.
    fn ensure_item_exists(&self, event: &Event) -> Result<()> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM items WHERE item_id = ?1)",
                params![event.item_id.as_str()],
                |row| row.get(0),
            )
            .context("check item exists")?;

        if !exists {
            self.conn
                .execute(
                    "INSERT INTO items (
                        item_id, title, kind, state, urgency,
                        is_deleted, search_labels, created_at_us, updated_at_us
                    ) VALUES (?1, '', 'task', 'open', 'default', 0, '', ?2, ?2)",
                    params![event.item_id.as_str(), event.wall_ts_us],
                )
                .with_context(|| format!("create placeholder item for {}", event.item_id))?;
        }

        Ok(())
    }
}

enum ProjectResult {
    Projected,
    Duplicate,
}

// ---------------------------------------------------------------------------
// Schema addition: projected_events tracking table
// ---------------------------------------------------------------------------

/// SQL to create the `projected_events` tracking table.
///
/// This is applied as part of the projection setup, not as a schema migration,
/// because it is projection-internal bookkeeping.
pub const PROJECTED_EVENTS_DDL: &str = "\
CREATE TABLE IF NOT EXISTS projected_events (
    event_hash TEXT PRIMARY KEY,
    item_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    projected_at_us INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_projected_events_item
    ON projected_events(item_id);
";

/// Ensure the `projected_events` tracking table exists.
///
/// Call this once after opening the projection database and before
/// projecting events.
///
/// # Errors
///
/// Returns an error if executing the DDL fails.
pub fn ensure_tracking_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(PROJECTED_EVENTS_DDL)
        .context("create projected_events tracking table")?;
    Ok(())
}

/// Drop all projection data for a full rebuild.
///
/// Clears all items, edge tables, comments, redactions, FTS index, and the
/// projected events tracking table. Schema structure is preserved.
///
/// # Errors
///
/// Returns an error if the truncation fails.
pub fn clear_projection(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM event_redactions;
         DELETE FROM item_comments;
         DELETE FROM item_dependencies;
         DELETE FROM item_assignees;
         DELETE FROM item_labels;
         DELETE FROM items;
         DELETE FROM projected_events;
         UPDATE projection_meta SET last_event_offset = 0, last_event_hash = NULL WHERE id = 1;",
    )
    .context("clear projection tables")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{migrations, query};
    use crate::event::data::*;
    use crate::event::types::EventType;
    use crate::model::item::{Kind, Size, State, Urgency};
    use crate::model::item_id::ItemId;
    use rusqlite::Connection;
    use std::collections::BTreeMap;

    fn test_db() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        ensure_tracking_table(&conn).expect("create tracking table");
        conn
    }

    fn make_event(
        event_type: EventType,
        item_id: &str,
        data: EventData,
        hash: &str,
        ts: i64,
    ) -> Event {
        Event {
            wall_ts_us: ts,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type,
            item_id: ItemId::new_unchecked(item_id),
            data,
            event_hash: format!("blake3:{hash}"),
        }
    }

    fn make_create(id: &str, title: &str, hash: &str, ts: i64) -> Event {
        make_event(
            EventType::Create,
            id,
            EventData::Create(CreateData {
                title: title.into(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: vec!["backend".into(), "auth".into()],
                parent: None,
                causation: None,
                description: Some("A detailed description".into()),
                extra: BTreeMap::new(),
            }),
            hash,
            ts,
        )
    }

    // -----------------------------------------------------------------------
    // Create
    // -----------------------------------------------------------------------

    #[test]
    fn project_create_inserts_item() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        let event = make_create("bn-001", "Fix auth timeout", "aaa", 1000);

        let result = projector.project_event(&event).unwrap();
        assert!(result, "should return true for new projection");

        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.title, "Fix auth timeout");
        assert_eq!(item.kind, "task");
        assert_eq!(item.state, "open");
        assert_eq!(item.urgency, "default");
        assert_eq!(item.size.as_deref(), Some("m"));
        assert_eq!(item.description.as_deref(), Some("A detailed description"));
        assert_eq!(item.created_at_us, 1000);
        assert_eq!(item.updated_at_us, 1000);
    }

    #[test]
    fn project_create_inserts_labels() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        let event = make_create("bn-001", "Fix auth", "aaa", 1000);
        projector.project_event(&event).unwrap();

        let labels = query::get_labels(&conn, "bn-001").unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].label, "auth");
        assert_eq!(labels[1].label, "backend");
    }

    // -----------------------------------------------------------------------
    // Update
    // -----------------------------------------------------------------------

    #[test]
    fn project_update_title() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Old title", "aaa", 1000))
            .unwrap();

        let update = make_event(
            EventType::Update,
            "bn-001",
            EventData::Update(UpdateData {
                field: "title".into(),
                value: serde_json::json!("New title"),
                extra: BTreeMap::new(),
            }),
            "bbb",
            2000,
        );
        projector.project_event(&update).unwrap();

        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.title, "New title");
        assert_eq!(item.updated_at_us, 2000);
    }

    #[test]
    fn project_update_description() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Item", "aaa", 1000))
            .unwrap();

        let update = make_event(
            EventType::Update,
            "bn-001",
            EventData::Update(UpdateData {
                field: "description".into(),
                value: serde_json::json!("Updated description"),
                extra: BTreeMap::new(),
            }),
            "bbb",
            2000,
        );
        projector.project_event(&update).unwrap();

        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.description.as_deref(), Some("Updated description"));
    }

    #[test]
    fn project_update_labels() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Item", "aaa", 1000))
            .unwrap();

        let update = make_event(
            EventType::Update,
            "bn-001",
            EventData::Update(UpdateData {
                field: "labels".into(),
                value: serde_json::json!(["frontend", "urgent"]),
                extra: BTreeMap::new(),
            }),
            "bbb",
            2000,
        );
        projector.project_event(&update).unwrap();

        let labels = query::get_labels(&conn, "bn-001").unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].label, "frontend");
        assert_eq!(labels[1].label, "urgent");
    }

    #[test]
    fn project_update_unknown_field_bumps_updated() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Item", "aaa", 1000))
            .unwrap();

        let update = make_event(
            EventType::Update,
            "bn-001",
            EventData::Update(UpdateData {
                field: "future_field".into(),
                value: serde_json::json!("whatever"),
                extra: BTreeMap::new(),
            }),
            "bbb",
            2000,
        );
        projector.project_event(&update).unwrap();

        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.updated_at_us, 2000);
    }

    // -----------------------------------------------------------------------
    // Move
    // -----------------------------------------------------------------------

    #[test]
    fn project_move_updates_state() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Item", "aaa", 1000))
            .unwrap();

        let mv = make_event(
            EventType::Move,
            "bn-001",
            EventData::Move(MoveData {
                state: State::Doing,
                reason: Some("Starting work".into()),
                extra: BTreeMap::new(),
            }),
            "bbb",
            2000,
        );
        projector.project_event(&mv).unwrap();

        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.state, "doing");
    }

    // -----------------------------------------------------------------------
    // Assign / Unassign
    // -----------------------------------------------------------------------

    #[test]
    fn project_assign_and_unassign() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Item", "aaa", 1000))
            .unwrap();

        // Assign alice
        let assign = make_event(
            EventType::Assign,
            "bn-001",
            EventData::Assign(AssignData {
                agent: "alice".into(),
                action: AssignAction::Assign,
                extra: BTreeMap::new(),
            }),
            "bbb",
            2000,
        );
        projector.project_event(&assign).unwrap();

        let assignees = query::get_assignees(&conn, "bn-001").unwrap();
        assert_eq!(assignees.len(), 1);
        assert_eq!(assignees[0].agent, "alice");

        // Unassign alice
        let unassign = make_event(
            EventType::Assign,
            "bn-001",
            EventData::Assign(AssignData {
                agent: "alice".into(),
                action: AssignAction::Unassign,
                extra: BTreeMap::new(),
            }),
            "ccc",
            3000,
        );
        projector.project_event(&unassign).unwrap();

        let assignees = query::get_assignees(&conn, "bn-001").unwrap();
        assert!(assignees.is_empty());
    }

    // -----------------------------------------------------------------------
    // Comment
    // -----------------------------------------------------------------------

    #[test]
    fn project_comment_inserts_row() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Item", "aaa", 1000))
            .unwrap();

        let comment = make_event(
            EventType::Comment,
            "bn-001",
            EventData::Comment(CommentData {
                body: "This is a comment".into(),
                extra: BTreeMap::new(),
            }),
            "bbb",
            2000,
        );
        projector.project_event(&comment).unwrap();

        let comments = query::get_comments(&conn, "bn-001").unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "This is a comment");
        assert_eq!(comments[0].author, "test-agent");
        assert_eq!(comments[0].event_hash, "blake3:bbb");
    }

    // -----------------------------------------------------------------------
    // Link / Unlink
    // -----------------------------------------------------------------------

    #[test]
    fn project_link_and_unlink() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Blocker", "aaa", 1000))
            .unwrap();
        projector
            .project_event(&make_create("bn-002", "Blocked", "bbb", 1001))
            .unwrap();

        // Link
        let link = make_event(
            EventType::Link,
            "bn-002",
            EventData::Link(LinkData {
                target: "bn-001".into(),
                link_type: "blocks".into(),
                extra: BTreeMap::new(),
            }),
            "ccc",
            2000,
        );
        projector.project_event(&link).unwrap();

        let deps = query::get_dependencies(&conn, "bn-002").unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].depends_on_item_id, "bn-001");

        // Unlink
        let unlink = make_event(
            EventType::Unlink,
            "bn-002",
            EventData::Unlink(UnlinkData {
                target: "bn-001".into(),
                link_type: Some("blocks".into()),
                extra: BTreeMap::new(),
            }),
            "ddd",
            3000,
        );
        projector.project_event(&unlink).unwrap();

        let deps = query::get_dependencies(&conn, "bn-002").unwrap();
        assert!(deps.is_empty());
    }

    // -----------------------------------------------------------------------
    // Delete
    // -----------------------------------------------------------------------

    #[test]
    fn project_delete_soft_deletes() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Item", "aaa", 1000))
            .unwrap();

        let delete = make_event(
            EventType::Delete,
            "bn-001",
            EventData::Delete(DeleteData {
                reason: Some("Duplicate".into()),
                extra: BTreeMap::new(),
            }),
            "bbb",
            2000,
        );
        projector.project_event(&delete).unwrap();

        // Not visible without include_deleted
        assert!(query::get_item(&conn, "bn-001", false).unwrap().is_none());
        // Visible with include_deleted
        let item = query::get_item(&conn, "bn-001", true).unwrap().unwrap();
        assert!(item.is_deleted);
        assert_eq!(item.deleted_at_us, Some(2000));
    }

    // -----------------------------------------------------------------------
    // Compact
    // -----------------------------------------------------------------------

    #[test]
    fn project_compact_sets_summary() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Item", "aaa", 1000))
            .unwrap();

        let compact = make_event(
            EventType::Compact,
            "bn-001",
            EventData::Compact(CompactData {
                summary: "TL;DR: auth fix".into(),
                extra: BTreeMap::new(),
            }),
            "bbb",
            2000,
        );
        projector.project_event(&compact).unwrap();

        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.compact_summary.as_deref(), Some("TL;DR: auth fix"));
    }

    // -----------------------------------------------------------------------
    // Snapshot
    // -----------------------------------------------------------------------

    #[test]
    fn project_snapshot_stores_json() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Item", "aaa", 1000))
            .unwrap();

        let snapshot = make_event(
            EventType::Snapshot,
            "bn-001",
            EventData::Snapshot(SnapshotData {
                state: serde_json::json!({"id": "bn-001", "title": "Snapshotted"}),
                extra: BTreeMap::new(),
            }),
            "bbb",
            2000,
        );
        projector.project_event(&snapshot).unwrap();

        let row: String = conn
            .query_row(
                "SELECT snapshot_json FROM items WHERE item_id = 'bn-001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&row).unwrap();
        assert_eq!(parsed["title"], "Snapshotted");
    }

    // -----------------------------------------------------------------------
    // Redact
    // -----------------------------------------------------------------------

    #[test]
    fn project_redact_records_and_blanks_comment() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Item", "aaa", 1000))
            .unwrap();

        // First add a comment
        let comment = make_event(
            EventType::Comment,
            "bn-001",
            EventData::Comment(CommentData {
                body: "Secret password: hunter2".into(),
                extra: BTreeMap::new(),
            }),
            "comment_hash",
            2000,
        );
        projector.project_event(&comment).unwrap();

        // Redact it
        let redact = make_event(
            EventType::Redact,
            "bn-001",
            EventData::Redact(RedactData {
                target_hash: "blake3:comment_hash".into(),
                reason: "Accidental secret".into(),
                extra: BTreeMap::new(),
            }),
            "redact_hash",
            3000,
        );
        projector.project_event(&redact).unwrap();

        // Check redaction record
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM event_redactions WHERE target_event_hash = 'blake3:comment_hash'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Check comment body is redacted
        let comments = query::get_comments(&conn, "bn-001").unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "[redacted]");
    }

    // -----------------------------------------------------------------------
    // Dedup
    // -----------------------------------------------------------------------

    #[test]
    fn duplicate_events_are_skipped() {
        let conn = test_db();
        let projector = Projector::new(&conn);

        let event = make_create("bn-001", "Item", "aaa", 1000);
        assert!(projector.project_event(&event).unwrap()); // first time
        assert!(!projector.project_event(&event).unwrap()); // duplicate

        // Only one item created
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn batch_dedup_counts() {
        let conn = test_db();
        let projector = Projector::new(&conn);

        let event1 = make_create("bn-001", "Item 1", "aaa", 1000);
        let event2 = make_create("bn-002", "Item 2", "bbb", 1001);

        // Project first batch
        let stats1 = projector
            .project_batch(&[event1.clone(), event2.clone()])
            .unwrap();
        assert_eq!(stats1.projected, 2);
        assert_eq!(stats1.duplicates, 0);

        // Replay same batch — all duplicates
        let stats2 = projector.project_batch(&[event1, event2]).unwrap();
        assert_eq!(stats2.projected, 0);
        assert_eq!(stats2.duplicates, 2);
    }

    // -----------------------------------------------------------------------
    // Full replay: incremental matches full rebuild
    // -----------------------------------------------------------------------

    #[test]
    fn incremental_matches_full_replay() {
        let events = vec![
            make_create("bn-001", "Auth bug", "h1", 1000),
            make_event(
                EventType::Move,
                "bn-001",
                EventData::Move(MoveData {
                    state: State::Doing,
                    reason: None,
                    extra: BTreeMap::new(),
                }),
                "h2",
                2000,
            ),
            make_event(
                EventType::Assign,
                "bn-001",
                EventData::Assign(AssignData {
                    agent: "alice".into(),
                    action: AssignAction::Assign,
                    extra: BTreeMap::new(),
                }),
                "h3",
                3000,
            ),
            make_event(
                EventType::Comment,
                "bn-001",
                EventData::Comment(CommentData {
                    body: "Working on it".into(),
                    extra: BTreeMap::new(),
                }),
                "h4",
                4000,
            ),
            make_event(
                EventType::Update,
                "bn-001",
                EventData::Update(UpdateData {
                    field: "title".into(),
                    value: serde_json::json!("Auth bug (fixed)"),
                    extra: BTreeMap::new(),
                }),
                "h5",
                5000,
            ),
            make_event(
                EventType::Move,
                "bn-001",
                EventData::Move(MoveData {
                    state: State::Done,
                    reason: Some("Shipped".into()),
                    extra: BTreeMap::new(),
                }),
                "h6",
                6000,
            ),
        ];

        // Full replay
        let conn_full = test_db();
        let proj_full = Projector::new(&conn_full);
        proj_full.project_batch(&events).unwrap();

        // Incremental: one by one
        let conn_inc = test_db();
        let proj_inc = Projector::new(&conn_inc);
        for event in &events {
            proj_inc.project_event(event).unwrap();
        }

        // Compare results
        let item_full = query::get_item(&conn_full, "bn-001", false)
            .unwrap()
            .unwrap();
        let item_inc = query::get_item(&conn_inc, "bn-001", false)
            .unwrap()
            .unwrap();

        assert_eq!(item_full.title, item_inc.title);
        assert_eq!(item_full.state, item_inc.state);
        assert_eq!(item_full.updated_at_us, item_inc.updated_at_us);

        let assignees_full = query::get_assignees(&conn_full, "bn-001").unwrap();
        let assignees_inc = query::get_assignees(&conn_inc, "bn-001").unwrap();
        assert_eq!(assignees_full.len(), assignees_inc.len());

        let comments_full = query::get_comments(&conn_full, "bn-001").unwrap();
        let comments_inc = query::get_comments(&conn_inc, "bn-001").unwrap();
        assert_eq!(comments_full.len(), comments_inc.len());
    }

    // -----------------------------------------------------------------------
    // Full rebuild (clear + replay)
    // -----------------------------------------------------------------------

    #[test]
    fn clear_and_replay_produces_same_result() {
        let conn = test_db();
        let projector = Projector::new(&conn);

        let events = vec![
            make_create("bn-001", "Item 1", "h1", 1000),
            make_create("bn-002", "Item 2", "h2", 1001),
            make_event(
                EventType::Link,
                "bn-002",
                EventData::Link(LinkData {
                    target: "bn-001".into(),
                    link_type: "blocks".into(),
                    extra: BTreeMap::new(),
                }),
                "h3",
                2000,
            ),
        ];

        // First pass
        projector.project_batch(&events).unwrap();
        let count1: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count1, 2);

        // Clear and replay
        clear_projection(&conn).unwrap();
        let count_after_clear: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_after_clear, 0);

        projector.project_batch(&events).unwrap();
        let count2: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count2, 2);

        let deps = query::get_dependencies(&conn, "bn-002").unwrap();
        assert_eq!(deps.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Placeholder item creation
    // -----------------------------------------------------------------------

    #[test]
    fn events_on_missing_item_create_placeholder() {
        let conn = test_db();
        let projector = Projector::new(&conn);

        // Comment on an item that hasn't been created yet
        let comment = make_event(
            EventType::Comment,
            "bn-ghost",
            EventData::Comment(CommentData {
                body: "Comment on missing item".into(),
                extra: BTreeMap::new(),
            }),
            "ccc",
            2000,
        );
        projector.project_event(&comment).unwrap();

        // Item exists as placeholder
        let item = query::get_item(&conn, "bn-ghost", false).unwrap().unwrap();
        assert_eq!(item.title, ""); // placeholder has empty title
        assert_eq!(item.state, "open");
    }

    // -----------------------------------------------------------------------
    // FTS integration
    // -----------------------------------------------------------------------

    #[test]
    fn project_create_populates_fts() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create(
                "bn-001",
                "Authentication timeout",
                "aaa",
                1000,
            ))
            .unwrap();

        let hits = query::search(&conn, "authentication", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item_id, "bn-001");
    }

    #[test]
    fn project_update_title_updates_fts() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Old title", "aaa", 1000))
            .unwrap();

        let update = make_event(
            EventType::Update,
            "bn-001",
            EventData::Update(UpdateData {
                field: "title".into(),
                value: serde_json::json!("Authorization failure"),
                extra: BTreeMap::new(),
            }),
            "bbb",
            2000,
        );
        projector.project_event(&update).unwrap();

        // Old title not found
        let hits_old = query::search(&conn, "Old", 10).unwrap();
        assert!(hits_old.is_empty());

        // New title found
        let hits_new = query::search(&conn, "authorization", 10).unwrap();
        assert_eq!(hits_new.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Batch stats
    // -----------------------------------------------------------------------

    #[test]
    fn batch_reports_correct_stats() {
        let conn = test_db();
        let projector = Projector::new(&conn);

        let events = vec![
            make_create("bn-001", "Item 1", "h1", 1000),
            make_create("bn-002", "Item 2", "h2", 1001),
            make_create("bn-003", "Item 3", "h3", 1002),
        ];

        let stats = projector.project_batch(&events).unwrap();
        assert_eq!(stats.projected, 3);
        assert_eq!(stats.duplicates, 0);
        assert_eq!(stats.errors, 0);
    }

    // -----------------------------------------------------------------------
    // All 11 event types in sequence
    // -----------------------------------------------------------------------

    #[test]
    fn full_lifecycle_all_event_types() {
        let conn = test_db();
        let projector = Projector::new(&conn);

        let mut events = vec![
            // 1. Create
            make_create("bn-001", "Auth bug", "h01", 1000),
            // Create a second item for linking
            make_create("bn-002", "Dep item", "h02", 1001),
        ];

        // 2. Update title
        events.push(make_event(
            EventType::Update,
            "bn-001",
            EventData::Update(UpdateData {
                field: "title".into(),
                value: serde_json::json!("Auth timeout bug"),
                extra: BTreeMap::new(),
            }),
            "h03",
            2000,
        ));

        // 3. Move to doing
        events.push(make_event(
            EventType::Move,
            "bn-001",
            EventData::Move(MoveData {
                state: State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
            "h04",
            3000,
        ));

        // 4. Assign
        events.push(make_event(
            EventType::Assign,
            "bn-001",
            EventData::Assign(AssignData {
                agent: "alice".into(),
                action: AssignAction::Assign,
                extra: BTreeMap::new(),
            }),
            "h05",
            4000,
        ));

        // 5. Comment
        events.push(make_event(
            EventType::Comment,
            "bn-001",
            EventData::Comment(CommentData {
                body: "Found root cause".into(),
                extra: BTreeMap::new(),
            }),
            "h06",
            5000,
        ));

        // 6. Link
        events.push(make_event(
            EventType::Link,
            "bn-001",
            EventData::Link(LinkData {
                target: "bn-002".into(),
                link_type: "blocks".into(),
                extra: BTreeMap::new(),
            }),
            "h07",
            6000,
        ));

        // 7. Unlink
        events.push(make_event(
            EventType::Unlink,
            "bn-001",
            EventData::Unlink(UnlinkData {
                target: "bn-002".into(),
                link_type: Some("blocks".into()),
                extra: BTreeMap::new(),
            }),
            "h08",
            7000,
        ));

        // 8. Compact
        events.push(make_event(
            EventType::Compact,
            "bn-001",
            EventData::Compact(CompactData {
                summary: "Auth token refresh race".into(),
                extra: BTreeMap::new(),
            }),
            "h09",
            8000,
        ));

        // 9. Snapshot
        events.push(make_event(
            EventType::Snapshot,
            "bn-001",
            EventData::Snapshot(SnapshotData {
                state: serde_json::json!({"id": "bn-001", "resolved": true}),
                extra: BTreeMap::new(),
            }),
            "h10",
            9000,
        ));

        // 10. Redact the comment
        events.push(make_event(
            EventType::Redact,
            "bn-001",
            EventData::Redact(RedactData {
                target_hash: "blake3:h06".into(),
                reason: "Contained secret".into(),
                extra: BTreeMap::new(),
            }),
            "h11",
            10000,
        ));

        // 11. Delete
        events.push(make_event(
            EventType::Delete,
            "bn-001",
            EventData::Delete(DeleteData {
                reason: Some("Duplicate".into()),
                extra: BTreeMap::new(),
            }),
            "h12",
            11000,
        ));

        let stats = projector.project_batch(&events).unwrap();
        assert_eq!(stats.projected, 12); // 2 creates + 10 mutations
        assert_eq!(stats.duplicates, 0);
        assert_eq!(stats.errors, 0);

        // Verify final state
        let item = query::get_item(&conn, "bn-001", true).unwrap().unwrap();
        assert_eq!(item.title, "Auth timeout bug");
        assert_eq!(item.state, "doing");
        assert!(item.is_deleted);
        assert_eq!(
            item.compact_summary.as_deref(),
            Some("Auth token refresh race")
        );
        let snapshot: Option<String> = conn
            .query_row(
                "SELECT snapshot_json FROM items WHERE item_id = 'bn-001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(snapshot.is_some());

        // Comment was redacted
        let comments = query::get_comments(&conn, "bn-001").unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "[redacted]");

        // Dependencies were unlinked
        let deps = query::get_dependencies(&conn, "bn-001").unwrap();
        assert!(deps.is_empty());

        // Redaction record exists
        let redaction_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM event_redactions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(redaction_count, 1);
    }
}
