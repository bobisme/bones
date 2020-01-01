use std::io::Write;
use std::path::Path;

use clap::{Args, ValueEnum};
use serde::Serialize;

use bones_core::db;
use bones_core::db::project;
use bones_core::db::query::{self, ItemFilter, SortOrder};
use bones_core::model::item::Urgency;
use bones_core::shard::ShardManager;
use bones_triage::graph::RawGraph;
use bones_triage::schedule::{
    WhittleConfig, assign_fallback, check_indexability, compute_whittle_indices,
    find_urgent_chain_front,
};

use std::collections::{HashMap, HashSet};

use crate::agent;
use crate::cmd::do_cmd;
use crate::cmd::triage_support::{RankedItem, build_triage_snapshot};
use crate::output::{CliError, OutputMode, render, render_error, render_mode};

/// Scheduling mode for multi-agent assignments.
#[derive(Clone, Copy, Debug, Default, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScheduleMode {
    /// Standard balanced scheduling (Whittle index / fallback).
    #[default]
    Balanced,
    /// Seed first slots with urgent-chain prerequisites, then fill with balanced.
    UrgentChain,
}

/// Arguments for `bn next`.
#[derive(Args, Debug)]
pub struct NextArgs {
    /// Number of parallel assignment slots to return.
    #[arg(value_name = "count", default_value_t = 1)]
    pub count: usize,

    /// Scheduling mode for multi-slot assignments.
    #[arg(long, value_enum, default_value_t = ScheduleMode::Balanced)]
    pub mode: ScheduleMode,

    /// Atomically claim the next bone(s) for yourself (open -> doing).
    /// Resolves agent from `--agent` flag, `BONES_AGENT`, or `AGENT` env.
    #[arg(long, conflicts_with = "assign_to")]
    pub take: bool,

