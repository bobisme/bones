use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use clap::Args;
use serde::Serialize;

use bones_core::db::query;

use crate::cmd::triage_support::{RankedItem, build_triage_snapshot};
use crate::output::{
    CliError, OutputMode, pretty_section, pretty_table, render_error, render_mode,
};

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
                    "run `bn admin rebuild` to initialize the projection",
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

    render_mode(
        output,
        &rows,
        |_, w| render_triage_text(w, &top_picks, &blockers, &quick_wins, &cycles),
        |_, w| render_triage_human(w, &top_picks, &blockers, &quick_wins, &cycles),
    )
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
) -> std::io::Result<()> {
    pretty_section(w, "Triage report")?;
    render_ranked_section(w, "Top Picks", top_picks)?;
    writeln!(w)?;
    render_blocker_section(w, blockers)?;
    writeln!(w)?;
    render_ranked_section(w, "Quick Wins", quick_wins)?;
    writeln!(w)?;
    pretty_section(w, "Cycles")?;
    if cycles.is_empty() {
        writeln!(w, "(none)")?;
    } else {
        let rows: Vec<Vec<String>> = cycles
            .iter()
            .enumerate()
            .map(|(idx, cycle)| vec![(idx + 1).to_string(), cycle.join(" -> ")])
            .collect();
        pretty_table(w, &["#", "CYCLE"], &rows)?;
    }
    Ok(())
}

fn render_ranked_section(
    w: &mut dyn Write,
    title: &str,
    items: &[&RankedItem],
) -> std::io::Result<()> {
    pretty_section(w, title)?;

    if items.is_empty() {
        writeln!(w, "(none)")?;
        return Ok(());
    }

    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            vec![
                item.id.clone(),
                format_score(item.score),
                item.title.clone(),
            ]
        })
        .collect();
    pretty_table(w, &["ID", "SCORE", "TITLE"], &rows)?;

    Ok(())
}

fn render_blocker_section(w: &mut dyn Write, blockers: &[&RankedItem]) -> std::io::Result<()> {
    pretty_section(w, "Blockers")?;

    if blockers.is_empty() {
        writeln!(w, "(none)")?;
        return Ok(());
    }

    let rows: Vec<Vec<String>> = blockers
        .iter()
        .map(|item| {
            vec![
                item.id.clone(),
                item.unblocks_active.to_string(),
                format_score(item.score),
                item.title.clone(),
            ]
        })
        .collect();
    pretty_table(w, &["ID", "BLOCKS", "SCORE", "TITLE"], &rows)?;

    Ok(())
}

fn render_triage_text(
    w: &mut dyn Write,
    top_picks: &[&RankedItem],
    blockers: &[&RankedItem],
    quick_wins: &[&RankedItem],
    cycles: &[Vec<String>],
) -> std::io::Result<()> {
    writeln!(w, "SECTION\tID\tBLOCKS\tSCORE\tTITLE")?;
    for item in top_picks {
        writeln!(
            w,
            "top_pick\t{}\t-\t{}\t{}",
            item.id,
            format_score(item.score),
            item.title.replace('\t', " ")
        )?;
    }
    for item in blockers {
        writeln!(
            w,
            "blocker\t{}\t{}\t{}\t{}",
            item.id,
            item.unblocks_active,
            format_score(item.score),
            item.title.replace('\t', " ")
        )?;
    }
    for item in quick_wins {
        writeln!(
            w,
            "quick_win\t{}\t-\t{}\t{}",
            item.id,
            format_score(item.score),
            item.title.replace('\t', " ")
        )?;
    }

    writeln!(w)?;
    writeln!(w, "CYCLES\tINDEX\tPATH")?;
    for (idx, cycle) in cycles.iter().enumerate() {
        writeln!(
            w,
            "cycle\t{}\t{}",
            idx + 1,
            cycle.join(" -> ").replace('\t', " ")
        )?;
    }
    if top_picks.is_empty() && blockers.is_empty() && quick_wins.is_empty() && cycles.is_empty() {
        writeln!(w, "advice  no-triage-items")?;
    }
    Ok(())
}

fn format_score(score: f64) -> String {
    if score == f64::MAX {
        "URGENT".to_string()
    } else if score == f64::NEG_INFINITY {
        "PUNT".to_string()
    } else {
        format!("{score:.4}")
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

    #[test]
    fn render_triage_text_includes_table_headers() {
        let top = vec![ranked("bn-top", "Top item", 0.9)];
        let blockers = vec![ranked("bn-block", "Block item", 0.8)];
        let quick = vec![ranked("bn-quick", "Quick item", 0.7)];
        let cycles = vec![vec!["bn-top".to_string(), "bn-block".to_string()]];

        let top_refs: Vec<&RankedItem> = top.iter().collect();
        let blocker_refs: Vec<&RankedItem> = blockers.iter().collect();
        let quick_refs: Vec<&RankedItem> = quick.iter().collect();

        let mut buf = Vec::new();
        render_triage_text(&mut buf, &top_refs, &blocker_refs, &quick_refs, &cycles)
            .expect("render triage text");
        let out = String::from_utf8(buf).expect("utf8");

        assert!(out.contains("SECTION\tID\tBLOCKS\tSCORE\tTITLE"));
        assert!(out.contains("CYCLES\tINDEX\tPATH"));
        assert!(out.contains("top_pick\tbn-top\t-\t0.9000\tTop item"));
    }
}
