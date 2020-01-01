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
use serde_json::json;

use bones_core::db::query::{get_item, item_exists, try_open_projection};
use bones_triage::graph::{
    build::RawGraph, find_all_cycles, normalize::NormalizedGraph, stats::GraphStats,
};

use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;

// ---------------------------------------------------------------------------
// Clap types
// ---------------------------------------------------------------------------

/// Output format for graph rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphFormat {
    /// Default ASCII tree / layered output.
    #[default]
    Ascii,
    /// Mermaid diagram syntax.
    Mermaid,
    /// Graphviz DOT syntax.
    Dot,
}

/// Arguments for `bn graph`.
#[derive(Args, Debug)]
pub struct GraphArgs {
    /// Bone ID to show graph for. If omitted, shows the project summary.
    pub id: Option<String>,

    /// Only show downstream bones (what this bone blocks).
    #[arg(long)]
    pub down: bool,

    /// Only show upstream bones (what blocks this bone).
    #[arg(long)]
    pub up: bool,

    /// Maximum traversal depth (default: unlimited).
    #[arg(long)]
    pub depth: Option<usize>,

    /// Output as a Mermaid diagram.
    #[arg(long, conflicts_with = "dot")]
    pub mermaid: bool,

    /// Output as a Graphviz DOT diagram.
    #[arg(long, conflicts_with = "mermaid")]
    pub dot: bool,
}

