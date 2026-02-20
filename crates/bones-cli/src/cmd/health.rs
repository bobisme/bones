//! `bn health` — dependency-graph health dashboard.

use std::io::Write;
use std::path::Path;

use bones_core::db::query;
use bones_triage::graph::{RawGraph, find_sccs, health_metrics};
use clap::Args;
use serde::Serialize;

use crate::output::{CliError, OutputMode, render, render_error};

/// Arguments for `bn health`.
#[derive(Args, Debug, Default)]
pub struct HealthArgs {}

#[derive(Debug, Serialize)]
struct HealthOutput {
    density: f64,
    scc_count: usize,
    critical_path_length: usize,
    blocker_count: usize,
}

/// Execute `bn health`.
pub fn run_health(
    _args: &HealthArgs,
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

    let raw = RawGraph::from_sqlite(&conn)
        .map_err(|e| anyhow::anyhow!("failed to load dependency graph: {e}"))?;

    let metrics = health_metrics(&raw.graph);
    let cycle_count = find_sccs(&raw.graph).len();

    let payload = HealthOutput {
        density: metrics.density,
        scc_count: metrics.scc_count,
        critical_path_length: metrics.critical_path_length,
        blocker_count: metrics.blocker_count,
    };

    render(output, &payload, |report, w| {
        render_health_human(report, cycle_count, w)
    })
}

fn render_health_human(
    report: &HealthOutput,
    cycle_count: usize,
    w: &mut dyn Write,
) -> std::io::Result<()> {
    let density_status = if report.density < 0.05 {
        "✓ sparse"
    } else if report.density < 0.2 {
        "◐ moderate"
    } else {
        "⚠ dense"
    };

    let cycle_status = if cycle_count == 0 {
        "✓ acyclic"
    } else {
        "⚠ cycles present"
    };

    let critical_path_status = if report.critical_path_length <= 3 {
        "✓ short"
    } else if report.critical_path_length <= 8 {
        "◐ medium"
    } else {
        "⚠ long"
    };

    let blocker_status = if report.blocker_count == 0 {
        "✓ no blockers"
    } else if report.blocker_count <= 5 {
        "◐ manageable"
    } else {
        "⚠ many blockers"
    };

    writeln!(w, "Project health dashboard")?;
    writeln!(w, "{:<24} {:>12}  Status", "Metric", "Value")?;
    writeln!(w, "{}", "-".repeat(56))?;
    writeln!(
        w,
        "{:<24} {:>12.3}  {density_status}",
        "density", report.density
    )?;
    writeln!(
        w,
        "{:<24} {:>12}  {cycle_status}",
        "scc_count", report.scc_count
    )?;
    writeln!(
        w,
        "{:<24} {:>12}  {critical_path_status}",
        "critical_path_length", report.critical_path_length
    )?;
    writeln!(
        w,
        "{:<24} {:>12}  {blocker_status}",
        "blocker_count", report.blocker_count
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_args_parse_no_flags() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: HealthArgs,
        }

        let parsed = Wrapper::parse_from(["test"]);
        let _ = parsed.args;
    }

    #[test]
    fn render_health_human_includes_table() {
        let report = HealthOutput {
            density: 0.12,
            scc_count: 3,
            critical_path_length: 4,
            blocker_count: 2,
        };
        let mut out = Vec::new();

        render_health_human(&report, 0, &mut out).expect("render");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("Project health dashboard"));
        assert!(rendered.contains("density"));
        assert!(rendered.contains("critical_path_length"));
    }
}
