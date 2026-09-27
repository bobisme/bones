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

use crate::compact::{LwwSnapshot, SNAPSHOT_FORMAT, SnapshotPayload};
use crate::crdt::OrSet;
use crate::crdt::item_state::LinkKey;
use crate::db::query;
use crate::event::Event;
use crate::event::data::{AssignAction, EventData};
use crate::event::types::EventType;
use crate::model::field_value::{self, can_link};
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
    /// This does not take the projection write lock or write the cursor. A
    /// caller that writes the cursor afterwards holds the lock across both
    /// (as `incremental_apply` and the rebuild do).
    ///
    /// # Errors
    ///
    /// Returns an error if the transaction fails to commit. Individual event
    /// projection errors are counted in `stats.errors` but do not abort the
    /// batch.
    pub fn project_batch(&self, events: &[Event]) -> Result<ProjectionStats> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .context("begin projection transaction")?;

        // An event that fails must leave nothing behind, as in
        // project_event: the field keys it claimed before the failure would
        // decide later writes, and its timestamp would stay in the item's
        // bounds. A savepoint per event costs about a fifth of a rebuild, and
        // events rarely fail. So the batch runs without them first, and on
        // the first failure it rolls back and runs again with them. Both
        // need a journal: the rebuild keeps one in memory (bn-39tj).
        let stats = if let Some(stats) = self.project_events(events, false)? {
            stats
        } else {
            self.conn
                .execute_batch("ROLLBACK; BEGIN IMMEDIATE")
                .context("restart projection transaction after an event failed")?;
            self.project_events(events, true)?
                .context("per-event savepoints absorb every event failure")?
        };

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

    /// Project `events` in the open transaction.
    ///
    /// Without `savepoints`, the first failed event stops the run and
    /// returns `None`: the caller must roll the transaction back. With them,
    /// a failed event is rolled back alone, logged and counted.
    fn project_events(
        &self,
        events: &[Event],
        savepoints: bool,
    ) -> Result<Option<ProjectionStats>> {
        let mut stats = ProjectionStats::default();
        for event in events {
            if savepoints {
                self.conn
                    .execute_batch("SAVEPOINT project_batch_event")
                    .context("begin batch event savepoint")?;
            }
            let result = self.project_event_inner(event);
            if savepoints {
                let end = if result.is_ok() {
                    "RELEASE project_batch_event"
                } else {
                    "ROLLBACK TO project_batch_event; RELEASE project_batch_event"
                };
                self.conn
                    .execute_batch(end)
                    .context("end batch event savepoint")?;
            }
            match result {
                Ok(ProjectResult::Projected) => stats.projected += 1,
                Ok(ProjectResult::Duplicate) => stats.duplicates += 1,
                Err(_) if !savepoints => return Ok(None),
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
        Ok(Some(stats))
    }

    /// Project a single event (outside of any managed transaction).
    ///
    /// Returns `true` if the event was projected, `false` if it was a
    /// duplicate.
    ///
    /// For an on-disk projection this takes the projection write lock (see
    /// [`crate::db::projection_lock`]). Do not call it while this thread
    /// holds that lock.
    ///
    /// # Errors
    ///
    /// Returns an error if the projection fails, or if the projection lock
    /// is not free within [`crate::db::PROJECTION_LOCK_TIMEOUT`].
    pub fn project_event(&self, event: &Event) -> Result<bool> {
        // Hold the projection write lock across the event rows and the
        // cursor write, so a rebuild cannot copy its pages in between and
        // leave the cursor covering an event it removed (bn-x6aa). A lock
        // timeout returns an error before any write: the cursor stays
        // behind the event, and the next ensure_projection replays it.
        let db_file = self.main_db_file()?;
        let _lock = match &db_file {
            Some(path) => Some(
                crate::db::lock_projection(path)
                    .context("lock projection for single-event projection")?,
            ),
            None => None,
        };

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

        #[cfg(test)]
        fault::before_cursor_write();
        if let Err(err) = self.update_cursor_to_event_log_end(&event.event_hash, db_file.as_deref())
        {
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

        #[cfg(test)]
        if fault::should_fail(&event.event_hash) {
            anyhow::bail!("injected failure after the handler of {}", event.event_hash);
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

    /// The file of the connection's main database, or `None` for an
    /// in-memory projection (tests), which has no event log.
    fn main_db_file(&self) -> Result<Option<std::path::PathBuf>> {
        let main_db_file: String = self
            .conn
            .query_row(
                "SELECT file FROM pragma_database_list WHERE name = 'main'",
                [],
                |row| row.get(0),
            )
            .context("read main database file from pragma_database_list")?;
        if main_db_file.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(std::path::PathBuf::from(main_db_file)))
    }

    fn update_cursor_to_event_log_end(
        &self,
        event_hash: &str,
        db_file: Option<&std::path::Path>,
    ) -> Result<()> {
        // In-memory projections used in tests have no on-disk log context.
        let Some(db_path) = db_file else {
            return Ok(());
        };
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
            data.description.as_deref().filter(|d| !d.is_empty())
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
        self.set_parent(data.parent.as_deref(), event)?;

        for label in &data.labels {
            if self.claim_member(LABELS, label, event)? {
                self.set_label(event, label, !is_redacted)?;
            }
        }
        self.refresh_search_labels(event.item_id.as_str(), event.order_ts())?;

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
            // Malformed values are no write, as in WorkItemState: see
            // `model::field_value` (bn-18fs). The raw values would also fail
            // the schema's CHECK and foreign key constraints.
            "title" => {
                if let Some(title) = field_value::title(&data.value) {
                    let title = if is_redacted { REDACTED } else { title };
                    self.set_field("title", title, event)?;
                }
            }
            "description" => {
                if let Some(description) = field_value::description(&data.value) {
                    let description = if is_redacted {
                        Some(REDACTED)
                    } else {
                        description
                    };
                    self.set_field("description", description, event)?;
                }
            }
            "kind" => {
                if let Some(kind) = field_value::kind(&data.value) {
                    self.set_field("kind", kind.to_string(), event)?;
                }
            }
            "size" => {
                if let Some(size) = field_value::size(&data.value) {
                    self.set_field("size", size.map(|s| s.to_string()), event)?;
                }
            }
            "urgency" => {
                if let Some(urgency) = field_value::urgency(&data.value) {
                    self.set_field("urgency", urgency.to_string(), event)?;
                }
            }
            "parent" => {
                if let Some(parent) = field_value::parent(&data.value) {
                    self.set_parent(parent, event)?;
                }
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
                self.refresh_search_labels(event.item_id.as_str(), event.order_ts())?;
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
        if can_link(event.item_id.as_str(), &data.target) {
            // Before the claim, as in set_parent: the target's placeholder
            // must not depend on whether this link wins.
            self.ensure_ref_exists(&data.target, event.order_ts())?;
        }

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
                    // order_ts, the key's timestamp: a snapshot that carries
                    // this delete writes the same value (merge_snapshot).
                    params![event.order_ts(), event.item_id.as_str()],
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
        let item_id = event.item_id.as_str();

        // A payload in another shape (older or foreign writers) is kept as
        // JSON only.
        let payload = serde_json::from_value::<SnapshotPayload>(data.state.clone())
            .map_err(|err| {
                tracing::debug!(
                    item_id = %event.item_id,
                    error = %err,
                    "snapshot payload not in SnapshotPayload shape; kept as JSON only"
                );
            })
            .ok();

        // The item's bounds are those of the snapshot's sources, which may
        // be gone from the log. The snapshot's own time (just after its
        // sources) counts as an update only, as it does not in the state.
        let ts = event.order_ts();
        let (created, updated) = match &payload {
            Some(p) if p.format >= SNAPSHOT_FORMAT && p.compacted_from > 0 => {
                (clock_ts(p.created_at), clock_ts(p.updated_at).max(ts))
            }
            _ => (ts, ts),
        };
        self.ensure_item_exists_at(item_id, created, updated)?;
        self.fold_bounds(item_id, created, updated)?;

        let is_redacted = self.record_snapshot_sources(event)?;
        let json_str = if is_redacted {
            REDACTED.to_string()
        } else {
            serde_json::to_string(&data.state).context("serialize snapshot state")?
        };
        self.set_field("snapshot_json", json_str, event)
            .with_context(|| format!("project snapshot for {}", event.item_id))?;

        // A payload of an older format (bn-t37g's 2 or before) can carry
        // values this bn never writes from the same events: it is JSON only.
        payload
            .filter(|p| p.format >= SNAPSHOT_FORMAT)
            .map_or(Ok(()), |payload| {
                self.merge_snapshot(event, &payload, is_redacted)
            })
    }

    /// Record the snapshot's source events (its `parents`), and return
    /// `true` when the snapshot counts as redacted: it is redacted itself,
    /// or one of its sources is (bn-1npc).
    ///
    /// A snapshot holds the content of its sources, so a redaction of a
    /// source redacts the snapshot as well. It then projects like a
    /// redacted snapshot: `snapshot_json` reads "[redacted]" and the labels
    /// it owns are absent, with their keys still claimed. A redaction that
    /// arrives after the snapshot finds it through `snapshot_sources` (see
    /// `project_redact`), so the result does not depend on the order.
    ///
    /// Its LWW fields need no such rule: they carry the keys of the source
    /// events that wrote them, so a redaction of a source already covers
    /// them. The labels are the gap: the snapshot claims them with its own
    /// hash (`lower_key`). This only shows when the log does not hold the
    /// source: a source in the log beats the snapshot's lower-bound claim.
    /// The payload does not say which source added which label, so every
    /// label the snapshot owns is hidden.
    fn record_snapshot_sources(&self, event: &Event) -> Result<bool> {
        let mut insert = self.conn.prepare_cached(
            "INSERT OR IGNORE INTO snapshot_sources (source_hash, snapshot_hash) VALUES (?1, ?2)",
        )?;
        for parent in &event.parents {
            insert
                .execute(params![parent, event.event_hash])
                .with_context(|| format!("record source {parent} of snapshot {}", event.item_id))?;
        }
        self.conn
            .prepare_cached(
                "SELECT EXISTS(
                     SELECT 1 FROM event_redactions
                     WHERE target_event_hash = ?1
                        OR target_event_hash IN
                           (SELECT source_hash FROM snapshot_sources WHERE snapshot_hash = ?1)
                 )",
            )?
            .query_row(params![event.event_hash], |row| row.get(0))
            .context("check snapshot redaction")
    }

    /// Merge a snapshot into the item field by field (bn-18fs).
    ///
    /// A snapshot is a lattice element, not one write at its own timestamp
    /// (see `crate::compact`). Each LWW field is claimed with the key that
    /// the payload stores for it, which is the key of the event that wrote
    /// the value. With that event also in the log, the two claims tie and
    /// write the same value, so the result does not depend on the order or
    /// on whether the log was compacted.
    ///
    /// The payload has no exact key for the other fields. Those claims use
    /// the lowest key that is still true, so that any real event of the
    /// same field at that time wins:
    ///
    /// - state: `(0, "", snapshot hash)`, the lowest key there is. The
    ///   epoch/phase state has no clock, so any real move wins, even one
    ///   older than the snapshot's sources (clock skew, time 0). The
    ///   snapshot's state applies only when the log holds no move.
    /// - set members present in the OR-Sets: `(newest live add tag, "",
    ///   snapshot hash)`. Tags hash the agent and event hash, so the add's
    ///   own key is lost. Members that the payload has only as tombstones
    ///   are skipped: the payload does not say when they were removed.
    ///
    /// Not merged: comments (the payload holds hashes, not bodies). `bn
    /// compact` keeps the comment events in the log, so this loses nothing
    /// (see `crate::compact`).
    ///
    /// A snapshot with a redacted source counts as redacted: see
    /// `record_snapshot_sources`.
    ///
    /// Only a payload of `SNAPSHOT_FORMAT` or newer is merged (see
    /// `project_snapshot`).
    fn merge_snapshot(
        &self,
        event: &Event,
        payload: &SnapshotPayload,
        is_redacted: bool,
    ) -> Result<()> {
        let item_id = event.item_id.as_str();

        if let Some(key) = register_key(item_id, &payload.title) {
            let title = if self.is_event_redacted(key.hash)? {
                REDACTED
            } else {
                payload.title.value.as_str()
            };
            self.set_field("title", title, key)?;
        }
        if let Some(key) = register_key(item_id, &payload.description) {
            let description = if self.is_event_redacted(key.hash)? {
                Some(REDACTED)
            } else {
                Some(payload.description.value.as_str()).filter(|d| !d.is_empty())
            };
            self.set_field("description", description, key)?;
        }
        if let Some(key) = register_key(item_id, &payload.kind) {
            self.set_field("kind", payload.kind.value.to_string(), key)?;
        }
        if let Some(key) = register_key(item_id, &payload.size) {
            self.set_field("size", payload.size.value.map(|s| s.to_string()), key)?;
        }
        if let Some(key) = register_key(item_id, &payload.urgency) {
            self.set_field("urgency", payload.urgency.value.to_string(), key)?;
        }
        if let Some(key) = register_key(item_id, &payload.parent) {
            self.set_parent(Some(payload.parent.value.as_str()), key)?;
        }
        if let Some(key) = register_key(item_id, &payload.deleted)
            && self.claim("deleted", key)?
        {
            let deleted_at = payload.deleted.value.then_some(key.ts);
            self.conn
                .execute(
                    "UPDATE items SET is_deleted = ?1, deleted_at_us = ?2 WHERE item_id = ?3",
                    params![payload.deleted.value, deleted_at, item_id],
                )
                .with_context(|| format!("merge snapshot deleted flag of {item_id}"))?;
        }

        // Written by format 2 snapshots only. A format 0 snapshot folded the
        // summary into the description, which stays the description here.
        if let Some(key) = register_key(item_id, &payload.compact_summary) {
            let summary = if self.is_event_redacted(key.hash)? {
                REDACTED
            } else {
                payload.compact_summary.value.as_str()
            };
            self.set_field("compact_summary", summary, key)?;
        }

        // Lower-bound keys, see above.
        self.set_field("state", payload.state.phase.as_str(), lower_key(event, 0))?;
        self.merge_snapshot_sets(event, payload, is_redacted)?;

        Ok(())
    }

    /// Merge the snapshot's OR-Sets: each live member is claimed with a
    /// lower-bound key (see `merge_snapshot`).
    fn merge_snapshot_sets(
        &self,
        event: &Event,
        payload: &SnapshotPayload,
        is_redacted: bool,
    ) -> Result<()> {
        let item_id = event.item_id.as_str();
        let lower_key = |ts: u64| lower_key(event, ts);

        for (label, ts) in live_members(&payload.labels) {
            if self.claim_member(LABELS, &label, lower_key(ts))? {
                // Like a redacted label event: claimed, but absent.
                self.set_label(lower_key(ts), &label, !is_redacted)?;
            }
        }
        self.refresh_search_labels(item_id, event.order_ts())?;

        for (agent, ts) in live_members(&payload.assignees) {
            if self.claim_member(ASSIGNEES, &agent, lower_key(ts))? {
                self.conn
                    .execute(
                        "INSERT OR REPLACE INTO item_assignees (item_id, agent, created_at_us)
                         VALUES (?1, ?2, ?3)",
                        params![item_id, agent, clock_ts(ts)],
                    )
                    .with_context(|| format!("merge snapshot assignee {agent} of {item_id}"))?;
            }
        }

        // Each link with its raw type (format 2 and newer).
        let links = live_members(&payload.links);
        for (link, ts) in links {
            let key = lower_key(ts);
            if can_link(item_id, &link.target) {
                self.ensure_ref_exists(&link.target, key.ts)?;
            }
            if self.claim_member(&links_group(&link.target), &link.link_type, key)? {
                self.set_link(key, &link.target, &link.link_type, true)?;
            }
        }

        Ok(())
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
                 WHERE (excluded.redacted_at_us, excluded.redacted_by, excluded.reason, \
                        excluded.item_id) \
                     < (event_redactions.redacted_at_us, event_redactions.redacted_by, \
                        event_redactions.reason, event_redactions.item_id)",
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
        //
        // A snapshot of the target counts as redacted too, so the fields it
        // owns are rewritten as well (bn-1npc, see record_snapshot_sources).
        let owned: Vec<(String, String)> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT item_id, field FROM field_clocks
                 WHERE event_hash = ?1
                    OR event_hash IN
                       (SELECT snapshot_hash FROM snapshot_sources WHERE source_hash = ?1)",
            )?;
            stmt.query_map(params![data.target_hash], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        let mut labels_changed = std::collections::BTreeSet::new();
        for (item_id, field) in &owned {
            let item_id = item_id.as_str();
            match field.as_str() {
                "title" | "description" | "compact_summary" | "snapshot_json" => {
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
    // its `(order_ts, agent, event_hash)` is greater than the stored key,
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
    fn claim<'k>(&self, field: &str, key: impl Into<Key<'k>>) -> Result<bool> {
        let key = key.into();
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
            .execute(params![key.item_id, field, key.ts, key.agent, key.hash])
            .with_context(|| format!("claim field {field} of {}", key.item_id))?;
        Ok(changed > 0)
    }

    /// `true` when `event` beats the stored key of `field`, or none is stored.
    fn beats(&self, field: &str, key: Key<'_>) -> Result<bool> {
        self.conn
            .prepare_cached(
                "SELECT NOT EXISTS(
                     SELECT 1 FROM field_clocks
                     WHERE item_id = ?1 AND field = ?2
                       AND (wall_ts_us, agent, event_hash) >= (?3, ?4, ?5)
                 )",
            )?
            .query_row(
                params![key.item_id, field, key.ts, key.agent, key.hash],
                |row| row.get(0),
            )
            .with_context(|| format!("compare field {field} of {}", key.item_id))
    }

    /// Claim `member` of set `group` for `event`. The event must also beat
    /// the group's last reset.
    ///
    /// A blank member is never claimed: the schema rejects it, and a handler
    /// must not fail after its first claim (`field_value::is_blank_member`).
    fn claim_member<'k>(&self, group: &str, member: &str, key: impl Into<Key<'k>>) -> Result<bool> {
        let key = key.into();
        if field_value::is_blank_member(member) {
            return Ok(false);
        }
        if !self.beats(&reset_field(group), key)? {
            return Ok(false);
        }
        self.claim(&member_field(group, member), key)
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
        let members: Vec<&str> = members
            .iter()
            .copied()
            .filter(|m| !field_value::is_blank_member(m))
            .collect();
        let members = members.as_slice();
        if !self.claim(&reset_field(group), event)? {
            return Ok(Vec::new());
        }

        let prefix = member_field(group, "");
        let mut known: std::collections::BTreeSet<String> = {
            // A byte range, not SQLite's substr() or length(): those stop
            // at the first NUL, and a member or a link target can hold one
            // (bn-1npc). Comparisons use every byte. The prefix ends with
            // '/', so every field that starts with it sorts before the
            // prefix with '/' changed to '0'.
            let end = format!("{}0", &prefix[..prefix.len() - 1]);
            let mut stmt = self.conn.prepare_cached(
                "SELECT field FROM field_clocks
                 WHERE item_id = ?1 AND field >= ?2 AND field < ?3",
            )?;
            stmt.query_map(params![event.item_id.as_str(), prefix, end], |row| {
                row.get::<_, String>(0)
            })?
            .map(|field| {
                field.map(|f| {
                    f.strip_prefix(prefix.as_str())
                        .unwrap_or_default()
                        .to_string()
                })
            })
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
    fn set_field<'k>(
        &self,
        column: &str,
        value: impl rusqlite::ToSql,
        key: impl Into<Key<'k>>,
    ) -> Result<()> {
        let key = key.into();
        if self.claim(column, key)? {
            self.conn
                .prepare_cached(&format!(
                    "UPDATE items SET {column} = ?1 WHERE item_id = ?2"
                ))?
                .execute(params![value, key.item_id])
                .with_context(|| format!("set {column} of {}", key.item_id))?;
        }
        Ok(())
    }

    /// Write the item's parent when the event wins `parent_id`.
    ///
    /// No parent, an empty string and a string that is not an item ID all
    /// clear the parent (`field_value::parent_or_none`). An update decides
    /// first whether its value is a write at all (`field_value::parent`).
    /// A parent that has no row yet gets a reference
    /// placeholder, so the write does not depend on whether the parent's
    /// create came first (the column is a foreign key).
    fn set_parent<'k>(&self, parent: Option<&str>, key: impl Into<Key<'k>>) -> Result<()> {
        let key = key.into();
        let parent = field_value::parent_or_none(parent);
        if let Some(parent) = parent {
            // Before the claim: the placeholder must exist whether or not
            // this write wins, or its existence depends on the order.
            self.ensure_ref_exists(parent, key.ts)?;
        }
        self.set_field("parent_id", parent, key)
    }

    /// Fold the event's timestamp into the item's created/updated bounds.
    ///
    /// `created_at_us` is the smallest non-zero event time, and 0 only when
    /// every event is at time 0 (a pre-epoch time orders as 0). This is the
    /// verified `min_nonzero` join that `WorkItemState` and its snapshots
    /// use, where 0 means "unknown" (bn-t37g).
    fn touch(&self, event: &Event) -> Result<()> {
        let ts = event.order_ts();
        self.fold_bounds(event.item_id.as_str(), ts, ts)
            .with_context(|| format!("touch {}", event.item_id))
    }

    /// Fold `created` into `created_at_us` by the `min_nonzero` rule and
    /// `updated` into `updated_at_us` by max (see `touch`).
    fn fold_bounds(&self, item_id: &str, created: i64, updated: i64) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE items
                 SET created_at_us = CASE
                         WHEN ?1 = 0 THEN created_at_us
                         WHEN created_at_us = 0 THEN ?1
                         ELSE MIN(created_at_us, ?1)
                     END,
                     updated_at_us = MAX(updated_at_us, ?2)
                 WHERE item_id = ?3",
            )?
            .execute(params![created, updated, item_id])
            .with_context(|| format!("fold created/updated bounds of {item_id}"))?;
        Ok(())
    }

    fn set_label<'k>(&self, key: impl Into<Key<'k>>, label: &str, present: bool) -> Result<()> {
        let key = key.into();
        if present {
            self.conn.execute(
                "INSERT OR REPLACE INTO item_labels (item_id, label, created_at_us)
                 VALUES (?1, ?2, ?3)",
                params![key.item_id, label, key.written_at],
            )?;
        } else {
            self.conn.execute(
                "DELETE FROM item_labels WHERE item_id = ?1 AND label = ?2",
                params![key.item_id, label],
            )?;
        }
        Ok(())
    }

    fn set_link<'k>(
        &self,
        key: impl Into<Key<'k>>,
        target: &str,
        link_type: &str,
        present: bool,
    ) -> Result<()> {
        let key = key.into();
        if present {
            if !can_link(key.item_id, target) {
                return Ok(());
            }
            self.conn.execute(
                "INSERT OR REPLACE INTO item_dependencies
                     (item_id, depends_on_item_id, link_type, created_at_us)
                 VALUES (?1, ?2, ?3, ?4)",
                params![key.item_id, target, link_type, key.written_at],
            )?;
        } else {
            self.conn.execute(
                "DELETE FROM item_dependencies
                 WHERE item_id = ?1 AND depends_on_item_id = ?2 AND link_type = ?3",
                params![key.item_id, target, link_type],
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
        let ts = event.order_ts();
        self.ensure_item_exists_at(event.item_id.as_str(), ts, ts)
    }

    /// `ensure_item_exists` for an event whose created/updated bounds are
    /// `created` and `updated` (a snapshot carries its sources' bounds).
    fn ensure_item_exists_at(&self, item_id: &str, created: i64, updated: i64) -> Result<()> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM items WHERE item_id = ?1)",
                params![item_id],
                |row| row.get(0),
            )
            .context("check item exists")?;

        if exists {
            // A reference placeholder (see ensure_ref_exists) is hidden and
            // carries the timestamps of the events that refer to it. The
            // item's first own event makes the row what the other branch
            // would insert, so the result does not depend on the order.
            self.conn
                .prepare_cached(
                    "UPDATE items SET created_at_us = ?2, updated_at_us = ?3, is_deleted = 0
                     WHERE item_id = ?1
                       AND NOT EXISTS(SELECT 1 FROM projected_events WHERE item_id = ?1)",
                )?
                .execute(params![item_id, created, updated])
                .with_context(|| format!("claim reference placeholder {item_id}"))?;
        } else {
            self.conn
                .execute(
                    "INSERT INTO items (
                        item_id, title, kind, state, urgency,
                        is_deleted, search_labels, created_at_us, updated_at_us
                    ) VALUES (?1, '', 'task', 'open', 'default', 0, '', ?2, ?3)",
                    params![item_id, created, updated],
                )
                .with_context(|| format!("create placeholder item for {item_id}"))?;
        }

        Ok(())
    }

    /// Ensure that `item_id`, which the event refers to as a parent or link
    /// target, has a row for the foreign key.
    ///
    /// Until the item has events of its own, the row is a hidden reference
    /// placeholder: `is_deleted = 1` with no `deleted_at_us`. Every read
    /// path already hides deleted items and ignores deleted blockers, so a
    /// link to a mistyped ID shows no ghost item and blocks nothing. Its
    /// created/updated bounds cover the events that refer to it
    /// (`min_nonzero`, as in `touch`). The item's first own event
    /// (`ensure_item_exists`) makes the row visible with its own timestamps.
    fn ensure_ref_exists(&self, item_id: &str, ts: i64) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO items (
                     item_id, title, kind, state, urgency,
                     is_deleted, search_labels, created_at_us, updated_at_us
                 ) VALUES (?1, '', 'task', 'open', 'default', 1, '', ?2, ?2)
                 ON CONFLICT(item_id) DO UPDATE SET
                     created_at_us = CASE
                         WHEN excluded.created_at_us = 0 THEN created_at_us
                         WHEN created_at_us = 0 THEN excluded.created_at_us
                         ELSE MIN(created_at_us, excluded.created_at_us)
                     END,
                     updated_at_us = MAX(updated_at_us, excluded.updated_at_us)
                 WHERE NOT EXISTS(SELECT 1 FROM projected_events WHERE item_id = ?1)",
            )?
            .execute(params![item_id, ts])
            .with_context(|| format!("create reference placeholder {item_id}"))?;
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

