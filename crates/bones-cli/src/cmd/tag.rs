//! `bn tag` and `bn untag` — add/remove labels from work items.

use crate::agent;
use crate::output::{CliError, OutputMode, render, render_error};
use bones_core::db::query::{get_labels, try_open_projection};
use bones_core::event::data::UpdateData;
use bones_core::event::writer::write_event;
use bones_core::event::{Event, EventData, EventType};
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;
use clap::Args;
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct TagArgs {
    /// Item ID to tag.
    pub id: String,

    /// Labels to add.
    #[arg(required = true)]
    pub labels: Vec<String>,
}

#[derive(Args, Debug)]
pub struct UntagArgs {
    /// Item ID to untag.
    pub id: String,

    /// Labels to remove.
    #[arg(required = true)]
    pub labels: Vec<String>,
}

/// Open the projection DB, returning a helpful error if it doesn't exist.
fn open_db(project_root: &std::path::Path) -> anyhow::Result<Connection> {
    let db_path = project_root.join(".bones").join("bones.db");
    match try_open_projection(&db_path)? {
        Some(conn) => Ok(conn),
        None => anyhow::bail!(
            "projection database not found or corrupt at {}.\n  Run `bn rebuild` to initialize it.",
            db_path.display()
        ),
    }
}

/// Read the current labels for an item from the projection DB.
fn read_current_labels(conn: &Connection, item_id: &str) -> anyhow::Result<Vec<String>> {
    // Check that the item exists
    if !bones_core::db::query::item_exists(conn, item_id)? {
        anyhow::bail!("item not found: {item_id}");
    }
    let labels = get_labels(conn, item_id)?;
    Ok(labels.into_iter().map(|l| l.label).collect())
}

