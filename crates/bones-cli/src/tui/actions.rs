//! TUI action helpers — write bones events without stdout rendering.
//!
//! These helpers allow the TUI to perform state transitions (do, done) and
//! item creation directly using the bones-core API, bypassing the CLI
//! rendering layer that would corrupt the terminal screen.

use anyhow::{Context, Result};
use bones_core::db::{project, query};
use bones_core::event::Event;
use bones_core::event::data::{CreateData, EventData, MoveData};
use bones_core::event::types::EventType;
use bones_core::event::writer;
use bones_core::model::item::{Kind, State, Urgency};
use bones_core::model::item_id::{ItemId, generate_item_id};
use bones_core::shard::ShardManager;
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// Transition an item to "doing" state.
///
/// Opens the projection DB, validates the transition, writes the event to the
/// shard, and projects the new state into SQLite.
pub fn do_item(project_root: &Path, db_path: &Path, agent: &str, item_id: &str) -> Result<()> {
    let conn = Connection::open(db_path).context("open projection db")?;
    let bones_dir = project_root.join(".bones");
    let shard_mgr = ShardManager::new(&bones_dir);

    let current_item = query::get_item(&conn, item_id, false)
        .context("look up item")?
        .ok_or_else(|| anyhow::anyhow!("item '{}' not found", item_id))?;

    let current_state: State = current_item.state.parse().map_err(|_| {
        anyhow::anyhow!(
            "item '{}' has invalid state '{}'",
            item_id,
            current_item.state
        )
    })?;

    let target_state = State::Doing;
    current_state
        .can_transition_to(target_state)
        .map_err(|e| anyhow::anyhow!("cannot transition '{}': {}", item_id, e.reason))?;

    let ts = shard_mgr.next_timestamp().context("get timestamp")?;

    let mut event = Event {
        wall_ts_us: ts,
        agent: agent.to_string(),
        itc: "itc:AQ".to_string(),
        parents: vec![],
        event_type: EventType::Move,
        item_id: ItemId::new_unchecked(item_id),
        data: EventData::Move(MoveData {
            state: target_state,
            reason: None,
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };

    let line = writer::write_event(&mut event).context("serialize event")?;
    shard_mgr
        .append(&line, false, Duration::from_secs(5))
        .context("append to shard")?;

    let projector = project::Projector::new(&conn);
    if let Err(e) = projector.project_event(&event) {
        tracing::warn!("TUI do projection failed (will recover on rebuild): {e}");
    }

    Ok(())
}

/// Transition an item to "done" state.
///
/// Validates the transition (open→done and doing→done are both allowed),
/// writes the event, and projects it.
pub fn done_item(project_root: &Path, db_path: &Path, agent: &str, item_id: &str) -> Result<()> {
    let conn = Connection::open(db_path).context("open projection db")?;
    let bones_dir = project_root.join(".bones");
    let shard_mgr = ShardManager::new(&bones_dir);

    let current_item = query::get_item(&conn, item_id, false)
        .context("look up item")?
        .ok_or_else(|| anyhow::anyhow!("item '{}' not found", item_id))?;

    let current_state: State = current_item.state.parse().map_err(|_| {
        anyhow::anyhow!(
            "item '{}' has invalid state '{}'",
            item_id,
            current_item.state
        )
    })?;

    let target_state = State::Done;
    current_state
        .can_transition_to(target_state)
        .map_err(|e| anyhow::anyhow!("cannot transition '{}': {}", item_id, e.reason))?;

    let ts = shard_mgr.next_timestamp().context("get timestamp")?;

    let mut event = Event {
        wall_ts_us: ts,
        agent: agent.to_string(),
        itc: "itc:AQ".to_string(),
        parents: vec![],
        event_type: EventType::Move,
        item_id: ItemId::new_unchecked(item_id),
        data: EventData::Move(MoveData {
            state: target_state,
            reason: None,
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };

    let line = writer::write_event(&mut event).context("serialize event")?;
    shard_mgr
        .append(&line, false, Duration::from_secs(5))
        .context("append to shard")?;

    let projector = project::Projector::new(&conn);
    if let Err(e) = projector.project_event(&event) {
        tracing::warn!("TUI done projection failed (will recover on rebuild): {e}");
    }

    Ok(())
}

/// Create a new task item with the given title.
///
/// Generates a unique item ID, writes the create event, and projects it.
/// Returns the generated item ID string.
pub fn create_task(
    project_root: &Path,
    db_path: &Path,
    agent: &str,
    title: &str,
) -> Result<String> {
    let conn = Connection::open(db_path).context("open projection db")?;
    let bones_dir = project_root.join(".bones");
    let shard_mgr = ShardManager::new(&bones_dir);

    // Count existing items so the ID generator picks a collision-free hash.
    let item_count: usize = conn
        .query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0) as usize;

    let item_id = generate_item_id(title, item_count, |candidate| {
        conn.query_row(
            "SELECT 1 FROM items WHERE item_id = ?1",
            [candidate],
            |_| Ok(()),
        )
        .is_ok()
    });

    let ts = shard_mgr.next_timestamp().context("get timestamp")?;

    let mut event = Event {
        wall_ts_us: ts,
        agent: agent.to_string(),
        itc: "itc:AQ".to_string(),
        parents: vec![],
        event_type: EventType::Create,
        item_id: item_id.clone(),
        data: EventData::Create(CreateData {
            title: title.to_string(),
            kind: Kind::Task,
            size: None,
            urgency: Urgency::Default,
            labels: vec![],
            parent: None,
            causation: None,
            description: None,
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };

    let line = writer::write_event(&mut event).context("serialize event")?;
    shard_mgr
        .append(&line, false, Duration::from_secs(5))
        .context("append to shard")?;

    let projector = project::Projector::new(&conn);
    if let Err(e) = projector.project_event(&event) {
        tracing::warn!("TUI create projection failed (will recover on rebuild): {e}");
    }

    Ok(item_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db::migrations;
    use bones_core::db::project::{Projector, ensure_tracking_table};
    use bones_core::event::data::EventData;
    use bones_core::event::types::EventType;
    use tempfile::tempdir;

    fn setup_project() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().expect("tempdir");
        let project_root = dir.path().to_path_buf();
        let bones_dir = project_root.join(".bones");
        std::fs::create_dir_all(&bones_dir).unwrap();
        let events_dir = bones_dir.join("events");
        std::fs::create_dir_all(&events_dir).unwrap();

        let db_path = bones_dir.join("bones.db");
        let mut conn = Connection::open(&db_path).unwrap();
        migrations::migrate(&mut conn).unwrap();
        ensure_tracking_table(&conn).unwrap();

        // Init shard manager
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        (dir, project_root)
    }

    fn insert_item(conn: &Connection, project_root: &Path, id: &str, title: &str) {
        let bones_dir = project_root.join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        let ts = shard_mgr.next_timestamp().unwrap();
        let mut event = Event {
            wall_ts_us: ts,
            agent: "test".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(id),
            data: EventData::Create(CreateData {
                title: title.into(),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        let line = writer::write_event(&mut event).unwrap();
        shard_mgr
            .append(&line, false, Duration::from_secs(5))
            .unwrap();
        let projector = Projector::new(conn);
        projector.project_event(&event).unwrap();
    }

    #[test]
    fn do_item_transitions_to_doing() {
        let (_dir, project_root) = setup_project();
        let db_path = project_root.join(".bones/bones.db");
        let conn = Connection::open(&db_path).unwrap();

        insert_item(&conn, &project_root, "bn-001", "Test Task");

        do_item(&project_root, &db_path, "test-agent", "bn-001").unwrap();

        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.state, "doing");
    }

    #[test]
    fn done_item_transitions_to_done() {
        let (_dir, project_root) = setup_project();
        let db_path = project_root.join(".bones/bones.db");
        let conn = Connection::open(&db_path).unwrap();

        insert_item(&conn, &project_root, "bn-001", "Test Task");

        done_item(&project_root, &db_path, "test-agent", "bn-001").unwrap();

        let item = query::get_item(&conn, "bn-001", false).unwrap().unwrap();
        assert_eq!(item.state, "done");
    }

    #[test]
    fn do_item_rejects_invalid_transition() {
        let (_dir, project_root) = setup_project();
        let db_path = project_root.join(".bones/bones.db");
        let conn = Connection::open(&db_path).unwrap();

        insert_item(&conn, &project_root, "bn-001", "Test Task");
        done_item(&project_root, &db_path, "test-agent", "bn-001").unwrap();

        // done → doing is not allowed
        let result = do_item(&project_root, &db_path, "test-agent", "bn-001");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("cannot transition")
        );
    }

    #[test]
    fn create_task_generates_item() {
        let (_dir, project_root) = setup_project();
        let db_path = project_root.join(".bones/bones.db");
        let conn = Connection::open(&db_path).unwrap();

        let id = create_task(&project_root, &db_path, "test-agent", "New Feature Task").unwrap();

        assert!(id.starts_with("bn-"));
        let item = query::get_item(&conn, &id, false).unwrap().unwrap();
        assert_eq!(item.title, "New Feature Task");
        assert_eq!(item.state, "open");
    }
}