/// The LWW key of one write to a field of an item: `(ts, agent, hash)`, the
/// order `field_clocks` keeps. An event's writes use the event's key. A
/// snapshot's writes use the keys that the snapshot carries per field.
#[derive(Clone, Copy)]
struct Key<'a> {
    item_id: &'a str,
    /// Ordering timestamp (`Event::order_ts`).
    ts: i64,
    agent: &'a str,
    hash: &'a str,
    /// Timestamp that set rows (labels, assignees, links) record.
    written_at: i64,
}

impl<'a> From<&'a Event> for Key<'a> {
    fn from(event: &'a Event) -> Self {
        Self {
            item_id: event.item_id.as_str(),
            ts: event.order_ts(),
            agent: &event.agent,
            hash: &event.event_hash,
            written_at: event.wall_ts_us,
        }
    }
}

/// The key of a snapshot's LWW register, or `None` when no event wrote it.
fn register_key<'a, T>(item_id: &'a str, register: &'a LwwSnapshot<T>) -> Option<Key<'a>> {
    if register.event_hash.is_empty() {
        return None;
    }
    let ts = clock_ts(register.wall_ts);
    Some(Key {
        item_id,
        ts,
        agent: &register.agent_id,
        hash: &register.event_hash,
        written_at: ts,
    })
}

