use anyhow::{Context, Result};
use bones_core::db::query::{self, ItemFilter, SortOrder};
use bones_core::model::item::Urgency;
use bones_triage::feedback::{load_agent_profile, sample_weights};
use bones_triage::graph::{NormalizedGraph, RawGraph, compute_critical_path, find_all_cycles};
use bones_triage::metrics::betweenness::betweenness_centrality;
use bones_triage::metrics::eigenvector::eigenvector_centrality;
use bones_triage::metrics::hits::hits;
use bones_triage::metrics::pagerank::{
    EdgeChange, EdgeChangeKind, PageRankConfig, PageRankMethod, PageRankResult, pagerank,
    pagerank_incremental,
};
use bones_triage::score::{CompositeWeights, MetricInputs, composite_score, normalize_metric};
use petgraph::{Direction, visit::EdgeRef};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use tracing::{debug, warn};

const MICROS_PER_DAY: f64 = 86_400_000_000.0;
const TOPOLOGY_BLEND_WEIGHT: f64 = 0.10;
const URGENT_CHAIN_BLEND_WEIGHT: f64 = 0.35;
const URGENT_CHAIN_DECAY: f64 = 0.60;
/// Maximum BFS depth for urgent chain pressure propagation.
const URGENT_CHAIN_MAX_DEPTH: usize = 6;
const PAGERANK_CACHE_FILE: &str = "triage_pagerank.json";

#[derive(Debug, Clone)]
pub struct RankedItem {
    pub id: String,
    pub title: String,
    pub size: Option<String>,
    pub urgency: Urgency,
    pub score: f64,
    pub explanation: String,
    pub blocked_by_active: usize,
    pub unblocks_active: usize,
    pub updated_at_us: i64,
}

#[derive(Debug, Clone)]
pub struct TriageSnapshot {
    pub ranked: Vec<RankedItem>,
    pub unblocked_ranked: Vec<RankedItem>,
    pub needs_decomposition: Vec<RankedItem>,
    pub cycles: Vec<Vec<String>>,
}

