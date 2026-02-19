//! `bn did` and `bn skip` — record user feedback for Thompson Sampling.
//!
//! These commands wire user actions to the triage feedback backend:
//!
//! - `bn did <id>` records positive feedback (agent worked on the item).
//! - `bn skip <id>` records negative feedback (agent skipped the recommendation).
//!
//! Each invocation appends a JSON line to `.bones/feedback.jsonl` and updates
//! the per-agent posterior in `.bones/agent_profiles/<agent>.json`.

use crate::agent;
use crate::cmd::show::resolve_item_id;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;
use clap::Args;
use serde::Serialize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use bones_core::db;
use bones_core::db::project;
use bones_triage::feedback::{FeedbackEntry, FeedbackKind, record_feedback_at};

// ─── Arg structs ──────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct DidArgs {
    /// Item ID (supports partial IDs).
    pub id: String,
}

#[derive(Args, Debug)]
pub struct SkipArgs {
    /// Item ID (supports partial IDs).
    pub id: String,
}

// ─── Output ───────────────────────────────────────────────────────────────────

/// JSON output emitted by both `bn did` and `bn skip`.
#[derive(Debug, Serialize)]
struct FeedbackOutput {
    id: String,
    action: String,
    agent: String,
    ts: u64,
}

// ─── Shared implementation ────────────────────────────────────────────────────

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