/// A key for a snapshot's write that has no exact key in the payload: the
/// lowest key at `ts` that still names the snapshot, so any real event of
/// the same field at `ts` or later wins.
fn lower_key(snapshot: &Event, ts: u64) -> Key<'_> {
    let ts = clock_ts(ts);
    Key {
        item_id: snapshot.item_id.as_str(),
        ts,
        agent: "",
        hash: &snapshot.event_hash,
        written_at: ts,
    }
}

/// A CRDT clock timestamp (an `order_ts` as u64) as a projection timestamp.
fn clock_ts(ts: u64) -> i64 {
    i64::try_from(ts).unwrap_or(i64::MAX)
}

/// The members of an OR-Set with a live add tag, each with the newest
/// timestamp among its live tags. Blank members are skipped: the schema
/// rejects them.
fn live_members<T: Member>(set: &OrSet<T>) -> std::collections::BTreeMap<T, u64> {
    let mut members = std::collections::BTreeMap::new();
    for (member, tag) in &set.elements {
        if member.is_blank() || set.tombstone.contains(&(member.clone(), tag.clone())) {
            continue;
        }
        let newest = members.entry(member.clone()).or_insert(tag.itc);
        *newest = (*newest).max(tag.itc);
    }
    members
}

/// An OR-Set member the projection can hold.
trait Member: Clone + Ord + std::hash::Hash + Eq {
    /// `true` when the schema rejects the member (a blank label or agent).
    fn is_blank(&self) -> bool;
}

