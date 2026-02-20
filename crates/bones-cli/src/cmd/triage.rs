use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::Path;

use clap::Args;
use serde::Serialize;

use bones_core::db::query;

use crate::cmd::triage_support::{RankedItem, build_triage_snapshot};
use crate::output::{CliError, OutputMode, render, render_error};

/// Arguments for `bn triage`.
#[derive(Args, Debug, Default)]
pub struct TriageArgs {}

#[derive(Debug, Clone, Serialize)]
struct TriageRow {
    id: String,
    title: String,
    score: f64,
    section: String,
}

/// Execute `bn triage`.
///
/// Produces four sections:
/// - Top Picks
/// - Blockers
/// - Quick Wins
/// - Cycles
pub fn run_triage(
    _args: &TriageArgs,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let db_path = project_root.join(".bones/bones.db");
    let conn = match query::try_open_projection(&db_path)? {
        Some(conn) => conn,
        None => {
            render_error(
                output,
                &CliError::with_details(
                    "projection database not found",
                    "run `bn rebuild` to initialize the projection",
                    "projection_missing",
                ),
            )?;
            anyhow::bail!("projection not found");
        }
    };

    let snapshot = build_triage_snapshot(&conn, chrono::Utc::now().timestamp_micros())?;

    let top_picks: Vec<&RankedItem> = snapshot.unblocked_ranked.iter().take(5).collect();

    let mut blockers: Vec<&RankedItem> = snapshot
        .ranked
        .iter()
        .filter(|item| item.unblocks_active > 0)
        .collect();
    blockers.sort_by(|a, b| {
        b.unblocks_active
            .cmp(&a.unblocks_active)
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| a.id.cmp(&b.id))
    });
    blockers.truncate(5);

    let mut quick_wins: Vec<&RankedItem> = snapshot
        .unblocked_ranked
        .iter()
        .filter(|item| is_small_size(item.size.as_deref()) || item.unblocks_active == 0)
        .collect();
    if quick_wins.is_empty() {
        quick_wins = snapshot.unblocked_ranked.iter().take(5).collect();
    }
    quick_wins.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    quick_wins.truncate(5);

    let cycles: Vec<Vec<String>> = snapshot.cycles.iter().take(5).cloned().collect();

    let title_map: HashMap<String, String> = snapshot
        .ranked
        .iter()
        .map(|item| (item.id.clone(), item.title.clone()))
        .collect();
    let score_map: HashMap<String, f64> = snapshot
        .ranked
        .iter()
        .map(|item| (item.id.clone(), item.score))
        .collect();

    let rows = build_rows(
        &top_picks,
        &blockers,
        &quick_wins,
        &cycles,
        &title_map,
        &score_map,
    );

    let use_color = std::io::stdout().is_terminal();
    render(output, &rows, |_, w| {
        render_triage_human(w, &top_picks, &blockers, &quick_wins, &cycles, use_color)
    })
}

fn build_rows(
    top_picks: &[&RankedItem],
    blockers: &[&RankedItem],
    quick_wins: &[&RankedItem],
    cycles: &[Vec<String>],
    title_map: &HashMap<String, String>,
    score_map: &HashMap<String, f64>,
) -> Vec<TriageRow> {
    let mut rows = Vec::new();

    push_rows(&mut rows, top_picks, "top_pick");
    push_rows(&mut rows, blockers, "blocker");
    push_rows(&mut rows, quick_wins, "quick_win");

    for cycle in cycles {
        for id in cycle {
            rows.push(TriageRow {
                id: id.clone(),
                title: title_map
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| "Cycle member".to_string()),
                score: score_map.get(id).copied().unwrap_or(0.0),
                section: "cycle".to_string(),
            });
        }
    }

    rows
}

fn push_rows(rows: &mut Vec<TriageRow>, section_rows: &[&RankedItem], section: &str) {
    for item in section_rows {
        rows.push(TriageRow {
            id: item.id.clone(),
            title: item.title.clone(),
            score: item.score,
            section: section.to_string(),
        });
    }
}

fn render_triage_human(
    w: &mut dyn Write,
    top_picks: &[&RankedItem],
    blockers: &[&RankedItem],
    quick_wins: &[&RankedItem],
    cycles: &[Vec<String>],
    use_color: bool,
) -> std::io::Result<()> {
    render_ranked_section(w, "Top Picks", "32;1", top_picks, use_color)?;
    writeln!(w)?;

    render_blocker_section(w, blockers, use_color)?;
    writeln!(w)?;

    render_ranked_section(w, "Quick Wins", "36;1", quick_wins, use_color)?;
    writeln!(w)?;

    let heading = colorize("Cycles", "31;1", use_color);
    writeln!(w, "{heading}")?;
    writeln!(w, "{:-<72}", "")?;
    if cycles.is_empty() {
        writeln!(w, "(none)")?;
    } else {
        for cycle in cycles {
            writeln!(w, "{}", cycle.join(" → "))?;
        }
    }

    Ok(())
}

