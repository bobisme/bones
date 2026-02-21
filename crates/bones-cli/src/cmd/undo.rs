//! `bn undo` — reverse the last N events on an item via compensating events.
//!
//! This command does **not** delete or modify existing events. It emits new
//! *compensating events* that reverse the observable effect of prior events,
//! preserving the append-only event log and Merkle-DAG integrity.
//!
//! # Usage
//!
//! ```text
//! # Undo the last event on an item
//! bn undo bn-abc
//!
//! # Undo the last 3 events
//! bn undo bn-abc --last 3
//!
//! # Undo a specific event by hash
//! bn undo --event blake3:abcdef...
//!
//! # Preview without emitting
//! bn undo bn-abc --dry-run
//! ```

use crate::agent;
use crate::itc_state::assign_next_itc;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;
use bones_core::db;
use bones_core::db::project;
use bones_core::event::Event;
use bones_core::event::parser::{ParsedLine, PartialParsedLine, parse_line, parse_line_partial};
use bones_core::event::writer;
use bones_core::shard::ShardManager;
use bones_core::undo::{UndoError, compensating_event};
use clap::Args;
use serde::Serialize;
use std::path::Path;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Argument structs
// ---------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct UndoArgs {
    /// Item ID to undo events on (mutually exclusive with --event).
    #[arg(conflicts_with = "event_hash")]
    pub id: Option<String>,

    /// Number of most-recent events to undo (default: 1). Only used with item ID.
    #[arg(long = "last", default_value = "1", requires = "id")]
    pub last_n: usize,

    /// Undo a specific event by its BLAKE3 hash (e.g. `blake3:abcdef...`).
    #[arg(long = "event", value_name = "EVENT_HASH", conflicts_with = "id")]
    pub event_hash: Option<String>,

    /// Preview the compensating events without emitting them to the event log.
    #[arg(long)]
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct UndoEventResult {
    /// Hash of the event being undone.
    original_hash: String,
    /// Type of the event being undone.
    original_type: String,
    /// Hash of the compensating event (absent on dry-run or skip).
    #[serde(skip_serializing_if = "Option::is_none")]
    compensating_hash: Option<String>,
    /// Type of the compensating event (absent on skip).
    #[serde(skip_serializing_if = "Option::is_none")]
    compensating_type: Option<String>,
    /// True if this event was skipped (cannot be undone).
    skipped: bool,
    /// Reason for skipping.
    #[serde(skip_serializing_if = "Option::is_none")]
    skip_reason: Option<String>,
    /// True when run with --dry-run.
    dry_run: bool,
}

#[derive(Debug, Serialize)]
struct UndoOutput {
    item_id: String,
    results: Vec<UndoEventResult>,
    dry_run: bool,
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

/// Read ALL events for a given item ID from the event shards, sorted ascending.
fn load_item_events(shard_mgr: &ShardManager, item_id: &str) -> anyhow::Result<Vec<Event>> {
    let mut events = Vec::new();

    for (year, month) in shard_mgr
        .list_shards()
        .map_err(|e| anyhow::anyhow!("list shards: {e}"))?
    {
        let content = shard_mgr
            .read_shard(year, month)
            .map_err(|e| anyhow::anyhow!("read shard: {e}"))?;

        for (line_no, line) in content.lines().enumerate() {
            let partial = match parse_line_partial(line) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("parse error at shard {year}-{month:02} line {line_no}: {e}");
                    continue;
                }
            };

            let PartialParsedLine::Event(partial_event) = partial else {
                continue;
            };

            if partial_event.item_id_raw != item_id {
                continue;
            }

            match parse_line(line) {
                Ok(ParsedLine::Event(event)) => events.push(*event),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        "full parse error at shard {year}-{month:02} line {line_no}: {e}"
                    );
                }
            }
        }
    }

    // Sort ascending by timestamp, then by event_hash for determinism
    events.sort_by(|a, b| {
        a.wall_ts_us
            .cmp(&b.wall_ts_us)
            .then_with(|| a.event_hash.cmp(&b.event_hash))
    });

    Ok(events)
}

