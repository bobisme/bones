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
use petgraph::visit::EdgeRef;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

const MICROS_PER_DAY: f64 = 86_400_000_000.0;
const TOPOLOGY_BLEND_WEIGHT: f64 = 0.10;
const PAGERANK_CACHE_FILE: &str = "triage_pagerank.json";

#[derive(Debug, Clone)]
pub(crate) struct RankedItem {
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
pub(crate) struct TriageSnapshot {
    pub ranked: Vec<RankedItem>,
    pub unblocked_ranked: Vec<RankedItem>,
    pub cycles: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PageRankDiskCache {
    version: u8,
    content_hash: String,
    scores: HashMap<String, f64>,
    edges: Vec<(String, String)>,
}

pub(crate) fn build_triage_snapshot(conn: &Connection, now_us: i64) -> Result<TriageSnapshot> {
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
            cycles,
        });
    }

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
                .map(|timing| timing.earliest_finish as f64)
                .unwrap_or(0.0)
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

    let unresolved_blockers = load_unresolved_blocker_counts(conn)?;
    let active_unblocks = load_active_unblocks_counts(conn)?;
    let weights = sampled_weights_from_feedback(seed_from_graph(normalized.content_hash()));

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
                score + (TOPOLOGY_BLEND_WEIGHT * topology_signal)
            } else {
                score
            };

            let blocked_by_active = unresolved_blockers.get(&item.item_id).copied().unwrap_or(0);
            let unblocks_active = active_unblocks.get(&item.item_id).copied().unwrap_or(0);

            let mut drivers = vec![
                ("critical-path", weights.alpha * cp_norm[idx]),
                ("pagerank", weights.beta * pr_norm[idx]),
                ("betweenness", weights.gamma * bc_norm[idx]),
                ("topology", TOPOLOGY_BLEND_WEIGHT * topology_signal),
                ("urgency", weights.delta * urgency_component(urgency)),
                ("decay", weights.epsilon * decay_component(decay_days)),
            ];
            drivers.sort_by(|a, b| b.1.total_cmp(&a.1));

            let driver_a = drivers.first().map(|(name, _)| *name).unwrap_or("priority");
            let driver_b = drivers.get(1).map(|(name, _)| *name).unwrap_or("signal");

            let explanation = if urgency == Urgency::Urgent {
                format!(
                    "Urgent override is active. Secondary signals: {driver_a} and {driver_b}. PageRank: {pagerank_method}."
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

    Ok(TriageSnapshot {
        ranked,
        unblocked_ranked,
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

fn pagerank_method_label(method: PageRankMethod) -> &'static str {
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

fn urgency_component(urgency: Urgency) -> f64 {
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
}