#[derive(Debug, Clone)]
struct ChainPressureResult {
    pressure: f64,
    urgent_sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PageRankDiskCache {
    version: u8,
    content_hash: String,
    scores: HashMap<String, f64>,
    edges: Vec<(String, String)>,
}

pub fn build_triage_snapshot(conn: &Connection, now_us: i64) -> Result<TriageSnapshot> {
    let all_items = query::list_items(
        conn,
        &ItemFilter {
            include_deleted: false,
            sort: SortOrder::UpdatedDesc,
            ..Default::default()
        },
    )
    .context("load items for triage")?;

    let active_items: Vec<_> = all_items
        .into_iter()
        .filter(|item| is_active_state(&item.state))
        .collect();

    let raw_graph = RawGraph::from_sqlite(conn).context("load dependency graph for triage")?;
    let cycles = find_all_cycles(&raw_graph.graph);

    if active_items.is_empty() {
        return Ok(TriageSnapshot {
            ranked: Vec::new(),
            unblocked_ranked: Vec::new(),
            needs_decomposition: Vec::new(),
            cycles,
        });
    }

    let mut unresolved_blockers = load_unresolved_blocker_counts(conn)?;
    let active_unblocks = load_active_unblocks_counts(conn)?;

    // Propagate blocked status from parent goals to their children.
    // If a goal is blocked (e.g. Phase I blocks Phase II), all children of
    // Phase II should also be treated as blocked so triage doesn't recommend
    // them before their parent's blockers are resolved.
    propagate_parent_blocked(&active_items, &mut unresolved_blockers);
    let urgency_by_id: HashMap<String, Urgency> = active_items
        .iter()
        .map(|item| {
            (
                item.item_id.clone(),
                item.urgency.parse::<Urgency>().unwrap_or(Urgency::Default),
            )
        })
        .collect();
    let urgent_chain_pressure =
        compute_urgent_chain_pressure(&raw_graph, &urgency_by_id, &unresolved_blockers);
    let direct_urgent_unblock_counts =
        compute_direct_urgent_unblocks(&raw_graph, &urgency_by_id, &unresolved_blockers);

    let mut normalized = NormalizedGraph::from_raw(raw_graph);
    normalized.condensed = normalized.reduced.clone();
    let critical_path = compute_critical_path(&normalized);
    let pagerank_result = compute_pagerank(conn, &normalized);
    let betweenness = betweenness_centrality(&normalized);
    let hits_result = hits(&normalized, 100, 1e-6);
    let eigenvector_result = eigenvector_centrality(&normalized, 100, 1e-6);
    let pagerank_method = pagerank_method_label(pagerank_result.method).to_string();

    let ids: Vec<String> = active_items
        .iter()
        .map(|item| item.item_id.clone())
        .collect();
    let cp_raw: Vec<f64> = ids
        .iter()
        .map(|id| {
            critical_path
                .item_timings
                .get(id)
                .map_or(0.0, |timing| timing.earliest_finish as f64)
        })
        .collect();
    let pr_raw: Vec<f64> = ids
        .iter()
        .map(|id| pagerank_result.scores.get(id).copied().unwrap_or(0.0))
        .collect();
    let bc_raw: Vec<f64> = ids
        .iter()
        .map(|id| betweenness.get(id).copied().unwrap_or(0.0))
        .collect();
    let hub_raw: Vec<f64> = ids
        .iter()
        .map(|id| hits_result.hubs.get(id).copied().unwrap_or(0.0))
        .collect();
    let auth_raw: Vec<f64> = ids
        .iter()
        .map(|id| hits_result.authorities.get(id).copied().unwrap_or(0.0))
        .collect();
    let eigen_raw: Vec<f64> = ids
        .iter()
        .map(|id| eigenvector_result.scores.get(id).copied().unwrap_or(0.0))
        .collect();

    let cp_norm = normalize_metric(&cp_raw);
    let pr_norm = normalize_metric(&pr_raw);
    let bc_norm = normalize_metric(&bc_raw);
    let hub_norm = normalize_metric(&hub_raw);
    let auth_norm = normalize_metric(&auth_raw);
    let eigen_norm = normalize_metric(&eigen_raw);
    let urgent_chain_raw: Vec<f64> = ids
        .iter()
        .map(|id| {
            urgent_chain_pressure
                .get(id)
                .map_or(0.0, |r| r.pressure)
        })
        .collect();
    let urgent_chain_norm = normalize_metric(&urgent_chain_raw);
    let weights = sampled_weights_from_feedback(seed_from_graph(normalized.content_hash()));

    // Build set of item IDs that have at least one active child.
    let ids_with_children: HashSet<&str> = active_items
        .iter()
        .filter_map(|item| item.parent_id.as_deref())
        .collect();

    let mut ranked: Vec<RankedItem> = active_items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let urgency = item.urgency.parse::<Urgency>().unwrap_or(Urgency::Default);
            let decay_days = if item.state == "doing" {
                ((now_us - item.updated_at_us).max(0) as f64) / MICROS_PER_DAY
            } else {
                0.0
            };

            let score = composite_score(
                &MetricInputs {
                    critical_path: cp_norm[idx],
                    pagerank: pr_norm[idx],
                    betweenness: bc_norm[idx],
                    urgency,
                    decay_days,
                },
                &weights,
            );
            let topology_signal = (hub_norm[idx] + auth_norm[idx] + eigen_norm[idx]) / 3.0;
            let score = if score.is_finite() {
                URGENT_CHAIN_BLEND_WEIGHT.mul_add(urgent_chain_norm[idx], TOPOLOGY_BLEND_WEIGHT.mul_add(topology_signal, score))
            } else {
                score
            };

            let blocked_by_active = unresolved_blockers.get(&item.item_id).copied().unwrap_or(0);
            let unblocks_active = active_unblocks.get(&item.item_id).copied().unwrap_or(0);
            let urgent_unblocks_direct = direct_urgent_unblock_counts
                .get(&item.item_id)
                .copied()
                .unwrap_or(0);
            let chain_result = urgent_chain_pressure.get(&item.item_id);

            let mut drivers = [("critical-path", weights.alpha * cp_norm[idx]),
                ("pagerank", weights.beta * pr_norm[idx]),
                ("betweenness", weights.gamma * bc_norm[idx]),
                ("topology", TOPOLOGY_BLEND_WEIGHT * topology_signal),
                (
                    "urgent-chain",
                    URGENT_CHAIN_BLEND_WEIGHT * urgent_chain_norm[idx],
                ),
                ("urgency", weights.delta * urgency_component(urgency)),
                ("decay", weights.epsilon * decay_component(decay_days))];
            drivers.sort_by(|a, b| b.1.total_cmp(&a.1));

            let driver_a = drivers.first().map_or("priority", |(name, _)| *name);
            let driver_b = drivers.get(1).map_or("signal", |(name, _)| *name);

            let has_urgent_sources = chain_result
                .is_some_and(|r| !r.urgent_sources.is_empty());

            let explanation = if urgency == Urgency::Urgent {
                format!(
                    "Urgent override is active. Secondary signals: {driver_a} and {driver_b}. PageRank: {pagerank_method}."
                )
            } else if blocked_by_active == 0 && has_urgent_sources {
                let ids_str = chain_result
                    .map(|r| r.urgent_sources.join(", "))
                    .unwrap_or_default();
                format!(
                    "Driven by {driver_a} and {driver_b}. Prioritized because it unblocks urgent item(s): {ids_str}. PageRank: {pagerank_method}."
                )
            } else if blocked_by_active == 0 && urgent_unblocks_direct > 0 {
                format!(
                    "Driven by {driver_a} and {driver_b}. Prioritized because it unblocks {urgent_unblocks_direct} urgent dependency(ies). PageRank: {pagerank_method}."
                )
            } else if blocked_by_active == 0 {
                format!(
                    "Driven by {driver_a} and {driver_b}. Ready now; unblocks {unblocks_active} active item(s). PageRank: {pagerank_method}."
                )
            } else {
                format!(
                    "Driven by {driver_a} and {driver_b}. Blocked by {blocked_by_active} active dependency(ies). PageRank: {pagerank_method}."
                )
            };

            // Penalize large tasks (L/XL) that have no children (need decomposition).
            // Goals are exempt since they are expected to have children managed separately.
            let (score, explanation) = {
                let size_lower = item.size.as_deref().unwrap_or("").to_lowercase();
                let is_large = size_lower == "l" || size_lower == "xl";
                let is_goal = item.kind == "goal";
                let has_children = ids_with_children.contains(item.item_id.as_str());

                if is_large && !is_goal && !has_children {
                    let multiplier = if size_lower == "xl" { 0.25 } else { 0.5 };
                    let penalized = score * multiplier;
                    let note = format!(
                        " Score reduced: large task needs decomposition ({}x{}).",
                        size_lower.to_uppercase(),
                        multiplier,
                    );
                    (penalized, format!("{explanation}{note}"))
                } else {
                    (score, explanation)
                }
            };

            RankedItem {
                id: item.item_id.clone(),
                title: item.title.clone(),
                size: item.size.clone(),
                urgency,
                score,
                explanation,
                blocked_by_active,
                unblocks_active,
                updated_at_us: item.updated_at_us,
            }
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.unblocks_active.cmp(&a.unblocks_active))
            .then_with(|| b.updated_at_us.cmp(&a.updated_at_us))
            .then_with(|| a.id.cmp(&b.id))
    });

    let unblocked_ranked = ranked
        .iter()
        .filter(|item| item.blocked_by_active == 0 && item.urgency != Urgency::Punt)
        .cloned()
        .collect();

    // L/XL tasks (not goals) with no active children, sorted by score desc.
    let mut needs_decomposition: Vec<RankedItem> = ranked
        .iter()
        .filter(|item| {
            matches!(item.size.as_deref(), Some("l" | "xl"))
                && !ids_with_children.contains(item.id.as_str())
        })
        .filter(|item| {
            // Exclude goals -- only tasks/bugs/etc.
            active_items
                .iter()
                .find(|ai| ai.item_id == item.id)
                .is_none_or(|ai| ai.kind != "goal")
        })
        .cloned()
        .collect();
    needs_decomposition.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    needs_decomposition.truncate(5);

    Ok(TriageSnapshot {
        ranked,
        unblocked_ranked,
        needs_decomposition,
        cycles,
    })
}