/// Find an event across ALL shards by its hash.
fn find_event_by_hash(
    shard_mgr: &ShardManager,
    hash: &str,
) -> anyhow::Result<Option<(Event, Vec<Event>)>> {
    // We need to load all events for the item the found event belongs to.
    // First pass: find the event and its item_id.
    let mut target_event: Option<Event> = None;

    'outer: for (year, month) in shard_mgr
        .list_shards()
        .map_err(|e| anyhow::anyhow!("list shards: {e}"))?
    {
        let content = shard_mgr
            .read_shard(year, month)
            .map_err(|e| anyhow::anyhow!("read shard: {e}"))?;

        for (line_no, line) in content.lines().enumerate() {
            let partial = match parse_line_partial(line) {
                Ok(p) => p,
                Err(_) => continue,
            };

            let PartialParsedLine::Event(partial_event) = partial else {
                continue;
            };

            if partial_event.event_hash_raw != hash {
                continue;
            }

            match parse_line(line) {
                Ok(ParsedLine::Event(event)) => {
                    target_event = Some(*event);
                    break 'outer;
                }
                _ => {
                    tracing::warn!(
                        "full parse error for hash {hash} at {year}-{month:02} line {line_no}"
                    );
                }
            }
        }
    }

    let target = match target_event {
        Some(e) => e,
        None => return Ok(None),
    };

    // Second pass: load all events for that item to get prior events context.
    let item_id = target.item_id.as_str().to_string();
    let all_events = load_item_events(shard_mgr, &item_id)?;

    Ok(Some((target, all_events)))
}

