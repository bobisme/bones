//! `bn graph` — dependency graph visualization.
//!
//! - `bn graph <id>` — show the dependency subgraph rooted at an item
//! - `bn graph`      — show the full project dependency graph summary
//!
//! # Edge Direction
//!
//! In bones, a "blocks" edge means "blocker → blocked".
//! An item's *upstream* (blocked-by / --up) lists what must complete first.
//! An item's *downstream* (blocks / --down) lists what this item unlocks.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as FmtWrite;
use std::path::Path;

use clap::Args;
use serde::Serialize;
use serde_json::json;

use bones_core::db::query::{ItemFilter, get_item, item_exists, list_items, try_open_projection};
use bones_triage::graph::{
    build::RawGraph, find_all_cycles, normalize::NormalizedGraph, stats::GraphStats,
};

use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;

// ---------------------------------------------------------------------------
// Clap types
// ---------------------------------------------------------------------------

/// Arguments for `bn graph`.
#[derive(Args, Debug)]
pub struct GraphArgs {
    /// Item ID to show graph for. If omitted, shows the project summary.
    pub id: Option<String>,

    /// Only show downstream items (what this item blocks).
    #[arg(long)]
    pub down: bool,

    /// Only show upstream items (what blocks this item).
    #[arg(long)]
    pub up: bool,

