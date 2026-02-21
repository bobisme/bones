use std::io::Write;
use std::path::Path;

use clap::Args;
use serde::Serialize;

use bones_core::db::query::{self, ItemFilter, SortOrder};
use bones_triage::graph::RawGraph;
use bones_triage::schedule::{
    WhittleConfig, assign_fallback, check_indexability, compute_whittle_indices,
};

use std::collections::{HashMap, HashSet};

use crate::cmd::triage_support::{RankedItem, build_triage_snapshot};
use crate::output::{CliError, OutputMode, render, render_error, render_mode};

/// Arguments for `bn next`.
#[derive(Args, Debug, Default)]
pub struct NextArgs {}

#[derive(Debug, Serialize)]
struct NextPick {
    id: String,
    title: String,
    score: f64,
    explanation: String,
}

#[derive(Debug, Serialize)]
struct NextAssignments {
    assignments: Vec<NextAssignment>,
}

#[derive(Debug, Serialize)]
struct NextAssignment {
    agent_slot: usize,
    id: String,
    title: String,
    score: f64,
    explanation: String,
}

#[derive(Debug, Serialize)]
struct EmptyNext {
    message: String,
}

/// Execute `bn next`.
///
/// - default: returns top-1 unblocked work item with explanation
/// - `bn next --agent N`: returns up to `N` ranked assignments (one per slot)
pub fn run_next(
    _args: &NextArgs,
    output: OutputMode,
    agent_flag: Option<&str>,
    project_root: &Path,
) -> anyhow::Result<()> {
    let agent_slots = match parse_agent_slots(agent_flag) {
        Ok(slots) => slots,
        Err(cli_err) => {
            render_error(output, &cli_err)?;
            anyhow::bail!(cli_err.message);
        }
    };

    let db_path = project_root.join(".bones/bones.db");
    let conn = match query::try_open_projection(&db_path)? {
        Some(conn) => conn,
        None => {
            render_error(
                output,
                &CliError::with_details(
                    "projection database not found",
                    "run `bn admin rebuild` to initialize the projection",
                    "projection_missing",
                ),
            )?;
            anyhow::bail!("projection not found");
        }
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

    if agent_slots == 1 {
        let top = &snapshot.unblocked_ranked[0];
        let next = NextPick {
            id: top.id.clone(),
            title: top.title.clone(),
            score: top.score,
            explanation: top.explanation.clone(),
        };

        let (min_score, max_score) = score_bounds(&snapshot.unblocked_ranked);

        return render_mode(
            output,
            &next,
            |item, w| render_next_text(item, w),
            |item, w| render_next_card(item, w, min_score, max_score),
        );
    }

    let assignments = multi_agent_assignments(&conn, &snapshot, agent_slots)?;

    let payload = NextAssignments { assignments };
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
    let indexability = check_indexability(&graph.graph);

    if indexability.indexable {
        let whittle = compute_whittle_indices(
            &graph.graph,
            &scores,
            &sizes,
            &in_progress,
            &WhittleConfig::default(),
        );

        let mut assignments = Vec::new();
        for item in whittle {
            if !unblocked_ids.contains(item.item_id.as_str()) {
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
            });
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

    let assignments = fallback
        .into_iter()
        .filter_map(|assignment| {
            ranked_by_id
                .get(assignment.item_id.as_str())
                .map(|item| NextAssignment {
                    agent_slot: assignment.agent_idx + 1,
                    id: item.id.clone(),
                    title: item.title.clone(),
                    score: item.score,
                    explanation: format!("{} (fallback-scheduler)", item.explanation),
                })
        })
        .take(agent_slots)
        .collect();

    Ok(assignments)
}

fn parse_agent_slots(agent_flag: Option<&str>) -> Result<usize, CliError> {
    let Some(raw) = agent_flag else {
        return Ok(1);
    };

    let slots = raw.parse::<usize>().map_err(|_| {
        CliError::with_details(
            "bn next expects --agent <N> where N is a positive integer",
            "example: bn next --agent 3",
            "invalid_agent_slots",
        )
    })?;

    if slots == 0 {
        return Err(CliError::with_details(
            "--agent count must be greater than zero",
            "example: bn next --agent 3",
            "invalid_agent_slots",
        ));
    }

    Ok(slots)
}

fn score_bounds(items: &[RankedItem]) -> (f64, f64) {
    let mut min_score = f64::INFINITY;
    let mut max_score = f64::NEG_INFINITY;

    for item in items {
        min_score = min_score.min(item.score);
        max_score = max_score.max(item.score);
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

fn render_next_card(
    item: &NextPick,
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
    writeln!(w, "Why:   {}", item.explanation)
}

fn render_assignments_human(payload: &NextAssignments, w: &mut dyn Write) -> std::io::Result<()> {
    if payload.assignments.is_empty() {
        return writeln!(w, "No assignments available.");
    }

    writeln!(w, "Assignments")?;
    writeln!(w, "{:-<96}", "")?;
    writeln!(w, "{:>4}  {:<16}  {:>8}  TITLE", "SLOT", "ID", "SCORE")?;
    writeln!(w, "{:-<96}", "")?;

    for assignment in &payload.assignments {
        let score = display_score(assignment.score);
        writeln!(
            w,
            "{:>4}  {:<16}  {:>8}  {}",
            assignment.agent_slot, assignment.id, score, assignment.title
        )?;
        writeln!(w, "      why: {}", assignment.explanation)?;
    }

    Ok(())
}

fn render_next_text(item: &NextPick, w: &mut dyn Write) -> std::io::Result<()> {
    let score = display_score(item.score);
    writeln!(
        w,
        "{}  next  score={}  {}  why={}",
        item.id, score, item.title, item.explanation
    )
}

fn render_assignments_text(payload: &NextAssignments, w: &mut dyn Write) -> std::io::Result<()> {
    if payload.assignments.is_empty() {
        writeln!(w, "advice  no-assignments")?;
        return Ok(());
    }

    for assignment in &payload.assignments {
        let score = display_score(assignment.score);
        writeln!(
            w,
            "slot={}  {}  score={}  {}  why={}",
            assignment.agent_slot, assignment.id, score, assignment.title, assignment.explanation
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_slots_defaults_to_one() {
        assert_eq!(parse_agent_slots(None).unwrap(), 1);
    }

    #[test]
    fn parse_agent_slots_accepts_positive_integer() {
        assert_eq!(parse_agent_slots(Some("3")).unwrap(), 3);
    }

    #[test]
    fn parse_agent_slots_rejects_zero_and_non_numeric() {
        assert!(parse_agent_slots(Some("0")).is_err());
        assert!(parse_agent_slots(Some("bones-dev")).is_err());
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