fn render_ranked_section(
    w: &mut dyn Write,
    title: &str,
    color: &str,
    items: &[&RankedItem],
    use_color: bool,
) -> std::io::Result<()> {
    let heading = colorize(title, color, use_color);
    writeln!(w, "{heading}")?;
    writeln!(w, "{:-<72}", "")?;

    if items.is_empty() {
        writeln!(w, "(none)")?;
        return Ok(());
    }

    writeln!(w, "{:<22}  {:>10}  TITLE", "ID", "SCORE")?;
    for item in items {
        writeln!(w, "{:<22}  {:>10.4}  {}", item.id, item.score, item.title)?;
    }

    Ok(())
}

fn render_blocker_section(
    w: &mut dyn Write,
    blockers: &[&RankedItem],
    use_color: bool,
) -> std::io::Result<()> {
    let heading = colorize("Blockers", "33;1", use_color);
    writeln!(w, "{heading}")?;
    writeln!(w, "{:-<72}", "")?;

    if blockers.is_empty() {
        writeln!(w, "(none)")?;
        return Ok(());
    }

    writeln!(w, "{:<22}  {:>6}  {:>10}  TITLE", "ID", "BLOCKS", "SCORE")?;
    for item in blockers {
        writeln!(
            w,
            "{:<22}  {:>6}  {:>10.4}  {}",
            item.id, item.unblocks_active, item.score, item.title
        )?;
    }

    Ok(())
}

fn colorize(text: &str, color: &str, use_color: bool) -> String {
    if use_color {
        format!("\x1b[{color}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn is_small_size(size: Option<&str>) -> bool {
    matches!(size, Some("xxs") | Some("xs") | Some("s"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::model::item::Urgency;

    fn ranked(id: &str, title: &str, score: f64) -> RankedItem {
        RankedItem {
            id: id.to_string(),
            title: title.to_string(),
            size: Some("s".to_string()),
            urgency: Urgency::Default,
            score,
            explanation: "test".to_string(),
            blocked_by_active: 0,
            unblocks_active: 1,
            updated_at_us: 0,
        }
    }

    #[test]
    fn small_size_classifier_matches_expected_values() {
        assert!(is_small_size(Some("xxs")));
        assert!(is_small_size(Some("xs")));
        assert!(is_small_size(Some("s")));
        assert!(!is_small_size(Some("m")));
        assert!(!is_small_size(None));
    }

    #[test]
    fn build_rows_emits_expected_sections() {
        let top = vec![ranked("bn-top", "Top", 0.9)];
        let blocker = vec![ranked("bn-block", "Block", 0.8)];
        let quick = vec![ranked("bn-quick", "Quick", 0.7)];
        let cycles = vec![vec!["bn-c1".to_string(), "bn-c2".to_string()]];

        let top_refs: Vec<&RankedItem> = top.iter().collect();
        let blocker_refs: Vec<&RankedItem> = blocker.iter().collect();
        let quick_refs: Vec<&RankedItem> = quick.iter().collect();

        let title_map = HashMap::from([
            ("bn-top".to_string(), "Top".to_string()),
            ("bn-block".to_string(), "Block".to_string()),
            ("bn-quick".to_string(), "Quick".to_string()),
            ("bn-c1".to_string(), "Cycle One".to_string()),
            ("bn-c2".to_string(), "Cycle Two".to_string()),
        ]);
        let score_map = HashMap::from([
            ("bn-top".to_string(), 0.9),
            ("bn-block".to_string(), 0.8),
            ("bn-quick".to_string(), 0.7),
            ("bn-c1".to_string(), 0.1),
            ("bn-c2".to_string(), 0.2),
        ]);

        let rows = build_rows(
            &top_refs,
            &blocker_refs,
            &quick_refs,
            &cycles,
            &title_map,
            &score_map,
        );

        assert!(rows.iter().any(|row| row.section == "top_pick"));
        assert!(rows.iter().any(|row| row.section == "blocker"));
        assert!(rows.iter().any(|row| row.section == "quick_win"));
        assert!(rows.iter().any(|row| row.section == "cycle"));
    }
}