fn compute_pagerank(conn: &Connection, normalized: &NormalizedGraph) -> PageRankResult {
    let config = PageRankConfig::default();
    let Some(cache_path) = pagerank_cache_path(conn) else {
        return pagerank(normalized, &config);
    };

    let current_edges = edge_list(&normalized.raw);

    if let Ok(Some(cache)) = load_pagerank_cache(&cache_path) {
        if cache.content_hash == normalized.content_hash() {
            debug!(
                nodes = normalized.condensed.node_count(),
                edges = normalized.raw.edge_count(),
                "PageRank cache hit (content hash match)"
            );
            return PageRankResult {
                scores: cache.scores,
                iterations: 0,
                converged: true,
                method: PageRankMethod::Incremental,
            };
        }

        let changes = diff_edge_changes(&cache.edges, &current_edges);
        if !changes.is_empty() {
            let result = pagerank_incremental(normalized, &cache.scores, &changes, &config);
            match result.method {
                PageRankMethod::Incremental => {
                    debug!(
                        nodes = normalized.condensed.node_count(),
                        edges = normalized.raw.edge_count(),
                        changes = changes.len(),
                        iterations = result.iterations,
                        converged = result.converged,
                        "PageRank incremental update completed"
                    );
                }
                PageRankMethod::IncrementalFallback => {
                    warn!(
                        nodes = normalized.condensed.node_count(),
                        edges = normalized.raw.edge_count(),
                        changes = changes.len(),
                        iterations = result.iterations,
                        converged = result.converged,
                        "PageRank incremental fell back to full recompute"
                    );
                }
                PageRankMethod::Full => {
                    debug!(
                        nodes = normalized.condensed.node_count(),
                        edges = normalized.raw.edge_count(),
                        changes = changes.len(),
                        iterations = result.iterations,
                        converged = result.converged,
                        "PageRank incremental entrypoint returned full method"
                    );
                }
            }
            let _ = save_pagerank_cache(
                &cache_path,
                &PageRankDiskCache {
                    version: 1,
                    content_hash: normalized.content_hash().to_string(),
                    scores: result.scores.clone(),
                    edges: current_edges,
                },
            );
            return result;
        }
    }

    let result = pagerank(normalized, &config);
    debug!(
        nodes = normalized.condensed.node_count(),
        edges = normalized.raw.edge_count(),
        iterations = result.iterations,
        converged = result.converged,
        "PageRank full recompute completed"
    );
    let _ = save_pagerank_cache(
        &cache_path,
        &PageRankDiskCache {
            version: 1,
            content_hash: normalized.content_hash().to_string(),
            scores: result.scores.clone(),
            edges: current_edges,
        },
    );

    result
}

const fn pagerank_method_label(method: PageRankMethod) -> &'static str {
    match method {
        PageRankMethod::Full => "full",
        PageRankMethod::Incremental => "incremental",
        PageRankMethod::IncrementalFallback => "incremental_fallback",
    }
}

fn pagerank_cache_path(conn: &Connection) -> Option<PathBuf> {
    let mut stmt = conn.prepare("PRAGMA database_list").ok()?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })
        .ok()?;

    for row in rows {
        let Ok((name, file_path)) = row else {
            continue;
        };
        if name != "main" || file_path.is_empty() {
            continue;
        }

        let db_path = PathBuf::from(file_path);
        let bones_dir = db_path.parent()?;
        return Some(bones_dir.join("cache").join(PAGERANK_CACHE_FILE));
    }

    None
}

fn load_pagerank_cache(path: &PathBuf) -> Result<Option<PageRankDiskCache>> {
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read(path).with_context(|| format!("read pagerank cache {}", path.display()))?;
    let cache = serde_json::from_slice::<PageRankDiskCache>(&raw)
        .with_context(|| format!("parse pagerank cache {}", path.display()))?;
    Ok(Some(cache))
}

fn save_pagerank_cache(path: &PathBuf, cache: &PageRankDiskCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create pagerank cache dir {}", parent.display()))?;
    }

    let body = serde_json::to_vec(cache).context("serialize pagerank cache")?;
    fs::write(path, body).with_context(|| format!("write pagerank cache {}", path.display()))
}

fn edge_list(raw: &RawGraph) -> Vec<(String, String)> {
    let mut edges = raw
        .graph
        .edge_references()
        .filter_map(|edge| {
            let source = raw.graph.node_weight(edge.source())?;
            let target = raw.graph.node_weight(edge.target())?;
            Some((source.clone(), target.clone()))
        })
        .collect::<Vec<_>>();
    edges.sort_unstable();
    edges
}

fn diff_edge_changes(
    previous: &[(String, String)],
    current: &[(String, String)],
) -> Vec<EdgeChange> {
    let previous_map: std::collections::HashSet<(String, String)> =
        previous.iter().cloned().collect();
    let current_map: std::collections::HashSet<(String, String)> =
        current.iter().cloned().collect();

    let mut changes = Vec::new();

    for (from, to) in current_map.difference(&previous_map) {
        changes.push(EdgeChange {
            from: from.clone(),
            to: to.clone(),
            kind: EdgeChangeKind::Added,
        });
    }

    for (from, to) in previous_map.difference(&current_map) {
        changes.push(EdgeChange {
            from: from.clone(),
            to: to.clone(),
            kind: EdgeChangeKind::Removed,
        });
    }

    changes
}

fn sampled_weights_from_feedback(seed: u64) -> CompositeWeights<f64> {
    let Ok(project_root) = std::env::current_dir() else {
        return CompositeWeights::default();
    };
    let agent_id = std::env::var("BONES_AGENT")
        .or_else(|_| std::env::var("AGENT"))
        .unwrap_or_else(|_| "default".to_string());
    let Ok(profile) = load_agent_profile(&project_root, &agent_id) else {
        return CompositeWeights::default();
    };

    if let Some(seed) = std::env::var("BONES_TRIAGE_RNG_SEED")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    {
        let mut rng = StdRng::seed_from_u64(seed);
        return sample_weights(&profile, &mut rng);
    }

    let mut rng = StdRng::seed_from_u64(seed);
    sample_weights(&profile, &mut rng)
}