    /// Atomically assign the next bone(s) to specific agent(s) (open -> doing).
    /// May be repeated; each agent gets one slot. Overrides count.
    #[arg(long = "assign-to", conflicts_with = "take")]
    pub assign_to: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NextAssignments {
    mode: ScheduleMode,
    assignments: Vec<NextAssignment>,
}

#[derive(Debug, Serialize)]
struct NextAssignment {
    agent_slot: usize,
    id: String,
    title: String,
    score: f64,
    explanation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_state: Option<String>,
}

#[derive(Debug, Serialize)]
struct EmptyNext {
    message: String,
}

/// Execute `bn next`.
///
/// - default: returns top-1 ready bone with explanation
/// - `bn next N`: returns up to `N` ranked assignments (one per slot)
/// - `bn next --take`: atomically pick next and transition to doing
/// - `bn next --assign-to a1 --assign-to a2`: assign next N bones to specific agents
#[tracing::instrument(skip_all, name = "cmd.next")]
pub fn run_next(
    args: &NextArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    // Resolve assignment agents (if any)
    let assign_agents: Vec<String> = if args.take {
        let agent = match agent::require_agent(agent_flag) {
            Ok(a) => a,
            Err(e) => {
                render_error(
                    output,
                    &CliError::with_details(
                        &e.message,
                        "Set --agent, BONES_AGENT, or AGENT env for --take",
                        e.code,
                    ),
                )?;
                anyhow::bail!("{}", e.message);
            }
        };
        vec![agent]
    } else if !args.assign_to.is_empty() {
        args.assign_to.clone()
    } else {
        vec![]
    };

    let effective_count = if !assign_agents.is_empty() && !args.take {
        assign_agents.len()
    } else {
        args.count
    };

    let agent_slots = match parse_assignment_count(effective_count) {
        Ok(slots) => slots,
        Err(cli_err) => {
            render_error(output, &cli_err)?;
            anyhow::bail!(cli_err.message);
        }
    };

    let db_path = project_root.join(".bones/bones.db");
    let conn = if let Some(conn) = query::try_open_projection(&db_path)? {
        conn
    } else {
        render_error(
            output,
            &CliError::with_details(
                "projection database not found",
                "run `bn admin rebuild` to initialize the projection",
                "projection_missing",
            ),
        )?;
        anyhow::bail!("projection not found");
    };

    let snapshot = build_triage_snapshot(&conn, chrono::Utc::now().timestamp_micros())?;
    if snapshot.unblocked_ranked.is_empty() {
        let empty = EmptyNext {
            message: "No unblocked items are currently ready".to_string(),
        };
        return render(output, &empty, |_, w| {
            writeln!(w, "(no unblocked items ready right now)")
        });
    }

    // Build a set of IDs that need decomposition for quick lookup.
    let needs_decomp: HashSet<String> = snapshot
        .needs_decomposition
        .iter()
        .map(|item| item.id.clone())
        .collect();

    if agent_slots == 1 {
        // Skip undecomposed L/XL tasks — pick the first ready item that
        // doesn't need decomposition, but warn about any skipped items.
        let mut skipped: Vec<&RankedItem> = Vec::new();
        let mut chosen: Option<&RankedItem> = None;
        for item in &snapshot.unblocked_ranked {
            if needs_decomp.contains(&item.id) {
                skipped.push(item);
            } else {
                chosen = Some(item);
                break;
            }
        }

        // Emit decomposition warnings for skipped items on stdout so agents see them.
        if !skipped.is_empty() {
            let stdout = &mut std::io::stdout();
            for item in &skipped {
                let size = item.size.as_deref().unwrap_or("?");
                let _ = writeln!(
                    stdout,
                    "warn: skipping {} ({}, {}) — needs decomposition into subtasks before work can begin",
                    item.id,
                    item.title,
                    size.to_uppercase(),
                );
            }
        }

        let top = if let Some(item) = chosen {
            item
        } else {
            // Every unblocked item needs decomposition — tell the agent.
            let empty = EmptyNext {
                message: "All unblocked items need decomposition into subtasks before work can begin. Run `bn triage` to see which items need breaking down.".to_string(),
            };
            return render(output, &empty, |_, w| {
                writeln!(
                    w,
                    "advice  decompose-first  All unblocked items are L/XL without subtasks. Decompose them before starting work."
                )
            });
        };

        // Build consistent NextAssignments structure (same shape as multi-slot)
        let mut assignment = NextAssignment {
            agent_slot: 1,
            id: top.id.clone(),
            title: top.title.clone(),
            score: top.score,
            explanation: top.explanation.clone(),
            agent: None,
            previous_state: None,
        };

        // Atomic claim if requested
        if !assign_agents.is_empty() {
            let claim_agent = assign_agents[0].clone();
            match claim_assignment(&assignment.id, &claim_agent, project_root) {
                Ok(prev) => {
                    assignment.agent = Some(claim_agent);
                    assignment.previous_state = Some(prev);
                }
                Err(e) => {
                    render_error(
                        output,
                        &CliError::with_details(
                            format!("failed to claim {}: {e}", assignment.id),
                            "the bone may already be in progress",
                            "claim_failed",
                        ),
                    )?;
                    anyhow::bail!("failed to claim {}: {e}", assignment.id);
                }
            }
        }

        let payload = NextAssignments {
            mode: args.mode,
            assignments: vec![assignment],
        };

        let (min_score, max_score) = score_bounds(&snapshot.unblocked_ranked);

        // JSON uses consistent NextAssignments format; text/pretty use card rendering
        return render_mode(
            output,
            &payload,
            |p, w| {
                if let Some(a) = p.assignments.first() {
                    render_next_text_from_assignment(a, w)
                } else {
                    Ok(())
                }
            },
            |p, w| {
                if let Some(a) = p.assignments.first() {
                    render_next_card_from_assignment(a, w, min_score, max_score)
                } else {
                    Ok(())
                }
            },
        );
    }

    let mut assignments =
        multi_agent_assignments(&conn, &snapshot, agent_slots, args.mode, &needs_decomp)?;

    // Atomic claim if requested
    if !assign_agents.is_empty() {
        for (i, assignment) in assignments.iter_mut().enumerate() {
            let claim_agent = if args.take {
                assign_agents[0].clone()
            } else {
                assign_agents
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| assign_agents.last().unwrap().clone())
            };
            match claim_assignment(&assignment.id, &claim_agent, project_root) {
                Ok(prev) => {
                    assignment.agent = Some(claim_agent);
                    assignment.previous_state = Some(prev);
                }
                Err(e) => {
                    // Log failure but continue with remaining assignments
                    tracing::warn!("failed to claim {}: {e}", assignment.id);
                }
            }
        }
    }

