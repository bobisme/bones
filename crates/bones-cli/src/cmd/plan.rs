//! `bn plan` — topological execution layers for parallel work.
//!
//! - `bn plan` shows layers across all open items.
//! - `bn plan <goal-id>` restricts to open children of a goal.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::Path;

use bones_core::db::query::{self, ItemFilter, SortOrder, item_exists};
use bones_triage::graph::{self, RawGraph};
use bones_triage::schedule::{ScheduleRegime, check_indexability};
use clap::Args;
use serde::Serialize;

use crate::cmd::triage_support::build_triage_snapshot;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;

/// Arguments for `bn plan`.
#[derive(Args, Debug)]
pub struct PlanArgs {
    /// Optional goal ID. When provided, only open children of this goal are planned.
    pub goal_id: Option<String>,

    /// Include dependency explanations for each layered bone.
    #[arg(long)]
    pub explain: bool,
}

#[derive(Debug, Serialize)]
struct PlanOutput {
    layers: Vec<Vec<String>>,
    explanations: HashMap<String, Vec<String>>,
    schedule_regime: Option<PlanScheduleRegime>,
}

#[derive(Debug, Serialize)]
struct PlanScheduleRegime {
    regime: String,
    detail: String,
    violations: Vec<String>,
}

/// Execute `bn plan`.
pub fn run_plan(args: &PlanArgs, output: OutputMode, project_root: &Path) -> anyhow::Result<()> {
    if let Some(goal_id) = &args.goal_id
        && let Err(e) = validate::validate_item_id(goal_id)
    {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }

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

    if let Some(goal_id) = &args.goal_id {
        if !item_exists(&conn, goal_id)? {
            let msg = format!("item not found: {goal_id}");
            render_error(output, &CliError::new(&msg))?;
            anyhow::bail!("{msg}");
        }

        let Some(item) = query::get_item(&conn, goal_id, false)? else {
            let msg = format!("item not found: {goal_id}");
            render_error(output, &CliError::new(&msg))?;
            anyhow::bail!("{msg}");
        };

        if item.kind != "goal" {
            let msg = format!(
                "triage plan requires a goal item; '{}' is kind={}",
                goal_id, item.kind
            );
            render_error(
                output,
                &CliError::with_details(
                    &msg,
                    "Use a goal ID (for example from `bn list --kind goal`) or run `bn plan` without an ID",
                    "invalid_plan_target",
                ),
            )?;
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

    let (layers, explanations, schedule_regime) = if scoped_ids.is_empty() {
        (Vec::new(), HashMap::new(), None)
    } else {
        let raw = RawGraph::from_sqlite(&conn)
            .map_err(|e| anyhow::anyhow!("failed to load dependency graph: {e}"))?;
        let scoped_graph = build_scoped_graph(&raw, &scoped_ids);
        let mut layers = graph::topological_layers(&scoped_graph, None);
        let score_map = build_score_map(&conn);
        sort_layers_by_score(&mut layers, &score_map);
        let explanations = build_layer_explanations(&scoped_graph, &layers);
        let schedule_regime = derive_schedule_regime(&scoped_graph);
        (layers, explanations, Some(schedule_regime))
    };

    let output_payload = PlanOutput {
        layers,
        explanations,
        schedule_regime,
    };

    render(output, &output_payload, |payload, w| {
        render_plan_human(
            payload,
            &open_items,
            args.goal_id.as_deref(),
            args.explain,
            w,
        )
    })
}

fn derive_schedule_regime(scoped_graph: &graph::DiGraph) -> PlanScheduleRegime {
    let indexability = check_indexability(scoped_graph);
    let regime = if indexability.indexable {
        ScheduleRegime::Whittle {
            indexability_score: 1.0,
        }
    } else {
        let reason = indexability
            .violations
            .first()
            .cloned()
            .unwrap_or_else(|| "indexability checks failed".to_string());
        ScheduleRegime::Fallback { reason }
    };

    let regime_name = if regime.is_whittle() {
        "whittle"
    } else {
        "fallback"
    }
    .to_string();

    PlanScheduleRegime {
        regime: regime_name,
        detail: regime.explain(),
        violations: indexability.violations,
    }
}

fn build_layer_explanations(
    scoped_graph: &graph::DiGraph,
    layers: &[Vec<String>],
) -> HashMap<String, Vec<String>> {
    let mut index_by_id = HashMap::new();
    for idx in scoped_graph.node_indices() {
        if let Some(id) = scoped_graph.node_weight(idx) {
            index_by_id.insert(id.clone(), idx);
        }
    }

    let mut explanations = HashMap::new();
    for layer in layers {
        for item_id in layer {
            let Some(&idx) = index_by_id.get(item_id) else {
                continue;
            };
            let mut blockers: Vec<String> = scoped_graph
                .neighbors_directed(idx, petgraph::Direction::Incoming)
                .filter_map(|n| scoped_graph.node_weight(n).cloned())
                .collect();
            blockers.sort_unstable();
            explanations.insert(item_id.clone(), blockers);
        }
    }

    explanations
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

/// Build a map of `item_id` -> composite triage score.
/// Falls back to an empty map if triage snapshot computation fails.
fn build_score_map(conn: &rusqlite::Connection) -> HashMap<String, f64> {
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0);

    match build_triage_snapshot(conn, now_us) {
        Ok(snapshot) => snapshot
            .ranked
            .into_iter()
            .map(|item| (item.id, item.score))
            .collect(),
        Err(_) => HashMap::new(),
    }
}

/// Re-sort each layer by triage score descending, with ID ascending as tie-break.
/// Items missing from the score map sort to the end.
fn sort_layers_by_score(layers: &mut [Vec<String>], score_map: &HashMap<String, f64>) {
    for layer in layers.iter_mut() {
        layer.sort_by(|a, b| {
            let sa = score_map.get(a);
            let sb = score_map.get(b);
            match (sa, sb) {
                (Some(&sa), Some(&sb)) => sb.total_cmp(&sa).then_with(|| a.cmp(b)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.cmp(b),
            }
        });
    }
}

fn render_plan_human(
    payload: &PlanOutput,
    scoped_items: &[query::QueryItem],
    goal_id: Option<&str>,
    explain: bool,
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

    if explain && let Some(regime) = &payload.schedule_regime {
        writeln!(w, "\nScheduler regime: {}", regime.detail)?;
        for violation in &regime.violations {
            writeln!(w, "  note: {violation}")?;
        }
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

            if explain {
                let blockers = payload
                    .explanations
                    .get(item_id)
                    .cloned()
                    .unwrap_or_default();
                if blockers.is_empty() {
                    writeln!(w, "    ready: no in-scope blockers")?;
                } else {
                    writeln!(w, "    depends_on: {}", blockers.join(", "))?;
                }
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
        assert!(!parsed.args.explain);
    }

    #[test]
    fn plan_args_parse_explain_flag() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: PlanArgs,
        }

        let parsed = Wrapper::parse_from(["test", "--explain"]);
        assert!(parsed.args.explain);
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
        let payload = PlanOutput {
            layers: Vec::new(),
            explanations: HashMap::new(),
            schedule_regime: None,
        };
        let mut out = Vec::new();

        render_plan_human(&payload, &[], None, false, &mut out).expect("render");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("Parallel execution plan"));
        assert!(rendered.contains("(no open items)"));
    }

    #[test]
    fn intra_layer_score_ordering() {
        // Items within the same layer should be sorted by score descending.
        let mut layers = vec![vec![
            "bn-low".to_string(),
            "bn-high".to_string(),
            "bn-mid".to_string(),
        ]];
        let scores: HashMap<String, f64> = HashMap::from([
            ("bn-low".to_string(), 1.0),
            ("bn-mid".to_string(), 5.0),
            ("bn-high".to_string(), 10.0),
        ]);

        sort_layers_by_score(&mut layers, &scores);

        assert_eq!(layers[0], vec!["bn-high", "bn-mid", "bn-low"]);
    }

    #[test]
    fn tiebreak_determinism() {
        // Items with equal scores should be ordered by ID ascending.
        let mut layers = vec![vec![
            "bn-charlie".to_string(),
            "bn-alpha".to_string(),
            "bn-bravo".to_string(),
        ]];
        let scores: HashMap<String, f64> = HashMap::from([
            ("bn-alpha".to_string(), 5.0),
            ("bn-bravo".to_string(), 5.0),
            ("bn-charlie".to_string(), 5.0),
        ]);

        sort_layers_by_score(&mut layers, &scores);

        assert_eq!(layers[0], vec!["bn-alpha", "bn-bravo", "bn-charlie"]);
    }

    #[test]
    fn layer_membership_unchanged() {
        // Sorting should not move items between layers.
        let mut layers = vec![
            vec!["bn-a".to_string(), "bn-b".to_string()],
            vec!["bn-c".to_string(), "bn-d".to_string()],
        ];
        let scores: HashMap<String, f64> = HashMap::from([
            ("bn-a".to_string(), 1.0),
            ("bn-b".to_string(), 10.0),
            ("bn-c".to_string(), 20.0),
            ("bn-d".to_string(), 2.0),
        ]);

        sort_layers_by_score(&mut layers, &scores);

        let layer0: BTreeSet<String> = layers[0].iter().cloned().collect();
        let layer1: BTreeSet<String> = layers[1].iter().cloned().collect();
        assert_eq!(
            layer0,
            BTreeSet::from(["bn-a".to_string(), "bn-b".to_string()])
        );
        assert_eq!(
            layer1,
            BTreeSet::from(["bn-c".to_string(), "bn-d".to_string()])
        );
    }

    #[test]
    fn missing_scores_sort_to_end() {
        // Items not in the score map should appear after scored items.
        let mut layers = vec![vec![
            "bn-unknown".to_string(),
            "bn-scored".to_string(),
            "bn-also-unknown".to_string(),
        ]];
        let scores: HashMap<String, f64> = HashMap::from([("bn-scored".to_string(), 5.0)]);

        sort_layers_by_score(&mut layers, &scores);

        assert_eq!(layers[0][0], "bn-scored");
        // The two unknowns should be sorted by ID ascending at the end.
        assert_eq!(layers[0][1], "bn-also-unknown");
        assert_eq!(layers[0][2], "bn-unknown");
    }

    #[test]
    fn urgent_ready_blocker_first_in_layer() {
        // An urgent item with a high score should appear first in its layer.
        let mut layers = vec![vec![
            "bn-normal".to_string(),
            "bn-urgent".to_string(),
            "bn-low".to_string(),
        ]];
        let scores: HashMap<String, f64> = HashMap::from([
            ("bn-normal".to_string(), 3.0),
            ("bn-urgent".to_string(), 9.0),
            ("bn-low".to_string(), 1.0),
        ]);

        sort_layers_by_score(&mut layers, &scores);

        assert_eq!(layers[0][0], "bn-urgent");
    }

    #[test]
    fn dependency_layers_preserved() {
        // Sorting within layers must not alter the layer structure produced by
        // topological_layers. Build a graph A -> B -> C, verify three layers,
        // then sort - layer count and membership must remain.
        let mut graph = graph::DiGraph::new();
        let a = graph.add_node("bn-a".to_string());
        let b = graph.add_node("bn-b".to_string());
        let c = graph.add_node("bn-c".to_string());
        graph.add_edge(a, b, ());
        graph.add_edge(b, c, ());

        let mut layers = graph::topological_layers(&graph, None);
        assert_eq!(layers.len(), 3, "three-node chain should produce 3 layers");

        let original_membership: Vec<BTreeSet<String>> =
            layers.iter().map(|l| l.iter().cloned().collect()).collect();

        let scores: HashMap<String, f64> = HashMap::from([
            ("bn-a".to_string(), 1.0),
            ("bn-b".to_string(), 5.0),
            ("bn-c".to_string(), 10.0),
        ]);
        sort_layers_by_score(&mut layers, &scores);

        let sorted_membership: Vec<BTreeSet<String>> =
            layers.iter().map(|l| l.iter().cloned().collect()).collect();
        assert_eq!(original_membership, sorted_membership);
    }

    #[test]
    fn derive_schedule_regime_reports_fallback_for_cycle() {
        let mut graph = graph::DiGraph::new();
        let a = graph.add_node("bn-a".to_string());
        let b = graph.add_node("bn-b".to_string());
        graph.add_edge(a, b, ());
        graph.add_edge(b, a, ());

        let regime = derive_schedule_regime(&graph);
        assert_eq!(regime.regime, "fallback");
        assert!(!regime.violations.is_empty());
    }
}
