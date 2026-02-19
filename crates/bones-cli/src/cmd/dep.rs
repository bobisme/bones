//! `bn dep` — manage dependency links between work items.
//!
//! Subcommands:
//! - `bn dep add <from> --blocks <to>` — emit `item.link` with type "blocks"
//! - `bn dep add <from> --relates <to>` — emit `item.link` with type "related_to"
//! - `bn dep rm <from> <to>` — emit `item.unlink` removing the dependency

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use clap::{Args, Subcommand};
use serde::Serialize;
use serde_json::json;

use bones_core::db::query::{item_exists, try_open_projection};
use bones_core::event::data::{EventData, LinkData, UnlinkData};
use bones_core::event::writer::write_event;
use bones_core::event::{Event, EventType};
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;

use crate::agent;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;

// ---------------------------------------------------------------------------
// Clap types
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct DepArgs {
    #[command(subcommand)]
    pub command: DepCommand,
}

#[derive(Subcommand, Debug)]
pub enum DepCommand {
    #[command(
        about = "Add a dependency link between two items",
        after_help = "EXAMPLES:\n    # A blocks B\n    bn dep add bn-abc --blocks bn-def\n\n    # A relates to B (informational)\n    bn dep add bn-abc --relates bn-def"
    )]
    Add(DepAddArgs),

    #[command(
        about = "Remove a dependency link between two items",
        after_help = "EXAMPLES:\n    # Remove the link: bn-abc blocks bn-def\n    bn dep rm bn-abc bn-def"
    )]
    Rm(DepRmArgs),
}

/// Arguments for `bn dep add`.
#[derive(Args, Debug)]
pub struct DepAddArgs {
    /// Source item ID (the blocker / origin of the link).
    pub from: String,

    /// Target item: <from> blocks <to> (creates a hard blocking dependency).
    #[arg(long, group = "link_target", value_name = "TO")]
    pub blocks: Option<String>,

    /// Target item: <from> relates to <to> (informational, no triage impact).
    #[arg(long, group = "link_target", value_name = "TO")]
    pub relates: Option<String>,
}

/// Arguments for `bn dep rm`.
#[derive(Args, Debug)]
pub struct DepRmArgs {
    /// Source item ID (the blocker).
    pub from: String,

    /// Target item ID (the item that was blocked).
    pub to: String,
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct DepAddOutput {
    ok: bool,
    from: String,
    to: String,
    link_type: String,
    event_hash: String,
}

#[derive(Debug, Serialize)]
struct DepRmOutput {
    ok: bool,
    from: String,
    to: String,
    event_hash: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// Emit an `item.link` or `item.unlink` event.
fn emit_event(
    bones_dir: &Path,
    agent: &str,
    event_type: EventType,
    item_id: &ItemId,
    data: EventData,
) -> anyhow::Result<String> {
    let shard_mgr = ShardManager::new(bones_dir);
    let ts = shard_mgr
        .next_timestamp()
        .map_err(|e| anyhow::anyhow!("timestamp error: {e}"))?;

    let mut event = Event {
        wall_ts_us: ts,
        agent: agent.to_string(),
        itc: "itc:AQ".to_string(),
        parents: vec![],
        event_type,
        item_id: item_id.clone(),
        data,
        event_hash: String::new(),
    };

    let line =
        write_event(&mut event).map_err(|e| anyhow::anyhow!("serialize event: {e}"))?;

    shard_mgr
        .append(&line, false, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("write event: {e}"))?;

    // Best-effort projection
    let db_path = bones_dir.join("bones.db");
    if let Ok(conn) = bones_core::db::open_projection(&db_path) {
        let _ = bones_core::db::project::ensure_tracking_table(&conn);
        let projector = bones_core::db::project::Projector::new(&conn);
        if let Err(e) = projector.project_event(&event) {
            tracing::warn!("projection failed (will be fixed on rebuild): {e}");
        }
    }

    Ok(event.event_hash)
}

// ---------------------------------------------------------------------------
// Command runners
// ---------------------------------------------------------------------------

pub fn run_dep(
    args: &DepArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    match &args.command {
        DepCommand::Add(a) => run_dep_add(a, agent_flag, output, project_root),
        DepCommand::Rm(a) => run_dep_rm(a, agent_flag, output, project_root),
    }
}

fn run_dep_add(
    args: &DepAddArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    // 1. Resolve agent
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

    // 2. Determine link target and type
    let (to_raw, link_type) = if let Some(ref to) = args.blocks {
        (to.clone(), "blocks")
    } else if let Some(ref to) = args.relates {
        (to.clone(), "related_to")
    } else {
        let msg = "must provide --blocks <to> or --relates <to>";
        render_error(output, &CliError::new(msg))?;
        anyhow::bail!("{}", msg);
    };

    // 3. Validate item IDs
    if let Err(e) = validate::validate_item_id(&args.from) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }
    if let Err(e) = validate::validate_item_id(&to_raw) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }

