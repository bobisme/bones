//! Event replay → `SQLite` projection pipeline.
//!
//! The [`Projector`] replays events from the TSJSON event log and upserts
//! the resulting state into the `SQLite` projection database. It handles all
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

use crate::db::query;
use crate::event::Event;
use crate::event::data::{AssignAction, EventData};
use crate::event::types::EventType;
use crate::shard::ShardManager;

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

/// Replays events into the `SQLite` projection.
///
/// Create a `Projector` with a connection, then call [`project_event`] for
/// each event or [`project_batch`] for a slice.
pub struct Projector<'conn> {
    conn: &'conn Connection,
    has_agent_column: bool,
}

impl<'conn> Projector<'conn> {
    /// Create a new projector backed by the given connection.
    ///
    /// Ensures the `projected_events` tracking table exists before any
    /// projection work is attempted.
    pub fn new(conn: &'conn Connection) -> Self {
        // Best-effort: if the DDL fails (e.g. read-only DB) we'll still
        // attempt projection and let it fail at the INSERT instead.
        let _ = ensure_tracking_table(conn);
        let has_agent_column = projected_events_has_agent_column(conn).unwrap_or(false);
        Self {
            conn,
            has_agent_column,
        }
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

        if stats.errors > 0 {
            let reason = format!(
                "project_batch encountered {} projection errors while applying {} events",
                stats.errors,
                events.len()
            );
            if let Err(err) = crate::db::mark_projection_dirty_from_connection(self.conn, &reason) {
                tracing::warn!(error = %err, "failed to mark projection dirty after batch errors");
            }
        }

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
        // One savepoint per event: a crash mid-event must not leave some of
        // its field keys claimed without the event recorded as projected.
        self.conn
            .execute_batch("SAVEPOINT project_event")
            .context("begin project_event savepoint")?;
        let result = self.project_event_inner(event);
        let end = if result.is_ok() {
            "RELEASE project_event"
        } else {
            "ROLLBACK TO project_event; RELEASE project_event"
        };
        self.conn
            .execute_batch(end)
            .context("end project_event savepoint")?;

        let projected = match result {
            Ok(ProjectResult::Projected) => true,
            Ok(ProjectResult::Duplicate) => false,
            Err(err) => {
                let reason = format!(
                    "project_event failed hash={} type={}",
                    event.event_hash, event.event_type
                );
                if let Err(mark_err) =
                    crate::db::mark_projection_dirty_from_connection(self.conn, &reason)
                {
                    tracing::warn!(
                        error = %mark_err,
                        event_hash = %event.event_hash,
                        "failed to mark projection dirty after single-event projection failure"
                    );
                }
                return Err(err);
            }
        };

        if let Err(err) = self.update_cursor_to_event_log_end(&event.event_hash) {
            tracing::warn!(
                event_hash = %event.event_hash,
                error = %err,
                "failed to update projection cursor after single-event projection"
            );
            let reason = format!(
                "cursor update failed after projecting hash={} error={err}",
                event.event_hash
            );
            let _ = crate::db::mark_projection_dirty_from_connection(self.conn, &reason);
        }

        Ok(projected)
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

    fn is_event_redacted(&self, event_hash: &str) -> Result<bool> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM event_redactions WHERE target_event_hash = ?1)",
                params![event_hash],
                |row| row.get(0),
            )
            .unwrap_or(false);
        Ok(exists)
    }

    fn record_projected_hash(&self, event_hash: &str, event: &Event) -> Result<()> {
        if self.has_agent_column {
            self.conn
                .execute(
                    "INSERT OR IGNORE INTO projected_events (event_hash, item_id, event_type, projected_at_us, agent) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        event_hash,
                        event.item_id.as_str(),
                        event.event_type.as_str(),
                        event.wall_ts_us,
                        event.agent.as_str(),
                    ],
                )
                .context("record projected event hash")?;
            return Ok(());
        }

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

    fn update_cursor_to_event_log_end(&self, event_hash: &str) -> Result<()> {
        let main_db_file: String = self
            .conn
            .query_row(
                "SELECT file FROM pragma_database_list WHERE name = 'main'",
                [],
                |row| row.get(0),
            )
            .context("read main database file from pragma_database_list")?;

        if main_db_file.trim().is_empty() {
            // In-memory projections used in tests have no on-disk log context.
            return Ok(());
        }

        let db_path = std::path::Path::new(&main_db_file);
        let Some(bones_dir) = db_path.parent() else {
            return Ok(());
        };

        let shard_mgr = ShardManager::new(bones_dir);
        let total_len = shard_mgr
            .total_content_len()
            .map_err(|e| anyhow::anyhow!("read event-log size for cursor update: {e}"))?;
        let total_len_i64 = i64::try_from(total_len).unwrap_or(i64::MAX);

        query::update_projection_cursor(self.conn, total_len_i64, Some(event_hash))
            .context("write projection cursor after single-event projection")?;
        crate::db::incremental::record_cursor_prefix(self.conn, &shard_mgr, total_len)
            .context("record cursor prefix after single-event projection")?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Event type handlers
    // -----------------------------------------------------------------------

    fn project_create(&self, event: &Event) -> Result<()> {
        let EventData::Create(ref data) = event.data else {
            anyhow::bail!("expected Create data for item.create event");
        };

        // A redaction that arrives first must give the same result as one
        // that arrives later and rewrites the fields this event owns (see
        // project_redact): text fields read "[redacted]" and labels are
        // absent, with the keys still claimed.
        let is_redacted = self.is_event_redacted(&event.event_hash)?;
        let title = if is_redacted { REDACTED } else { &data.title };
        let description = if is_redacted {
            Some(REDACTED)
        } else {
            data.description.as_deref()
        };

        self.ensure_item_exists(event)?;
        self.touch(event)?;

        // A create is an ordinary write of each field it sets, so a later
        // update wins over it and an earlier one loses, in any log order.
        self.set_field("title", title, event)?;
        self.set_field("description", description, event)?;
        self.set_field("kind", data.kind.to_string(), event)?;
        self.set_field("urgency", data.urgency.to_string(), event)?;
        self.set_field("size", data.size.map(|s| s.to_string()), event)?;
        self.set_field("parent_id", data.parent.as_deref(), event)?;

        for label in &data.labels {
            if self.claim_member(LABELS, label, event)? {
                self.set_label(event, label, !is_redacted)?;
            }
        }
        self.refresh_search_labels(event.item_id.as_str(), event.wall_ts_us)?;

        Ok(())
    }

    fn project_update(&self, event: &Event) -> Result<()> {
        let EventData::Update(ref data) = event.data else {
            anyhow::bail!("expected Update data for item.update event");
        };

        self.ensure_item_exists(event)?;
        self.touch(event)?;

        let is_redacted = self.is_event_redacted(&event.event_hash)?;

        match data.field.as_str() {
            "title" => {
                let value = if is_redacted {
                    REDACTED
                } else {
                    data.value.as_str().unwrap_or_default()
                };
                self.set_field("title", value, event)?;
            }
            "description" => {
                let value = if is_redacted {
                    Some(REDACTED.to_string())
                } else {
                    data.value.as_str().map(String::from)
                };
                self.set_field("description", value, event)?;
            }
            "kind" => {
                self.set_field("kind", data.value.as_str().unwrap_or("task"), event)?;
            }
            "size" => {
                self.set_field("size", data.value.as_str().map(String::from), event)?;
            }
            "urgency" => {
                self.set_field("urgency", data.value.as_str().unwrap_or("default"), event)?;
            }
            "parent" => {
                self.set_field("parent_id", data.value.as_str().map(String::from), event)?;
            }
            "labels" => {
                // A redacted label update still claims its members but
                // leaves them absent, as project_redact does after the fact.
                //
                // Labels update: supports both legacy array replacement
                // and new add/remove action format (CRDT-friendly).
                if let Some(labels) = data.value.as_array() {
                    // Legacy: replace the entire label set.
                    let wanted: Vec<&str> = labels.iter().filter_map(|l| l.as_str()).collect();
                    for (label, present) in self.claim_reset(LABELS, &wanted, event)? {
                        self.set_label(event, &label, present && !is_redacted)?;
                    }
                } else if let Some(obj) = data.value.as_object() {
                    // New: add/remove single label
                    let action = obj.get("action").and_then(|v| v.as_str()).unwrap_or("");
                    let label = obj.get("label").and_then(|v| v.as_str()).unwrap_or("");
                    let present = match action {
                        "add" => Some(true),
                        "remove" => Some(false),
                        _ => None,
                    };
                    if let Some(present) = present
                        && !label.is_empty()
                        && self.claim_member(LABELS, label, event)?
                    {
                        self.set_label(event, label, present && !is_redacted)?;
                    }
                }
                self.refresh_search_labels(event.item_id.as_str(), event.wall_ts_us)?;
            }
            _ => {
                // Unknown field: touch() already folded in the timestamp.
                tracing::debug!(
                    field = %data.field,
                    item_id = %event.item_id,
                    "ignoring update for unknown field"
                );
            }
        }

        Ok(())
    }

    fn project_move(&self, event: &Event) -> Result<()> {
        let EventData::Move(ref data) = event.data else {
            anyhow::bail!("expected Move data for item.move event");
        };

        self.ensure_item_exists(event)?;
        self.touch(event)?;
        self.set_field("state", data.state.to_string(), event)
            .with_context(|| format!("project move for {}", event.item_id))
    }

    fn project_assign(&self, event: &Event) -> Result<()> {
        let EventData::Assign(ref data) = event.data else {
            anyhow::bail!("expected Assign data for item.assign event");
        };

        self.ensure_item_exists(event)?;
        self.touch(event)?;

        if !self.claim_member(ASSIGNEES, &data.agent, event)? {
            return Ok(());
        }
        let item_id = event.item_id.as_str();
        match data.action {
            AssignAction::Assign => {
                self.conn
                    .execute(
                        "INSERT OR REPLACE INTO item_assignees (item_id, agent, created_at_us)
                         VALUES (?1, ?2, ?3)",
                        params![item_id, data.agent, event.wall_ts_us],
                    )
                    .with_context(|| format!("assign {} to {}", data.agent, event.item_id))?;
            }
            AssignAction::Unassign => {
                self.conn
                    .execute(
                        "DELETE FROM item_assignees WHERE item_id = ?1 AND agent = ?2",
                        params![item_id, data.agent],
                    )
                    .with_context(|| format!("unassign {} from {}", data.agent, event.item_id))?;
            }
        }

        Ok(())
    }

    fn project_comment(&self, event: &Event) -> Result<()> {
        let EventData::Comment(ref data) = event.data else {
            anyhow::bail!("expected Comment data for item.comment event");
        };

        self.ensure_item_exists(event)?;

        let is_redacted = self.is_event_redacted(&event.event_hash)?;
        let body = if is_redacted {
            "[redacted]"
        } else {
            &data.body
        };

        self.conn
            .execute(
                "INSERT OR IGNORE INTO item_comments (item_id, event_hash, author, body, created_at_us)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    event.item_id.as_str(),
                    event.event_hash,
                    event.agent,
                    body,
                    event.wall_ts_us,
                ],
            )
            .with_context(|| format!("project comment for {}", event.item_id))?;

        self.touch(event)?;

        Ok(())
    }

    fn project_link(&self, event: &Event) -> Result<()> {
        let EventData::Link(ref data) = event.data else {
            anyhow::bail!("expected Link data for item.link event");
        };

        self.ensure_item_exists(event)?;
        self.touch(event)?;

        if self.claim_member(&links_group(&data.target), &data.link_type, event)? {
            self.set_link(event, &data.target, &data.link_type, true)
                .with_context(|| format!("project link {} -> {}", event.item_id, data.target))?;
        }

        Ok(())
    }

    fn project_unlink(&self, event: &Event) -> Result<()> {
        let EventData::Unlink(ref data) = event.data else {
            anyhow::bail!("expected Unlink data for item.unlink event");
        };

        self.ensure_item_exists(event)?;
        self.touch(event)?;

        let group = links_group(&data.target);
        if let Some(ref link_type) = data.link_type {
            if self.claim_member(&group, link_type, event)? {
                self.set_link(event, &data.target, link_type, false)
                    .with_context(|| format!("unlink {} -/-> {}", event.item_id, data.target))?;
            }
        } else {
            // No link_type: remove all links to target, as a reset of the
            // target's link set to empty.
            for (link_type, present) in self.claim_reset(&group, &[], event)? {
                self.set_link(event, &data.target, &link_type, present)
                    .with_context(|| {
                        format!("unlink all {} -/-> {}", event.item_id, data.target)
                    })?;
            }
        }

        Ok(())
    }

    fn project_delete(&self, event: &Event) -> Result<()> {
        self.ensure_item_exists(event)?;
        self.touch(event)?;

        if self.claim("deleted", event)? {
            self.conn
                .execute(
                    "UPDATE items SET is_deleted = 1, deleted_at_us = ?1 WHERE item_id = ?2",
                    params![event.wall_ts_us, event.item_id.as_str()],
                )
                .with_context(|| format!("project delete for {}", event.item_id))?;
        }

        Ok(())
    }

    fn project_compact(&self, event: &Event) -> Result<()> {
        let EventData::Compact(ref data) = event.data else {
            anyhow::bail!("expected Compact data for item.compact event");
        };

        self.ensure_item_exists(event)?;
        self.touch(event)?;

        let is_redacted = self.is_event_redacted(&event.event_hash)?;
        let summary = if is_redacted {
            "[redacted]"
        } else {
            data.summary.as_str()
        };

        self.set_field("compact_summary", summary, event)
            .with_context(|| format!("project compact for {}", event.item_id))
    }

    fn project_snapshot(&self, event: &Event) -> Result<()> {
        let EventData::Snapshot(ref data) = event.data else {
            anyhow::bail!("expected Snapshot data for item.snapshot event");
        };

        self.ensure_item_exists(event)?;
        self.touch(event)?;

        let json_str = serde_json::to_string(&data.state).context("serialize snapshot state")?;
        self.set_field("snapshot_json", json_str, event)
            .with_context(|| format!("project snapshot for {}", event.item_id))
    }

    fn project_redact(&self, event: &Event) -> Result<()> {
        let EventData::Redact(ref data) = event.data else {
            anyhow::bail!("expected Redact data for item.redact event");
        };

        // Like every other event, a redaction counts toward the item's
        // created/updated bounds, so those do not depend on whether it
        // arrives before or after the item's other events.
        self.ensure_item_exists(event)?;
        self.touch(event)?;

        // Insert redaction record
        self.conn
            .execute(
                // Several redactions of one target: keep the lowest
                // (redacted_at, redacted_by, reason), whatever the arrival order.
                "INSERT INTO event_redactions \
                 (target_event_hash, item_id, reason, redacted_by, redacted_at_us) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(target_event_hash) DO UPDATE SET \
                     item_id = excluded.item_id, \
                     reason = excluded.reason, \
                     redacted_by = excluded.redacted_by, \
                     redacted_at_us = excluded.redacted_at_us \
                 WHERE (excluded.redacted_at_us, excluded.redacted_by, excluded.reason) \
                     < (event_redactions.redacted_at_us, event_redactions.redacted_by, \
                        event_redactions.reason)",
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

        // Rewrite whatever the target still owns, to match what its handler
        // writes when the redaction is already known (bn-2gl2). Fields that a
        // newer event owns show that event in either order, so they stay.
        // Look up by hash alone: handlers check redaction by hash on any
        // item, so the target may belong to another item than this event.
        let owned: Vec<(String, String)> = {
            let mut stmt = self
                .conn
                .prepare_cached("SELECT item_id, field FROM field_clocks WHERE event_hash = ?1")?;
            stmt.query_map(params![data.target_hash], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        let mut labels_changed = std::collections::BTreeSet::new();
        for (item_id, field) in &owned {
            let item_id = item_id.as_str();
            match field.as_str() {
                "title" | "description" | "compact_summary" => {
                    self.conn
                        .execute(
                            &format!("UPDATE items SET {field} = ?1 WHERE item_id = ?2"),
                            params![REDACTED, item_id],
                        )
                        .with_context(|| format!("redact {field} of {item_id}"))?;
                }
                _ => {
                    if let Some(label) = field.strip_prefix(&member_field(LABELS, "")) {
                        self.conn.execute(
                            "DELETE FROM item_labels WHERE item_id = ?1 AND label = ?2",
                            params![item_id, label],
                        )?;
                        labels_changed.insert(item_id);
                    }
                }
            }
        }
        for item_id in labels_changed {
            // i64::MIN: leave updated_at alone. Only the redaction's own item
            // folded in its timestamp, the same in either order.
            self.refresh_search_labels(item_id, i64::MIN)?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Order-independent writes (bn-1ugh)
    // -----------------------------------------------------------------------
    //
    // Every projected field is guarded by the key of the event that last
    // wrote it, stored in `field_clocks`. An event writes a field only when
    // its `(wall_ts_us, agent, event_hash)` is greater than the stored key,
    // the same order LWW merge and replay use. The projection of an event
    // set therefore does not depend on the order of lines in the log.
    //
    // Set members (a label, an assignee, a link type to a target) are fields
    // of their own. A whole-set replacement (legacy label arrays, unlink
    // without a type) is a group reset: it claims every member it beats, and
    // a later member write must beat the reset as well as the member's key.

    /// Claim `field` of the event's item for `event`.
    ///
    /// Returns `true`, and records the event's key, when the event beats the
    /// stored key or no key is stored.
    fn claim(&self, field: &str, event: &Event) -> Result<bool> {
        // Cached statements: these helpers run several times per event.
        let changed = self
            .conn
            .prepare_cached(
                "INSERT INTO field_clocks (item_id, field, wall_ts_us, agent, event_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(item_id, field) DO UPDATE SET
                     wall_ts_us = excluded.wall_ts_us,
                     agent = excluded.agent,
                     event_hash = excluded.event_hash
                 WHERE (excluded.wall_ts_us, excluded.agent, excluded.event_hash)
                     > (field_clocks.wall_ts_us, field_clocks.agent, field_clocks.event_hash)",
            )?
            .execute(params![
                event.item_id.as_str(),
                field,
                event.wall_ts_us,
                event.agent,
                event.event_hash,
            ])
            .with_context(|| format!("claim field {field} of {}", event.item_id))?;
        Ok(changed > 0)
    }

    /// `true` when `event` beats the stored key of `field`, or none is stored.
    fn beats(&self, field: &str, event: &Event) -> Result<bool> {
        self.conn
            .prepare_cached(
                "SELECT NOT EXISTS(
                     SELECT 1 FROM field_clocks
                     WHERE item_id = ?1 AND field = ?2
                       AND (wall_ts_us, agent, event_hash) >= (?3, ?4, ?5)
                 )",
            )?
            .query_row(
                params![
                    event.item_id.as_str(),
                    field,
                    event.wall_ts_us,
                    event.agent,
                    event.event_hash,
                ],
                |row| row.get(0),
            )
            .with_context(|| format!("compare field {field} of {}", event.item_id))
    }

    /// Claim `member` of set `group` for `event`. The event must also beat
    /// the group's last reset.
    fn claim_member(&self, group: &str, member: &str, event: &Event) -> Result<bool> {
        if !self.beats(&reset_field(group), event)? {
            return Ok(false);
        }
        self.claim(&member_field(group, member), event)
    }

    /// Replace set `group` with `members`.
    ///
    /// Returns each member whose presence this event now decides, with
    /// `true` when it must be present. Members with a newer write keep it.
    fn claim_reset(
        &self,
        group: &str,
        members: &[&str],
        event: &Event,
    ) -> Result<Vec<(String, bool)>> {
        if !self.claim(&reset_field(group), event)? {
            return Ok(Vec::new());
        }

        let prefix = member_field(group, "");
        let mut known: std::collections::BTreeSet<String> = {
            // SQLite measures the prefix itself: substr counts characters,
            // not the bytes Rust's len() would give.
            let mut stmt = self.conn.prepare_cached(
                "SELECT substr(field, length(?2) + 1) FROM field_clocks
                 WHERE item_id = ?1 AND substr(field, 1, length(?2)) = ?2",
            )?;
            stmt.query_map(params![event.item_id.as_str(), prefix], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        known.extend(members.iter().map(|m| (*m).to_string()));

        let mut decided = Vec::new();
        for member in known {
            if self.claim(&member_field(group, &member), event)? {
                let present = members.contains(&member.as_str());
                decided.push((member, present));
            }
        }
        Ok(decided)
    }

    /// Write `column` of the event's item when the event wins the column.
    ///
    /// `column` must be a fixed column name, never user input.
    fn set_field(&self, column: &str, value: impl rusqlite::ToSql, event: &Event) -> Result<()> {
        if self.claim(column, event)? {
            self.conn
                .prepare_cached(&format!(
                    "UPDATE items SET {column} = ?1 WHERE item_id = ?2"
                ))?
                .execute(params![value, event.item_id.as_str()])
                .with_context(|| format!("set {column} of {}", event.item_id))?;
        }
        Ok(())
    }

    /// Fold the event's timestamp into the item's created/updated bounds.
    fn touch(&self, event: &Event) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE items
                 SET created_at_us = MIN(created_at_us, ?1),
                     updated_at_us = MAX(updated_at_us, ?1)
                 WHERE item_id = ?2",
            )?
            .execute(params![event.wall_ts_us, event.item_id.as_str()])
            .with_context(|| format!("touch {}", event.item_id))?;
        Ok(())
    }

    fn set_label(&self, event: &Event, label: &str, present: bool) -> Result<()> {
        if present {
            self.conn.execute(
                "INSERT OR REPLACE INTO item_labels (item_id, label, created_at_us)
                 VALUES (?1, ?2, ?3)",
                params![event.item_id.as_str(), label, event.wall_ts_us],
            )?;
        } else {
            self.conn.execute(
                "DELETE FROM item_labels WHERE item_id = ?1 AND label = ?2",
                params![event.item_id.as_str(), label],
            )?;
        }
        Ok(())
    }

    fn set_link(&self, event: &Event, target: &str, link_type: &str, present: bool) -> Result<()> {
        if present {
            self.conn.execute(
                "INSERT OR REPLACE INTO item_dependencies
                     (item_id, depends_on_item_id, link_type, created_at_us)
                 VALUES (?1, ?2, ?3, ?4)",
                params![event.item_id.as_str(), target, link_type, event.wall_ts_us],
            )?;
        } else {
            self.conn.execute(
                "DELETE FROM item_dependencies
                 WHERE item_id = ?1 AND depends_on_item_id = ?2 AND link_type = ?3",
                params![event.item_id.as_str(), target, link_type],
            )?;
        }
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

    fn refresh_search_labels(&self, item_id: &str, updated_at_us: i64) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT label FROM item_labels WHERE item_id = ?1 ORDER BY label")?;
        let label_rows = stmt.query_map(params![item_id], |row| row.get::<_, String>(0))?;

        let mut label_strings = Vec::new();
        for label_res in label_rows {
            label_strings.push(label_res?);
        }

        let search_labels = label_strings.join(" ");
        self.conn.execute(
            "UPDATE items
             SET search_labels = ?1,
                 updated_at_us = MAX(updated_at_us, ?2)
             WHERE item_id = ?3",
            params![search_labels, updated_at_us, item_id],
        )?;
        Ok(())
    }
}

/// Text that replaces redacted content in the projection.
const REDACTED: &str = "[redacted]";

/// Set group of an item's labels in `field_clocks`.
const LABELS: &str = "label";
/// Set group of an item's assignees in `field_clocks`.
const ASSIGNEES: &str = "assignee";

/// Set group of an item's link types to `target`.
///
/// The target is length-prefixed so that no target or link type containing
/// '/' can alias another target's keys: without it, target `a` with type
/// `b/c` and target `a/b` with type `c` both map to `link/a/b/c`, and the
/// reset prefix `link/a/` also matches target `a/b` (bn-3scg).
fn links_group(target: &str) -> String {
    format!("link/{}:{target}", target.len())
}

/// `field_clocks` field of one member of a set group.
fn member_field(group: &str, member: &str) -> String {
    format!("{group}/{member}")
}

/// `field_clocks` field of a set group's last whole-set replacement.
fn reset_field(group: &str) -> String {
    format!("{group}*")
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
    projected_at_us INTEGER NOT NULL,
    agent TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_projected_events_item
    ON projected_events(item_id);
CREATE INDEX IF NOT EXISTS idx_projected_events_agent
    ON projected_events(agent);
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

    if !projected_events_has_agent_column(conn)? {
        conn.execute(
            "ALTER TABLE projected_events ADD COLUMN agent TEXT NOT NULL DEFAULT ''",
            [],
        )
        .context("add agent column to projected_events")?;
    }

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_projected_events_agent ON projected_events(agent);",
    )
    .context("create projected_events_agent index")?;

    Ok(())
}

fn projected_events_has_agent_column(conn: &Connection) -> Result<bool> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(projected_events)")
        .context("inspect projected_events schema")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;

    for row in rows {
        let name = row.context("read projected_events column")?;
        if name == "agent" {
            return Ok(true);
        }
    }

    Ok(false)
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
         DELETE FROM field_clocks;
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
    use crate::event::writer;
    use crate::model::item::{Kind, Size, State, Urgency};
    use crate::model::item_id::ItemId;
    use crate::shard::ShardManager;
    use rusqlite::Connection;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

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

    #[test]
    fn project_event_updates_projection_cursor_for_file_backed_db() {
        let dir = TempDir::new().expect("tempdir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(&bones_dir).expect("create .bones");

        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().expect("init shard manager");

        let db_path = bones_dir.join("bones.db");
        let mut conn = Connection::open(&db_path).expect("open projection db");
        migrations::migrate(&mut conn).expect("migrate");
        ensure_tracking_table(&conn).expect("create tracking table");

        let projector = Projector::new(&conn);
        let mut event = make_create("bn-001", "Cursor update", "h1", 1000);
        let line = writer::write_event(&mut event).expect("serialize event");
        shard_mgr
            .append(&line, false, std::time::Duration::from_secs(5))
            .expect("append event line");

        projector.project_event(&event).expect("project event");

        let (offset, hash) = query::get_projection_cursor(&conn).expect("read projection cursor");
        let expected_offset =
            i64::try_from(shard_mgr.total_content_len().expect("content len")).unwrap_or(i64::MAX);

        assert_eq!(offset, expected_offset);
        assert_eq!(hash.as_deref(), Some(event.event_hash.as_str()));
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

        let comments = query::get_comments(&conn, "bn-001", None, None).unwrap();
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

    #[test]
    fn link_keys_do_not_collide_across_targets_with_slashes() {
        let conn = test_db();
        let projector = Projector::new(&conn);
        for (id, hash, ts) in [
            ("bn-001", "a01", 1000),
            ("bn-x", "a02", 1001),
            ("bn-x/a", "a03", 1002),
        ] {
            projector
                .project_event(&make_create(id, "Item", hash, ts))
                .unwrap();
        }
        let link = |target: &str, link_type: &str, hash: &str, ts: i64| {
            make_event(
                EventType::Link,
                "bn-001",
                EventData::Link(LinkData {
                    target: target.into(),
                    link_type: link_type.into(),
                    extra: BTreeMap::new(),
                }),
                hash,
                ts,
            )
        };
        // Both would be keyed `link/bn-x/a/b` without the length prefix, and
        // the older write would lose to the newer one.
        projector
            .project_event(&link("bn-x", "a/b", "b01", 3000))
            .unwrap();
        projector
            .project_event(&link("bn-x/a", "b", "b02", 2000))
            .unwrap();
        // Removing every link to `x` must not touch links to `x/a`.
        let unlink_all = make_event(
            EventType::Unlink,
            "bn-001",
            EventData::Unlink(UnlinkData {
                target: "bn-x".into(),
                link_type: None,
                extra: BTreeMap::new(),
            }),
            "b03",
            4000,
        );
        projector.project_event(&unlink_all).unwrap();

        let links: Vec<(String, String)> = conn
            .prepare(
                "SELECT depends_on_item_id, link_type FROM item_dependencies
                 WHERE item_id = 'bn-001' ORDER BY 1, 2",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(links, vec![("bn-x/a".to_string(), "b".to_string())]);
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
        let comments = query::get_comments(&conn, "bn-001", None, None).unwrap();
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

        let comments_full = query::get_comments(&conn_full, "bn-001", None, None).unwrap();
        let comments_inc = query::get_comments(&conn_inc, "bn-001", None, None).unwrap();
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
        let comments = query::get_comments(&conn, "bn-001", None, None).unwrap();
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

    #[test]
    fn late_create_populates_placeholder() {
        let conn = test_db();
        let projector = Projector::new(&conn);

        // 1. Comment triggers placeholder creation
        let comment = make_event(
            EventType::Comment,
            "bn-late",
            EventData::Comment(CommentData {
                body: "Comment first".into(),
                extra: BTreeMap::new(),
            }),
            "h1",
            1000,
        );
        projector.project_event(&comment).unwrap();

        let item = query::get_item(&conn, "bn-late", false).unwrap().unwrap();
        assert_eq!(item.title, "");
        assert_eq!(item.created_at_us, 1000);

        // 2. Late Create arrives
        let create = make_create("bn-late", "Real Title", "h2", 900);

        projector.project_event(&create).unwrap();

        let item = query::get_item(&conn, "bn-late", false).unwrap().unwrap();
        assert_eq!(item.title, "Real Title");
        assert_eq!(item.created_at_us, 900);
    }

    #[test]
    fn late_create_backfills_placeholder_after_field_update() {
        let conn = test_db();
        let projector = Projector::new(&conn);

        let update = make_event(
            EventType::Update,
            "bn-late",
            EventData::Update(UpdateData {
                field: "title".into(),
                value: serde_json::json!("Updated before create"),
                extra: BTreeMap::new(),
            }),
            "h1",
            1000,
        );
        projector.project_event(&update).unwrap();

        let create = make_create("bn-late", "Initial title", "h2", 900);
        projector.project_event(&create).unwrap();

        let item = query::get_item(&conn, "bn-late", false).unwrap().unwrap();
        assert_eq!(item.title, "Updated before create");
        assert_eq!(item.kind, "task");
        assert_eq!(item.size.as_deref(), Some("m"));
        assert_eq!(item.description.as_deref(), Some("A detailed description"));
        assert_eq!(item.created_at_us, 900);
        assert_eq!(item.updated_at_us, 1000);

        let labels = query::get_labels(&conn, "bn-late").unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(item.search_labels, "auth backend");
    }

    /// Regression: Projector::new() must create the projected_events table
    /// on a migrated DB that never had ensure_tracking_table() called.
    /// Without this, fresh installs fail with "record projected event hash".
    #[test]
    fn projector_new_creates_tracking_table_on_fresh_db() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        // Do NOT call ensure_tracking_table — simulate fresh install.

        // Projector::new should create the table automatically.
        let projector = Projector::new(&conn);
        let event = make_create("bn-fresh", "Fresh item", "h1", 1000);
        projector
            .project_event(&event)
            .expect("project_event should succeed on fresh DB");

        let item = query::get_item(&conn, "bn-fresh", false).unwrap().unwrap();
        assert_eq!(item.title, "Fresh item");
    }
}