fn seed_from_graph(content_hash: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content_hash.hash(&mut hasher);
    hasher.finish()
}

fn is_active_state(state: &str) -> bool {
    !matches!(state, "done" | "archived")
}

/// Walk parent chains and propagate blocked status downward.
///
/// If a goal has `blocked_by_active > 0`, every descendant that currently
/// has `blocked_by_active == 0` gets an inherited blocker count so that
/// triage treats it as blocked.
fn propagate_parent_blocked(
    active_items: &[query::QueryItem],
    unresolved_blockers: &mut HashMap<String, usize>,
) {
    // Build parent_id lookup for active items.
    let parent_of: HashMap<&str, &str> = active_items
        .iter()
        .filter_map(|item| {
            item.parent_id
                .as_deref()
                .map(|pid| (item.item_id.as_str(), pid))
        })
        .collect();

    let active_ids: HashSet<&str> = active_items
        .iter()
        .map(|item| item.item_id.as_str())
        .collect();

    for item in active_items {
        // Only propagate to items that appear unblocked on their own.
        if unresolved_blockers.get(&item.item_id).copied().unwrap_or(0) > 0 {
            continue;
        }

        // Walk up the parent chain looking for a blocked ancestor.
        let mut cursor = item.item_id.as_str();
        while let Some(&pid) = parent_of.get(cursor) {
            if !active_ids.contains(pid) {
                break;
            }
            if unresolved_blockers.get(pid).copied().unwrap_or(0) > 0 {
                // Ancestor is blocked — mark this item as inherited-blocked.
                unresolved_blockers.insert(item.item_id.clone(), 1);
                break;
            }
            cursor = pid;
        }
    }
}

fn load_unresolved_blocker_counts(conn: &Connection) -> Result<HashMap<String, usize>> {
    let mut stmt = conn
        .prepare(
            "SELECT d.item_id, COUNT(DISTINCT d.depends_on_item_id)
             FROM item_dependencies d
             JOIN items blockers ON blockers.item_id = d.depends_on_item_id
             WHERE d.link_type IN ('blocks', 'blocked_by')
               AND blockers.state NOT IN ('done', 'archived')
               AND blockers.is_deleted = 0
             GROUP BY d.item_id",
        )
        .context("prepare unresolved blocker query")?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1).unwrap_or_default(),
            ))
        })
        .context("run unresolved blocker query")?;

    let mut map = HashMap::new();
    for row in rows {
        let (item_id, count) = row.context("read unresolved blocker row")?;
        map.insert(item_id, usize::try_from(count).unwrap_or_default());
    }

    Ok(map)
}

fn load_active_unblocks_counts(conn: &Connection) -> Result<HashMap<String, usize>> {
    let mut stmt = conn
        .prepare(
            "SELECT d.depends_on_item_id, COUNT(DISTINCT d.item_id)
             FROM item_dependencies d
             JOIN items blocked ON blocked.item_id = d.item_id
             WHERE d.link_type IN ('blocks', 'blocked_by')
               AND blocked.state NOT IN ('done', 'archived')
               AND blocked.is_deleted = 0
             GROUP BY d.depends_on_item_id",
        )
        .context("prepare active unblocks query")?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1).unwrap_or_default(),
            ))
        })
        .context("run active unblocks query")?;

    let mut map = HashMap::new();
    for row in rows {
        let (item_id, count) = row.context("read active unblocks row")?;
        map.insert(item_id, usize::try_from(count).unwrap_or_default());
    }

    Ok(map)
}

fn compute_urgent_chain_pressure(
    raw_graph: &RawGraph,
    urgency_by_id: &HashMap<String, Urgency>,
    unresolved_blockers: &HashMap<String, usize>,
) -> HashMap<String, ChainPressureResult> {
    let mut results = HashMap::new();

    for source_id in urgency_by_id.keys() {
        let Some(source_idx) = raw_graph.node_index(source_id) else {
            continue;
        };

        let mut visited = HashSet::new();
        let mut queue: VecDeque<(petgraph::graph::NodeIndex, usize)> = raw_graph
            .graph
            .neighbors_directed(source_idx, Direction::Outgoing)
            .map(|neighbor| (neighbor, 1))
            .collect();

        let mut source_pressure = 0.0;
        let mut urgent_sources: Vec<String> = Vec::new();

        while let Some((node, depth)) = queue.pop_front() {
            if depth > URGENT_CHAIN_MAX_DEPTH {
                continue;
            }
            if !visited.insert(node) {
                continue;
            }

            if let Some(descendant_id) = raw_graph.item_id(node) {
                let unresolved = unresolved_blockers.get(descendant_id).copied().unwrap_or(0);
                if unresolved > 0 {
                    let urgency = urgency_by_id
                        .get(descendant_id)
                        .copied()
                        .unwrap_or(Urgency::Default);
                    let seed = urgent_chain_seed(urgency);
                    if seed > 0.0 {
                        let fan_in = raw_graph
                            .graph
                            .neighbors_directed(node, Direction::Incoming)
                            .count()
                            .max(1) as f64;
                        let branching_damp = 1.0 / fan_in.sqrt();
                        let attenuation = URGENT_CHAIN_DECAY.powi(depth.saturating_sub(1) as i32);
                        source_pressure += seed * attenuation * branching_damp;
                        if urgency == Urgency::Urgent {
                            urgent_sources.push(descendant_id.to_string());
                        }
                    }
                }
            }

            for neighbor in raw_graph
                .graph
                .neighbors_directed(node, Direction::Outgoing)
            {
                if !visited.contains(&neighbor) {
                    queue.push_back((neighbor, depth + 1));
                }
            }
        }

        urgent_sources.sort_unstable();
        urgent_sources.dedup();
        results.insert(
            source_id.clone(),
            ChainPressureResult {
                pressure: source_pressure,
                urgent_sources,
            },
        );
    }

    results
}