    let from_id = ItemId::parse(&args.from)
        .map_err(|e| anyhow::anyhow!("invalid item ID '{}': {}", args.from, e))?;
    let to_id = ItemId::parse(&to_raw)
        .map_err(|e| anyhow::anyhow!("invalid item ID '{}': {}", to_raw, e))?;

    if from_id == to_id {
        let msg = "cannot link an item to itself";
        render_error(output, &CliError::new(msg))?;
        anyhow::bail!("{}", msg);
    }

    // 4. Find .bones dir and DB
    let bones_dir = find_bones_dir(project_root).ok_or_else(|| {
        let msg = "Not a bones project: .bones directory not found";
        render_error(
            output,
            &CliError::with_details(msg, "Run 'bn init' to create a new project", "not_a_project"),
        )
        .ok();
        anyhow::anyhow!("{}", msg)
    })?;

    let db_path = bones_dir.join("bones.db");

    // 5. Verify both items exist
    if let Some(conn) = try_open_projection(&db_path)? {
        if !item_exists(&conn, from_id.as_str())? {
            let msg = format!("item not found: {}", from_id);
            render_error(output, &CliError::new(&msg))?;
            anyhow::bail!("{}", msg);
        }
        if !item_exists(&conn, to_id.as_str())? {
            let msg = format!("item not found: {}", to_id);
            render_error(output, &CliError::new(&msg))?;
            anyhow::bail!("{}", msg);
        }

        // 6. Cycle check for blocking links
        if link_type == "blocks" {
            if let Err(cycle_msg) = check_would_create_cycle(&conn, from_id.as_str(), to_id.as_str()) {
                render_error(output, &CliError::new(&cycle_msg))?;
                anyhow::bail!("{}", cycle_msg);
            }
        }
    } else {
        let msg = "projection database not found; run `bn rebuild` first";
        render_error(output, &CliError::new(msg))?;
        anyhow::bail!("{}", msg);
    }

    // 7. Emit event: item_id = to (blocked), target = from (blocker)
    let event_hash = emit_event(
        &bones_dir,
        &agent,
        EventType::Link,
        &to_id,
        EventData::Link(LinkData {
            target: from_id.as_str().to_string(),
            link_type: link_type.to_string(),
            extra: BTreeMap::new(),
        }),
    )?;

    // 8. Output
    let result = DepAddOutput {
        ok: true,
        from: from_id.as_str().to_string(),
        to: to_id.as_str().to_string(),
        link_type: link_type.to_string(),
        event_hash,
    };

    render(output, &result, |r, w| {
        use std::io::Write;
        let arrow = if r.link_type == "blocks" { "blocks" } else { "relates to" };
        writeln!(w, "✓ {} {} {}", r.from, arrow, r.to)
    })?;

    Ok(())
}

/// Check whether adding a blocking edge `from → to` would create a cycle.
///
/// Returns `Ok(())` if safe, `Err(msg)` with cycle description if not.
fn check_would_create_cycle(
    conn: &rusqlite::Connection,
    from: &str,
    to: &str,
) -> Result<(), String> {
    use bones_triage::graph::{RawGraph, would_create_cycle};

    let raw = RawGraph::from_sqlite(conn)
        .map_err(|e| format!("failed to load dependency graph: {e}"))?;

    // Get or insert node indices for from and to
    let from_idx = match raw.node_index(from) {
        Some(idx) => idx,
        None => return Ok(()), // Item not in graph yet — no cycle possible
    };
    let to_idx = match raw.node_index(to) {
        Some(idx) => idx,
        None => return Ok(()), // Item not in graph yet — no cycle possible
    };

    if let Some(cycle_path) = would_create_cycle(&raw.graph, from_idx, to_idx) {
        let path_str = cycle_path.join(" → ");
        Err(format!(
            "adding this dependency would create a cycle: {path_str}"
        ))
    } else {
        Ok(())
    }
}