impl GraphArgs {
    /// Resolve the requested graph format from CLI flags.
    const fn format(&self) -> GraphFormat {
        if self.mermaid {
            GraphFormat::Mermaid
        } else if self.dot {
            GraphFormat::Dot
        } else {
            GraphFormat::Ascii
        }
    }
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
    let root_idx = if let Some(idx) = raw.node_index(root_id) { idx } else {
        let _ = writeln!(out, "  (no connections)");
        return;
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

        let state_mark = meta.get(id).map_or(" ", ItemMeta::display_state);
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
            visited.insert(id.clone());

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
// Mermaid rendering
// ---------------------------------------------------------------------------

/// Escape a label for Mermaid node text (inside quotes).
fn mermaid_escape(s: &str) -> String {
    s.replace('"', "#quot;")
}

/// Sanitize an ID for use as a Mermaid node identifier (alphanumeric + dash/underscore).
fn mermaid_node_id(id: &str) -> String {
    id.replace('.', "_")
}

/// Collect the subgraph reachable from `root_id` in the given `direction`,
/// respecting an optional `depth_limit`, and return the set of visited node
/// IDs plus the directed edges found.
fn collect_subgraph(
    raw: &RawGraph,
    root_id: &str,
    direction: petgraph::Direction,
    depth_limit: Option<usize>,
) -> (HashSet<String>, Vec<(String, String)>) {
    let mut visited = HashSet::new();
    let mut edges = Vec::new();

    let root_idx = match raw.node_index(root_id) {
        Some(idx) => idx,
        None => return (visited, edges),
    };
    visited.insert(root_id.to_string());

    let mut frontier = vec![(root_idx, root_id.to_string(), 0usize)];
    while let Some((idx, id, depth)) = frontier.pop() {
        if depth_limit.is_some_and(|d| depth >= d) {
            continue;
        }
        for neighbor_idx in raw.graph.neighbors_directed(idx, direction) {
            if let Some(neighbor_id) = raw.graph.node_weight(neighbor_idx) {
                // Always record the edge (in blocker→blocked direction)
                match direction {
                    petgraph::Direction::Outgoing => {
                        edges.push((id.clone(), neighbor_id.clone()));
                    }
                    petgraph::Direction::Incoming => {
                        edges.push((neighbor_id.clone(), id.clone()));
                    }
                }
                if visited.insert(neighbor_id.clone()) {
                    frontier.push((neighbor_idx, neighbor_id.clone(), depth + 1));
                }
            }
        }
    }
    (visited, edges)
}

/// Render a Mermaid diagram for a single item's dependency subgraph.
fn render_mermaid_item(
    raw: &RawGraph,
    meta: &HashMap<String, ItemMeta>,
    id: &str,
    args: &GraphArgs,
) -> String {
    let show_up = !args.down;
    let show_down = !args.up;

    let mut all_nodes = HashSet::new();
    let mut all_edges = Vec::new();
    all_nodes.insert(id.to_string());

    if show_down {
        let (nodes, edges) =
            collect_subgraph(raw, id, petgraph::Direction::Outgoing, args.depth);
        all_nodes.extend(nodes);
        all_edges.extend(edges);
    }
    if show_up {
        let (nodes, edges) =
            collect_subgraph(raw, id, petgraph::Direction::Incoming, args.depth);
        all_nodes.extend(nodes);
        all_edges.extend(edges);
    }

    // Deduplicate edges
    all_edges.sort();
    all_edges.dedup();

    let mut out = String::from("graph TD\n");
    // Node declarations
    let mut sorted_nodes: Vec<&String> = all_nodes.iter().collect();
    sorted_nodes.sort();
    for node_id in &sorted_nodes {
        let label = meta
            .get(*node_id)
            .map(|m| format!("{node_id} — {}", mermaid_escape(&truncate(&m.title, 40))))
            .unwrap_or_else(|| (*node_id).clone());
        let mid = mermaid_node_id(node_id);
        let _ = writeln!(out, "    {mid}[\"{label}\"]");
    }
    // Style the root node
    let mid = mermaid_node_id(id);
    let _ = writeln!(out, "    style {mid} stroke-width:3px");
    // State-based styling
    for node_id in &sorted_nodes {
        if let Some(m) = meta.get(*node_id) {
            let mid = mermaid_node_id(node_id);
            match m.state.as_str() {
                "done" | "archived" => {
                    let _ = writeln!(out, "    style {mid} fill:#d4edda,stroke:#28a745");
                }
                "doing" => {
                    let _ = writeln!(out, "    style {mid} fill:#fff3cd,stroke:#ffc107");
                }
                _ => {}
            }
        }
    }
    // Edges: blocker --> blocked
    for (src, tgt) in &all_edges {
        let s = mermaid_node_id(src);
        let t = mermaid_node_id(tgt);
        let _ = writeln!(out, "    {s} --> {t}");
    }
    out
}

/// Render a Mermaid diagram for the full project dependency graph (open items only).
fn render_mermaid_summary(
    _raw: &RawGraph,
    meta: &HashMap<String, ItemMeta>,
    open_connected: &HashSet<String>,
    open_edges: &[(String, String)],
) -> String {
    let mut out = String::from("graph TD\n");
    let mut sorted_nodes: Vec<&String> = open_connected.iter().collect();
    sorted_nodes.sort();
    for node_id in &sorted_nodes {
        let label = meta
            .get(*node_id)
            .map(|m| format!("{node_id} — {}", mermaid_escape(&truncate(&m.title, 40))))
            .unwrap_or_else(|| (*node_id).clone());
        let mid = mermaid_node_id(node_id);
        let _ = writeln!(out, "    {mid}[\"{label}\"]");
    }
    // State-based styling
    for node_id in &sorted_nodes {
        if let Some(m) = meta.get(*node_id) {
            let mid = mermaid_node_id(node_id);
            match m.state.as_str() {
                "done" | "archived" => {
                    let _ = writeln!(out, "    style {mid} fill:#d4edda,stroke:#28a745");
                }
                "doing" => {
                    let _ = writeln!(out, "    style {mid} fill:#fff3cd,stroke:#ffc107");
                }
                _ => {}
            }
        }
    }
    for (src, tgt) in open_edges {
        let s = mermaid_node_id(src);
        let t = mermaid_node_id(tgt);
        let _ = writeln!(out, "    {s} --> {t}");
    }
    out
}

// ---------------------------------------------------------------------------
// DOT rendering
// ---------------------------------------------------------------------------

/// Escape a label for DOT (inside double quotes).
fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Map item state to a DOT fill color.
fn dot_fill_color(state: &str) -> &'static str {
    match state {
        "done" | "archived" => "#d4edda",
        "doing" => "#fff3cd",
        _ => "#ffffff",
    }
}

/// Render a DOT digraph for a single item's dependency subgraph.
fn render_dot_item(
    raw: &RawGraph,
    meta: &HashMap<String, ItemMeta>,
    id: &str,
    args: &GraphArgs,
) -> String {
    let show_up = !args.down;
    let show_down = !args.up;

    let mut all_nodes = HashSet::new();
    let mut all_edges = Vec::new();
    all_nodes.insert(id.to_string());

    if show_down {
        let (nodes, edges) =
            collect_subgraph(raw, id, petgraph::Direction::Outgoing, args.depth);
        all_nodes.extend(nodes);
        all_edges.extend(edges);
    }
    if show_up {
        let (nodes, edges) =
            collect_subgraph(raw, id, petgraph::Direction::Incoming, args.depth);
        all_nodes.extend(nodes);
        all_edges.extend(edges);
    }

    all_edges.sort();
    all_edges.dedup();

    let mut out = String::from("digraph bones {\n    rankdir=TB;\n    node [shape=box, style=filled, fontname=\"Helvetica\"];\n\n");

    let mut sorted_nodes: Vec<&String> = all_nodes.iter().collect();
    sorted_nodes.sort();
    for node_id in &sorted_nodes {
        let label = meta
            .get(*node_id)
            .map(|m| format!("{node_id}\\n{}", dot_escape(&truncate(&m.title, 40))))
            .unwrap_or_else(|| (*node_id).clone());
        let fill = meta
            .get(*node_id)
            .map(|m| dot_fill_color(&m.state))
            .unwrap_or("#ffffff");
        let penwidth = if *node_id == id { "3.0" } else { "1.0" };
        let _ = writeln!(
            out,
            "    \"{node_id}\" [label=\"{label}\", fillcolor=\"{fill}\", penwidth={penwidth}];"
        );
    }
    let _ = writeln!(out);
    for (src, tgt) in &all_edges {
        let _ = writeln!(out, "    \"{src}\" -> \"{tgt}\";");
    }
    out.push_str("}\n");
    out
}

/// Render a DOT digraph for the full project dependency graph (open items only).
fn render_dot_summary(
    meta: &HashMap<String, ItemMeta>,
    open_connected: &HashSet<String>,
    open_edges: &[(String, String)],
) -> String {
    let mut out = String::from("digraph bones {\n    rankdir=TB;\n    node [shape=box, style=filled, fontname=\"Helvetica\"];\n\n");

    let mut sorted_nodes: Vec<&String> = open_connected.iter().collect();
    sorted_nodes.sort();
    for node_id in &sorted_nodes {
        let label = meta
            .get(*node_id)
            .map(|m| format!("{node_id}\\n{}", dot_escape(&truncate(&m.title, 40))))
            .unwrap_or_else(|| (*node_id).clone());
        let fill = meta
            .get(*node_id)
            .map(|m| dot_fill_color(&m.state))
            .unwrap_or("#ffffff");
        let _ = writeln!(
            out,
            "    \"{node_id}\" [label=\"{label}\", fillcolor=\"{fill}\"];"
        );
    }
    let _ = writeln!(out);
    for (src, tgt) in open_edges {
        let _ = writeln!(out, "    \"{src}\" -> \"{tgt}\";");
    }
    out.push_str("}\n");
    out
}

// ---------------------------------------------------------------------------
// Command runner
// ---------------------------------------------------------------------------

pub fn run_graph(args: &GraphArgs, output: OutputMode, project_root: &Path) -> anyhow::Result<()> {
    // Validate item ID if given
    if let Some(ref id) = args.id
        && let Err(e) = validate::validate_item_id(id) {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
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
        anyhow::anyhow!("{msg}")
    })?;

