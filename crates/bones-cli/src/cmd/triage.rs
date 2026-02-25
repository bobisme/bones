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
/// Produces six sections:
/// - Top Picks
/// - Actionable Blockers (ready items that unblock others)
/// - Blocked Hubs (blocked items that also unblock others)
/// - Quick Wins
/// - Needs Decomposition (L/XL tasks with no children)
/// - Cycles
pub fn run_triage(
    _args: &TriageArgs,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let db_path = project_root.join(".bones/bones.db");
    let conn = if let Some(conn) = query::try_open_projection(&db_path)? { conn } else {
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

    let top_picks: Vec<&RankedItem> = snapshot.unblocked_ranked.iter().take(5).collect();

    let mut actionable_blockers: Vec<&RankedItem> = snapshot
        .ranked
        .iter()
        .filter(|item| item.blocked_by_active == 0 && item.unblocks_active > 0)
        .collect();
    actionable_blockers.sort_by(|a, b| {
        b.unblocks_active
            .cmp(&a.unblocks_active)
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| a.id.cmp(&b.id))
    });
    actionable_blockers.truncate(5);

    let mut blocked_hubs: Vec<&RankedItem> = snapshot
        .ranked
        .iter()
        .filter(|item| item.unblocks_active > 0 && item.blocked_by_active > 0)
        .collect();
    blocked_hubs.sort_by(|a, b| {
        b.unblocks_active
            .cmp(&a.unblocks_active)
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| a.id.cmp(&b.id))
    });
    blocked_hubs.truncate(5);

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

    let needs_decomposition: Vec<&RankedItem> = snapshot.needs_decomposition.iter().collect();

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
        &actionable_blockers,
        &blocked_hubs,
        &quick_wins,
        &needs_decomposition,
        &cycles,
        &title_map,
        &score_map,
    );

    render_mode(
        output,
        &rows,
        |_, w| {
            render_triage_text(
                w,
                &top_picks,
                &actionable_blockers,
                &blocked_hubs,
                &quick_wins,
                &needs_decomposition,
                &cycles,
            )
        },
        |_, w| {
            render_triage_human(
                w,
                &top_picks,
                &actionable_blockers,
                &blocked_hubs,
                &quick_wins,
                &needs_decomposition,
                &cycles,
            )
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn build_rows(
    top_picks: &[&RankedItem],
    actionable_blockers: &[&RankedItem],
    blocked_hubs: &[&RankedItem],
    quick_wins: &[&RankedItem],
    needs_decomposition: &[&RankedItem],
    cycles: &[Vec<String>],
    title_map: &HashMap<String, String>,
    score_map: &HashMap<String, f64>,
) -> Vec<TriageRow> {
    let mut rows = Vec::new();

    push_rows(&mut rows, top_picks, "top_pick");
    push_rows(&mut rows, actionable_blockers, "actionable_blocker");
    push_rows(&mut rows, blocked_hubs, "blocked_hub");
    push_rows(&mut rows, quick_wins, "quick_win");
    push_rows(&mut rows, needs_decomposition, "needs_decomposition");

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
    actionable_blockers: &[&RankedItem],
    blocked_hubs: &[&RankedItem],
    quick_wins: &[&RankedItem],
    needs_decomposition: &[&RankedItem],
    cycles: &[Vec<String>],
) -> std::io::Result<()> {
    pretty_section(w, "Triage report")?;
    render_ranked_section(w, "Top Picks", top_picks)?;
    writeln!(w)?;
    render_actionable_blocker_section(w, actionable_blockers)?;
    writeln!(w)?;
    render_hub_section(w, blocked_hubs)?;
    writeln!(w)?;
    render_ranked_section(w, "Quick Wins", quick_wins)?;
    writeln!(w)?;
    render_decomposition_section(w, needs_decomposition)?;
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

fn render_actionable_blocker_section(
    w: &mut dyn Write,
    items: &[&RankedItem],
) -> std::io::Result<()> {
    pretty_section(w, "Actionable Blockers")?;

    if items.is_empty() {
        writeln!(w, "(none)")?;
        return Ok(());
    }

    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            vec![
                item.id.clone(),
                format!("ready; unblocks {}", item.unblocks_active),
                format_score(item.score),
                item.title.clone(),
            ]
        })
        .collect();
    pretty_table(w, &["ID", "STATUS", "SCORE", "TITLE"], &rows)?;

    Ok(())
}

fn render_hub_section(w: &mut dyn Write, items: &[&RankedItem]) -> std::io::Result<()> {
    pretty_section(w, "Blocked Hubs")?;

    if items.is_empty() {
        writeln!(w, "(none)")?;
        return Ok(());
    }

    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            vec![
                item.id.clone(),
                format!(
                    "blocked by {}; unblocks {}",
                    item.blocked_by_active, item.unblocks_active
                ),
                format_score(item.score),
                item.title.clone(),
            ]
        })
        .collect();
    pretty_table(w, &["ID", "STATUS", "SCORE", "TITLE"], &rows)?;

    Ok(())
}