    let payload = NextAssignments {
        mode: args.mode,
        assignments,
    };
    render_mode(
        output,
        &payload,
        |assignments, w| render_assignments_text(assignments, w),
        |assignments, w| render_assignments_human(assignments, w),
    )
}

fn multi_agent_assignments(
    conn: &rusqlite::Connection,
    snapshot: &crate::cmd::triage_support::TriageSnapshot,
    agent_slots: usize,
    mode: ScheduleMode,
    needs_decomp: &HashSet<String>,
) -> anyhow::Result<Vec<NextAssignment>> {
    let ranked_by_id: HashMap<&str, &RankedItem> = snapshot
        .unblocked_ranked
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect();

    let unblocked_ids: HashSet<&str> = ranked_by_id.keys().copied().collect();
    let mut scores: HashMap<String, f64> = HashMap::new();
    let mut sizes: HashMap<String, String> = HashMap::new();
    for item in &snapshot.ranked {
        scores.insert(item.id.clone(), item.score);
        if let Some(size) = &item.size {
            sizes.insert(item.id.clone(), size.to_ascii_lowercase());
        }
    }

    let active_items = query::list_items(
        conn,
        &ItemFilter {
            include_deleted: false,
            sort: SortOrder::UpdatedDesc,
            ..Default::default()
        },
    )?;
    let in_progress: Vec<String> = active_items
        .into_iter()
        .filter(|item| item.state == "doing")
        .map(|item| item.item_id)
        .collect();

    let graph = RawGraph::from_sqlite(conn)
        .map_err(|e| anyhow::anyhow!("failed to load dependency graph for scheduling: {e}"))?;

    // --- Urgent-chain seeding (only in UrgentChain mode) ---
    let mut assignments: Vec<NextAssignment> = Vec::new();
    let mut assigned_ids: HashSet<String> = HashSet::new();

    if matches!(mode, ScheduleMode::UrgentChain) {
        let urgent_ids: HashSet<&str> = snapshot
            .ranked
            .iter()
            .filter(|item| item.urgency == Urgency::Urgent)
            .map(|item| item.id.as_str())
            .collect();

        let chain_result =
            find_urgent_chain_front(&graph.graph, &scores, &unblocked_ids, &urgent_ids);

        if chain_result.has_urgent_chain {
            for chain_id in &chain_result.chain_front {
                if assignments.len() >= agent_slots {
                    break;
                }
                if needs_decomp.contains(chain_id) {
                    continue;
                }
                let Some(base) = ranked_by_id.get(chain_id.as_str()) else {
                    continue;
                };
                assignments.push(NextAssignment {
                    agent_slot: assignments.len() + 1,
                    id: base.id.clone(),
                    title: base.title.clone(),
                    score: base.score,
                    explanation: format!(
                        "{} (urgent-chain: prerequisite of blocked urgent item)",
                        base.explanation
                    ),
                    agent: None,
                    previous_state: None,
                });
                assigned_ids.insert(base.id.clone());
            }
        }
    }

    if assignments.len() >= agent_slots {
        return Ok(assignments);
    }

    // --- Standard scheduling for remaining slots ---
    let indexability = check_indexability(&graph.graph);

    if indexability.indexable {
        let whittle = compute_whittle_indices(
            &graph.graph,
            &scores,
            &sizes,
            &in_progress,
            &WhittleConfig::default(),
        );

        for item in whittle {
            if !unblocked_ids.contains(item.item_id.as_str()) {
                continue;
            }
            if assigned_ids.contains(&item.item_id) {
                continue;
            }
            if needs_decomp.contains(&item.item_id) {
                continue;
            }
            let Some(base) = ranked_by_id.get(item.item_id.as_str()) else {
                continue;
            };
            assignments.push(NextAssignment {
                agent_slot: assignments.len() + 1,
                id: base.id.clone(),
                title: base.title.clone(),
                score: base.score,
                explanation: format!("{} (whittle={:.4})", base.explanation, item.index),
                agent: None,
                previous_state: None,
            });
            assigned_ids.insert(base.id.clone());
            if assignments.len() >= agent_slots {
                return Ok(assignments);
            }
        }
    }

    let fallback_items: Vec<String> = snapshot
        .unblocked_ranked
        .iter()
        .map(|item| item.id.clone())
        .collect();
    let fallback = assign_fallback(&fallback_items, agent_slots, &scores, &[]);

    for assignment in fallback {
        if assigned_ids.contains(&assignment.item_id) {
            continue;
        }
        if needs_decomp.contains(&assignment.item_id) {
            continue;
        }
        let Some(item) = ranked_by_id.get(assignment.item_id.as_str()) else {
            continue;
        };
        assignments.push(NextAssignment {
            agent_slot: assignments.len() + 1,
            id: item.id.clone(),
            title: item.title.clone(),
            score: item.score,
            explanation: format!("{} (fallback-scheduler)", item.explanation),
            agent: None,
            previous_state: None,
        });
        assigned_ids.insert(item.id.clone());
        if assignments.len() >= agent_slots {
            break;
        }
    }

    Ok(assignments)
}

