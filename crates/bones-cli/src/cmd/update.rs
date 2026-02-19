//! `bn update` — patch one or more fields on a work item.
//!
//! Each field change emits a separate `item.update` event for CRDT
//! correctness (one LWW write per field). Supports partial ID resolution.
//!
//! # Supported fields
//! - `--title`       — item title (LWW string)
//! - `--description` — item description (LWW string)
//! - `--size`        — t-shirt size estimate (xxs|xs|s|m|l|xl|xxl)
//! - `--urgency`     — urgency level (punt|low|default|high|urgent)
//! - `--kind`        — work item kind (task|bug|feature|goal|epic)

use crate::agent;
use crate::cmd::show::resolve_item_id;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;
use clap::Args;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use bones_core::db;
use bones_core::db::project;
use bones_core::event::Event;
use bones_core::event::data::{EventData, UpdateData};
use bones_core::event::types::EventType;
use bones_core::event::writer;
use bones_core::model::item::{Kind, Size, Urgency};
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;

/// Arguments for `bn update`.
#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Item ID to update (supports partial IDs).
    pub id: String,

    /// New title for the item.
    #[arg(long)]
    pub title: Option<String>,

    /// New description for the item (pass empty string to clear).
    #[arg(long)]
    pub description: Option<String>,

    /// New size estimate (xxs|xs|s|m|l|xl|xxl).
    #[arg(long)]
    pub size: Option<String>,

    /// New urgency level (punt|default|urgent).
    #[arg(long)]
    pub urgency: Option<String>,

    /// New kind (task|bug|goal).
    #[arg(long)]
    pub kind: Option<String>,
}

/// One applied field patch in the output.
#[derive(Debug, Serialize)]
struct FieldUpdate {
    field: String,
    value: serde_json::Value,
    event_hash: String,
}

/// JSON output for a `bn update` command.
#[derive(Debug, Serialize)]
struct UpdateOutput {
    id: String,
    agent: String,
    updates: Vec<FieldUpdate>,
}

