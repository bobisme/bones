use std::path::Path;

use anyhow::{Context, Result};

use crate::output::{OutputMode, pretty_kv, pretty_section};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemanticIndexState {
    tables_ready: bool,
    embeddings: usize,
    deferred: bool,
}

fn ensure_semantic_index_state(conn: &rusqlite::Connection) -> Result<SemanticIndexState> {
    bones_search::semantic::ensure_semantic_index_schema(conn)
        .context("initialize semantic index schema")?;

    let embeddings: i64 = conn
        .query_row("SELECT COUNT(*) FROM item_embeddings", [], |row| row.get(0))
        .context("count semantic embeddings after rebuild")?;

    let projection_cursor = bones_core::db::query::get_projection_cursor(conn)
        .context("read projection cursor after rebuild")?;
    let semantic_cursor: (i64, Option<String>) = conn
        .query_row(
            "SELECT last_event_offset, last_event_hash FROM semantic_meta WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .context("read semantic cursor after rebuild")?;

    Ok(SemanticIndexState {
        tables_ready: true,
        embeddings: usize::try_from(embeddings).unwrap_or(0),
        deferred: semantic_cursor != projection_cursor,
    })
}

/// Run `bn admin rebuild` and refresh both projection DB and binary cache.
///
/// # Errors
///
/// Returns an error if projection rebuild or cache rebuild fails.
pub fn run_rebuild(project_root: &Path, _incremental: bool, output: OutputMode) -> Result<()> {
    let bones_dir = project_root.join(".bones");
    let events_dir = bones_dir.join("events");
    let db_path = bones_dir.join("bones.db");
    let cache_path = bones_dir.join("cache/events.bin");

    let (db_report, conn) = if _incremental {
        let apply = bones_core::db::incremental::incremental_apply(&events_dir, &db_path, false)?;
        let conn = bones_core::db::open_projection(&db_path)?;
        let item_count: usize =
            conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))?;
        (
            bones_core::db::rebuild::RebuildReport {
                event_count: apply.events_applied,
                item_count,
                elapsed: apply.elapsed,
                shard_count: apply.shards_scanned,
                fts5_rebuilt: false,
            },
            conn,
        )
    } else {
        let report = bones_core::db::rebuild::rebuild(&events_dir, &db_path)?;
        let conn = bones_core::db::open_projection(&db_path)?;
        (report, conn)
    };
    let cache_stats = bones_core::cache::rebuild_cache(&events_dir, &cache_path)?;
    let semantic_state = ensure_semantic_index_state(&conn)?;

    match output {
        OutputMode::Json => {
            let val = serde_json::json!({
                "projection_events": db_report.event_count,
                "projection_items": db_report.item_count,
                "shards": db_report.shard_count,
                "cache_events": cache_stats.total_events,
                "cache_bytes": cache_stats.file_size_bytes,
                "semantic_tables_ready": semantic_state.tables_ready,
                "semantic_embeddings": semantic_state.embeddings,
                "semantic_deferred": semantic_state.deferred,
            });
            println!("{}", serde_json::to_string_pretty(&val)?);
        }
        OutputMode::Text => {
            println!(
                "rebuild projection_events={} items={} shards={} cache_events={} cache_bytes={} semantic_tables_ready={} semantic_embeddings={} semantic_deferred={}",
                db_report.event_count,
                db_report.item_count,
                db_report.shard_count,
                cache_stats.total_events,
                cache_stats.file_size_bytes,
                semantic_state.tables_ready,
                semantic_state.embeddings,
                semantic_state.deferred,
            );
        }
        OutputMode::Pretty => {
            let stdout = std::io::stdout();
            let mut w = stdout.lock();
            pretty_section(&mut w, "Rebuild Complete")?;
            pretty_kv(
                &mut w,
                "Projection events",
                db_report.event_count.to_string(),
            )?;
            pretty_kv(&mut w, "Items", db_report.item_count.to_string())?;
            pretty_kv(&mut w, "Shards", db_report.shard_count.to_string())?;
            pretty_kv(&mut w, "Cache events", cache_stats.total_events.to_string())?;
            pretty_kv(
                &mut w,
                "Cache bytes",
                cache_stats.file_size_bytes.to_string(),
            )?;
            pretty_kv(
                &mut w,
                "Semantic tables",
                if semantic_state.tables_ready {
                    "ready".to_string()
                } else {
                    "missing".to_string()
                },
            )?;
            pretty_kv(
                &mut w,
                "Semantic embeddings",
                semantic_state.embeddings.to_string(),
            )?;
            pretty_kv(
                &mut w,
                "Semantic indexing",
                if semantic_state.deferred {
                    "deferred (run semantic search to populate embeddings)".to_string()
                } else {
                    "up-to-date".to_string()
                },
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::event::data::CreateData;
    use bones_core::event::types::EventType;
    use bones_core::event::{Event, EventData, writer};
    use bones_core::model::item::{Kind, Size, Urgency};
    use bones_core::model::item_id::ItemId;
    use bones_core::shard::ShardManager;
    use rusqlite::Connection;
    use std::collections::BTreeMap;

    fn setup_project_with_single_event() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let bones_dir = dir.path().join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.ensure_dirs().expect("ensure bones dirs");
        shard_mgr.init().expect("init shard");

        let mut create = Event {
            wall_ts_us: 1000,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-001"),
            data: EventData::Create(CreateData {
                title: "Semantic rebuild coverage".into(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: vec!["semantic".into()],
                parent: None,
                causation: None,
                description: Some("Verify rebuild semantic schema behavior".into()),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        writer::write_event(&mut create).expect("compute event hash");
        let line = writer::write_line(&create).expect("serialize event line");

        let (year, month) = shard_mgr
            .active_shard()
            .expect("active shard")
            .expect("exists");
        shard_mgr
            .append_raw(year, month, &line)
            .expect("append create event");

        dir
    }

    #[test]
    fn rebuild_materializes_semantic_tables() {
        let dir = setup_project_with_single_event();
        run_rebuild(dir.path(), false, OutputMode::Json).expect("rebuild should succeed");

        let db_path = dir.path().join(".bones").join("bones.db");
        let conn = Connection::open(db_path).expect("open rebuilt db");

        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('item_embeddings', 'semantic_meta')",
                [],
                |row| row.get(0),
            )
            .expect("count semantic tables");
        assert_eq!(table_count, 2, "expected semantic tables to exist");
    }

    #[test]
    fn rebuild_reports_semantic_index_as_deferred_after_event_replay() {
        let dir = setup_project_with_single_event();
        run_rebuild(dir.path(), false, OutputMode::Json).expect("rebuild should succeed");

        let db_path = dir.path().join(".bones").join("bones.db");
        let conn = Connection::open(db_path).expect("open rebuilt db");
        let state = ensure_semantic_index_state(&conn).expect("semantic state");

        assert!(state.tables_ready);
        assert_eq!(state.embeddings, 0);
        assert!(
            state.deferred,
            "fresh rebuild should defer embedding generation until semantic queries run"
        );
    }
}