    let db_path = bones_dir.join("bones.db");
    let conn = if let Some(c) = try_open_projection(&db_path)? { c } else {
        let msg = "projection database not found; run `bn admin rebuild` first";
        render_error(output, &CliError::new(msg))?;
        anyhow::bail!("{msg}");
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
        anyhow::bail!("{msg}");
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
            "title": root_meta.map_or("", |m| m.title.as_str()),
            "state": root_meta.map_or("", |m| m.state.as_str()),
            "blocked_by": up_ids,
            "blocks": down_ids,
        });
        render(output, &val, |_, _| Ok(()))?;
        return Ok(());
    }

    // Mermaid / DOT / ASCII output
    match args.format() {
        GraphFormat::Mermaid => {
            print!("{}", render_mermaid_item(raw, &meta, id, args));
        }
        GraphFormat::Dot => {
            print!("{}", render_dot_item(raw, &meta, id, args));
        }
        GraphFormat::Ascii => {
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
        }
    }
    Ok(())
}

/// Collect all directed edges as `(source_id, target_id)` pairs, sorted.
fn collect_edges(raw: &RawGraph) -> Vec<(String, String)> {
    let mut edges: Vec<(String, String)> = raw
        .graph
        .edge_indices()
        .filter_map(|eidx| {
            let (src, tgt) = raw.graph.edge_endpoints(eidx)?;
            let src_id = raw.graph.node_weight(src)?.clone();
            let tgt_id = raw.graph.node_weight(tgt)?.clone();
            Some((src_id, tgt_id))
        })
        .collect();
    edges.sort();
    edges
}