fn render_decomposition_section(w: &mut dyn Write, items: &[&RankedItem]) -> std::io::Result<()> {
    pretty_section(w, "Needs Decomposition")?;

    if items.is_empty() {
        writeln!(w, "(none)")?;
        return Ok(());
    }

    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            vec![
                item.id.clone(),
                format!("{}; no children", item.size.as_deref().unwrap_or("?")),
                format_score(item.score),
                item.title.clone(),
            ]
        })
        .collect();
    pretty_table(w, &["ID", "STATUS", "SCORE", "TITLE"], &rows)?;

    Ok(())
}

fn render_triage_text(
    w: &mut dyn Write,
    top_picks: &[&RankedItem],
    actionable_blockers: &[&RankedItem],
    blocked_hubs: &[&RankedItem],
    quick_wins: &[&RankedItem],
    needs_decomposition: &[&RankedItem],
    cycles: &[Vec<String>],
) -> std::io::Result<()> {
    writeln!(w, "SECTION\tID\tSTATUS\tSCORE\tTITLE")?;
    for item in top_picks {
        writeln!(
            w,
            "top_pick\t{}\t-\t{}\t{}",
            item.id,
            format_score(item.score),
            item.title.replace('\t', " ")
        )?;
    }
    for item in actionable_blockers {
        writeln!(
            w,
            "actionable_blocker\t{}\tready; unblocks {}\t{}\t{}",
            item.id,
            item.unblocks_active,
            format_score(item.score),
            item.title.replace('\t', " ")
        )?;
    }
    for item in blocked_hubs {
        writeln!(
            w,
            "blocked_hub\t{}\tblocked by {}; unblocks {}\t{}\t{}",
            item.id,
            item.blocked_by_active,
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
    for item in needs_decomposition {
        writeln!(
            w,
            "needs_decomposition\t{}\t{}; no children\t{}\t{}",
            item.id,
            item.size.as_deref().unwrap_or("?"),
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
    if top_picks.is_empty()
        && actionable_blockers.is_empty()
        && blocked_hubs.is_empty()
        && quick_wins.is_empty()
        && needs_decomposition.is_empty()
        && cycles.is_empty()
    {
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
    matches!(size, Some("xs" | "s"))
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

    fn ranked_hub(id: &str, title: &str, score: f64) -> RankedItem {
        RankedItem {
            id: id.to_string(),
            title: title.to_string(),
            size: Some("s".to_string()),
            urgency: Urgency::Default,
            score,
            explanation: "test".to_string(),
            blocked_by_active: 2,
            unblocks_active: 3,
            updated_at_us: 0,
        }
    }

    fn ranked_decomp(id: &str, title: &str, score: f64, size: &str) -> RankedItem {
        RankedItem {
            id: id.to_string(),
            title: title.to_string(),
            size: Some(size.to_string()),
            urgency: Urgency::Default,
            score,
            explanation: "test".to_string(),
            blocked_by_active: 0,
            unblocks_active: 0,
            updated_at_us: 0,
        }
    }

    #[test]
    fn small_size_classifier_matches_expected_values() {
        assert!(is_small_size(Some("xs")));
        assert!(is_small_size(Some("s")));
        assert!(!is_small_size(Some("m")));
        assert!(!is_small_size(None));
    }

    #[test]
    fn build_rows_emits_expected_sections() {
        let top = vec![ranked("bn-top", "Top", 0.9)];
        let actionable = vec![ranked("bn-block", "Block", 0.8)];
        let hub = vec![ranked_hub("bn-hub", "Hub", 0.6)];
        let quick = vec![ranked("bn-quick", "Quick", 0.7)];
        let decomp = vec![ranked_decomp("bn-decomp", "Decomp", 0.5, "xl")];
        let cycles = vec![vec!["bn-c1".to_string(), "bn-c2".to_string()]];

        let top_refs: Vec<&RankedItem> = top.iter().collect();
        let actionable_refs: Vec<&RankedItem> = actionable.iter().collect();
        let hub_refs: Vec<&RankedItem> = hub.iter().collect();
        let quick_refs: Vec<&RankedItem> = quick.iter().collect();
        let decomp_refs: Vec<&RankedItem> = decomp.iter().collect();

        let title_map = HashMap::from([
            ("bn-top".to_string(), "Top".to_string()),
            ("bn-block".to_string(), "Block".to_string()),
            ("bn-hub".to_string(), "Hub".to_string()),
            ("bn-quick".to_string(), "Quick".to_string()),
            ("bn-decomp".to_string(), "Decomp".to_string()),
            ("bn-c1".to_string(), "Cycle One".to_string()),
            ("bn-c2".to_string(), "Cycle Two".to_string()),
        ]);
        let score_map = HashMap::from([
            ("bn-top".to_string(), 0.9),
            ("bn-block".to_string(), 0.8),
            ("bn-hub".to_string(), 0.6),
            ("bn-quick".to_string(), 0.7),
            ("bn-decomp".to_string(), 0.5),
            ("bn-c1".to_string(), 0.1),
            ("bn-c2".to_string(), 0.2),
        ]);

        let rows = build_rows(
            &top_refs,
            &actionable_refs,
            &hub_refs,
            &quick_refs,
            &decomp_refs,
            &cycles,
            &title_map,
            &score_map,
        );

        assert!(rows.iter().any(|row| row.section == "top_pick"));
        assert!(rows.iter().any(|row| row.section == "actionable_blocker"));
        assert!(rows.iter().any(|row| row.section == "blocked_hub"));
        assert!(rows.iter().any(|row| row.section == "quick_win"));
        assert!(rows.iter().any(|row| row.section == "needs_decomposition"));
        assert!(rows.iter().any(|row| row.section == "cycle"));
    }

    #[test]
    fn render_triage_text_includes_table_headers() {
        let top = vec![ranked("bn-top", "Top item", 0.9)];
        let actionable = vec![ranked("bn-act", "Actionable item", 0.8)];
        let hubs = vec![ranked_hub("bn-hub", "Hub item", 0.6)];
        let quick = vec![ranked("bn-quick", "Quick item", 0.7)];
        let decomp = vec![ranked_decomp("bn-big", "Big item", 0.5, "xl")];
        let cycles = vec![vec!["bn-top".to_string(), "bn-act".to_string()]];

        let top_refs: Vec<&RankedItem> = top.iter().collect();
        let actionable_refs: Vec<&RankedItem> = actionable.iter().collect();
        let hub_refs: Vec<&RankedItem> = hubs.iter().collect();
        let quick_refs: Vec<&RankedItem> = quick.iter().collect();
        let decomp_refs: Vec<&RankedItem> = decomp.iter().collect();

        let mut buf = Vec::new();
        render_triage_text(
            &mut buf,
            &top_refs,
            &actionable_refs,
            &hub_refs,
            &quick_refs,
            &decomp_refs,
            &cycles,
        )
        .expect("render triage text");
        let out = String::from_utf8(buf).expect("utf8");

        assert!(out.contains("SECTION\tID\tSTATUS\tSCORE\tTITLE"));
        assert!(out.contains("CYCLES\tINDEX\tPATH"));
        assert!(out.contains("top_pick\tbn-top\t-\t0.9000\tTop item"));
        assert!(out.contains("needs_decomposition\tbn-big\txl; no children\t0.5000\tBig item"));
    }

    #[test]
    fn actionable_blockers_separated_from_blocked_hubs() {
        let actionable_item = ranked("bn-act", "Actionable", 0.9);
        let hub_item = ranked_hub("bn-hub", "Hub", 0.7);

        // Verify actionable blocker: blocked_by_active == 0, unblocks_active > 0
        assert_eq!(actionable_item.blocked_by_active, 0);
        assert!(actionable_item.unblocks_active > 0);

        // Verify blocked hub: blocked_by_active > 0, unblocks_active > 0
        assert!(hub_item.blocked_by_active > 0);
        assert!(hub_item.unblocks_active > 0);

        let actionable_vec = vec![actionable_item];
        let hub_vec = vec![hub_item];
        let empty: Vec<RankedItem> = vec![];

        let actionable_refs: Vec<&RankedItem> = actionable_vec.iter().collect();
        let hub_refs: Vec<&RankedItem> = hub_vec.iter().collect();
        let empty_refs: Vec<&RankedItem> = empty.iter().collect();

        let title_map = HashMap::from([
            ("bn-act".to_string(), "Actionable".to_string()),
            ("bn-hub".to_string(), "Hub".to_string()),
        ]);
        let score_map = HashMap::from([("bn-act".to_string(), 0.9), ("bn-hub".to_string(), 0.7)]);

        let rows = build_rows(
            &empty_refs,
            &actionable_refs,
            &hub_refs,
            &empty_refs,
            &empty_refs,
            &[],
            &title_map,
            &score_map,
        );

        let actionable_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.section == "actionable_blocker")
            .collect();
        let hub_rows: Vec<_> = rows.iter().filter(|r| r.section == "blocked_hub").collect();

        assert_eq!(actionable_rows.len(), 1);
        assert_eq!(actionable_rows[0].id, "bn-act");
        assert_eq!(hub_rows.len(), 1);
        assert_eq!(hub_rows[0].id, "bn-hub");

        // Verify human rendering includes both sections
        let mut buf = Vec::new();
        render_triage_human(
            &mut buf,
            &empty_refs,
            &actionable_refs,
            &hub_refs,
            &empty_refs,
            &empty_refs,
            &[],
        )
        .expect("render human");
        let out = String::from_utf8(buf).expect("utf8");

        assert!(out.contains("Actionable Blockers"));
        assert!(out.contains("Blocked Hubs"));
        assert!(out.contains("ready; unblocks 1"));
        assert!(out.contains("blocked by 2; unblocks 3"));
    }
}