/// Atomically transition a bone to "doing" state, returning the previous state.
fn claim_assignment(
    item_id: &str,
    claim_agent: &str,
    project_root: &Path,
) -> anyhow::Result<String> {
    let bones_dir = do_cmd::find_bones_dir(project_root)
        .ok_or_else(|| anyhow::anyhow!(".bones directory not found"))?;
    let db_path = bones_dir.join("bones.db");
    let conn = db::open_projection(&db_path)?;
    let _ = project::ensure_tracking_table(&conn);
    let shard_mgr = ShardManager::new(&bones_dir);

    let result = do_cmd::run_do_single(project_root, &conn, &shard_mgr, claim_agent, item_id)?;
    Ok(result.previous_state)
}

fn parse_assignment_count(count: usize) -> Result<usize, CliError> {
    if count == 0 {
        return Err(CliError::with_details(
            "count must be greater than zero",
            "example: bn next 3",
            "invalid_agent_slots",
        ));
    }

    Ok(count)
}

fn score_bounds(items: &[RankedItem]) -> (f64, f64) {
    let mut min_score = f64::INFINITY;
    let mut max_score = f64::NEG_INFINITY;

    for item in items {
        if item.score.is_finite() {
            min_score = min_score.min(item.score);
            max_score = max_score.max(item.score);
        }
    }

    if !min_score.is_finite() {
        min_score = 0.0;
    }
    if !max_score.is_finite() {
        max_score = 1.0;
    }

    (min_score, max_score)
}

fn score_bar(score: f64, min_score: f64, max_score: f64) -> String {
    const WIDTH: usize = 20;

    let normalized = if score.is_infinite() {
        if score.is_sign_positive() { 1.0 } else { 0.0 }
    } else if (max_score - min_score).abs() <= f64::EPSILON {
        1.0
    } else {
        ((score - min_score) / (max_score - min_score)).clamp(0.0, 1.0)
    };

    let filled = (normalized * WIDTH as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(WIDTH - filled))
}

fn display_score(score: f64) -> String {
    if score.is_infinite() {
        if score.is_sign_positive() {
            return "URGENT".to_string();
        }
        return "PUNT".to_string();
    }

    if score >= f64::MAX / 2.0 {
        "URGENT".to_string()
    } else if score <= -f64::MAX / 2.0 {
        "PUNT".to_string()
    } else {
        format!("{score:.4}")
    }
}

fn render_next_card_from_assignment(
    item: &NextAssignment,
    w: &mut dyn Write,
    min_score: f64,
    max_score: f64,
) -> std::io::Result<()> {
    let bar = score_bar(item.score, min_score, max_score);
    let score = display_score(item.score);

    writeln!(w, "Next item")?;
    writeln!(w, "{:-<72}", "")?;
    writeln!(w, "ID:    {}", item.id)?;
    writeln!(w, "Title: {}", item.title)?;
    writeln!(w, "Score: [{bar}] {score}")?;
    writeln!(w, "Why:   {}", item.explanation)?;
    if let Some(ref agent) = item.agent {
        writeln!(
            w,
            "State: {} -> doing (assigned to {agent})",
            item.previous_state.as_deref().unwrap_or("?"),
        )?;
    }
    Ok(())
}