fn compute_direct_urgent_unblocks(
    raw_graph: &RawGraph,
    urgency_by_id: &HashMap<String, Urgency>,
    unresolved_blockers: &HashMap<String, usize>,
) -> HashMap<String, usize> {
    let mut counts = HashMap::new();

    for source_id in urgency_by_id.keys() {
        let Some(source_idx) = raw_graph.node_index(source_id) else {
            continue;
        };

        let mut count = 0usize;
        for neighbor in raw_graph
            .graph
            .neighbors_directed(source_idx, Direction::Outgoing)
        {
            let Some(descendant_id) = raw_graph.item_id(neighbor) else {
                continue;
            };
            let unresolved = unresolved_blockers.get(descendant_id).copied().unwrap_or(0);
            if unresolved == 0 {
                continue;
            }

            let urgency = urgency_by_id
                .get(descendant_id)
                .copied()
                .unwrap_or(Urgency::Default);
            if urgency == Urgency::Urgent {
                count += 1;
            }
        }

        counts.insert(source_id.clone(), count);
    }

    counts
}

const fn urgent_chain_seed(urgency: Urgency) -> f64 {
    match urgency {
        Urgency::Urgent => 1.0,
        Urgency::Default => 0.15,
        Urgency::Punt => 0.0,
    }
}

const fn urgency_component(urgency: Urgency) -> f64 {
    match urgency {
        Urgency::Urgent => 1.0,
        Urgency::Default => 0.5,
        Urgency::Punt => 0.0,
    }
}

