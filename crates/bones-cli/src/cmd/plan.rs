//! `bn plan` — topological execution layers for parallel work.
//!
//! - `bn plan` shows layers across all open items.
//! - `bn plan <goal-id>` restricts to open children of a goal.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::Path;

use bones_core::db::query::{self, ItemFilter, SortOrder, item_exists};
use bones_triage::graph::{self, RawGraph};
use clap::Args;
use serde::Serialize;

use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;

/// Arguments for `bn plan`.
#[derive(Args, Debug)]
pub struct PlanArgs {
    /// Optional goal ID. When provided, only open children of this goal are planned.
    pub goal_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct PlanOutput {
    layers: Vec<Vec<String>>,
}

/// Execute `bn plan`.
pub fn run_plan(args: &PlanArgs, output: OutputMode, project_root: &Path) -> anyhow::Result<()> {
    if let Some(goal_id) = &args.goal_id {
        if let Err(e) = validate::validate_item_id(goal_id) {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
        }
    }

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

    if let Some(goal_id) = &args.goal_id {
        if !item_exists(&conn, goal_id)? {
            let msg = format!("item not found: {goal_id}");
            render_error(output, &CliError::new(&msg))?;
            anyhow::bail!("{msg}");
        }
    }

    let open_items = query::list_items(
        &conn,
        &ItemFilter {
            state: Some("open".to_string()),
            parent_id: args.goal_id.clone(),
            sort: SortOrder::CreatedAsc,
            ..Default::default()
        },
    )?;

    let scoped_ids: BTreeSet<String> = open_items.iter().map(|item| item.item_id.clone()).collect();

    let layers = if scoped_ids.is_empty() {
        Vec::new()
    } else {
        let raw = RawGraph::from_sqlite(&conn)
            .map_err(|e| anyhow::anyhow!("failed to load dependency graph: {e}"))?;
        let scoped_graph = build_scoped_graph(&raw, &scoped_ids);
        graph::topological_layers(&scoped_graph, None)
    };

    let output_payload = PlanOutput { layers };

    render(output, &output_payload, |payload, w| {
        render_plan_human(payload, &open_items, args.goal_id.as_deref(), w)
    })
}

fn build_scoped_graph(raw: &RawGraph, scoped_ids: &BTreeSet<String>) -> graph::DiGraph {
    let mut graph = graph::DiGraph::new();
    let mut node_map: HashMap<String, petgraph::graph::NodeIndex> =
        HashMap::with_capacity(scoped_ids.len());

    for item_id in scoped_ids {
        let idx = graph.add_node(item_id.clone());
        node_map.insert(item_id.clone(), idx);
    }

    for from_id in scoped_ids {
        let Some(from_raw_idx) = raw.node_index(from_id) else {
            continue;
        };
        let Some(&from_scoped_idx) = node_map.get(from_id) else {
            continue;
        };

        for to_raw_idx in raw
            .graph
            .neighbors_directed(from_raw_idx, petgraph::Direction::Outgoing)
        {
            let Some(to_id) = raw.graph.node_weight(to_raw_idx) else {
                continue;
            };
            let Some(&to_scoped_idx) = node_map.get(to_id.as_str()) else {
                continue;
            };

            if !graph.contains_edge(from_scoped_idx, to_scoped_idx) {
                graph.add_edge(from_scoped_idx, to_scoped_idx, ());
            }
        }
    }

    graph
}

fn render_plan_human(
    payload: &PlanOutput,
    scoped_items: &[query::QueryItem],
    goal_id: Option<&str>,
    w: &mut dyn Write,
) -> std::io::Result<()> {
    let titles: HashMap<&str, &str> = scoped_items
        .iter()
        .map(|item| (item.item_id.as_str(), item.title.as_str()))
        .collect();

    match goal_id {
        Some(goal_id) => writeln!(w, "Parallel execution plan for goal {goal_id}")?,
        None => writeln!(w, "Parallel execution plan")?,
    }

    if payload.layers.is_empty() {
        writeln!(w, "(no open items)")?;
        return Ok(());
    }

    for (idx, layer) in payload.layers.iter().enumerate() {
        let noun = if layer.len() == 1 { "item" } else { "items" };
        writeln!(w, "\nLayer {} ({} {noun}):", idx + 1, layer.len())?;

        for item_id in layer {
            if let Some(title) = titles.get(item_id.as_str()) {
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
    use petgraph::graph::DiGraph;

    #[test]
    fn plan_args_parse_goal_id() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: PlanArgs,
        }

        let parsed = Wrapper::parse_from(["test", "bn-goal"]);
        assert_eq!(parsed.args.goal_id.as_deref(), Some("bn-goal"));
    }

    #[test]
    fn build_scoped_graph_filters_external_edges() {
        let mut raw_graph = DiGraph::<String, ()>::new();
        let a = raw_graph.add_node("bn-a".to_string());
        let b = raw_graph.add_node("bn-b".to_string());
        let c = raw_graph.add_node("bn-c".to_string());

        raw_graph.add_edge(a, b, ());
        raw_graph.add_edge(b, c, ());

        let raw = RawGraph {
            graph: raw_graph,
            node_map: HashMap::from([
                ("bn-a".to_string(), a),
                ("bn-b".to_string(), b),
                ("bn-c".to_string(), c),
            ]),
            content_hash: "blake3:test".to_string(),
        };

        let scoped_ids = BTreeSet::from(["bn-a".to_string(), "bn-b".to_string()]);
        let scoped = build_scoped_graph(&raw, &scoped_ids);

        assert_eq!(scoped.node_count(), 2);
        assert_eq!(scoped.edge_count(), 1);
    }

    #[test]
    fn render_plan_human_empty_plan() {
        let payload = PlanOutput { layers: Vec::new() };
        let mut out = Vec::new();

        render_plan_human(&payload, &[], None, &mut out).expect("render");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("Parallel execution plan"));
        assert!(rendered.contains("(no open items)"));
    }
}
