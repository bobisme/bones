//! `bn cycles` — list dependency cycles (strongly connected components).

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::Path;

use bones_core::db::query;
use bones_triage::graph::{RawGraph, find_sccs};
use clap::Args;
use serde::Serialize;

use crate::output::{CliError, OutputMode, render, render_error};

/// Arguments for `bn cycles`.
#[derive(Args, Debug, Default)]
pub struct CyclesArgs {}

#[derive(Debug, Serialize)]
struct CyclesOutput {
    cycles: Vec<Vec<String>>,
}

/// Execute `bn cycles`.
pub fn run_cycles(
    _args: &CyclesArgs,
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

    let raw = RawGraph::from_sqlite(&conn)
        .map_err(|e| anyhow::anyhow!("failed to load dependency graph: {e}"))?;

    let cycles = find_sccs(&raw.graph);
    let payload = CyclesOutput { cycles };

    let cycle_titles = load_cycle_titles(&conn, &payload.cycles);

    render(output, &payload, |report, w| {
        render_cycles_human(report, &cycle_titles, w)
    })
}

fn load_cycle_titles(
    conn: &rusqlite::Connection,
    cycles: &[Vec<String>],
) -> HashMap<String, String> {
    let mut titles = HashMap::new();
    let ids: BTreeSet<&str> = cycles
        .iter()
        .flat_map(|cycle| cycle.iter().map(String::as_str))
        .collect();

    for item_id in ids {
        if let Ok(Some(item)) = query::get_item(conn, item_id, false) {
            titles.insert(item_id.to_string(), item.title);
        }
    }

    titles
}

fn render_cycles_human(
    payload: &CyclesOutput,
    titles: &HashMap<String, String>,
    w: &mut dyn Write,
) -> std::io::Result<()> {
    if payload.cycles.is_empty() {
        writeln!(w, "No dependency cycles found.")?;
        return Ok(());
    }

    writeln!(w, "Dependency cycles ({})", payload.cycles.len())?;

    for (idx, cycle) in payload.cycles.iter().enumerate() {
        writeln!(w, "\nCycle {}:", idx + 1)?;
        for item_id in cycle {
            if let Some(title) = titles.get(item_id) {
                writeln!(w, "  - {item_id} — {title}")?;
            } else {
                writeln!(w, "  - {item_id}")?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycles_args_parse_no_flags() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: CyclesArgs,
        }

        let parsed = Wrapper::parse_from(["test"]);
        let _ = parsed.args;
    }

    #[test]
    fn render_cycles_human_no_cycles() {
        let payload = CyclesOutput { cycles: Vec::new() };
        let mut out = Vec::new();

        render_cycles_human(&payload, &HashMap::new(), &mut out).expect("render");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("No dependency cycles found."));
    }

    #[test]
    fn render_cycles_human_lists_groups() {
        let payload = CyclesOutput {
            cycles: vec![vec!["bn-a".to_string(), "bn-b".to_string()]],
        };
        let titles = HashMap::from([
            ("bn-a".to_string(), "Alpha".to_string()),
            ("bn-b".to_string(), "Beta".to_string()),
        ]);

        let mut out = Vec::new();
        render_cycles_human(&payload, &titles, &mut out).expect("render");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("Cycle 1"));
        assert!(rendered.contains("bn-a — Alpha"));
        assert!(rendered.contains("bn-b — Beta"));
    }
}