fn decay_component(decay_days: f64) -> f64 {
    if !decay_days.is_finite() {
        return 0.0;
    }

    (decay_days.max(0.0) / 14.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db::migrations;
    use rusqlite::{Connection, params};

    fn test_db() -> Connection {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        conn
    }

    fn insert_item(
        conn: &Connection,
        item_id: &str,
        title: &str,
        state: &str,
        urgency: &str,
        size: Option<&str>,
        updated_at_us: i64,
    ) {
        conn.execute(
            "INSERT INTO items (
                item_id,
                title,
                kind,
                state,
                urgency,
                size,
                is_deleted,
                search_labels,
                created_at_us,
                updated_at_us
            ) VALUES (?1, ?2, 'task', ?3, ?4, ?5, 0, '', 0, ?6)",
            params![item_id, title, state, urgency, size, updated_at_us],
        )
        .expect("insert item");
    }

    fn insert_blocks_edge(conn: &Connection, blocker_id: &str, blocked_id: &str) {
        conn.execute(
            "INSERT INTO item_dependencies (item_id, depends_on_item_id, link_type, created_at_us)
             VALUES (?1, ?2, 'blocks', 0)",
            params![blocked_id, blocker_id],
        )
        .expect("insert dependency");
    }

    #[test]
    fn blocked_items_are_excluded_from_unblocked_ranked() {
        let conn = test_db();
        insert_item(&conn, "bn-ready", "Ready item", "open", "default", None, 10);
        insert_item(
            &conn,
            "bn-blocked",
            "Blocked item",
            "open",
            "default",
            None,
            20,
        );
        insert_blocks_edge(&conn, "bn-ready", "bn-blocked");

        let snapshot = build_triage_snapshot(&conn, 100).expect("snapshot");
        let unblocked_ids: Vec<&str> = snapshot
            .unblocked_ranked
            .iter()
            .map(|item| item.id.as_str())
            .collect();

        assert!(unblocked_ids.contains(&"bn-ready"));
        assert!(!unblocked_ids.contains(&"bn-blocked"));
    }

    #[test]
    fn urgent_items_rank_above_default() {
        let conn = test_db();
        insert_item(&conn, "bn-default", "Default", "open", "default", None, 10);
        insert_item(&conn, "bn-urgent", "Urgent", "open", "urgent", None, 11);

        let snapshot = build_triage_snapshot(&conn, 100).expect("snapshot");

        assert_eq!(
            snapshot.ranked.first().map(|item| item.id.as_str()),
            Some("bn-urgent")
        );
    }

    #[test]
    fn urgent_chain_pressure_decays_with_distance() {
        let conn = test_db();
        insert_item(
            &conn,
            "bn-root",
            "Root blocker",
            "open",
            "default",
            None,
            10,
        );
        insert_item(&conn, "bn-mid", "Mid blocker", "open", "default", None, 11);
        insert_item(
            &conn,
            "bn-urgent",
            "Urgent blocked",
            "open",
            "urgent",
            None,
            12,
        );

        insert_blocks_edge(&conn, "bn-root", "bn-mid");
        insert_blocks_edge(&conn, "bn-mid", "bn-urgent");

        let raw = RawGraph::from_sqlite(&conn).expect("raw graph");
        let unresolved = load_unresolved_blocker_counts(&conn).expect("unresolved blockers");
        let urgency_by_id = HashMap::from([
            ("bn-root".to_string(), Urgency::Default),
            ("bn-mid".to_string(), Urgency::Default),
            ("bn-urgent".to_string(), Urgency::Urgent),
        ]);

        let pressure = compute_urgent_chain_pressure(&raw, &urgency_by_id, &unresolved);
        let mid = pressure.get("bn-mid").map(|r| r.pressure).unwrap_or(0.0);
        let root = pressure.get("bn-root").map(|r| r.pressure).unwrap_or(0.0);

        assert!(
            mid > root,
            "direct urgent blocker should get stronger pressure"
        );
        assert!(root > 0.0, "ancestor should still receive decayed pressure");
    }

    #[test]
    fn urgent_blocked_item_boosts_ready_blocker_priority() {
        let conn = test_db();
        insert_item(
            &conn,
            "bn-blocker",
            "Prerequisite",
            "open",
            "default",
            None,
            90,
        );
        insert_item(
            &conn,
            "bn-work",
            "Downstream work",
            "open",
            "urgent",
            None,
            80,
        );
        insert_blocks_edge(&conn, "bn-blocker", "bn-work");

        // Verify that the raw chain pressure is higher for urgent than default seed
        let raw = RawGraph::from_sqlite(&conn).expect("raw graph");
        let unresolved = load_unresolved_blocker_counts(&conn).expect("unresolved");
        let urgency_by_id = HashMap::from([
            ("bn-blocker".to_string(), Urgency::Default),
            ("bn-work".to_string(), Urgency::Urgent),
        ]);
        let pressure = compute_urgent_chain_pressure(&raw, &urgency_by_id, &unresolved);
        let blocker_pressure = pressure
            .get("bn-blocker")
            .map(|r| r.pressure)
            .unwrap_or(0.0);
        assert!(
            blocker_pressure > 0.0,
            "blocker should receive chain pressure from urgent descendant"
        );

        let snapshot = build_triage_snapshot(&conn, 100).expect("snapshot");
        let blocker = snapshot
            .ranked
            .iter()
            .find(|item| item.id == "bn-blocker")
            .expect("blocker in ranked");

        // The unblocked blocker should be the top pick among unblocked items
        // (bn-work is blocked so it won't be in unblocked_ranked)
        assert_eq!(
            snapshot
                .unblocked_ranked
                .first()
                .map(|item| item.id.as_str()),
            Some("bn-blocker")
        );
        assert!(
            blocker.explanation.contains("unblocks urgent item(s)")
                || blocker.explanation.contains("urgent dependency"),
            "expected urgent-unblock explanation, got: {}",
            blocker.explanation
        );
    }

    #[test]
    fn done_and_archived_items_are_excluded_from_ranked_view() {
        let conn = test_db();
        insert_item(&conn, "bn-open", "Open", "open", "default", None, 10);
        insert_item(&conn, "bn-done", "Done", "done", "urgent", None, 20);
        insert_item(
            &conn,
            "bn-archived",
            "Archived",
            "archived",
            "urgent",
            None,
            30,
        );

        let snapshot = build_triage_snapshot(&conn, 100).expect("snapshot");
        let ids: Vec<&str> = snapshot
            .ranked
            .iter()
            .map(|item| item.id.as_str())
            .collect();

        assert_eq!(ids, vec!["bn-open"]);
    }

    #[test]
    fn urgent_chain_depth_cap_limits_propagation() {
        let conn = test_db();
        // Build a chain longer than URGENT_CHAIN_MAX_DEPTH
        let depth = URGENT_CHAIN_MAX_DEPTH + 2;
        for i in 0..=depth {
            let id = format!("bn-n{i}");
            let urgency = if i == depth { "urgent" } else { "default" };
            insert_item(
                &conn,
                &id,
                &format!("Node {i}"),
                "open",
                urgency,
                None,
                i as i64,
            );
        }
        for i in 0..depth {
            insert_blocks_edge(&conn, &format!("bn-n{i}"), &format!("bn-n{}", i + 1));
        }

        let raw = RawGraph::from_sqlite(&conn).expect("raw graph");
        let unresolved = load_unresolved_blocker_counts(&conn).expect("unresolved");
        let mut urgency_by_id = HashMap::new();
        for i in 0..=depth {
            let id = format!("bn-n{i}");
            let u = if i == depth {
                Urgency::Urgent
            } else {
                Urgency::Default
            };
            urgency_by_id.insert(id, u);
        }

        let pressure = compute_urgent_chain_pressure(&raw, &urgency_by_id, &unresolved);

        // The node just within depth cap should have some pressure
        let near_id = format!("bn-n{}", depth - URGENT_CHAIN_MAX_DEPTH + 1);
        let near_pressure = pressure.get(&near_id).map(|r| r.pressure).unwrap_or(0.0);

        // The node beyond depth cap (bn-n0) should have zero urgent pressure from urgent source
        let far_pressure = pressure.get("bn-n0").map(|r| r.pressure).unwrap_or(0.0);

        assert!(
            near_pressure > far_pressure,
            "near node ({near_id} = {near_pressure}) should have more pressure than far node (bn-n0 = {far_pressure})"
        );
    }

    #[test]
    fn no_propagation_from_done_items() {
        let conn = test_db();
        insert_item(&conn, "bn-blocker", "Blocker", "open", "default", None, 10);
        insert_item(
            &conn,
            "bn-done-urgent",
            "Done urgent",
            "done",
            "urgent",
            None,
            20,
        );
        insert_blocks_edge(&conn, "bn-blocker", "bn-done-urgent");

        let snapshot = build_triage_snapshot(&conn, 100).expect("snapshot");
        // done items are not active, so they don't appear in ranked at all
        let ids: Vec<&str> = snapshot.ranked.iter().map(|r| r.id.as_str()).collect();
        assert!(!ids.contains(&"bn-done-urgent"));
    }

    #[test]
    fn deterministic_tiebreaker_for_equal_scores() {
        let conn = test_db();
        // Two items with no deps, same urgency - order should be deterministic
        insert_item(&conn, "bn-aaa", "Alpha", "open", "default", None, 10);
        insert_item(&conn, "bn-bbb", "Beta", "open", "default", None, 10);

        let snap1 = build_triage_snapshot(&conn, 100).expect("snap1");
        let snap2 = build_triage_snapshot(&conn, 100).expect("snap2");

        let ids1: Vec<&str> = snap1.ranked.iter().map(|r| r.id.as_str()).collect();
        let ids2: Vec<&str> = snap2.ranked.iter().map(|r| r.id.as_str()).collect();

        assert_eq!(ids1, ids2, "tiebreaker should be deterministic");
    }

    #[test]
    fn multi_hop_pressure_is_monotonically_decreasing() {
        let conn = test_db();
        insert_item(&conn, "bn-a", "A", "open", "default", None, 10);
        insert_item(&conn, "bn-b", "B", "open", "default", None, 11);
        insert_item(&conn, "bn-c", "C", "open", "default", None, 12);
        insert_item(&conn, "bn-u", "Urgent", "open", "urgent", None, 13);

        insert_blocks_edge(&conn, "bn-a", "bn-b");
        insert_blocks_edge(&conn, "bn-b", "bn-c");
        insert_blocks_edge(&conn, "bn-c", "bn-u");

        let raw = RawGraph::from_sqlite(&conn).expect("raw graph");
        let unresolved = load_unresolved_blocker_counts(&conn).expect("unresolved");
        let urgency_by_id = HashMap::from([
            ("bn-a".to_string(), Urgency::Default),
            ("bn-b".to_string(), Urgency::Default),
            ("bn-c".to_string(), Urgency::Default),
            ("bn-u".to_string(), Urgency::Urgent),
        ]);

        let pressure = compute_urgent_chain_pressure(&raw, &urgency_by_id, &unresolved);
        let pa = pressure.get("bn-a").map(|r| r.pressure).unwrap_or(0.0);
        let pb = pressure.get("bn-b").map(|r| r.pressure).unwrap_or(0.0);
        let pc = pressure.get("bn-c").map(|r| r.pressure).unwrap_or(0.0);

        assert!(
            pc > pb && pb > pa,
            "pressure should decrease with distance: c={pc} > b={pb} > a={pa}"
        );
    }

    #[test]
    fn explanation_includes_urgent_item_ids() {
        let conn = test_db();
        insert_item(
            &conn,
            "bn-prereq",
            "Prerequisite",
            "open",
            "default",
            None,
            10,
        );
        insert_item(
            &conn,
            "bn-urgent-target",
            "Urgent target",
            "open",
            "urgent",
            None,
            20,
        );
        insert_blocks_edge(&conn, "bn-prereq", "bn-urgent-target");

        let snapshot = build_triage_snapshot(&conn, 100).expect("snapshot");
        let prereq = snapshot
            .ranked
            .iter()
            .find(|r| r.id == "bn-prereq")
            .expect("prereq in ranked");

        assert!(
            prereq.explanation.contains("bn-urgent-target"),
            "explanation should include the specific urgent item ID, got: {}",
            prereq.explanation
        );
    }

    fn insert_goal_with_children(
        conn: &Connection,
        goal_id: &str,
        goal_title: &str,
        child_ids: &[(&str, &str)],
    ) {
        // Insert goal
        conn.execute(
            "INSERT INTO items (
                item_id, title, kind, state, urgency, size,
                is_deleted, search_labels, created_at_us, updated_at_us
            ) VALUES (?1, ?2, 'goal', 'open', 'default', NULL, 0, '', 0, 10)",
            params![goal_id, goal_title],
        )
        .expect("insert goal");

        // Insert children with parent_id set
        for &(child_id, child_title) in child_ids {
            conn.execute(
                "INSERT INTO items (
                    item_id, title, kind, state, urgency, size,
                    parent_id, is_deleted, search_labels, created_at_us, updated_at_us
                ) VALUES (?1, ?2, 'task', 'open', 'default', NULL, ?3, 0, '', 0, 10)",
                params![child_id, child_title, goal_id],
            )
            .expect("insert child");
        }
    }

    #[test]
    fn children_of_blocked_goal_are_excluded_from_unblocked() {
        let conn = test_db();

        // Phase I goal with one task
        insert_goal_with_children(
            &conn,
            "bn-phase1",
            "Phase I",
            &[("bn-task1", "Phase 1 task")],
        );
        // Phase II goal with one task; Phase I blocks Phase II
        insert_goal_with_children(
            &conn,
            "bn-phase2",
            "Phase II",
            &[("bn-task2", "Phase 2 task")],
        );
        insert_blocks_edge(&conn, "bn-phase1", "bn-phase2");

        let snapshot = build_triage_snapshot(&conn, 100).expect("snapshot");
        let unblocked_ids: Vec<&str> = snapshot
            .unblocked_ranked
            .iter()
            .map(|item| item.id.as_str())
            .collect();

        // Phase I goal and its task should be unblocked
        assert!(
            unblocked_ids.contains(&"bn-phase1"),
            "Phase I goal should be unblocked"
        );
        assert!(
            unblocked_ids.contains(&"bn-task1"),
            "Phase I task should be unblocked"
        );
        // Phase II goal is directly blocked
        assert!(
            !unblocked_ids.contains(&"bn-phase2"),
            "Phase II goal should be blocked"
        );
        // Phase II task should inherit blocked status from its parent
        assert!(
            !unblocked_ids.contains(&"bn-task2"),
            "Phase II task should be blocked (inherited from parent goal)"
        );
    }

    #[test]
    fn deeply_nested_children_inherit_blocked_from_grandparent() {
        let conn = test_db();

        // Blocker goal
        insert_goal_with_children(&conn, "bn-g1", "Goal 1", &[]);
        // Blocked goal with a sub-goal, which has a task
        insert_goal_with_children(&conn, "bn-g2", "Goal 2", &[]);
        insert_blocks_edge(&conn, "bn-g1", "bn-g2");

        // Sub-goal under g2
        conn.execute(
            "INSERT INTO items (
                item_id, title, kind, state, urgency, size,
                parent_id, is_deleted, search_labels, created_at_us, updated_at_us
            ) VALUES ('bn-sub', 'Sub-goal', 'goal', 'open', 'default', NULL, 'bn-g2', 0, '', 0, 10)",
            [],
        )
        .expect("insert sub-goal");

        // Task under sub-goal
        conn.execute(
            "INSERT INTO items (
                item_id, title, kind, state, urgency, size,
                parent_id, is_deleted, search_labels, created_at_us, updated_at_us
            ) VALUES ('bn-leaf', 'Leaf task', 'task', 'open', 'default', NULL, 'bn-sub', 0, '', 0, 10)",
            [],
        )
        .expect("insert leaf");

        let snapshot = build_triage_snapshot(&conn, 100).expect("snapshot");
        let unblocked_ids: Vec<&str> = snapshot
            .unblocked_ranked
            .iter()
            .map(|item| item.id.as_str())
            .collect();

        assert!(
            !unblocked_ids.contains(&"bn-sub"),
            "sub-goal should inherit blocked from g2"
        );
        assert!(
            !unblocked_ids.contains(&"bn-leaf"),
            "leaf task should inherit blocked from grandparent g2"
        );
    }

    #[test]
    fn needs_decomposition_lists_large_tasks_without_children() {
        let conn = test_db();
        // Large task with no children => should appear in needs_decomposition
        insert_item(
            &conn,
            "bn-big",
            "Big task",
            "open",
            "default",
            Some("l"),
            10,
        );
        // XL task with no children => should appear
        insert_item(
            &conn,
            "bn-huge",
            "Huge task",
            "open",
            "default",
            Some("xl"),
            20,
        );
        // Small task => should NOT appear
        insert_item(
            &conn,
            "bn-small",
            "Small task",
            "open",
            "default",
            Some("s"),
            30,
        );
        // Large goal => should NOT appear (goals are excluded)
        conn.execute(
            "INSERT INTO items (
                item_id, title, kind, state, urgency, size,
                is_deleted, search_labels, created_at_us, updated_at_us
            ) VALUES ('bn-goal', 'Big goal', 'goal', 'open', 'default', 'l', 0, '', 0, 40)",
            [],
        )
        .expect("insert goal");
        // Large task WITH a child => should NOT appear
        insert_item(
            &conn,
            "bn-parent",
            "Parent task",
            "open",
            "default",
            Some("l"),
            50,
        );
        conn.execute(
            "INSERT INTO items (
                item_id, title, kind, state, urgency, size,
                parent_id, is_deleted, search_labels, created_at_us, updated_at_us
            ) VALUES ('bn-child', 'Child task', 'task', 'open', 'default', 's', 'bn-parent', 0, '', 0, 60)",
            [],
        )
        .expect("insert child");

        let snapshot = build_triage_snapshot(&conn, 100).expect("snapshot");
        let decomp_ids: Vec<&str> = snapshot
            .needs_decomposition
            .iter()
            .map(|item| item.id.as_str())
            .collect();

        assert!(
            decomp_ids.contains(&"bn-big"),
            "L task without children should be in needs_decomposition"
        );
        assert!(
            decomp_ids.contains(&"bn-huge"),
            "XL task without children should be in needs_decomposition"
        );
        assert!(
            !decomp_ids.contains(&"bn-small"),
            "small task should NOT be in needs_decomposition"
        );
        assert!(
            !decomp_ids.contains(&"bn-goal"),
            "goal should NOT be in needs_decomposition"
        );
        assert!(
            !decomp_ids.contains(&"bn-parent"),
            "L task WITH children should NOT be in needs_decomposition"
        );
    }

    fn insert_item_with_kind(
        conn: &Connection,
        item_id: &str,
        title: &str,
        kind: &str,
        state: &str,
        urgency: &str,
        size: Option<&str>,
        parent_id: Option<&str>,
        updated_at_us: i64,
    ) {
        conn.execute(
            "INSERT INTO items (
                item_id, title, kind, state, urgency, size, parent_id,
                is_deleted, search_labels, created_at_us, updated_at_us
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, '', 0, ?8)",
            params![
                item_id,
                title,
                kind,
                state,
                urgency,
                size,
                parent_id,
                updated_at_us
            ],
        )
        .expect("insert item with kind");
    }

    #[test]
    fn large_task_without_children_gets_score_penalty() {
        let conn = test_db();

        // A small task (no size penalty)
        insert_item(
            &conn,
            "bn-small",
            "Small task",
            "open",
            "default",
            Some("s"),
            10,
        );
        // An L task with no children (should get 0.5x penalty)
        insert_item(
            &conn,
            "bn-large",
            "Large task",
            "open",
            "default",
            Some("l"),
            10,
        );
        // An XL task with no children (should get 0.25x penalty)
        insert_item(
            &conn,
            "bn-xlarge",
            "XL task",
            "open",
            "default",
            Some("xl"),
            10,
        );

        let snapshot = build_triage_snapshot(&conn, 100).expect("snapshot");

        let small = snapshot
            .ranked
            .iter()
            .find(|r| r.id == "bn-small")
            .expect("small");
        let large = snapshot
            .ranked
            .iter()
            .find(|r| r.id == "bn-large")
            .expect("large");
        let xlarge = snapshot
            .ranked
            .iter()
            .find(|r| r.id == "bn-xlarge")
            .expect("xlarge");

        // L task score should be lower than the small task score due to 0.5x multiplier
        assert!(
            large.score < small.score,
            "L task ({}) should score lower than small task ({}) due to decomposition penalty",
            large.score,
            small.score
        );
        assert!(
            large
                .explanation
                .contains("Score reduced: large task needs decomposition"),
            "L task explanation should mention penalty, got: {}",
            large.explanation
        );

        // XL task score should be lower than L task due to harsher 0.25x penalty
        assert!(
            xlarge.score < large.score,
            "XL task ({}) should score lower than L task ({}) due to harsher penalty",
            xlarge.score,
            large.score
        );
        assert!(
            xlarge
                .explanation
                .contains("Score reduced: large task needs decomposition"),
            "XL task explanation should mention penalty, got: {}",
            xlarge.explanation
        );
    }

    #[test]
    fn large_task_with_children_is_not_penalized() {
        let conn = test_db();

        // An L task that has a child (should NOT be penalized)
        insert_item(
            &conn,
            "bn-parent",
            "Parent L task",
            "open",
            "default",
            Some("l"),
            10,
        );
        // Child of the L task
        insert_item_with_kind(
            &conn,
            "bn-child",
            "Child task",
            "task",
            "open",
            "default",
            Some("s"),
            Some("bn-parent"),
            10,
        );

        let snapshot = build_triage_snapshot(&conn, 100).expect("snapshot");
        let parent = snapshot
            .ranked
            .iter()
            .find(|r| r.id == "bn-parent")
            .expect("parent");

        assert!(
            !parent.explanation.contains("Score reduced"),
            "L task with children should not be penalized, got: {}",
            parent.explanation
        );
    }

    #[test]
    fn large_goal_is_not_penalized() {
        let conn = test_db();

        // An L goal with no children (goals are exempt)
        insert_item_with_kind(
            &conn,
            "bn-goal",
            "Big goal",
            "goal",
            "open",
            "default",
            Some("l"),
            None,
            10,
        );

        let snapshot = build_triage_snapshot(&conn, 100).expect("snapshot");
        let goal = snapshot
            .ranked
            .iter()
            .find(|r| r.id == "bn-goal")
            .expect("goal");

        assert!(
            !goal.explanation.contains("Score reduced"),
            "goal should not be penalized for lack of children, got: {}",
            goal.explanation
        );
    }
}