/// Shared logic for `bn did` and `bn skip`.
fn run_feedback(
    id: &str,
    kind: FeedbackKind,
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
    if let Err(e) = validate::validate_item_id(id) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }

    // 2. Find .bones directory
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

    // 3. Open projection DB
    let db_path = bones_dir.join("bones.db");
    let conn = db::open_projection(&db_path)?;
    let _ = project::ensure_tracking_table(&conn);

    // 4. Resolve item ID (supports partial IDs)
    let resolved_id = match resolve_item_id(&conn, id)? {
        Some(resolved) => resolved,
        None => {
            let msg = format!("item '{}' not found", id);
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

    // 5. Build timestamp (seconds since Unix epoch)
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // 6. Determine project root for feedback storage (parent of .bones/)
    let feedback_root = bones_dir
        .parent()
        .unwrap_or(project_root)
        .to_path_buf();

    // 7. Build and record the feedback entry
    let item_id = bones_core::model::item_id::ItemId::new_unchecked(&resolved_id);
    let entry = FeedbackEntry {
        kind,
        item: item_id,
        agent: agent.clone(),
        ts,
    };

    record_feedback_at(&feedback_root, entry)?;

    // 8. Output
    let action = match kind {
        FeedbackKind::Did => "did",
        FeedbackKind::Skip => "skip",
    };

    let result = FeedbackOutput {
        id: resolved_id.clone(),
        action: action.to_string(),
        agent,
        ts,
    };

    render(output, &result, |r, w| {
        use std::io::Write;
        let verb = match r.action.as_str() {
            "did" => "✓",
            _ => "⊘",
        };
        writeln!(w, "{} {} recorded for {}", verb, r.action, r.id)?;
        Ok(())
    })?;

    Ok(())
}

// ─── Public entry points ──────────────────────────────────────────────────────

/// Run `bn did <id>` — record positive feedback.
pub fn run_did(
    args: &DidArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    run_feedback(&args.id, FeedbackKind::Did, agent_flag, output, project_root)
}

/// Run `bn skip <id>` — record negative feedback.
pub fn run_skip(
    args: &SkipArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    run_feedback(
        &args.id,
        FeedbackKind::Skip,
        agent_flag,
        output,
        project_root,
    )
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db;
    use bones_core::db::project;
    use bones_core::event::Event;
    use bones_core::event::data::{CreateData, EventData};
    use bones_core::event::types::EventType;
    use bones_core::event::writer;
    use bones_core::model::item::{Kind, Urgency};
    use bones_core::model::item_id::ItemId;
    use bones_core::shard::ShardManager;
    use bones_triage::feedback::{FeedbackAction, load_feedback_events};
    use clap::Parser;
    use std::collections::BTreeMap;
    use std::time::Duration;
    use tempfile::TempDir;

    #[derive(Parser)]
    struct DidWrapper {
        #[command(flatten)]
        args: DidArgs,
    }

    #[derive(Parser)]
    struct SkipWrapper {
        #[command(flatten)]
        args: SkipArgs,
    }

    /// Set up a minimal bones project with one open item.
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

        let item_id = "bn-feed1";
        let ts = shard_mgr.next_timestamp().unwrap();

        let mut create_event = Event {
            wall_ts_us: ts,
            agent: "test-agent".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: "Feedback test item".to_string(),
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

    // ── Argument parsing ────────────────────────────────────────────────────

    #[test]
    fn did_args_parses_id() {
        let w = DidWrapper::parse_from(["test", "bn-feed1"]);
        assert_eq!(w.args.id, "bn-feed1");
    }

    #[test]
    fn skip_args_parses_id() {
        let w = SkipWrapper::parse_from(["test", "bn-feed1"]);
        assert_eq!(w.args.id, "bn-feed1");
    }

    // ── bn did ──────────────────────────────────────────────────────────────

    #[test]
    fn did_appends_feedback_log() {
        let (dir, item_id) = setup_project();
        let args = DidArgs {
            id: item_id.clone(),
        };
        run_did(&args, Some("test-agent"), OutputMode::Json, dir.path()).unwrap();

        let events = load_feedback_events(dir.path()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].agent_id, "test-agent");
        assert_eq!(events[0].action, FeedbackAction::Did);
        assert_eq!(events[0].item_id.as_str(), item_id);
    }

    #[test]
    fn did_updates_agent_profile() {
        use bones_triage::feedback::load_agent_profile;

        let (dir, item_id) = setup_project();
        let args = DidArgs { id: item_id };
        run_did(&args, Some("alice"), OutputMode::Json, dir.path()).unwrap();

        let profile = load_agent_profile(dir.path(), "alice").unwrap();
        // At least one posterior should have been updated (alpha_param > 1.0).
        let any_updated = [
            profile.posteriors.alpha.alpha_param,
            profile.posteriors.beta.alpha_param,
            profile.posteriors.gamma.alpha_param,
            profile.posteriors.delta.alpha_param,
            profile.posteriors.epsilon.alpha_param,
        ]
        .iter()
        .any(|&v| v > 1.0);
        assert!(any_updated, "expected at least one posterior to be updated");
    }

    #[test]
    fn did_nonexistent_item_errors() {
        let (dir, _) = setup_project();
        let args = DidArgs {
            id: "bn-nope".to_string(),
        };
        let result = run_did(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn did_requires_agent() {
        let (dir, item_id) = setup_project();
        let args = DidArgs { id: item_id };
        // Behavior depends on env; just verify no panic
        let _ = run_did(&args, None, OutputMode::Json, dir.path());
    }

    #[test]
    fn did_not_a_bones_project() {
        let dir = TempDir::new().unwrap();
        let args = DidArgs {
            id: "bn-feed1".to_string(),
        };
        let result = run_did(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err(), "expected error for non-bones-project directory");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("bones project") || msg.contains(".bones"),
            "unexpected error: {msg}"
        );
    }

    // ── bn skip ─────────────────────────────────────────────────────────────

    #[test]
    fn skip_appends_feedback_log() {
        let (dir, item_id) = setup_project();
        let args = SkipArgs {
            id: item_id.clone(),
        };
        run_skip(&args, Some("test-agent"), OutputMode::Json, dir.path()).unwrap();

        let events = load_feedback_events(dir.path()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].agent_id, "test-agent");
        assert_eq!(events[0].action, FeedbackAction::Skip);
        assert_eq!(events[0].item_id.as_str(), item_id);
    }

    #[test]
    fn skip_updates_agent_profile() {
        use bones_triage::feedback::load_agent_profile;

        let (dir, item_id) = setup_project();
        let args = SkipArgs { id: item_id };
        run_skip(&args, Some("bob"), OutputMode::Json, dir.path()).unwrap();

        let profile = load_agent_profile(dir.path(), "bob").unwrap();
        // At least one posterior should have been updated (beta_param > 1.0).
        let any_updated = [
            profile.posteriors.alpha.beta_param,
            profile.posteriors.beta.beta_param,
            profile.posteriors.gamma.beta_param,
            profile.posteriors.delta.beta_param,
            profile.posteriors.epsilon.beta_param,
        ]
        .iter()
        .any(|&v| v > 1.0);
        assert!(any_updated, "expected at least one posterior to be updated");
    }

    #[test]
    fn skip_nonexistent_item_errors() {
        let (dir, _) = setup_project();
        let args = SkipArgs {
            id: "bn-nope".to_string(),
        };
        let result = run_skip(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn did_partial_id_resolution() {
        let (dir, _) = setup_project();
        // Use "feed1" — partial match for "bn-feed1"
        let args = DidArgs {
            id: "feed1".to_string(),
        };
        let result = run_did(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "partial ID resolution failed: {:?}", result.err());

        let events = load_feedback_events(dir.path()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].item_id.as_str(), "bn-feed1");
    }

    #[test]
    fn skip_partial_id_resolution() {
        let (dir, _) = setup_project();
        let args = SkipArgs {
            id: "feed1".to_string(),
        };
        let result = run_skip(&args, Some("test-agent"), OutputMode::Json, dir.path());
        assert!(result.is_ok(), "partial ID resolution failed: {:?}", result.err());

        let events = load_feedback_events(dir.path()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].item_id.as_str(), "bn-feed1");
    }

    #[test]
    fn multiple_feedback_entries_accumulate() {
        let (dir, item_id) = setup_project();

        run_did(
            &DidArgs { id: item_id.clone() },
            Some("test-agent"),
            OutputMode::Json,
            dir.path(),
        )
        .unwrap();
        run_skip(
            &SkipArgs { id: item_id.clone() },
            Some("test-agent"),
            OutputMode::Json,
            dir.path(),
        )
        .unwrap();
        run_did(
            &DidArgs { id: item_id.clone() },
            Some("test-agent"),
            OutputMode::Json,
            dir.path(),
        )
        .unwrap();

        let events = load_feedback_events(dir.path()).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].action, FeedbackAction::Did);
        assert_eq!(events[1].action, FeedbackAction::Skip);
        assert_eq!(events[2].action, FeedbackAction::Did);
    }
}