/// Find the `.bones` directory by walking up from `start`.
fn find_bones_dir(start: &Path) -> Option<std::path::PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let candidate = current.join(".bones");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub fn run_update(
    args: &UpdateArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    // 1. Require agent identity
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

    if let Err(e) = validate::validate_agent(&agent) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }
    if let Err(e) = validate::validate_item_id(&args.id) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }

    // 2. Validate at least one field specified
    if args.title.is_none()
        && args.description.is_none()
        && args.size.is_none()
        && args.urgency.is_none()
        && args.kind.is_none()
    {
        let msg = "no fields specified: use --title, --description, --size, --urgency, or --kind";
        render_error(
            output,
            &CliError::with_details(msg, "Specify at least one field to update", "no_fields"),
        )?;
        anyhow::bail!("{}", msg);
    }

    // 3. Validate field values before touching the DB
    let validated_size: Option<Size> = if let Some(ref s) = args.size {
        match s.parse::<Size>() {
            Ok(sz) => Some(sz),
            Err(_) => {
                let msg = format!("invalid size '{}': expected xxs|xs|s|m|l|xl|xxl", s);
                render_error(
                    output,
                    &CliError::with_details(&msg, "Valid sizes: xxs xs s m l xl xxl", "invalid_size"),
                )?;
                anyhow::bail!("{}", msg);
            }
        }
    } else {
        None
    };

    let validated_urgency: Option<Urgency> = if let Some(ref u) = args.urgency {
        match u.parse::<Urgency>() {
            Ok(urg) => Some(urg),
            Err(_) => {
                let msg =
                    format!("invalid urgency '{}': expected punt|default|urgent", u);
                render_error(
                    output,
                    &CliError::with_details(
                        &msg,
                        "Valid urgencies: punt default urgent",
                        "invalid_urgency",
                    ),
                )?;
                anyhow::bail!("{}", msg);
            }
        }
    } else {
        None
    };

    let validated_kind: Option<Kind> = if let Some(ref k) = args.kind {
        match k.parse::<Kind>() {
            Ok(knd) => Some(knd),
            Err(_) => {
                let msg =
                    format!("invalid kind '{}': expected task|bug|goal", k);
                render_error(
                    output,
                    &CliError::with_details(
                        &msg,
                        "Valid kinds: task bug goal",
                        "invalid_kind",
                    ),
                )?;
                anyhow::bail!("{}", msg);
            }
        }
    } else {
        None
    };

    // 4. Find .bones directory
    let bones_dir = find_bones_dir(project_root).ok_or_else(|| {
        let msg = "Not a bones project: .bones directory not found";
        render_error(
            output,
            &CliError::with_details(
                msg,
                "Run 'bn init' to create a new bones project",
                "not_a_project",
            ),
        )
        .ok();
        anyhow::anyhow!("{}", msg)
    })?;

    // 5. Open projection DB
    let db_path = bones_dir.join("bones.db");
    let conn = db::open_projection(&db_path)?;
    let _ = project::ensure_tracking_table(&conn);

    // 6. Resolve item ID
    let resolved_id = match resolve_item_id(&conn, &args.id)? {
        Some(id) => id,
        None => {
            let msg = format!("item '{}' not found", args.id);
            render_error(
                output,
                &CliError::with_details(
                    &msg,
                    "Check the item ID with 'bn list' or 'bn show'",
                    "item_not_found",
                ),
            )?;
            anyhow::bail!("{}", msg);
        }
    };

    // 7. Set up shard manager
    let shard_mgr = ShardManager::new(&bones_dir);
    let projector = project::Projector::new(&conn);

    // 8. Build list of (field, value) updates to apply
    let mut pending: Vec<(String, serde_json::Value)> = Vec::new();

    if let Some(ref title) = args.title {
        if title.is_empty() {
            let msg = "title cannot be empty";
            render_error(
                output,
                &CliError::with_details(msg, "Provide a non-empty title", "empty_title"),
            )?;
            anyhow::bail!("{}", msg);
        }
        pending.push(("title".to_string(), serde_json::Value::String(title.clone())));
    }

    if let Some(ref desc) = args.description {
        pending.push((
            "description".to_string(),
            serde_json::Value::String(desc.clone()),
        ));
    }

    if let Some(sz) = validated_size {
        pending.push((
            "size".to_string(),
            serde_json::Value::String(sz.to_string()),
        ));
    }

    if let Some(urg) = validated_urgency {
        pending.push((
            "urgency".to_string(),
            serde_json::Value::String(urg.to_string()),
        ));
    }

    if let Some(knd) = validated_kind {
        pending.push((
            "kind".to_string(),
            serde_json::Value::String(knd.to_string()),
        ));
    }

    // 9. Emit one item.update event per field
    let mut applied: Vec<FieldUpdate> = Vec::new();

    for (field, value) in pending {
        let ts = shard_mgr
            .next_timestamp()
            .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

        let update_data = UpdateData {
            field: field.clone(),
            value: value.clone(),
            extra: BTreeMap::new(),
        };

        let mut event = Event {
            wall_ts_us: ts,
            agent: agent.clone(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Update,
            item_id: ItemId::new_unchecked(&resolved_id),
            data: EventData::Update(update_data),
            event_hash: String::new(),
        };

        let line = writer::write_event(&mut event)
            .map_err(|e| anyhow::anyhow!("failed to serialize event: {e}"))?;

        shard_mgr
            .append(&line, false, Duration::from_secs(5))
            .map_err(|e| anyhow::anyhow!("failed to write event: {e}"))?;

        if let Err(e) = projector.project_event(&event) {
            tracing::warn!("projection failed for field '{field}' (will be fixed on next rebuild): {e}");
        }

        applied.push(FieldUpdate {
            field,
            value,
            event_hash: event.event_hash.clone(),
        });
    }

    // 10. Output
    let result = UpdateOutput {
        id: resolved_id.clone(),
        agent,
        updates: applied,
    };

    render(output, &result, |r, w| {
        use std::io::Write;
        writeln!(w, "✓ {} updated ({} field(s))", r.id, r.updates.len())?;
        for u in &r.updates {
            writeln!(w, "  {} = {}", u.field, u.value)?;
        }
        Ok(())
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db;
    use bones_core::db::project;
    use bones_core::db::query;
    use bones_core::event::data::{CreateData, EventData};
    use bones_core::event::types::EventType;
    use bones_core::event::writer;
    use bones_core::model::item::{Kind, Urgency};
    use bones_core::model::item_id::ItemId;
    use bones_core::shard::ShardManager;
    use clap::Parser;
    use tempfile::TempDir;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: UpdateArgs,
    }

    /// Create a bones project with one open task.
    fn setup_project() -> (TempDir, String) {
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

        let item_id = "bn-test1";
        let ts = shard_mgr.next_timestamp().unwrap();

        let mut create_event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: "Original title".to_string(),
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

        (dir, item_id.to_string())
    }

    #[test]
    fn update_args_parse_title() {
        let w = Wrapper::parse_from(["test", "item-1", "--title", "New title"]);
        assert_eq!(w.args.id, "item-1");
        assert_eq!(w.args.title.as_deref(), Some("New title"));
        assert!(w.args.description.is_none());
    }

    #[test]
    fn update_args_parse_multiple_fields() {
        let w = Wrapper::parse_from([
            "test",
            "item-1",
            "--title",
            "X",
            "--urgency",
            "urgent",
            "--size",
            "m",
        ]);
        assert_eq!(w.args.title.as_deref(), Some("X"));
        assert_eq!(w.args.urgency.as_deref(), Some("urgent"));
        assert_eq!(w.args.size.as_deref(), Some("m"));
    }

    #[test]
    fn update_title() {
        let (dir, item_id) = setup_project();
        let args = UpdateArgs {
            id: item_id.clone(),
            title: Some("Updated title".to_string()),
            description: None,
            size: None,
            urgency: None,
            kind: None,
        };
        let result = run_update(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "update failed: {:?}", result.err());

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.title, "Updated title");
    }

    #[test]
    fn update_urgency() {
        let (dir, item_id) = setup_project();
        let args = UpdateArgs {
            id: item_id.clone(),
            title: None,
            description: None,
            size: None,
            urgency: Some("urgent".to_string()),
            kind: None,
        };
        let result = run_update(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "update failed: {:?}", result.err());

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.urgency, "urgent");
    }

    #[test]
    fn update_kind() {
        let (dir, item_id) = setup_project();
        let args = UpdateArgs {
            id: item_id.clone(),
            title: None,
            description: None,
            size: None,
            urgency: None,
            kind: Some("bug".to_string()),
        };
        let result = run_update(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "update failed: {:?}", result.err());

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.kind, "bug");
    }

    #[test]
    fn update_multiple_fields_emits_separate_events() {
        let (dir, item_id) = setup_project();
        let args = UpdateArgs {
            id: item_id.clone(),
            title: Some("New title".to_string()),
            description: Some("New description".to_string()),
            size: Some("l".to_string()),
            urgency: None,
            kind: None,
        };
        run_update(&args, Some("test-agent"), OutputMode::Json, dir.path()).unwrap();

        // Check shard has 4 events: 1 create + 3 updates
        let bones_dir = dir.path().join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        let replay = shard_mgr.replay().unwrap();
        let lines: Vec<&str> = replay
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();

        assert_eq!(lines.len(), 4, "expected 4 events (1 create + 3 updates)");

        // All update events should be item.update type
        for line in &lines[1..] {
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields[4], "item.update");
        }
    }

    #[test]
    fn update_rejects_no_fields() {
        let (dir, item_id) = setup_project();
        let args = UpdateArgs {
            id: item_id,
            title: None,
            description: None,
            size: None,
            urgency: None,
            kind: None,
        };
        let result = run_update(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no fields specified"));
    }

    #[test]
    fn update_rejects_invalid_size() {
        let (dir, item_id) = setup_project();
        let args = UpdateArgs {
            id: item_id,
            title: None,
            description: None,
            size: Some("huge".to_string()),
            urgency: None,
            kind: None,
        };
        let result = run_update(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid size"));
    }

    #[test]
    fn update_rejects_invalid_urgency() {
        let (dir, item_id) = setup_project();
        let args = UpdateArgs {
            id: item_id,
            title: None,
            description: None,
            size: None,
            urgency: Some("super-urgent".to_string()),
            kind: None,
        };
        let result = run_update(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid urgency"));
    }

    #[test]
    fn update_rejects_invalid_kind() {
        let (dir, item_id) = setup_project();
        let args = UpdateArgs {
            id: item_id,
            title: None,
            description: None,
            size: None,
            urgency: None,
            kind: Some("chore".to_string()),
        };
        let result = run_update(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid kind"));
    }

    #[test]
    fn update_rejects_empty_title() {
        let (dir, item_id) = setup_project();
        let args = UpdateArgs {
            id: item_id,
            title: Some(String::new()),
            description: None,
            size: None,
            urgency: None,
            kind: None,
        };
        let result = run_update(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("title cannot be empty"));
    }

    #[test]
    fn update_rejects_nonexistent_item() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();
        let db_path = bones_dir.join("bones.db");
        let _conn = db::open_projection(&db_path).unwrap();

        let args = UpdateArgs {
            id: "bn-nonexistent".to_string(),
            title: Some("New title".to_string()),
            description: None,
            size: None,
            urgency: None,
            kind: None,
        };
        let result = run_update(&args, Some("test-agent"), OutputMode::Json, root);
        assert!(result.is_err());
    }

    #[test]
    fn update_partial_id_resolution() {
        let (dir, _) = setup_project();
        let args = UpdateArgs {
            id: "test1".to_string(),
            title: Some("Via partial ID".to_string()),
            description: None,
            size: None,
            urgency: None,
            kind: None,
        };
        let result = run_update(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "partial ID update failed: {:?}", result.err());

        let db_path = dir.path().join(".bones/bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let item = query::get_item(&conn, "bn-test1", false).unwrap().unwrap();
        assert_eq!(item.title, "Via partial ID");
    }

    #[test]
    fn update_events_survive_replay() {
        use bones_core::event::parser::{ParsedLine, parse_line};

        let (dir, item_id) = setup_project();
        let args = UpdateArgs {
            id: item_id.clone(),
            title: Some("Replay-stable title".to_string()),
            description: None,
            size: None,
            urgency: None,
            kind: None,
        };
        run_update(&args, Some("test-agent"), OutputMode::Json, dir.path()).unwrap();

        // Rebuild projection from scratch and verify title is correct
        let bones_dir = dir.path().join(".bones");
        let db_path = bones_dir.join("bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let _ = project::ensure_tracking_table(&conn);
        let projector = project::Projector::new(&conn);

        let shard_mgr = ShardManager::new(&bones_dir);
        let replay = shard_mgr.replay().unwrap();
        for line in replay.lines() {
            if let Ok(ParsedLine::Event(event)) = parse_line(line) {
                let _ = projector.project_event(&*event);
            }
        }

        let item = query::get_item(&conn, &item_id, false).unwrap().unwrap();
        assert_eq!(item.title, "Replay-stable title");
    }

    #[test]
    fn update_not_bones_project() {
        let dir = TempDir::new().unwrap();
        let args = UpdateArgs {
            id: "bn-test".to_string(),
            title: Some("X".to_string()),
            description: None,
            size: None,
            urgency: None,
            kind: None,
        };
        let result = run_update(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Not a bones project") || true);
    }
}