/// Truncate a string to `max_len` characters, adding "..." if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

/// Show the full project directed dependency graph.
///
/// Renders all open items organized into topological layers with explicit
/// dependency arrows. Items that are done/archived are excluded from the
/// visual graph but still counted in the stats header.
fn run_graph_summary(
    raw: &RawGraph,
    conn: &rusqlite::Connection,
    args: &GraphArgs,
    output: OutputMode,
) -> anyhow::Result<()> {
    let ng = NormalizedGraph::from_raw(RawGraph::from_sqlite(conn)?);
    let stats = GraphStats::from_normalized(&ng);

    // Find cycles
    let cycles = find_all_cycles(&raw.graph);

    // Collect all edges
    let edges = collect_edges(raw);

    // Build the set of IDs that participate in at least one edge
    let mut connected_ids: HashSet<String> = HashSet::new();
    for (src, tgt) in &edges {
        connected_ids.insert(src.clone());
        connected_ids.insert(tgt.clone());
    }

    // Load metadata for all nodes
    let all_ids: Vec<String> = raw.graph.node_weights().cloned().collect();
    let meta = load_item_meta(conn, all_ids.into_iter());

    // Filter to only open/doing items (not done/archived) for display
    let open_connected: HashSet<String> = connected_ids
        .iter()
        .filter(|id| {
            meta.get(*id)
                .is_none_or(|m| m.state != "done" && m.state != "archived") // include unknown items
        })
        .cloned()
        .collect();

    // Filter edges to only those touching at least one open item
    let open_edges: Vec<&(String, String)> = edges
        .iter()
        .filter(|(src, tgt)| open_connected.contains(src) || open_connected.contains(tgt))
        .collect();

    // Compute topological layers using the diagnostics module
    use bones_triage::graph::diagnostics::topological_layers;
    let layers = topological_layers(&raw.graph, None);

    // Filter layers to only open connected items
    let filtered_layers: Vec<Vec<String>> = layers
        .iter()
        .map(|layer| {
            layer
                .iter()
                .filter(|id| open_connected.contains(*id))
                .cloned()
                .collect::<Vec<_>>()
        })
        .filter(|layer| !layer.is_empty())
        .collect();

    if output.is_json() {
        let mut nodes: Vec<serde_json::Value> = open_connected
            .iter()
            .map(|id| {
                let m = meta.get(id);
                json!({
                    "id": id,
                    "title": m.map_or("", |m| m.title.as_str()),
                    "state": m.map_or("", |m| m.state.as_str()),
                })
            })
            .collect();
        nodes.sort_by(|a, b| {
            a.get("id")
                .and_then(|v| v.as_str())
                .cmp(&b.get("id").and_then(|v| v.as_str()))
        });

        let edge_vals: Vec<serde_json::Value> = open_edges
            .iter()
            .map(|(src, tgt)| json!({"from": src, "to": tgt}))
            .collect();

        let layer_vals: Vec<Vec<&str>> = filtered_layers
            .iter()
            .map(|layer| layer.iter().map(String::as_str).collect())
            .collect();

        let val = json!({
            "items": stats.node_count,
            "blocking_edges": stats.edge_count,
            "connected_components": stats.weakly_connected_component_count,
            "cycles": stats.cycle_count,
            "cycle_members": cycles,
            "density": stats.density,
            "nodes": nodes,
            "edges": edge_vals,
            "layers": layer_vals,
        });
        render(output, &val, |_, _| Ok(()))?;
        return Ok(());
    }

    // Owned edges for mermaid/dot renderers (they need &[(String, String)])
    let open_edges_owned: Vec<(String, String)> = open_edges
        .iter()
        .map(|(src, tgt)| (src.clone(), tgt.clone()))
        .collect();

    // Mermaid / DOT / ASCII output
    match args.format() {
        GraphFormat::Mermaid => {
            print!(
                "{}",
                render_mermaid_summary(raw, &meta, &open_connected, &open_edges_owned)
            );
        }
        GraphFormat::Dot => {
            print!(
                "{}",
                render_dot_summary(&meta, &open_connected, &open_edges_owned)
            );
        }
        GraphFormat::Ascii => {
            let mut out = String::new();
            let _ = writeln!(
                out,
                "Dependency graph  ({} items, {} edges, {} components)",
                stats.node_count, stats.edge_count, stats.weakly_connected_component_count
            );

            if !cycles.is_empty() {
                let _ = writeln!(out);
                let _ = writeln!(out, "  ⚠ cycles:");
                for cycle in &cycles {
                    let _ = writeln!(out, "    {}", cycle.join(" → "));
                }
            }

            if open_edges.is_empty() {
                let _ = writeln!(out, "\n  (no blocking dependencies among open items)");
                print!("{out}");
                return Ok(());
            }

            // Render layered graph
            let _ = writeln!(out);
            for (layer_idx, layer) in filtered_layers.iter().enumerate() {
                let _ = writeln!(out, "  layer {layer_idx}:");
                for id in layer {
                    let state_mark = meta.get(id).map_or(" ", ItemMeta::display_state);
                    let title = meta
                        .get(id)
                        .map(|m| truncate(&m.title, 50))
                        .unwrap_or_default();

                    // Show outgoing edges inline
                    let mut targets: Vec<&str> = open_edges
                        .iter()
                        .filter(|(src, _)| src == id)
                        .map(|(_, tgt)| tgt.as_str())
                        .collect();
                    targets.sort_unstable();

                    if targets.is_empty() {
                        let _ = writeln!(out, "    [{state_mark}] {id} -- {title}");
                    } else {
                        let arrow_str = targets.join(", ");
                        let _ = writeln!(
                            out,
                            "    [{state_mark}] {id} -- {title}  --> {arrow_str}"
                        );
                    }
                }
            }

            // Also list any open connected items not in any layer
            let layered_ids: HashSet<String> = filtered_layers
                .iter()
                .flat_map(|layer| layer.iter().cloned())
                .collect();
            let mut unlayered: Vec<&String> = open_connected
                .iter()
                .filter(|id| !layered_ids.contains(*id))
                .collect();
            unlayered.sort();
            if !unlayered.is_empty() {
                let _ = writeln!(out, "  unlayered (cycle members):");
                for id in &unlayered {
                    let state_mark = meta.get(*id).map_or(" ", ItemMeta::display_state);
                    let title = meta
                        .get(*id)
                        .map(|m| truncate(&m.title, 50))
                        .unwrap_or_default();
                    let _ = writeln!(out, "    [{state_mark}] {id} -- {title}");
                }
            }

            // Edge list for completeness
            let _ = writeln!(out);
            let _ = writeln!(out, "  edges:");
            for (src, tgt) in &open_edges {
                let _ = writeln!(out, "    {src} --> {tgt}");
            }

            print!("{out}");
        }
    }
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
            mermaid: false,
            dot: false,
        };

        // Just check it doesn't panic/error
        let result = run_graph_summary(&raw, &conn, &args, OutputMode::Human);
        assert!(result.is_ok());
    }

    #[test]
    fn graph_summary_shows_directed_graph() {
        use bones_core::db::migrations;
        use rusqlite::{Connection, params};

        let mut conn = Connection::open_in_memory().expect("in-memory db");
        migrations::migrate(&mut conn).expect("migrate");

        for id in ["bn-001", "bn-002", "bn-003"] {
            conn.execute(
                "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, created_at_us, updated_at_us)
                 VALUES (?1, ?1, 'task', 'open', 'default', 0, 1000, 1000)",
                params![id],
            )
            .expect("insert item");
        }

        conn.execute(
            "INSERT INTO item_dependencies (item_id, depends_on_item_id, link_type, created_at_us)
             VALUES ('bn-002', 'bn-001', 'blocks', 1000)",
            [],
        )
        .expect("insert dep");
        conn.execute(
            "INSERT INTO item_dependencies (item_id, depends_on_item_id, link_type, created_at_us)
             VALUES ('bn-003', 'bn-002', 'blocks', 1000)",
            [],
        )
        .expect("insert dep");

        let raw = RawGraph::from_sqlite(&conn).expect("build graph");
        let args = GraphArgs {
            id: None,
            down: false,
            up: false,
            depth: None,
            mermaid: false,
            dot: false,
        };

        let result = run_graph_summary(&raw, &conn, &args, OutputMode::Human);
        assert!(result.is_ok());
    }

    #[test]
    fn graph_summary_collect_edges_helper() {
        use petgraph::graph::DiGraph;
        use std::collections::HashMap as HM;

        let mut graph = DiGraph::<String, ()>::new();
        let a = graph.add_node("bn-aaa".into());
        let b = graph.add_node("bn-bbb".into());
        graph.add_edge(a, b, ());

        let mut node_map = HM::new();
        node_map.insert("bn-aaa".into(), a);
        node_map.insert("bn-bbb".into(), b);

        let raw = RawGraph {
            graph,
            node_map,
            content_hash: "blake3:test".into(),
        };

        let edges = collect_edges(&raw);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0], ("bn-aaa".to_string(), "bn-bbb".to_string()));
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let long = "a".repeat(60);
        let result = truncate(&long, 50);
        assert!(result.len() <= 50);
        assert!(result.ends_with("..."));
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
            mermaid: false,
            dot: false,
        };
        let result = run_graph(&args, OutputMode::Human, &root);
        assert!(result.is_ok(), "run_graph failed: {:?}", result.err());
    }

    // --- Mermaid / DOT unit tests ---

    #[test]
    fn mermaid_item_renders_graph_td() {
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

        let mut meta = HashMap::new();
        meta.insert(
            "bn-aaa".into(),
            ItemMeta { title: "Alpha".into(), state: "open".into() },
        );
        meta.insert(
            "bn-bbb".into(),
            ItemMeta { title: "Beta".into(), state: "doing".into() },
        );

        let args = GraphArgs {
            id: Some("bn-aaa".into()),
            down: false,
            up: false,
            depth: None,
            mermaid: true,
            dot: false,
        };

        let out = render_mermaid_item(&raw, &meta, "bn-aaa", &args);
        assert!(out.starts_with("graph TD\n"), "should start with mermaid header: {out}");
        assert!(out.contains("bn-aaa"), "should contain root node: {out}");
        assert!(out.contains("bn-bbb"), "should contain downstream node: {out}");
        assert!(out.contains("-->"), "should contain edge arrow: {out}");
        assert!(out.contains("Alpha"), "should contain title: {out}");
        // Doing state should get yellow styling
        assert!(out.contains("#fff3cd"), "should style doing node: {out}");
    }

    #[test]
    fn mermaid_item_respects_direction_flags() {
        use petgraph::graph::DiGraph;
        use std::collections::HashMap as HM;

        // A → B → C
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

        // --down only from B: should show C but not A
        let args_down = GraphArgs {
            id: Some("bn-bbb".into()),
            down: true,
            up: false,
            depth: None,
            mermaid: true,
            dot: false,
        };
        let out = render_mermaid_item(&raw, &meta, "bn-bbb", &args_down);
        assert!(out.contains("bn-ccc"), "downstream should show C: {out}");
        assert!(!out.contains("bn-aaa"), "downstream-only should NOT show A: {out}");

        // --up only from B: should show A but not C
        let args_up = GraphArgs {
            id: Some("bn-bbb".into()),
            down: false,
            up: true,
            depth: None,
            mermaid: true,
            dot: false,
        };
        let out = render_mermaid_item(&raw, &meta, "bn-bbb", &args_up);
        assert!(out.contains("bn-aaa"), "upstream should show A: {out}");
        assert!(!out.contains("bn-ccc"), "upstream-only should NOT show C: {out}");
    }

    #[test]
    fn dot_item_renders_digraph() {
        use petgraph::graph::DiGraph;
        use std::collections::HashMap as HM;

        let mut graph = DiGraph::<String, ()>::new();
        let a = graph.add_node("bn-aaa".into());
        let b = graph.add_node("bn-bbb".into());
        graph.add_edge(a, b, ());

        let mut node_map = HM::new();
        node_map.insert("bn-aaa".into(), a);
        node_map.insert("bn-bbb".into(), b);

        let raw = RawGraph {
            graph,
            node_map,
            content_hash: "blake3:test".into(),
        };

        let mut meta = HashMap::new();
        meta.insert(
            "bn-aaa".into(),
            ItemMeta { title: "Alpha".into(), state: "open".into() },
        );
        meta.insert(
            "bn-bbb".into(),
            ItemMeta { title: "Beta".into(), state: "done".into() },
        );

        let args = GraphArgs {
            id: Some("bn-aaa".into()),
            down: false,
            up: false,
            depth: None,
            mermaid: false,
            dot: true,
        };

        let out = render_dot_item(&raw, &meta, "bn-aaa", &args);
        assert!(out.starts_with("digraph bones {"), "should start with digraph: {out}");
        assert!(out.contains("\"bn-aaa\""), "should contain root node: {out}");
        assert!(out.contains("\"bn-bbb\""), "should contain downstream node: {out}");
        assert!(out.contains("->"), "should contain edge arrow: {out}");
        assert!(out.contains("Alpha"), "should contain title: {out}");
        assert!(out.contains("penwidth=3.0"), "root should have thick border: {out}");
        // Done state should get green fill
        assert!(out.contains("#d4edda"), "should style done node: {out}");
        assert!(out.ends_with("}\n"), "should end with closing brace: {out}");
    }

    #[test]
    fn mermaid_summary_renders_all_open_edges() {
        let mut open_connected = HashSet::new();
        open_connected.insert("bn-001".into());
        open_connected.insert("bn-002".into());

        let open_edges = vec![("bn-001".to_string(), "bn-002".to_string())];

        let mut meta = HashMap::new();
        meta.insert(
            "bn-001".into(),
            ItemMeta { title: "First".into(), state: "open".into() },
        );
        meta.insert(
            "bn-002".into(),
            ItemMeta { title: "Second".into(), state: "doing".into() },
        );

        // We need a dummy RawGraph for the signature
        let raw = RawGraph {
            graph: petgraph::graph::DiGraph::new(),
            node_map: std::collections::HashMap::new(),
            content_hash: "blake3:test".into(),
        };

        let out = render_mermaid_summary(&raw, &meta, &open_connected, &open_edges);
        assert!(out.starts_with("graph TD\n"), "mermaid header: {out}");
        assert!(out.contains("bn-001"), "should contain first node: {out}");
        assert!(out.contains("bn-002"), "should contain second node: {out}");
        assert!(out.contains("bn-001 --> bn-002"), "should contain edge: {out}");
    }

    #[test]
    fn dot_summary_renders_all_open_edges() {
        let mut open_connected = HashSet::new();
        open_connected.insert("bn-001".into());
        open_connected.insert("bn-002".into());

        let open_edges = vec![("bn-001".to_string(), "bn-002".to_string())];

        let mut meta = HashMap::new();
        meta.insert(
            "bn-001".into(),
            ItemMeta { title: "First".into(), state: "open".into() },
        );
        meta.insert(
            "bn-002".into(),
            ItemMeta { title: "Second".into(), state: "open".into() },
        );

        let out = render_dot_summary(&meta, &open_connected, &open_edges);
        assert!(out.starts_with("digraph bones {"), "dot header: {out}");
        assert!(out.contains("\"bn-001\""), "should contain first node: {out}");
        assert!(out.contains("\"bn-002\""), "should contain second node: {out}");
        assert!(out.contains("\"bn-001\" -> \"bn-002\""), "should contain edge: {out}");
        assert!(out.ends_with("}\n"), "should end properly: {out}");
    }

    #[test]
    fn mermaid_escapes_quotes() {
        assert_eq!(mermaid_escape("hello \"world\""), "hello #quot;world#quot;");
    }

    #[test]
    fn dot_escapes_special_chars() {
        assert_eq!(dot_escape("hello \"world\""), "hello \\\"world\\\"");
        assert_eq!(dot_escape("line\nnewline"), "line\\nnewline");
        assert_eq!(dot_escape("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn mermaid_node_id_sanitizes_dots() {
        assert_eq!(mermaid_node_id("bn-1st.3"), "bn-1st_3");
        assert_eq!(mermaid_node_id("bn-abc"), "bn-abc");
    }

    #[test]
    fn graph_format_from_args() {
        let args = GraphArgs {
            id: None, down: false, up: false, depth: None,
            mermaid: false, dot: false,
        };
        assert_eq!(args.format(), GraphFormat::Ascii);

        let args = GraphArgs {
            id: None, down: false, up: false, depth: None,
            mermaid: true, dot: false,
        };
        assert_eq!(args.format(), GraphFormat::Mermaid);

        let args = GraphArgs {
            id: None, down: false, up: false, depth: None,
            mermaid: false, dot: true,
        };
        assert_eq!(args.format(), GraphFormat::Dot);
    }

    #[test]
    fn clap_parses_mermaid_flag() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: GraphArgs,
        }

        let w = Wrapper::parse_from(["test", "bn-abc", "--mermaid"]);
        assert!(w.args.mermaid);
        assert!(!w.args.dot);
    }

    #[test]
    fn clap_parses_dot_flag() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: GraphArgs,
        }

        let w = Wrapper::parse_from(["test", "--dot"]);
        assert!(!w.args.mermaid);
        assert!(w.args.dot);
    }
}