impl Member for String {
    fn is_blank(&self) -> bool {
        field_value::is_blank_member(self)
    }
}

impl Member for LinkKey {
    fn is_blank(&self) -> bool {
        field_value::is_blank_member(&self.target) || field_value::is_blank_member(&self.link_type)
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

/// Test-only fault injection: the event with the chosen hash fails after its
/// handler has written everything, the worst case of a handler that fails
/// part-way. Thread-local, so parallel tests do not see each other's faults.
#[cfg(test)]
pub(crate) mod fault {
    use std::cell::RefCell;

    thread_local! {
        static FAIL_HASH: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    /// Make the event with `hash` fail after its handler, or clear it.
    pub(crate) fn fail_after_handler(hash: Option<&str>) {
        FAIL_HASH.with(|h| *h.borrow_mut() = hash.map(str::to_string));
    }

    pub(super) fn should_fail(hash: &str) -> bool {
        FAIL_HASH.with(|h| h.borrow().as_deref() == Some(hash))
    }

    /// A test hook that runs on the current thread at one point of a
    /// projection write, to force an interleaving with another thread.
    pub(crate) type Hook = Box<dyn FnMut()>;

    thread_local! {
        static BEFORE_CURSOR_WRITE: RefCell<Option<Hook>> = const { RefCell::new(None) };
        static BEFORE_INSTALL: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    fn run(slot: &'static std::thread::LocalKey<RefCell<Option<Hook>>>) {
        let hook = slot.with(|h| h.borrow_mut().take());
        if let Some(mut hook) = hook {
            hook();
            slot.with(|h| *h.borrow_mut() = Some(hook));
        }
    }

    /// Run `hook` after event rows commit and before the cursor write.
    pub(crate) fn set_before_cursor_write(hook: Option<Hook>) {
        BEFORE_CURSOR_WRITE.with(|h| *h.borrow_mut() = hook);
    }

    /// Run `hook` after a rebuild stages and before it installs.
    pub(crate) fn set_before_install(hook: Option<Hook>) {
        BEFORE_INSTALL.with(|h| *h.borrow_mut() = hook);
    }

    #[inline]
    pub(crate) fn before_cursor_write() {
        run(&BEFORE_CURSOR_WRITE);
    }

    #[inline]
    pub(crate) fn before_install() {
        run(&BEFORE_INSTALL);
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
         DELETE FROM snapshot_sources;
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

    #[test]
    fn negative_timestamps_order_like_the_crdt() {
        // Negative timestamps clamp to 0 for ordering (Event::order_ts), so
        // the projection and WorkItemState pick the same winner (bn-3is9).
        // By raw i64, -1 would win; clamped, both tie and the hash decides.
        let title = |value: &str, hash: &str, ts: i64| {
            make_event(
                EventType::Update,
                "bn-001",
                EventData::Update(UpdateData {
                    field: "title".into(),
                    value: serde_json::json!(value),
                    extra: BTreeMap::new(),
                }),
                hash,
                ts,
            )
        };
        let events = [
            make_create("bn-001", "Created", "000", -10),
            title("raw winner", "a01", -1),
            title("clamped winner", "z01", -5),
        ];

        let conn = test_db();
        let projector = Projector::new(&conn);
        // apply_event overwrites, so merge one state per event: merge is
        // where LWW picks the winner.
        let mut state = crate::crdt::item_state::WorkItemState::new();
        for event in events.iter().rev() {
            projector.project_event(event).unwrap();
            let mut single = crate::crdt::item_state::WorkItemState::new();
            single.apply_event(event);
            state.merge(&single);
        }
        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(state.title.value, "clamped winner");
        assert_eq!(item.title, state.title.value);
        assert_eq!(item.created_at_us, 0);
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
    fn tied_redactions_record_the_same_row_in_any_order() {
        // Two redactions of one target from different items, with the same
        // agent, timestamp and reason. item_id breaks the tie (bn-36n3).
        let redact = |item: &str, hash: &str| {
            make_event(
                EventType::Redact,
                item,
                EventData::Redact(RedactData {
                    target_hash: "blake3:target".into(),
                    reason: "secret".into(),
                    extra: BTreeMap::new(),
                }),
                hash,
                100,
            )
        };
        let events = [redact("bn-a1", "r1"), redact("bn-b2", "r2")];
        let row = |order: [usize; 2]| {
            let conn = test_db();
            let projector = Projector::new(&conn);
            for i in order {
                projector.project_event(&events[i]).unwrap();
            }
            conn.query_row(
                "SELECT item_id FROM event_redactions WHERE target_event_hash = 'blake3:target'",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
        };
        assert_eq!(row([0, 1]), "bn-a1");
        assert_eq!(row([1, 0]), "bn-a1");
    }

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

    // -----------------------------------------------------------------------
    // Order independence and snapshots (bn-18fs)
    // -----------------------------------------------------------------------

    fn update_event(
        item: &str,
        field: &str,
        value: serde_json::Value,
        hash: &str,
        ts: i64,
    ) -> Event {
        make_event(
            EventType::Update,
            item,
            EventData::Update(UpdateData {
                field: field.into(),
                value,
                extra: BTreeMap::new(),
            }),
            hash,
            ts,
        )
    }

    fn with_agent(mut event: Event, agent: &str) -> Event {
        event.agent = agent.into();
        event
    }

    /// Everything the user sees of `item`, except the snapshot JSON,
    /// `updated_at_us` and comments, which a compacted log does not keep.
    fn visible(conn: &Connection, item: &str) -> Vec<String> {
        let mut out = Vec::new();
        for sql in [
            "SELECT title, description, kind, state, urgency, size, parent_id, is_deleted, \
             deleted_at_us, search_labels, created_at_us FROM items WHERE item_id = ?1",
            "SELECT label, created_at_us FROM item_labels WHERE item_id = ?1 ORDER BY 1",
            "SELECT agent, created_at_us FROM item_assignees WHERE item_id = ?1 ORDER BY 1",
            "SELECT depends_on_item_id, link_type, created_at_us FROM item_dependencies \
             WHERE item_id = ?1 ORDER BY 1, 2",
        ] {
            let mut stmt = conn.prepare(sql).unwrap();
            let columns = stmt.column_count();
            let rows = stmt
                .query_map([item], |row| {
                    (0..columns)
                        .map(|i| {
                            row.get::<_, rusqlite::types::Value>(i)
                                .map(|v| format!("{v:?}"))
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap();
            for row in rows {
                out.push(row.unwrap().join(" | "));
            }
        }
        out
    }

    fn project_all(events: &[&Event]) -> Connection {
        let conn = test_db();
        let projector = Projector::new(&conn);
        for event in events {
            projector.project_event(event).unwrap();
        }
        conn
    }

    /// Source events of one item that touch every field a snapshot keeps.
    fn snapshot_sources() -> Vec<Event> {
        let mut create = make_create("bn-001", "First title", "s01", 1000);
        create.agent = "alice".into();
        vec![
            make_create("bn-002", "Blocker", "s00", 900),
            create,
            update_event(
                "bn-001",
                "title",
                serde_json::json!("Second title"),
                "s02",
                1100,
            ),
            update_event(
                "bn-001",
                "labels",
                serde_json::json!({"action": "remove", "label": "auth"}),
                "s03",
                1200,
            ),
            update_event("bn-001", "parent", serde_json::json!("bn-002"), "s04", 1250),
            make_event(
                EventType::Assign,
                "bn-001",
                EventData::Assign(AssignData {
                    agent: "carol".into(),
                    action: AssignAction::Assign,
                    extra: BTreeMap::new(),
                }),
                "s05",
                1300,
            ),
            make_event(
                EventType::Link,
                "bn-001",
                EventData::Link(LinkData {
                    target: "bn-002".into(),
                    link_type: "blocks".into(),
                    extra: BTreeMap::new(),
                }),
                "s06",
                1400,
            ),
            make_event(
                EventType::Move,
                "bn-001",
                EventData::Move(MoveData {
                    state: State::Done,
                    reason: None,
                    extra: BTreeMap::new(),
                }),
                "s07",
                1500,
            ),
        ]
    }

    fn snapshot_of(events: &[Event], item: &str) -> Event {
        let own: Vec<Event> = events
            .iter()
            .filter(|e| e.item_id.as_str() == item)
            .cloned()
            .collect();
        crate::compact::compact_item(item, &own, "compactor", &std::collections::HashSet::new())
            .expect("snapshot")
    }

    #[test]
    fn compacted_log_projects_like_its_source_events() {
        // Old behaviour: the snapshot only set snapshot_json, so an item
        // whose events were compacted away lost every field.
        let sources = snapshot_sources();
        let snapshot = snapshot_of(&sources, "bn-001");

        let original = project_all(&sources.iter().collect::<Vec<_>>());
        let compacted = project_all(&[&sources[0], &snapshot]);
        assert_eq!(visible(&compacted, "bn-001"), visible(&original, "bn-001"));
        assert_eq!(
            visible(&compacted, "bn-001")[0],
            "Text(\"Second title\") | Text(\"A detailed description\") | Text(\"task\") | \
             Text(\"done\") | Text(\"default\") | Text(\"m\") | Text(\"bn-002\") | Integer(0) | \
             Null | Text(\"backend\") | Integer(1000)"
        );
    }

    #[test]
    fn snapshot_next_to_its_source_events_changes_nothing() {
        let sources = snapshot_sources();
        let snapshot = snapshot_of(&sources, "bn-001");
        let original = project_all(&sources.iter().collect::<Vec<_>>());

        let mut with_snapshot: Vec<&Event> = sources.iter().collect();
        with_snapshot.push(&snapshot);
        let after = project_all(&with_snapshot);
        with_snapshot.rotate_right(1);
        let before = project_all(&with_snapshot);

        assert_eq!(visible(&after, "bn-001"), visible(&original, "bn-001"));
        assert_eq!(visible(&before, "bn-001"), visible(&original, "bn-001"));
    }

    #[test]
    fn snapshot_fields_keep_their_own_keys() {
        // The snapshot sorts at 1501, but its title was written at 1100. A
        // concurrent title at 1300 that the snapshot never saw must win, in
        // either order. As one write at 1501 the snapshot would win.
        let sources = snapshot_sources();
        let snapshot = snapshot_of(&sources, "bn-001");
        assert_eq!(snapshot.wall_ts_us, 1501);
        let newer = with_agent(
            update_event(
                "bn-001",
                "title",
                serde_json::json!("Concurrent"),
                "c01",
                1300,
            ),
            "zed",
        );
        let older = with_agent(
            update_event("bn-001", "title", serde_json::json!("Stale"), "c02", 1050),
            "zed",
        );
        for events in [
            [&sources[0], &snapshot, &newer, &older],
            [&older, &newer, &snapshot, &sources[0]],
        ] {
            let conn = project_all(&events);
            let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
            assert_eq!(item.title, "Concurrent");
        }
    }

    #[test]
    fn snapshot_state_loses_to_any_move() {
        // The epoch/phase state has no clock. The snapshot claims it with
        // the lowest true key, so a move the snapshot never saw wins.
        let sources = snapshot_sources();
        let snapshot = snapshot_of(&sources, "bn-001");
        let reopen = make_event(
            EventType::Move,
            "bn-001",
            EventData::Move(MoveData {
                state: State::Open,
                reason: None,
                extra: BTreeMap::new(),
            }),
            "c03",
            1450,
        );
        // A move older than every source of the snapshot (clock skew)
        // wins too.
        let mut skewed = reopen.clone();
        skewed.wall_ts_us = 500;
        skewed.agent = "zed".into();
        writer::write_event(&mut skewed).unwrap();
        assert!(skewed.wall_ts_us < snapshot_sources()[1].wall_ts_us);
        for events in [
            [&snapshot, &reopen],
            [&reopen, &snapshot],
            [&snapshot, &skewed],
            [&skewed, &snapshot],
        ] {
            let conn = project_all(&events);
            let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
            assert_eq!(item.state, "open");
        }
        let conn = project_all(&[&snapshot]);
        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.state, "done");
    }

    #[test]
    fn redacted_snapshot_sources_stay_redacted_in_any_order() {
        let sources = snapshot_sources();
        let snapshot = snapshot_of(&sources, "bn-001");
        let redact = |target: &str, hash: &str| {
            make_event(
                EventType::Redact,
                "bn-001",
                EventData::Redact(RedactData {
                    target_hash: target.into(),
                    reason: "secret".into(),
                    extra: BTreeMap::new(),
                }),
                hash,
                2000,
            )
        };
        // The title's source event, and the snapshot itself.
        let redact_title = redact("blake3:s02", "r01");
        let redact_snapshot = redact(&snapshot.event_hash, "r02");
        for events in [
            [&snapshot, &redact_title, &redact_snapshot],
            [&redact_snapshot, &redact_title, &snapshot],
        ] {
            let conn = project_all(&events);
            let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
            assert_eq!(item.title, "[redacted]");
            let json: String = conn
                .query_row(
                    "SELECT snapshot_json FROM items WHERE item_id = 'bn-001'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(json, "[redacted]");
            // Labels the snapshot adds belong to the snapshot's key.
            assert!(query::get_labels(&conn, "bn-001").unwrap().is_empty());
        }
    }

    /// Every order of `items`.
    fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        let mut out = Vec::new();
        for i in 0..items.len() {
            let mut rest = items.to_vec();
            let first = rest.remove(i);
            for mut tail in permutations(&rest) {
                tail.insert(0, first.clone());
                out.push(tail);
            }
        }
        out
    }

    #[test]
    fn redacted_source_redacts_its_snapshot_in_every_order() {
        // bn-1npc: E adds a label, a snapshot S of the item holds it, and a
        // redaction R targets E. S claims the label with a lower-bound key
        // that names S, not E. With E gone from the log (a log that holds
        // the snapshot only), S owned the label and R could not reach it.
        // S's JSON also kept E's content.
        let create = make_create("bn-001", "Item", "p01", 1000);
        let secret = update_event(
            "bn-001",
            "labels",
            serde_json::json!({"action": "add", "label": "secret"}),
            "p02",
            1100,
        );
        let snapshot = snapshot_of(&[create.clone(), secret.clone()], "bn-001");
        assert!(snapshot.parents.contains(&secret.event_hash));
        let redact = make_event(
            EventType::Redact,
            "bn-001",
            EventData::Redact(RedactData {
                target_hash: secret.event_hash.clone(),
                reason: "secret".into(),
                extra: BTreeMap::new(),
            }),
            "p03",
            2000,
        );

        let observe = |events: &[&Event]| {
            let conn = project_all(events);
            let json: Option<String> = conn
                .query_row(
                    "SELECT snapshot_json FROM items WHERE item_id = 'bn-001'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            (visible(&conn, "bn-001"), json)
        };

        for log in [
            vec![&create, &secret, &snapshot, &redact],
            vec![&create, &snapshot, &redact],
            vec![&snapshot, &redact],
        ] {
            let orders = permutations(&log);
            let first = observe(&orders[0]);
            assert_eq!(first.1.as_deref(), Some("[redacted]"), "log {}", log.len());
            let labels: Vec<&String> = first
                .0
                .iter()
                .filter(|row| row.contains("secret"))
                .collect();
            assert!(labels.is_empty(), "redacted label shows: {labels:?}");
            for order in &orders[1..] {
                assert_eq!(observe(order), first, "log {}", log.len());
            }
        }

        // The same snapshot without the redaction keeps the label.
        let conn = project_all(&[&snapshot]);
        let labels: Vec<String> = query::get_labels(&conn, "bn-001")
            .unwrap()
            .into_iter()
            .map(|l| l.label)
            .collect();
        assert!(labels.contains(&"secret".to_string()), "{labels:?}");
    }

    #[test]
    fn delete_and_its_snapshot_agree_on_deleted_at() {
        // Negative wall clocks order as 0. The delete wrote its raw -5 and
        // the snapshot, whose key carries the ordering timestamp, wrote 0.
        // The two claims tie, so the first one decided.
        let create = make_create("bn-001", "Item", "d01", -10);
        let delete = make_event(
            EventType::Delete,
            "bn-001",
            EventData::Delete(DeleteData {
                reason: None,
                extra: BTreeMap::new(),
            }),
            "d02",
            -5,
        );
        let snapshot = snapshot_of(&[create.clone(), delete.clone()], "bn-001");
        let deleted_at = |events: &[&Event]| {
            let conn = project_all(events);
            query::get_item(&conn, "bn-001", true)
                .unwrap()
                .unwrap()
                .deleted_at_us
        };
        assert_eq!(deleted_at(&[&create, &delete, &snapshot]), Some(0));
        assert_eq!(deleted_at(&[&snapshot, &create, &delete]), Some(0));
    }

    #[test]
    fn failed_event_in_a_batch_leaves_nothing_behind() {
        // An event that fails after its handler touched the item and claimed
        // a field. project_event rolled that back, project_batch kept it:
        // rebuild and live projection disagreed on updated_at_us.
        let create = make_create("bn-001", "Item", "f01", 1000);
        let doomed = update_event("bn-001", "title", serde_json::json!("Doomed"), "f02", 2000);
        fault::fail_after_handler(Some(&doomed.event_hash));

        let single = test_db();
        let projector = Projector::new(&single);
        projector.project_event(&create).unwrap();
        assert!(projector.project_event(&doomed).is_err());

        let batch = test_db();
        let stats = Projector::new(&batch)
            .project_batch(&[create.clone(), doomed.clone()])
            .unwrap();
        assert_eq!(stats.errors, 1);
        fault::fail_after_handler(None);

        for (name, conn) in [("single", &single), ("batch", &batch)] {
            let item = query::get_item(conn, "bn-001", false).unwrap().unwrap();
            assert_eq!(
                (item.title.as_str(), item.updated_at_us),
                ("Item", 1000),
                "{name}"
            );
            let claims: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM field_clocks WHERE event_hash = 'blake3:f02'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(claims, 0, "{name}");
        }
    }

    #[test]
    fn blank_members_are_no_write() {
        // The schema rejects blank labels, assignees and link types. A
        // handler skips them before its first claim instead of failing
        // part-way (bn-39tj), and WorkItemState skips them too.
        let create = make_create("bn-001", "Item", "b01", 1000);
        let events = [
            create.clone(),
            make_create("bn-002", "Other", "b00", 900),
            make_event(
                EventType::Assign,
                "bn-001",
                EventData::Assign(AssignData {
                    agent: "   ".into(),
                    action: AssignAction::Assign,
                    extra: BTreeMap::new(),
                }),
                "b02",
                2000,
            ),
            update_event(
                "bn-001",
                "labels",
                serde_json::json!({"action": "add", "label": " "}),
                "b03",
                2001,
            ),
            update_event(
                "bn-001",
                "labels",
                serde_json::json!(["", "kept"]),
                "b04",
                2002,
            ),
            make_event(
                EventType::Link,
                "bn-001",
                EventData::Link(LinkData {
                    target: "bn-002".into(),
                    link_type: "  ".into(),
                    extra: BTreeMap::new(),
                }),
                "b05",
                2003,
            ),
        ];
        let conn = test_db();
        let stats = Projector::new(&conn).project_batch(&events).unwrap();
        assert_eq!(stats.errors, 0);
        let labels: Vec<String> = query::get_labels(&conn, "bn-001")
            .unwrap()
            .into_iter()
            .map(|l| l.label)
            .collect();
        assert_eq!(labels, ["kept"]);
        assert!(query::get_assignees(&conn, "bn-001").unwrap().is_empty());
        assert!(query::get_dependencies(&conn, "bn-001").unwrap().is_empty());

        let own: Vec<&Event> = events
            .iter()
            .filter(|e| e.item_id.as_str() == "bn-001")
            .collect();
        let state = crate::crdt::item_state::WorkItemState::from_events(own);
        assert_eq!(state.label_names().len(), 1);
        assert!(state.assignee_names().is_empty());
        assert!(state.link_keys().is_empty());
    }

    #[test]
    fn members_with_a_nul_follow_sqlite() {
        // bn-1npc: SQLite's length() stops at the first NUL, so the CHECK
        // rejects "\0x" and " \0" but accepts "x\0". The first two are
        // blank (no write) and the event projects without error. A member
        // with a NUL inside must also be found by a later whole-set reset:
        // substr() stopped at the NUL and missed it.
        let assign = |agent: &str, hash: &str, ts: i64| {
            make_event(
                EventType::Assign,
                "bn-001",
                EventData::Assign(AssignData {
                    agent: agent.into(),
                    action: AssignAction::Assign,
                    extra: BTreeMap::new(),
                }),
                hash,
                ts,
            )
        };
        let label = |label: &str, hash: &str, ts: i64| {
            update_event(
                "bn-001",
                "labels",
                serde_json::json!({"action": "add", "label": label}),
                hash,
                ts,
            )
        };
        let mut events = vec![
            make_create("bn-001", "Item", "n01", 1000),
            label("\u{0}x", "n02", 1100),
            label(" \u{0}", "n03", 1101),
            assign("\u{0}", "n04", 1102),
            label("x\u{0}", "n05", 1103),
            assign("a\u{0}b", "n06", 1104),
        ];
        let conn = test_db();
        let stats = Projector::new(&conn).project_batch(&events).unwrap();
        assert_eq!(stats.errors, 0);
        let labels = |conn: &Connection| -> Vec<String> {
            query::get_labels(conn, "bn-001")
                .unwrap()
                .into_iter()
                .map(|l| l.label)
                .collect()
        };
        assert_eq!(labels(&conn), ["auth", "backend", "x\u{0}"]);
        let assignees: Vec<String> = query::get_assignees(&conn, "bn-001")
            .unwrap()
            .into_iter()
            .map(|a| a.agent)
            .collect();
        assert_eq!(assignees, ["a\u{0}b"]);

        // A legacy whole-set replacement removes "x\0" too.
        events.push(update_event(
            "bn-001",
            "labels",
            serde_json::json!(["auth"]),
            "n07",
            1200,
        ));
        let conn = test_db();
        let stats = Projector::new(&conn).project_batch(&events).unwrap();
        assert_eq!(stats.errors, 0);
        assert_eq!(labels(&conn), ["auth"]);

        let state = crate::crdt::item_state::WorkItemState::from_events(&events);
        let mut names: Vec<&String> = state.label_names().into_iter().collect();
        names.sort();
        assert_eq!(names, ["auth"]);
        assert_eq!(state.assignee_names().len(), 1);
    }

    #[test]
    fn parent_and_link_targets_without_a_row_get_a_placeholder() {
        // The parent and link target bn-002 has no create yet. Both writes
        // hit the foreign key and failed; in the other order they worked.
        let parent = update_event("bn-001", "parent", serde_json::json!("bn-002"), "p01", 2000);
        let link = make_event(
            EventType::Link,
            "bn-001",
            EventData::Link(LinkData {
                target: "bn-002".into(),
                link_type: "blocks".into(),
                extra: BTreeMap::new(),
            }),
            "p02",
            2100,
        );
        let own = make_create("bn-001", "Child", "p00", 1000);
        let target = make_create("bn-002", "Parent", "p03", 3000);

        // Until bn-002 has events of its own, its row is hidden: a
        // mistyped link target must not show a ghost item.
        let pending = project_all(&[&own, &parent, &link]);
        assert!(
            query::get_item(&pending, "bn-002", false)
                .unwrap()
                .is_none()
        );
        let hidden = query::get_item(&pending, "bn-002", true).unwrap().unwrap();
        assert_eq!(hidden.deleted_at_us, None);
        let visible_items: i64 = pending
            .query_row("SELECT COUNT(*) FROM items WHERE is_deleted = 0", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(visible_items, 1);
        let first = project_all(&[&own, &parent, &link, &target]);
        let last = project_all(&[&target, &own, &link, &parent]);
        for item in ["bn-001", "bn-002"] {
            assert_eq!(visible(&first, item), visible(&last, item));
        }
        let child = query::get_item(&first, "bn-001", false).unwrap().unwrap();
        assert_eq!(child.parent_id.as_deref(), Some("bn-002"));
        // The placeholder's bounds are replaced by the item's own events.
        let parent_item = query::get_item(&first, "bn-002", false).unwrap().unwrap();
        assert_eq!(parent_item.created_at_us, 3000);
        assert_eq!(parent_item.updated_at_us, 3000);
    }

    #[test]
    fn snapshot_links_keep_raw_types_and_older_formats_stay_json() {
        // The current format keeps each link's raw type and the compact
        // summary. An older format (here bn-t37g's 2) can hold values this
        // bn never writes, so it is JSON only.
        let mut sources = snapshot_sources();
        let EventData::Link(ref mut link) = sources[6].data else {
            panic!("link event");
        };
        link.link_type = "blocked_by".into();
        sources.push(make_event(
            EventType::Compact,
            "bn-001",
            EventData::Compact(CompactData {
                summary: "Short".into(),
                extra: BTreeMap::new(),
            }),
            "s08",
            1600,
        ));
        let snapshot = snapshot_of(&sources, "bn-001");
        let links = |conn: &Connection| -> Vec<String> {
            query::get_dependencies(conn, "bn-001")
                .unwrap()
                .into_iter()
                .map(|d| format!("{} {}", d.depends_on_item_id, d.link_type))
                .collect()
        };

        let conn = project_all(&[&sources[0], &snapshot]);
        assert_eq!(links(&conn), ["bn-002 blocked_by"]);
        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.compact_summary.as_deref(), Some("Short"));
        assert_eq!(item.title, "Second title");

        let mut payload = crate::compact::extract_snapshot_payload(&snapshot).unwrap();
        payload.format = 2;
        let mut older = snapshot.clone();
        older.data = EventData::Snapshot(SnapshotData {
            state: serde_json::to_value(&payload).unwrap(),
            extra: BTreeMap::new(),
        });
        writer::write_event(&mut older).unwrap();
        let conn = project_all(&[&sources[0], &older]);
        assert!(links(&conn).is_empty());
        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.compact_summary, None);
        assert_eq!(item.title, "");
        // Its own time only: the payload's bounds are not read either.
        assert_eq!(item.created_at_us, older.wall_ts_us);
        let json: Option<String> = conn
            .query_row(
                "SELECT snapshot_json FROM items WHERE item_id = 'bn-001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(json.is_some_and(|j| j.contains("\"_format\":2")));
    }

    #[test]
    fn reference_placeholder_bounds_follow_min_nonzero() {
        // A reference at time 0 is "unknown" for created_at, as in touch.
        let link = |item: &str, hash: &str, ts: i64| {
            make_event(
                EventType::Link,
                item,
                EventData::Link(LinkData {
                    target: "bn-009".into(),
                    link_type: "blocks".into(),
                    extra: BTreeMap::new(),
                }),
                hash,
                ts,
            )
        };
        let at_zero = link("bn-001", "z01", 0);
        let later = link("bn-002", "z02", 500);
        for events in [[&at_zero, &later], [&later, &at_zero]] {
            let conn = project_all(&events);
            let target = query::get_item(&conn, "bn-009", true).unwrap().unwrap();
            assert_eq!((target.created_at_us, target.updated_at_us), (500, 500));
        }
    }

    #[test]
    fn malformed_update_values_are_no_write() {
        // The rule of `model::field_value`, shared with WorkItemState. The
        // raw values failed the schema's CHECK or foreign key constraints.
        let conn = test_db();
        let projector = Projector::new(&conn);
        projector
            .project_event(&make_create("bn-001", "Item", "o00", 1000))
            .unwrap();
        let project = |field: &str, value: serde_json::Value, n: i64| {
            projector
                .project_event(&update_event(
                    "bn-001",
                    field,
                    value.clone(),
                    &format!("o{n:02}"),
                    2000 + n,
                ))
                .unwrap_or_else(|e| panic!("{field} = {value}: {e:#}"));
        };
        for (n, (field, value)) in [
            ("title", serde_json::json!(5)),
            ("kind", serde_json::json!("bogus")),
            ("kind", serde_json::Value::Null),
            ("urgency", serde_json::json!("")),
            ("size", serde_json::json!("bogus")),
            ("parent", serde_json::json!("no id")),
            ("description", serde_json::json!(7)),
        ]
        .into_iter()
        .enumerate()
        {
            project(field, value, i64::try_from(n).unwrap() + 1);
        }
        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.title, "Item");
        assert_eq!(item.kind, "task");
        assert_eq!(item.urgency, "default");
        assert_eq!(item.size.as_deref(), Some("m"));
        assert_eq!(item.description.as_deref(), Some("A detailed description"));

        // null, and "" for the parent and description, clear.
        project("size", serde_json::Value::Null, 20);
        project("parent", serde_json::json!(""), 21);
        project("description", serde_json::json!(""), 22);
        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.size, None);
        assert_eq!(item.parent_id, None);
        assert_eq!(item.description, None);
    }
}
