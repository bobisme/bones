use std::io::Write;
use std::path::Path;

use clap::Args;
use serde::Serialize;

use bones_core::db::query;

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

    let assignments: Vec<NextAssignment> = snapshot
        .unblocked_ranked
        .iter()
        .take(agent_slots)
        .enumerate()
        .map(|(idx, item)| NextAssignment {
            agent_slot: idx + 1,
            id: item.id.clone(),
            title: item.title.clone(),
            score: item.score,
            explanation: item.explanation.clone(),
        })
        .collect();

    let payload = NextAssignments { assignments };
    render_mode(
        output,
        &payload,
        |assignments, w| render_assignments_text(assignments, w),
        |assignments, w| render_assignments_human(assignments, w),
    )
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

fn render_next_card(
    item: &NextPick,
    w: &mut dyn Write,
    min_score: f64,
    max_score: f64,
) -> std::io::Result<()> {
    let bar = score_bar(item.score, min_score, max_score);

    writeln!(w, "Next item")?;
    writeln!(w, "{:-<72}", "")?;
    writeln!(w, "ID:    {}", item.id)?;
    writeln!(w, "Title: {}", item.title)?;
    writeln!(w, "Score: [{bar}] {:.4}", item.score)?;
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
        writeln!(
            w,
            "{:>4}  {:<16}  {:>8.4}  {}",
            assignment.agent_slot, assignment.id, assignment.score, assignment.title
        )?;
        writeln!(w, "      why: {}", assignment.explanation)?;
    }

    Ok(())
}

fn render_next_text(item: &NextPick, w: &mut dyn Write) -> std::io::Result<()> {
    writeln!(
        w,
        "{}  next  score={:.4}  {}  why={}",
        item.id, item.score, item.title, item.explanation
    )
}

fn render_assignments_text(payload: &NextAssignments, w: &mut dyn Write) -> std::io::Result<()> {
    if payload.assignments.is_empty() {
        writeln!(w, "advice  no-assignments")?;
        return Ok(());
    }

    for assignment in &payload.assignments {
        writeln!(
            w,
            "slot={}  {}  score={:.4}  {}  why={}",
            assignment.agent_slot,
            assignment.id,
            assignment.score,
            assignment.title,
            assignment.explanation
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
}
