//! `bn close` — convenience alias for transitioning an item to "done" state.
//!
//! This is semantically equivalent to `bn done <id>`. It validates the
//! current state allows a transition to done (open→done, doing→done),
//! emits an `item.move` event, and outputs the result.
//!
//! See [`crate::cmd::done`] for the full implementation.

use crate::cmd::done::{DoneArgs, run_done};
use crate::output::OutputMode;
use clap::Args;
use std::path::Path;

/// Arguments for `bn close`.
#[derive(Args, Debug)]
pub struct CloseArgs {
    /// Item ID to close (supports partial IDs).
    pub id: String,

    /// Additional item IDs to close in the same command.
    #[arg(value_name = "ID")]
    pub ids: Vec<String>,

    /// Optional reason for closing this item.
    #[arg(long)]
    pub reason: Option<String>,
}

/// Run the `bn close` command.
///
/// Delegates to [`run_done`] — the two commands are semantically identical.
pub fn run_close(
    args: &CloseArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let done_args = DoneArgs {
        id: args.id.clone(),
        ids: args.ids.clone(),
        reason: args.reason.clone(),
    };
    run_done(&done_args, agent_flag, output, project_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db;
    use bones_core::db::project;
    use bones_core::db::query;
    use bones_core::event::Event;
    use bones_core::event::data::{CreateData, EventData, MoveData};
    use bones_core::event::types::EventType;
    use bones_core::event::writer;
    use bones_core::model::item::{Kind, State, Urgency};
    use bones_core::model::item_id::ItemId;
    use bones_core::shard::ShardManager;
    use clap::Parser;
    use std::collections::BTreeMap;
    use std::time::Duration;
    use tempfile::TempDir;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: CloseArgs,
    }

    /// Create a bones project with one item at the given state.
    fn setup_project(state: &str) -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();

        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let db_path = bones_dir.join("bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let _ = project::ensure_tracking_table(&conn);
        let projector = project::Projector::new(&conn);

        let item_id = "bn-close1";
        let ts = shard_mgr.next_timestamp().unwrap();

        let mut create_event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: "Close test".to_string(),
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
        let line = writer::write_event(&mut create_event).unwrap();
        shard_mgr
            .append(&line, false, Duration::from_secs(5))
            .unwrap();
        projector.project_event(&create_event).unwrap();

        if state != "open" {
            let steps = match state {
                "doing" => vec![State::Doing],
                "done" => vec![State::Doing, State::Done],
                _ => vec![],
            };
            for step_state in steps {
                let ts2 = shard_mgr.next_timestamp().unwrap();
                let mut move_event = Event {
                    wall_ts_us: ts2,
                    agent: "test-agent".to_string(),
                    itc: "itc:AQ".to_string(),
                    parents: vec![],
                    event_type: EventType::Move,
                    item_id: ItemId::new_unchecked(item_id),
                    data: EventData::Move(MoveData {
                        state: step_state,
                        reason: None,
                        extra: BTreeMap::new(),
                    }),
                    event_hash: String::new(),
                };
                let line = writer::write_event(&mut move_event).unwrap();
                shard_mgr
                    .append(&line, false, Duration::from_secs(5))
                    .unwrap();
                projector.project_event(&move_event).unwrap();
            }
        }

        (dir, item_id.to_string())
    }

    #[test]
    fn close_args_parse_id() {
        let w = Wrapper::parse_from(["test", "item-5"]);
        assert_eq!(w.args.id, "item-5");
        assert!(w.args.reason.is_none());
    }

    #[test]
    fn close_args_parse_with_reason() {
        let w = Wrapper::parse_from(["test", "item-5", "--reason", "Shipped"]);
        assert_eq!(w.args.reason.as_deref(), Some("Shipped"));
    }

    #[test]
    fn close_from_open() {
        let (dir, item_id) = setup_project("open");
        let args = CloseArgs {
            id: item_id.clone(),
            ids: vec![],
            reason: None,
        };
        let result = run_close(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "close failed: {:?}", result.err());

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.state, "done");
    }

    #[test]
    fn close_from_doing() {
        let (dir, item_id) = setup_project("doing");
        let args = CloseArgs {
            id: item_id.clone(),
            ids: vec![],
            reason: Some("All done".to_string()),
        };
        let result = run_close(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(
            result.is_ok(),
            "close from doing failed: {:?}",
            result.err()
        );

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.state, "done");
    }

    #[test]
    fn close_rejects_already_done() {
        let (dir, item_id) = setup_project("done");
        let args = CloseArgs {
            id: item_id,
            ids: vec![],
            reason: None,
        };
        let result = run_close(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("cannot transition")
        );
    }

    #[test]
    fn close_partial_id() {
        let (dir, _) = setup_project("open");
        let args = CloseArgs {
            id: "close1".to_string(),
            ids: vec![],
            reason: None,
        };
        let result = run_close(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(
            result.is_ok(),
            "close via partial ID failed: {:?}",
            result.err()
        );

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, "bn-close1", false).unwrap().unwrap();
        assert_eq!(item.state, "done");
    }
}