    /// Maximum traversal depth (default: unlimited).
    #[arg(long)]
    pub depth: Option<usize>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_bones_dir(start: &Path) -> Option<std::path::PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let candidate = current.join(".bones");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Metadata about an item for display purposes.
#[derive(Debug, Clone)]
struct ItemMeta {
    title: String,
    state: String,
}

impl ItemMeta {
    fn display_state(&self) -> &str {
        match self.state.as_str() {
            "done" | "archived" => "✓",
            "doing" => "→",
            _ => " ",
        }
    }
}

/// Load title/state metadata for all items in the graph (best-effort).
fn load_item_meta(
    conn: &rusqlite::Connection,
    ids: impl Iterator<Item = String>,
) -> HashMap<String, ItemMeta> {
    let mut map = HashMap::new();
    for id in ids {
        if let Ok(Some(item)) = get_item(conn, &id, false) {
            map.insert(
                id,
                ItemMeta {
                    title: item.title,
                    state: item.state,
                },
            );
        }
    }
    map
}

// ---------------------------------------------------------------------------
// ASCII tree rendering
// ---------------------------------------------------------------------------

/// Render an ASCII tree rooted at `root_id` in the given direction.
///
/// `graph` is the raw dependency graph where edge A → B means "A blocks B".
///
/// - `Outgoing` = downstream (what `root_id` blocks)
/// - `Incoming` = upstream (what blocks `root_id`)
fn render_tree(
    raw: &RawGraph,
    meta: &HashMap<String, ItemMeta>,
    root_id: &str,
    direction: petgraph::Direction,
    depth_limit: Option<usize>,
    out: &mut String,
) {
    let root_idx = match raw.node_index(root_id) {
        Some(idx) => idx,
        None => {
            let _ = writeln!(out, "  (no connections)");
            return;
        }
    };

    // Check if there are any edges in this direction
    let neighbors: Vec<_> = raw.graph.neighbors_directed(root_idx, direction).collect();

    if neighbors.is_empty() {
        let _ = writeln!(out, "  (none)");
        return;
    }

    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(root_id.to_string());

    let mut sorted_neighbors: Vec<String> = neighbors
        .into_iter()
        .filter_map(|idx| raw.graph.node_weight(idx).cloned())
        .collect();
    sorted_neighbors.sort();

    render_tree_nodes(
        raw,
        meta,
        &sorted_neighbors,
        direction,
        depth_limit,
        0,
        &mut visited,
        "  ",
        out,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_tree_nodes(
    raw: &RawGraph,
    meta: &HashMap<String, ItemMeta>,
    ids: &[String],
    direction: petgraph::Direction,
    depth_limit: Option<usize>,
    current_depth: usize,
    visited: &mut HashSet<String>,
    prefix: &str,
    out: &mut String,
) {
    let count = ids.len();
    for (i, id) in ids.iter().enumerate() {
        let is_last = i + 1 == count;
        let connector = if is_last { "└── " } else { "├── " };
        let child_prefix = format!("{}{}", prefix, if is_last { "    " } else { "│   " });

        let state_mark = meta.get(id).map(|m| m.display_state()).unwrap_or(" ");
        let title = meta
            .get(id)
            .map(|m| format!(" — {}", m.title))
            .unwrap_or_default();

        if visited.contains(id) {
            let _ = writeln!(
                out,
                "{prefix}{connector}[{state_mark}] {id}{title} [⟳ cycle]"
            );
        } else {
            let _ = writeln!(out, "{prefix}{connector}[{state_mark}] {id}{title}");
            visited.insert(id.to_string());

            // Recurse if within depth limit
            let at_limit = depth_limit.is_some_and(|d| current_depth + 1 >= d);
            if !at_limit {
                if let Some(idx) = raw.node_index(id) {
                    let mut child_ids: Vec<String> = raw
                        .graph
                        .neighbors_directed(idx, direction)
                        .filter_map(|n| raw.graph.node_weight(n).cloned())
                        .collect();
                    child_ids.sort();

                    if !child_ids.is_empty() {
                        render_tree_nodes(
                            raw,
                            meta,
                            &child_ids,
                            direction,
                            depth_limit,
                            current_depth + 1,
                            visited,
                            &child_prefix,
                            out,
                        );
                    }
                }
            } else if let Some(idx) = raw.node_index(id) {
                // Check if there are more children we're not showing
                let child_count = raw.graph.neighbors_directed(idx, direction).count();
                if child_count > 0 {
                    let _ = writeln!(
                        out,
                        "{child_prefix}└── … {child_count} more (use --depth to increase)"
                    );
                }
            }

            visited.remove(id);
        }
    }
}

// ---------------------------------------------------------------------------
// Command runner
// ---------------------------------------------------------------------------

pub fn run_graph(args: &GraphArgs, output: OutputMode, project_root: &Path) -> anyhow::Result<()> {
    // Validate item ID if given
    if let Some(ref id) = args.id {
        if let Err(e) = validate::validate_item_id(id) {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
        }
    }

    // Find .bones dir
    let bones_dir = find_bones_dir(project_root).ok_or_else(|| {
        let msg = "Not a bones project: .bones directory not found";
        render_error(
            output,
            &CliError::with_details(
                msg,
                "Run 'bn init' to create a new project",
                "not_a_project",
            ),
        )
        .ok();
        anyhow::anyhow!("{}", msg)
    })?;

    let db_path = bones_dir.join("bones.db");
    let conn = match try_open_projection(&db_path)? {
        Some(c) => c,
        None => {
            let msg = "projection database not found; run `bn rebuild` first";
            render_error(output, &CliError::new(msg))?;
            anyhow::bail!("{}", msg);
        }
    };

    // Build raw graph
    let raw = RawGraph::from_sqlite(&conn)
        .map_err(|e| anyhow::anyhow!("failed to load dependency graph: {e}"))?;

    match &args.id {
        Some(id) => run_graph_item(&raw, &conn, id, args, output),
        None => run_graph_summary(&raw, &conn, args, output),
    }
}

/// Show the dependency subgraph for a single item.
fn run_graph_item(
    raw: &RawGraph,
    conn: &rusqlite::Connection,
    id: &str,
    args: &GraphArgs,
    output: OutputMode,
) -> anyhow::Result<()> {
    // Verify item exists
    if !item_exists(conn, id)? {
        let msg = format!("item not found: {id}");
        render_error(output, &CliError::new(&msg))?;
        anyhow::bail!("{}", msg);
    }

    // Load metadata for all nodes
    let all_ids: Vec<String> = raw.graph.node_weights().cloned().collect();
    let meta = load_item_meta(conn, all_ids.into_iter());

    // Determine which directions to show
    let show_up = !args.down; // show upstream unless --down only
    let show_down = !args.up; // show downstream unless --up only

    if output.is_json() {
        // JSON output: return lists of up/down deps
        let up_ids: Vec<String> = if show_up {
            if let Some(idx) = raw.node_index(id) {
                let mut ids: Vec<String> = raw
                    .graph
                    .neighbors_directed(idx, petgraph::Direction::Incoming)
                    .filter_map(|n| raw.graph.node_weight(n).cloned())
                    .collect();
                ids.sort();
                ids
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        let down_ids: Vec<String> = if show_down {
            if let Some(idx) = raw.node_index(id) {
                let mut ids: Vec<String> = raw
                    .graph
                    .neighbors_directed(idx, petgraph::Direction::Outgoing)
                    .filter_map(|n| raw.graph.node_weight(n).cloned())
                    .collect();
                ids.sort();
                ids
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        let root_meta = meta.get(id);
        let val = json!({
            "id": id,
            "title": root_meta.map(|m| m.title.as_str()).unwrap_or(""),
            "state": root_meta.map(|m| m.state.as_str()).unwrap_or(""),
            "blocked_by": up_ids,
            "blocks": down_ids,
        });
        render(output, &val, |_, _| Ok(()))?;
        return Ok(());
    }

    // Human output
    let root_meta = meta.get(id);
    let title_str = root_meta
        .map(|m| format!(" — {}", m.title))
        .unwrap_or_default();
    let state_str = root_meta
        .map(|m| format!("[{}]", m.state))
        .unwrap_or_default();

    let mut out = String::new();
    let _ = writeln!(out, "{id}{title_str} {state_str}");

    if show_up {
        let _ = writeln!(out, "\nblocked by (must complete first):");
        render_tree(
            raw,
            &meta,
            id,
            petgraph::Direction::Incoming,
            args.depth,
            &mut out,
        );
    }

    if show_down {
        let _ = writeln!(out, "\nblocks (waiting for this):");
        render_tree(
            raw,
            &meta,
            id,
            petgraph::Direction::Outgoing,
            args.depth,
            &mut out,
        );
    }

    print!("{out}");
    Ok(())
}

/// Show the full project dependency graph summary.
fn run_graph_summary(
    raw: &RawGraph,
    conn: &rusqlite::Connection,
    _args: &GraphArgs,
    output: OutputMode,
) -> anyhow::Result<()> {
    let ng = NormalizedGraph::from_raw(RawGraph::from_sqlite(conn)?);
    let stats = GraphStats::from_normalized(&ng);

    // Find cycles
    let cycles = find_all_cycles(&raw.graph);

    // Top items by out-degree (most blocker = bottlenecks)
    let mut by_in_degree: Vec<(String, usize)> = raw
        .graph
        .node_indices()
        .map(|idx| {
            let id = raw.graph.node_weight(idx).cloned().unwrap_or_default();
            let in_deg = raw
                .graph
                .neighbors_directed(idx, petgraph::Direction::Incoming)
                .count();
            (id, in_deg)
        })
        .filter(|(_, d)| *d > 0)
        .collect();
    by_in_degree.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let bottlenecks: Vec<_> = by_in_degree.into_iter().take(5).collect();

    // Top items by out-degree (most items blocked = high-value targets)
    let mut by_out_degree: Vec<(String, usize)> = raw
        .graph
        .node_indices()
        .map(|idx| {
            let id = raw.graph.node_weight(idx).cloned().unwrap_or_default();
            let out_deg = raw
                .graph
                .neighbors_directed(idx, petgraph::Direction::Outgoing)
                .count();
            (id, out_deg)
        })
        .filter(|(_, d)| *d > 0)
        .collect();
    by_out_degree.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let high_value: Vec<_> = by_out_degree.into_iter().take(5).collect();

    if output.is_json() {
        let val = json!({
            "items": stats.node_count,
            "blocking_edges": stats.edge_count,
            "connected_components": stats.weakly_connected_component_count,
            "cycles": stats.cycle_count,
            "cycle_members": cycles,
            "bottlenecks": bottlenecks.iter().map(|(id, deg)| json!({"id": id, "blocked_by_count": deg})).collect::<Vec<_>>(),
            "high_value": high_value.iter().map(|(id, deg)| json!({"id": id, "blocks_count": deg})).collect::<Vec<_>>(),
            "density": stats.density,
        });
        render(output, &val, |_, _| Ok(()))?;
        return Ok(());
    }

    // Human output
    let mut out = String::new();
    let _ = writeln!(out, "Project dependency graph");
    let _ = writeln!(out, "  items:                {}", stats.node_count);
    let _ = writeln!(out, "  blocking edges:       {}", stats.edge_count);
    let _ = writeln!(
        out,
        "  connected components: {}",
        stats.weakly_connected_component_count
    );
    let _ = writeln!(out, "  cycles:               {}", stats.cycle_count);

    if !cycles.is_empty() {
        let _ = writeln!(out, "\n  ⚠ cycles detected:");
        for cycle in &cycles {
            let _ = writeln!(out, "    {}", cycle.join(" → "));
        }
    }

    if !bottlenecks.is_empty() {
        let _ = writeln!(out, "\n  bottlenecks (most blocked-by):");
        for (id, deg) in &bottlenecks {
            let _ = writeln!(out, "    {id} ({deg} incoming)");
        }
    }

    if !high_value.is_empty() {
        let _ = writeln!(out, "\n  high-value targets (unblocks most):");
        for (id, deg) in &high_value {
            let _ = writeln!(out, "    {id} (blocks {deg})");
        }
    }

    if stats.edge_count == 0 {
        let _ = writeln!(out, "\n  (no blocking dependencies defined)");
    }

    print!("{out}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_args_no_id_defaults() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: GraphArgs,
        }

        let w = Wrapper::parse_from(["test"]);
        assert!(w.args.id.is_none());
        assert!(!w.args.down);
        assert!(!w.args.up);
        assert!(w.args.depth.is_none());
    }

    #[test]
    fn graph_args_with_id_and_flags() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: GraphArgs,
        }

        let w = Wrapper::parse_from(["test", "bn-abc", "--down", "--depth", "3"]);
        assert_eq!(w.args.id.as_deref(), Some("bn-abc"));
        assert!(w.args.down);
        assert!(!w.args.up);
        assert_eq!(w.args.depth, Some(3));
    }

    #[test]
    fn render_tree_empty_graph() {
        let raw = RawGraph {
            graph: petgraph::graph::DiGraph::new(),
            node_map: std::collections::HashMap::new(),
            content_hash: "blake3:test".into(),
        };
        let meta = HashMap::new();
        let mut out = String::new();
        render_tree(
            &raw,
            &meta,
            "bn-missing",
            petgraph::Direction::Outgoing,
            None,
            &mut out,
        );
        assert!(out.contains("(no connections)"));
    }

    #[test]
    fn render_tree_single_edge() {
        use petgraph::graph::DiGraph;
        use std::collections::HashMap as HM;

        let mut graph = DiGraph::<String, ()>::new();
        let a = graph.add_node("bn-aaa".into());
        let b = graph.add_node("bn-bbb".into());
        graph.add_edge(a, b, ()); // a blocks b

        let mut node_map = HM::new();
        node_map.insert("bn-aaa".into(), a);
        node_map.insert("bn-bbb".into(), b);

        let raw = RawGraph {
            graph,
            node_map,
            content_hash: "blake3:test".into(),
        };
        let meta = HashMap::new();

        // downstream of a: should show b
        let mut out = String::new();
        render_tree(
            &raw,
            &meta,
            "bn-aaa",
            petgraph::Direction::Outgoing,
            None,
            &mut out,
        );
        assert!(out.contains("bn-bbb"), "should show downstream: {out}");

        // upstream of b: should show a
        let mut out2 = String::new();
        render_tree(
            &raw,
            &meta,
            "bn-bbb",
            petgraph::Direction::Incoming,
            None,
            &mut out2,
        );
        assert!(out2.contains("bn-aaa"), "should show upstream: {out2}");
    }

    #[test]
    fn render_tree_depth_limit() {
        use petgraph::graph::DiGraph;
        use std::collections::HashMap as HM;

        // A → B → C (chain)
        let mut graph = DiGraph::<String, ()>::new();
        let a = graph.add_node("bn-aaa".into());
        let b = graph.add_node("bn-bbb".into());
        let c = graph.add_node("bn-ccc".into());
        graph.add_edge(a, b, ());
        graph.add_edge(b, c, ());

        let mut node_map = HM::new();
        node_map.insert("bn-aaa".into(), a);
        node_map.insert("bn-bbb".into(), b);
        node_map.insert("bn-ccc".into(), c);

        let raw = RawGraph {
            graph,
            node_map,
            content_hash: "blake3:test".into(),
        };
        let meta = HashMap::new();

        // With depth=1: should see b but not c
        let mut out = String::new();
        render_tree(
            &raw,
            &meta,
            "bn-aaa",
            petgraph::Direction::Outgoing,
            Some(1),
            &mut out,
        );
        assert!(out.contains("bn-bbb"), "should show b at depth 0: {out}");
        assert!(
            !out.contains("bn-ccc"),
            "should NOT show c at depth 1: {out}"
        );
    }

    #[test]
    fn graph_summary_empty_project() {
        use bones_core::db::migrations;
        use rusqlite::Connection;

        let mut conn = Connection::open_in_memory().expect("in-memory db");
        migrations::migrate(&mut conn).expect("migrate");

        let raw = RawGraph::from_sqlite(&conn).expect("build graph");
        let args = GraphArgs {
            id: None,
            down: false,
            up: false,
            depth: None,
        };

        // Just check it doesn't panic/error
        let result = run_graph_summary(&raw, &conn, &args, OutputMode::Human);
        assert!(result.is_ok());
    }

    /// Full integration: set up project, add deps, render graph.
    #[test]
    fn graph_item_shows_dependencies() {
        use bones_core::db::rebuild;
        use bones_core::event::data::{CreateData, LinkData};
        use bones_core::event::writer::write_event;
        use bones_core::event::{Event, EventData, EventType};
        use bones_core::model::item::Kind;
        use bones_core::model::item_id::ItemId;
        use bones_core::shard::ShardManager;
        use std::collections::BTreeMap;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("create temp dir");
        let root = dir.path().to_path_buf();
        let bones_dir = root.join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init shard");

        // Create A, B, C
        for item_id in ["bn-ga1", "bn-ga2", "bn-ga3"] {
            let ts = shard_mgr.next_timestamp().expect("ts");
            let mut evt = Event {
                wall_ts_us: ts,
                agent: "test-agent".into(),
                itc: "itc:AQ".into(),
                parents: vec![],
                event_type: EventType::Create,
                item_id: ItemId::new_unchecked(item_id),
                data: EventData::Create(CreateData {
                    title: format!("Item {item_id}"),
                    kind: Kind::Task,
                    size: None,
                    urgency: bones_core::model::item::Urgency::Default,
                    labels: vec![],
                    parent: None,
                    causation: None,
                    description: None,
                    extra: BTreeMap::new(),
                }),
                event_hash: String::new(),
            };
            let line = write_event(&mut evt).expect("write");
            shard_mgr
                .append(&line, false, Duration::from_secs(5))
                .expect("append");
        }

        // A blocks B: event.item_id = B, target = A
        {
            let ts = shard_mgr.next_timestamp().expect("ts");
            let mut evt = Event {
                wall_ts_us: ts,
                agent: "test-agent".into(),
                itc: "itc:AQ".into(),
                parents: vec![],
                event_type: EventType::Link,
                item_id: ItemId::new_unchecked("bn-ga2"),
                data: EventData::Link(LinkData {
                    target: "bn-ga1".into(),
                    link_type: "blocks".into(),
                    extra: BTreeMap::new(),
                }),
                event_hash: String::new(),
            };
            let line = write_event(&mut evt).expect("write");
            shard_mgr
                .append(&line, false, Duration::from_secs(5))
                .expect("append");
        }

        // Rebuild
        let events_dir = bones_dir.join("events");
        let db_path = bones_dir.join("bones.db");
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild");

        // Run graph for A: should show B as downstream
        let args = GraphArgs {
            id: Some("bn-ga1".into()),
            down: false,
            up: false,
            depth: None,
        };
        let result = run_graph(&args, OutputMode::Human, &root);
        assert!(result.is_ok(), "run_graph failed: {:?}", result.err());
    }
}