/// Emit an `item.update` event for the labels field.
fn emit_labels_event(
    project_root: &std::path::Path,
    agent: &str,
    item_id: &ItemId,
    new_labels: &[String],
) -> anyhow::Result<()> {
    let bones_dir = project_root.join(".bones");
    let shard_mgr = ShardManager::new(&bones_dir);

    // Get a monotonic timestamp
    let ts = shard_mgr
        .next_timestamp()
        .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

    let mut event = Event {
        wall_ts_us: ts,
        agent: agent.to_string(),
        itc: "itc:AQ".into(),
        parents: vec![],
        event_type: EventType::Update,
        item_id: item_id.clone(),
        data: EventData::Update(UpdateData {
            field: "labels".into(),
            value: json!(new_labels),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };

    let line = write_event(&mut event)
        .map_err(|e| anyhow::anyhow!("failed to serialize event: {e}"))?;

    shard_mgr
        .append(&line, false, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("failed to write event: {e}"))?;

    Ok(())
}

pub fn run_tag(
    args: &TagArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &std::path::Path,
) -> anyhow::Result<()> {
    let agent = match agent::require_agent(agent_flag) {
        Ok(a) => a,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(&e.message, "Set --agent, BONES_AGENT, or AGENT", e.code),
            )?;
            anyhow::bail!("{}", e.message);
        }
    };

    // Parse and validate item ID
    let item_id = ItemId::parse(&args.id).map_err(|e| {
        anyhow::anyhow!("invalid item ID '{}': {}", args.id, e)
    })?;

    // Open projection DB and read current labels
    let conn = match open_db(project_root) {
        Ok(c) => c,
        Err(e) => {
            render_error(output, &CliError::new(e.to_string()))?;
            return Err(e);
        }
    };

    let current_labels = match read_current_labels(&conn, item_id.as_str()) {
        Ok(l) => l,
        Err(e) => {
            render_error(output, &CliError::new(e.to_string()))?;
            return Err(e);
        }
    };

    // Compute new labels: current + args.labels (deduplicated, preserving order)
    let mut new_labels = current_labels.clone();
    for label in &args.labels {
        if !new_labels.contains(label) {
            new_labels.push(label.clone());
        }
    }
    // Identify which labels were actually added (not already present)
    let added: Vec<&str> = args
        .labels
        .iter()
        .filter(|l| !current_labels.contains(*l))
        .map(String::as_str)
        .collect();

    // Emit event
    if let Err(e) = emit_labels_event(project_root, &agent, &item_id, &new_labels) {
        render_error(output, &CliError::new(e.to_string()))?;
        return Err(e);
    }

    // Output result
    let val = json!({
        "ok": true,
        "item_id": item_id.as_str(),
        "labels": new_labels,
        "added": added,
    });
    render(output, &val, |v, w| {
        let item = v["item_id"].as_str().unwrap_or("");
        let added_list: Vec<&str> = v["added"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        let all_labels: Vec<&str> = v["labels"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        if added_list.is_empty() {
            writeln!(
                w,
                "✓ {item}: labels unchanged (all already present): {}",
                all_labels.join(", ")
            )
        } else {
            writeln!(
                w,
                "✓ {item}: added {} → labels: {}",
                added_list.join(", "),
                all_labels.join(", ")
            )
        }
    })?;
    Ok(())
}

pub fn run_untag(
    args: &UntagArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &std::path::Path,
) -> anyhow::Result<()> {
    let agent = match agent::require_agent(agent_flag) {
        Ok(a) => a,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(&e.message, "Set --agent, BONES_AGENT, or AGENT", e.code),
            )?;
            anyhow::bail!("{}", e.message);
        }
    };

    // Parse and validate item ID
    let item_id = ItemId::parse(&args.id).map_err(|e| {
        anyhow::anyhow!("invalid item ID '{}': {}", args.id, e)
    })?;

    // Open projection DB and read current labels
    let conn = match open_db(project_root) {
        Ok(c) => c,
        Err(e) => {
            render_error(output, &CliError::new(e.to_string()))?;
            return Err(e);
        }
    };

    let current_labels = match read_current_labels(&conn, item_id.as_str()) {
        Ok(l) => l,
        Err(e) => {
            render_error(output, &CliError::new(e.to_string()))?;
            return Err(e);
        }
    };

    // Compute new labels: current minus args.labels
    let labels_to_remove: std::collections::HashSet<&str> =
        args.labels.iter().map(String::as_str).collect();
    let new_labels: Vec<String> = current_labels
        .iter()
        .filter(|l| !labels_to_remove.contains(l.as_str()))
        .cloned()
        .collect();

    // Identify which labels were actually removed
    let removed: Vec<&str> = args
        .labels
        .iter()
        .filter(|l| current_labels.contains(*l))
        .map(String::as_str)
        .collect();

    // Emit event
    if let Err(e) = emit_labels_event(project_root, &agent, &item_id, &new_labels) {
        render_error(output, &CliError::new(e.to_string()))?;
        return Err(e);
    }

    // Output result
    let val = json!({
        "ok": true,
        "item_id": item_id.as_str(),
        "labels": new_labels,
        "removed": removed,
    });
    render(output, &val, |v, w| {
        let item = v["item_id"].as_str().unwrap_or("");
        let removed_list: Vec<&str> = v["removed"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        let all_labels: Vec<&str> = v["labels"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        if removed_list.is_empty() {
            writeln!(
                w,
                "✓ {item}: labels unchanged (none of the specified labels were present): {}",
                if all_labels.is_empty() {
                    "(none)".to_string()
                } else {
                    all_labels.join(", ")
                }
            )
        } else {
            writeln!(
                w,
                "✓ {item}: removed {} → labels: {}",
                removed_list.join(", "),
                if all_labels.is_empty() {
                    "(none)".to_string()
                } else {
                    all_labels.join(", ")
                }
            )
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_args_parses() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: TagArgs,
        }
        let w = Wrapper::parse_from(["test", "item-1", "bug", "urgent"]);
        assert_eq!(w.args.id, "item-1");
        assert_eq!(w.args.labels, vec!["bug", "urgent"]);
    }

    #[test]
    fn untag_args_parses() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: UntagArgs,
        }
        let w = Wrapper::parse_from(["test", "item-1", "stale"]);
        assert_eq!(w.args.id, "item-1");
        assert_eq!(w.args.labels, vec!["stale"]);
    }

    #[test]
    fn tag_deduplicates_labels() {
        // Simulate: current = ["a", "b"], adding ["b", "c"] -> ["a", "b", "c"]
        let current = vec!["a".to_string(), "b".to_string()];
        let to_add = vec!["b".to_string(), "c".to_string()];
        let mut new_labels = current.clone();
        for label in &to_add {
            if !new_labels.contains(label) {
                new_labels.push(label.clone());
            }
        }
        assert_eq!(new_labels, vec!["a", "b", "c"]);
    }

    #[test]
    fn untag_removes_specified_labels() {
        // Simulate: current = ["a", "b", "c"], removing ["b", "d"] -> ["a", "c"]
        let current = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let to_remove: std::collections::HashSet<&str> = ["b", "d"].iter().copied().collect();
        let new_labels: Vec<String> = current
            .iter()
            .filter(|l| !to_remove.contains(l.as_str()))
            .cloned()
            .collect();
        assert_eq!(new_labels, vec!["a", "c"]);
    }

    #[test]
    fn tag_idempotent_same_labels() {
        // Adding already-present labels should not change the set
        let current = vec!["a".to_string(), "b".to_string()];
        let to_add = vec!["a".to_string(), "b".to_string()];
        let mut new_labels = current.clone();
        for label in &to_add {
            if !new_labels.contains(label) {
                new_labels.push(label.clone());
            }
        }
        assert_eq!(new_labels, current);
    }

    #[test]
    fn untag_missing_labels_idempotent() {
        // Removing non-existent labels should leave the set unchanged
        let current = vec!["a".to_string(), "b".to_string()];
        let to_remove: std::collections::HashSet<&str> = ["x", "y"].iter().copied().collect();
        let new_labels: Vec<String> = current
            .iter()
            .filter(|l| !to_remove.contains(l.as_str()))
            .cloned()
            .collect();
        assert_eq!(new_labels, current);
    }

    #[test]
    fn untag_all_labels_produces_empty() {
        let current = vec!["a".to_string(), "b".to_string()];
        let to_remove: std::collections::HashSet<&str> = ["a", "b"].iter().copied().collect();
        let new_labels: Vec<String> = current
            .iter()
            .filter(|l| !to_remove.contains(l.as_str()))
            .cloned()
            .collect();
        assert!(new_labels.is_empty());
    }

    #[test]
    fn emit_labels_event_data_structure() {
        // Verify the UpdateData structure is built correctly
        let labels = vec!["bug".to_string(), "urgent".to_string()];
        let data = UpdateData {
            field: "labels".into(),
            value: json!(labels),
            extra: BTreeMap::new(),
        };
        assert_eq!(data.field, "labels");
        assert_eq!(data.value, json!(["bug", "urgent"]));
    }

    // -----------------------------------------------------------------------
    // Integration tests
    // -----------------------------------------------------------------------

    /// Set up a minimal bones project in a temp dir: initialize shard, create
    /// an item event, rebuild the projection DB.
    #[cfg(test)]
    fn setup_test_project() -> (tempfile::TempDir, std::path::PathBuf, String) {
        use bones_core::db::rebuild;
        use bones_core::event::data::CreateData;
        use bones_core::event::{Event, EventData, EventType};
        use bones_core::event::writer::write_event;
        use bones_core::model::item::Kind;
        use bones_core::model::item_id::ItemId;
        use bones_core::shard::ShardManager;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path().to_path_buf();
        let bones_dir = root.join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init shard");

        // Create a test item via event
        let item_id = "bn-tst1";
        let ts = shard_mgr.next_timestamp().expect("get timestamp");
        let mut create_event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: "Test item".into(),
                kind: Kind::Task,
                size: None,
                urgency: bones_core::model::item::Urgency::Default,
                labels: vec!["initial".into()],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        let line = write_event(&mut create_event).expect("write event");
        shard_mgr
            .append(&line, false, Duration::from_secs(5))
            .expect("append create event");

        // Rebuild the projection DB
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild projection");

        (dir, root, item_id.to_string())
    }

    #[test]
    fn run_tag_adds_labels_to_item() {
        use crate::output::OutputMode;
        use bones_core::db::query::{get_labels, try_open_projection};

        let (_dir, root, item_id) = setup_test_project();

        // Add "bug" and "urgent" labels
        let args = TagArgs {
            id: item_id.clone(),
            labels: vec!["bug".to_string(), "urgent".to_string()],
        };
        run_tag(&args, Some("test-agent"), OutputMode::Human, &root)
            .expect("run_tag should succeed");

        // Rebuild projection to pick up the new event
        let bones_dir = root.join(".bones");
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        bones_core::db::rebuild::rebuild(&events_dir, &db_path).expect("rebuild");

        // Verify labels in projection
        let conn = try_open_projection(&db_path)
            .expect("open db")
            .expect("db should exist");
        let labels = get_labels(&conn, &item_id).expect("get labels");
        let label_names: Vec<&str> = labels.iter().map(|l| l.label.as_str()).collect();
        assert!(label_names.contains(&"bug"), "should contain 'bug'");
        assert!(label_names.contains(&"urgent"), "should contain 'urgent'");
        assert!(label_names.contains(&"initial"), "should contain original 'initial' label");
    }

    #[test]
    fn run_tag_is_idempotent() {
        use crate::output::OutputMode;
        use bones_core::db::query::{get_labels, try_open_projection};
        use bones_core::db::rebuild;

        let (_dir, root, item_id) = setup_test_project();

        // Add the same label twice
        let args = TagArgs {
            id: item_id.clone(),
            labels: vec!["initial".to_string()], // already present
        };
        run_tag(&args, Some("test-agent"), OutputMode::Human, &root)
            .expect("run_tag should succeed even for existing labels");

        // Rebuild and verify no duplicates
        let bones_dir = root.join(".bones");
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild");

        let conn = try_open_projection(&db_path)
            .expect("open db")
            .expect("db should exist");
        let labels = get_labels(&conn, &item_id).expect("get labels");
        let initial_count = labels.iter().filter(|l| l.label == "initial").count();
        assert_eq!(initial_count, 1, "label 'initial' should appear exactly once");
    }

    #[test]
    fn run_untag_removes_labels() {
        use crate::output::OutputMode;
        use bones_core::db::query::{get_labels, try_open_projection};
        use bones_core::db::rebuild;

        let (_dir, root, item_id) = setup_test_project();

        // First add some labels
        let tag_args = TagArgs {
            id: item_id.clone(),
            labels: vec!["a".to_string(), "b".to_string()],
        };
        run_tag(&tag_args, Some("test-agent"), OutputMode::Human, &root)
            .expect("run_tag should succeed");

        // Rebuild to pick up the tags
        let bones_dir = root.join(".bones");
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild");

        // Now remove "a"
        let untag_args = UntagArgs {
            id: item_id.clone(),
            labels: vec!["a".to_string()],
        };
        run_untag(&untag_args, Some("test-agent"), OutputMode::Human, &root)
            .expect("run_untag should succeed");

        // Rebuild again and verify
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild");

        let conn = try_open_projection(&db_path)
            .expect("open db")
            .expect("db should exist");
        let labels = get_labels(&conn, &item_id).expect("get labels");
        let label_names: Vec<&str> = labels.iter().map(|l| l.label.as_str()).collect();
        assert!(!label_names.contains(&"a"), "'a' should be removed");
        assert!(label_names.contains(&"b"), "'b' should remain");
    }

    #[test]
    fn run_tag_fails_on_missing_db() {
        use crate::output::OutputMode;

        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();

        // No .bones/ directory at all
        let args = TagArgs {
            id: "bn-a7x".to_string(),
            labels: vec!["bug".to_string()],
        };
        let result = run_tag(&args, Some("test-agent"), OutputMode::Human, root);
        assert!(result.is_err(), "should fail when no projection DB exists");
    }

    #[test]
    fn run_tag_fails_on_nonexistent_item() {
        use crate::output::OutputMode;
        use bones_core::db::rebuild;
        use bones_core::shard::ShardManager;

        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();
        let bones_dir = root.join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init shard");

        // Rebuild to create an empty projection DB
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild");

        // Try to tag a non-existent item
        let args = TagArgs {
            id: "bn-tst9".to_string(),
            labels: vec!["bug".to_string()],
        };
        let result = run_tag(&args, Some("test-agent"), OutputMode::Human, root);
        assert!(result.is_err(), "should fail when item does not exist");
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not found"), "error should mention item not found");
    }
}