fn render_assignments_human(payload: &NextAssignments, w: &mut dyn Write) -> std::io::Result<()> {
    if payload.assignments.is_empty() {
        return writeln!(w, "No assignments available.");
    }

    let has_agents = payload.assignments.iter().any(|a| a.agent.is_some());

    writeln!(w, "Assignments")?;
    writeln!(w, "{:-<96}", "")?;
    if has_agents {
        writeln!(
            w,
            "{:>4}  {:<16}  {:>8}  {:<16}  TITLE",
            "SLOT", "ID", "SCORE", "AGENT"
        )?;
    } else {
        writeln!(w, "{:>4}  {:<16}  {:>8}  TITLE", "SLOT", "ID", "SCORE")?;
    }
    writeln!(w, "{:-<96}", "")?;

    for assignment in &payload.assignments {
        let score = display_score(assignment.score);
        if has_agents {
            writeln!(
                w,
                "{:>4}  {:<16}  {:>8}  {:<16}  {}",
                assignment.agent_slot,
                assignment.id,
                score,
                assignment.agent.as_deref().unwrap_or("-"),
                assignment.title,
            )?;
            if let Some(ref prev) = assignment.previous_state {
                writeln!(w, "      state: {prev} -> doing")?;
            }
        } else {
            writeln!(
                w,
                "{:>4}  {:<16}  {:>8}  {}",
                assignment.agent_slot, assignment.id, score, assignment.title
            )?;
        }
        writeln!(w, "      why: {}", assignment.explanation)?;
    }

    Ok(())
}

fn render_next_text_from_assignment(
    item: &NextAssignment,
    w: &mut dyn Write,
) -> std::io::Result<()> {
    let score = display_score(item.score);
    if item.agent.is_some() {
        writeln!(w, "SLOT\tID\tSCORE\tTITLE\tAGENT\tSTATE\tREASON")?;
        writeln!(
            w,
            "{}\t{}\t{}\t{}\t{}\t{} -> doing\t{}",
            item.agent_slot,
            item.id,
            score,
            item.title,
            item.agent.as_deref().unwrap_or(""),
            item.previous_state.as_deref().unwrap_or("?"),
            item.explanation,
        )
    } else {
        writeln!(w, "SLOT\tID\tSCORE\tTITLE\tREASON")?;
        writeln!(
            w,
            "{}\t{}\t{}\t{}\t{}",
            item.agent_slot, item.id, score, item.title, item.explanation
        )
    }
}

fn render_assignments_text(payload: &NextAssignments, w: &mut dyn Write) -> std::io::Result<()> {
    if payload.assignments.is_empty() {
        writeln!(w, "advice  no-assignments")?;
        return Ok(());
    }

    let has_agents = payload.assignments.iter().any(|a| a.agent.is_some());
    if has_agents {
        writeln!(w, "SLOT\tID\tSCORE\tTITLE\tAGENT\tSTATE\tREASON")?;
        for assignment in &payload.assignments {
            let score = display_score(assignment.score);
            writeln!(
                w,
                "{}\t{}\t{}\t{}\t{}\t{} -> doing\t{}",
                assignment.agent_slot,
                assignment.id,
                score,
                assignment.title,
                assignment.agent.as_deref().unwrap_or(""),
                assignment.previous_state.as_deref().unwrap_or("?"),
                assignment.explanation,
            )?;
        }
    } else {
        writeln!(w, "SLOT\tID\tSCORE\tTITLE\tREASON")?;
        for assignment in &payload.assignments {
            let score = display_score(assignment.score);
            writeln!(
                w,
                "{}\t{}\t{}\t{}\t{}",
                assignment.agent_slot,
                assignment.id,
                score,
                assignment.title,
                assignment.explanation
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_assignment_count_accepts_positive_integer() {
        assert_eq!(parse_assignment_count(1).unwrap(), 1);
        assert_eq!(parse_assignment_count(3).unwrap(), 3);
    }

    #[test]
    fn parse_assignment_count_rejects_zero() {
        assert!(parse_assignment_count(0).is_err());
    }

    #[test]
    fn score_bar_handles_infinite_scores() {
        let hi = score_bar(f64::MAX, 0.0, 1.0);
        let lo = score_bar(f64::NEG_INFINITY, 0.0, 1.0);

        assert_eq!(hi, "█".repeat(20));
        assert_eq!(lo, "░".repeat(20));
    }

    #[test]
    fn display_score_maps_urgent_and_punt() {
        assert_eq!(display_score(f64::MAX), "URGENT");
        assert_eq!(display_score(f64::NEG_INFINITY), "PUNT");
        assert_eq!(display_score(0.12567), "0.1257");
    }
}