/// Emit a compensating event and project it.
fn emit_compensating_event(
    project_root: &Path,
    conn: &rusqlite::Connection,
    shard_mgr: &ShardManager,
    original: &Event,
    prior_events: &[&Event],
    agent: &str,
    now: i64,
    dry_run: bool,
) -> UndoEventResult {
    match compensating_event(original, prior_events, agent, now) {
        Err(UndoError::GrowOnly(et)) => UndoEventResult {
            original_hash: original.event_hash.clone(),
            original_type: original.event_type.as_str().to_string(),
            compensating_hash: None,
            compensating_type: None,
            skipped: true,
            skip_reason: Some(format!("{et} is grow-only and cannot be undone")),
            dry_run,
        },
        Err(UndoError::NoPriorState(msg)) => UndoEventResult {
            original_hash: original.event_hash.clone(),
            original_type: original.event_type.as_str().to_string(),
            compensating_hash: None,
            compensating_type: None,
            skipped: true,
            skip_reason: Some(msg),
            dry_run,
        },
        Ok(mut comp_event) => {
            let comp_type = comp_event.event_type.as_str().to_string();

            if dry_run {
                // Compute hash for display but don't write
                let _ = writer::compute_event_hash(&comp_event).map(|h| comp_event.event_hash = h);
                return UndoEventResult {
                    original_hash: original.event_hash.clone(),
                    original_type: original.event_type.as_str().to_string(),
                    compensating_hash: Some(comp_event.event_hash.clone()),
                    compensating_type: Some(comp_type),
                    skipped: false,
                    skip_reason: None,
                    dry_run: true,
                };
            }

            if let Err(e) = assign_next_itc(project_root, &mut comp_event) {
                return UndoEventResult {
                    original_hash: original.event_hash.clone(),
                    original_type: original.event_type.as_str().to_string(),
                    compensating_hash: None,
                    compensating_type: Some(comp_type),
                    skipped: true,
                    skip_reason: Some(format!("failed to assign ITC stamp: {e}")),
                    dry_run: false,
                };
            }

            // Write the event to the shard
            let line = match writer::write_event(&mut comp_event) {
                Ok(l) => l,
                Err(e) => {
                    return UndoEventResult {
                        original_hash: original.event_hash.clone(),
                        original_type: original.event_type.as_str().to_string(),
                        compensating_hash: None,
                        compensating_type: Some(comp_type),
                        skipped: true,
                        skip_reason: Some(format!("failed to serialize event: {e}")),
                        dry_run: false,
                    };
                }
            };

            if let Err(e) = shard_mgr.append(&line, false, Duration::from_secs(5)) {
                return UndoEventResult {
                    original_hash: original.event_hash.clone(),
                    original_type: original.event_type.as_str().to_string(),
                    compensating_hash: None,
                    compensating_type: Some(comp_type),
                    skipped: true,
                    skip_reason: Some(format!("failed to append event: {e}")),
                    dry_run: false,
                };
            }

            // Project the compensating event
            let projector = project::Projector::new(conn);
            if let Err(e) = projector.project_event(&comp_event) {
                tracing::warn!(
                    "projection of compensating event {} failed (rebuild to fix): {e}",
                    comp_event.event_hash
                );
            }

            UndoEventResult {
                original_hash: original.event_hash.clone(),
                original_type: original.event_type.as_str().to_string(),
                compensating_hash: Some(comp_event.event_hash.clone()),
                compensating_type: Some(comp_type),
                skipped: false,
                skip_reason: None,
                dry_run: false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run_undo(
    args: &UndoArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    // Validate that exactly one of id/event_hash is provided
    if args.id.is_none() && args.event_hash.is_none() {
        let msg = "either an item ID or --event <hash> must be provided";
        render_error(
            output,
            &CliError::with_details(
                msg,
                "Usage: bn undo <item-id>  OR  bn undo --event <hash>",
                "missing_args",
            ),
        )?;
        anyhow::bail!("{msg}");
    }

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

    let bones_dir = find_bones_dir(project_root).ok_or_else(|| {
        let msg = "Not a bones project: .bones directory not found";
        render_error(
            output,
            &CliError::with_details(
                msg,
                "Run 'bn init' to create a bones project",
                "not_a_project",
            ),
        )
        .ok();
        anyhow::anyhow!("{msg}")
    })?;

    let db_path = bones_dir.join("bones.db");
    let conn = db::open_projection(&db_path)?;
    let _ = project::ensure_tracking_table(&conn);
    let shard_mgr = ShardManager::new(&bones_dir);

    // Acquire a timestamp for all compensating events (monotonic, same second is fine)
    let now = shard_mgr
        .next_timestamp()
        .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

    // ---------------------------------------------------------------------------
    // Mode 1: undo by event hash
    // ---------------------------------------------------------------------------
    if let Some(ref hash) = args.event_hash {
        let (target_event, all_events) = match find_event_by_hash(&shard_mgr, hash)? {
            Some(pair) => pair,
            None => {
                let msg = format!("event '{hash}' not found in event log");
                render_error(output, &CliError::with_details(&msg, "", "not_found"))?;
                anyhow::bail!("{msg}");
            }
        };

        let item_id = target_event.item_id.as_str().to_string();

        // Collect prior events (events that appeared before target in chronological order)
        let prior_refs: Vec<&Event> = all_events
            .iter()
            .take_while(|e| e.event_hash != target_event.event_hash)
            .collect();

        let result = emit_compensating_event(
            project_root,
            &conn,
            &shard_mgr,
            &target_event,
            &prior_refs,
            &agent,
            now,
            args.dry_run,
        );

        let output_payload = UndoOutput {
            item_id: item_id.clone(),
            results: vec![result],
            dry_run: args.dry_run,
        };

        return render(output, &output_payload, |o, w| {
            let r = &o.results[0];
            let prefix = if o.dry_run { "(dry-run) " } else { "" };
            if r.skipped {
                writeln!(
                    w,
                    "⚠  skipped {}: {}",
                    r.original_type,
                    r.skip_reason.as_deref().unwrap_or("")
                )?;
            } else {
                writeln!(
                    w,
                    "{}✓ undid {} via {}  {}",
                    prefix,
                    r.original_type,
                    r.compensating_type.as_deref().unwrap_or("?"),
                    r.compensating_hash.as_deref().unwrap_or("")
                )?;
            }
            Ok(())
        });
    }

    // ---------------------------------------------------------------------------
    // Mode 2: undo last N events on item
    // ---------------------------------------------------------------------------
    let raw_id = args.id.as_deref().unwrap();

    // Validate item ID format
    if let Err(e) = validate::validate_item_id(raw_id) {
        render_error(
            output,
            &CliError::with_details(
                &format!("invalid item ID '{}': {}", e.value, e.reason),
                "",
                "invalid_id",
            ),
        )?;
        anyhow::bail!("invalid item ID");
    }

    let all_events = load_item_events(&shard_mgr, raw_id)?;

    if all_events.is_empty() {
        let msg = format!("item '{raw_id}' not found or has no events");
        render_error(output, &CliError::with_details(&msg, "", "not_found"))?;
        anyhow::bail!("{msg}");
    }

    let item_id = all_events[0].item_id.as_str().to_string();
    let n = args.last_n.min(all_events.len());

    // We undo the last N events in reverse chronological order (most recent first)
    let to_undo_start = all_events.len() - n;
    let events_to_undo = &all_events[to_undo_start..];

    let mut results = Vec::new();

    for (idx, original) in events_to_undo.iter().enumerate().rev() {
        // prior events: everything that came chronologically before this one
        let global_idx = to_undo_start + idx;
        let prior_refs: Vec<&Event> = all_events[..global_idx].iter().collect();

        let result = emit_compensating_event(
            project_root,
            &conn,
            &shard_mgr,
            original,
            &prior_refs,
            &agent,
            now,
            args.dry_run,
        );
        results.push(result);
    }

    let output_payload = UndoOutput {
        item_id: item_id.clone(),
        results,
        dry_run: args.dry_run,
    };

    render(output, &output_payload, |o, w| {
        let prefix = if o.dry_run { "(dry-run) " } else { "" };
        for r in &o.results {
            if r.skipped {
                writeln!(
                    w,
                    "⚠  skipped {} [{}]: {}",
                    r.original_type,
                    r.original_hash,
                    r.skip_reason.as_deref().unwrap_or("")
                )?;
            } else {
                writeln!(
                    w,
                    "{}✓ undid {} via {}  (original: {})  (compensating: {})",
                    prefix,
                    r.original_type,
                    r.compensating_type.as_deref().unwrap_or("?"),
                    r.original_hash,
                    r.compensating_hash.as_deref().unwrap_or("(dry-run)")
                )?;
            }
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db;
    use bones_core::db::project;
    use bones_core::event::data::{CreateData, EventData, MoveData};
    use bones_core::event::types::EventType;
    use bones_core::event::writer;
    use bones_core::model::item::{Kind, State, Urgency};
    use bones_core::model::item_id::ItemId;
    use clap::Parser;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: UndoArgs,
    }

    fn setup_project_with_events() -> (TempDir, String) {
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

        let item_id = "bn-undo1";

        // emit item.create
        let ts1 = shard_mgr.next_timestamp().unwrap();
        let mut create_event = Event {
            wall_ts_us: ts1,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: "Original title".into(),
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

        // emit item.move (open → doing)
        let ts2 = shard_mgr.next_timestamp().unwrap();
        let mut move_event = Event {
            wall_ts_us: ts2,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![create_event.event_hash.clone()],
            event_type: EventType::Move,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Move(MoveData {
                state: State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        let line2 = writer::write_event(&mut move_event).unwrap();
        shard_mgr
            .append(&line2, false, Duration::from_secs(5))
            .unwrap();
        projector.project_event(&move_event).unwrap();

        (dir, item_id.to_string())
    }

    #[test]
    fn undo_args_item_id_only() {
        let w = Wrapper::parse_from(["bn", "bn-abc"]);
        assert_eq!(w.args.id.as_deref(), Some("bn-abc"));
        assert_eq!(w.args.last_n, 1);
        assert!(!w.args.dry_run);
        assert!(w.args.event_hash.is_none());
    }

    #[test]
    fn undo_args_with_last() {
        let w = Wrapper::parse_from(["bn", "bn-abc", "--last", "3"]);
        assert_eq!(w.args.last_n, 3);
    }

    #[test]
    fn undo_args_dry_run() {
        let w = Wrapper::parse_from(["bn", "bn-abc", "--dry-run"]);
        assert!(w.args.dry_run);
    }

    #[test]
    fn undo_args_event_hash() {
        let w = Wrapper::parse_from(["bn", "--event", "blake3:abc123"]);
        assert_eq!(w.args.event_hash.as_deref(), Some("blake3:abc123"));
        assert!(w.args.id.is_none());
    }

    #[test]
    fn undo_last_event_dry_run() {
        let (dir, item_id) = setup_project_with_events();
        let args = UndoArgs {
            id: Some(item_id.clone()),
            last_n: 1,
            event_hash: None,
            dry_run: true,
        };
        let result = run_undo(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "undo dry-run failed: {:?}", result.err());
    }

    #[test]
    fn undo_last_event_emits_compensating() {
        let (dir, item_id) = setup_project_with_events();
        let args = UndoArgs {
            id: Some(item_id.clone()),
            last_n: 1,
            event_hash: None,
            dry_run: false,
        };
        let result = run_undo(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "undo failed: {:?}", result.err());
    }

    #[test]
    fn undo_fails_for_missing_item() {
        let (dir, _) = setup_project_with_events();
        let args = UndoArgs {
            id: Some("bn-nonexistent".to_string()),
            last_n: 1,
            event_hash: None,
            dry_run: false,
        };
        let result = run_undo(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not found or has no events")
        );
    }

    #[test]
    fn undo_requires_id_or_event() {
        let (dir, _) = setup_project_with_events();
        let args = UndoArgs {
            id: None,
            last_n: 1,
            event_hash: None,
            dry_run: false,
        };
        let result = run_undo(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
    }
}