fn run_dep_rm(
    args: &DepRmArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    // 1. Resolve agent
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

    // 2. Validate item IDs
    if let Err(e) = validate::validate_item_id(&args.from) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }
    if let Err(e) = validate::validate_item_id(&args.to) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }

    let from_id = ItemId::parse(&args.from)
        .map_err(|e| anyhow::anyhow!("invalid item ID '{}': {}", args.from, e))?;
    let to_id = ItemId::parse(&args.to)
        .map_err(|e| anyhow::anyhow!("invalid item ID '{}': {}", args.to, e))?;

    // 3. Find .bones dir
    let bones_dir = find_bones_dir(project_root).ok_or_else(|| {
        let msg = "Not a bones project: .bones directory not found";
        render_error(
            output,
            &CliError::with_details(msg, "Run 'bn init' to create a new project", "not_a_project"),
        )
        .ok();
        anyhow::anyhow!("{}", msg)
    })?;

    // 4. Emit unlink event: item_id = to (blocked), target = from (blocker)
    let event_hash = emit_event(
        &bones_dir,
        &agent,
        EventType::Unlink,
        &to_id,
        EventData::Unlink(UnlinkData {
            target: from_id.as_str().to_string(),
            link_type: None, // remove any link between the two items
            extra: BTreeMap::new(),
        }),
    )?;

    // 5. Output
    let result = DepRmOutput {
        ok: true,
        from: from_id.as_str().to_string(),
        to: to_id.as_str().to_string(),
        event_hash,
    };

    render(output, &result, |r, w| {
        use std::io::Write;
        writeln!(w, "✓ removed link: {} → {}", r.from, r.to)
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dep_add_args_blocks() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(subcommand)]
            cmd: DepCommand,
        }

        let w = Wrapper::parse_from(["test", "add", "bn-abc", "--blocks", "bn-def"]);
        if let DepCommand::Add(a) = w.cmd {
            assert_eq!(a.from, "bn-abc");
            assert_eq!(a.blocks.as_deref(), Some("bn-def"));
            assert!(a.relates.is_none());
        } else {
            panic!("expected Add");
        }
    }

    #[test]
    fn dep_add_args_relates() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(subcommand)]
            cmd: DepCommand,
        }

        let w = Wrapper::parse_from(["test", "add", "bn-abc", "--relates", "bn-xyz"]);
        if let DepCommand::Add(a) = w.cmd {
            assert_eq!(a.from, "bn-abc");
            assert!(a.blocks.is_none());
            assert_eq!(a.relates.as_deref(), Some("bn-xyz"));
        } else {
            panic!("expected Add");
        }
    }

    #[test]
    fn dep_rm_args() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(subcommand)]
            cmd: DepCommand,
        }

        let w = Wrapper::parse_from(["test", "rm", "bn-aaa", "bn-bbb"]);
        if let DepCommand::Rm(a) = w.cmd {
            assert_eq!(a.from, "bn-aaa");
            assert_eq!(a.to, "bn-bbb");
        } else {
            panic!("expected Rm");
        }
    }

    #[test]
    fn dep_add_cannot_have_both_blocks_and_relates() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(subcommand)]
            cmd: DepCommand,
        }

        // clap's `group` enforcement: --blocks and --relates conflict
        let result =
            Wrapper::try_parse_from(["test", "add", "bn-abc", "--blocks", "x", "--relates", "y"]);
        assert!(result.is_err(), "conflicting flags should be rejected");
    }

    /// Integration test: add and remove a blocking dependency.
    #[test]
    fn dep_add_and_rm_end_to_end() {
        use bones_core::db::rebuild;
        use bones_core::event::data::CreateData;
        use bones_core::event::writer::write_event;
        use bones_core::event::{Event, EventData, EventType};
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

        // Create two items
        for item_id in ["bn-aaa", "bn-bbb"] {
            let ts = shard_mgr.next_timestamp().expect("ts");
            let mut evt = Event {
                wall_ts_us: ts,
                agent: "test-agent".into(),
                itc: "itc:AQ".into(),
                parents: vec![],
                event_type: EventType::Create,
                item_id: ItemId::new_unchecked(item_id),
                data: EventData::Create(CreateData {
                    title: item_id.into(),
                    kind: Kind::Task,
                    size: None,
                    urgency: bones_core::model::item::Urgency::Default,
                    labels: vec![],
                    parent: None,
                    causation: None,
                    description: None,
                    extra: BTreeMap::new(),
                }),
                event_hash: String::new(),
            };
            let line = write_event(&mut evt).expect("write");
            shard_mgr.append(&line, false, Duration::from_secs(5)).expect("append");
        }

        // Rebuild projection
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild");

        // dep add bn-aaa --blocks bn-bbb
        let add_args = DepAddArgs {
            from: "bn-aaa".into(),
            blocks: Some("bn-bbb".into()),
            relates: None,
        };
        run_dep_add(&add_args, Some("test-agent"), OutputMode::Human, &root)
            .expect("dep add should succeed");

        // Rebuild and verify dependency exists
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild");
        let conn = bones_core::db::query::try_open_projection(&db_path)
            .expect("open db")
            .expect("db exists");
        let deps = bones_core::db::query::get_dependencies(&conn, "bn-bbb").expect("get deps");
        assert_eq!(deps.len(), 1, "should have one dep");
        assert_eq!(deps[0].depends_on_item_id, "bn-aaa");
        assert_eq!(deps[0].link_type, "blocks");

        // dep rm bn-aaa bn-bbb
        let rm_args = DepRmArgs {
            from: "bn-aaa".into(),
            to: "bn-bbb".into(),
        };
        run_dep_rm(&rm_args, Some("test-agent"), OutputMode::Human, &root)
            .expect("dep rm should succeed");

        // Rebuild and verify dependency removed
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild");
        let conn = bones_core::db::query::try_open_projection(&db_path)
            .expect("open db")
            .expect("db exists");
        let deps_after = bones_core::db::query::get_dependencies(&conn, "bn-bbb").expect("get deps");
        assert!(deps_after.is_empty(), "dep should be removed");
    }

    #[test]
    fn dep_add_rejects_cycle() {
        use bones_core::db::rebuild;
        use bones_core::event::data::CreateData;
        use bones_core::event::writer::write_event;
        use bones_core::event::{Event, EventData, EventType};
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

        // Create three items: A, B, C
        for item_id in ["bn-ca1", "bn-ca2", "bn-ca3"] {
            let ts = shard_mgr.next_timestamp().expect("ts");
            let mut evt = Event {
                wall_ts_us: ts,
                agent: "test-agent".into(),
                itc: "itc:AQ".into(),
                parents: vec![],
                event_type: EventType::Create,
                item_id: ItemId::new_unchecked(item_id),
                data: EventData::Create(CreateData {
                    title: item_id.into(),
                    kind: Kind::Task,
                    size: None,
                    urgency: bones_core::model::item::Urgency::Default,
                    labels: vec![],
                    parent: None,
                    causation: None,
                    description: None,
                    extra: BTreeMap::new(),
                }),
                event_hash: String::new(),
            };
            let line = write_event(&mut evt).expect("write");
            shard_mgr.append(&line, false, Duration::from_secs(5)).expect("append");
        }

        // Add A blocks B, B blocks C
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild");

        for (from, to) in [("bn-ca1", "bn-ca2"), ("bn-ca2", "bn-ca3")] {
            let args = DepAddArgs {
                from: from.into(),
                blocks: Some(to.into()),
                relates: None,
            };
            run_dep_add(&args, Some("test-agent"), OutputMode::Human, &root)
                .expect("initial dep add should succeed");
            rebuild::rebuild(&events_dir, &db_path).expect("rebuild");
        }

        // Now try C blocks A — should fail with cycle error
        let cycle_args = DepAddArgs {
            from: "bn-ca3".into(),
            blocks: Some("bn-ca1".into()),
            relates: None,
        };
        let result =
            run_dep_add(&cycle_args, Some("test-agent"), OutputMode::Human, &root);
        assert!(result.is_err(), "cycle should be rejected");
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("cycle"),
            "error should mention cycle: {err_str}"
        );
    }

    #[test]
    fn dep_add_self_link_rejected() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path();

        let args = DepAddArgs {
            from: "bn-abc".into(),
            blocks: Some("bn-abc".into()),
            relates: None,
        };
        let result = run_dep_add(&args, Some("test-agent"), OutputMode::Human, root);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("itself"), "should mention self-link: {msg}");
    }

    /// Verify that the output struct serializes correctly.
    #[test]
    fn dep_add_output_serialization() {
        let out = DepAddOutput {
            ok: true,
            from: "bn-abc".into(),
            to: "bn-def".into(),
            link_type: "blocks".into(),
            event_hash: "blake3:abc123".into(),
        };
        let json = serde_json::to_value(&out).expect("serialize");
        assert_eq!(json["ok"], json!(true));
        assert_eq!(json["from"], json!("bn-abc"));
        assert_eq!(json["to"], json!("bn-def"));
        assert_eq!(json["link_type"], json!("blocks"));
    }
}
