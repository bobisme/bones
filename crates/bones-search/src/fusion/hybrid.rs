//! Hybrid search orchestration across lexical, semantic, and structural layers.
//!
//! The orchestrator intentionally degrades gracefully:
//! - lexical search always runs
//! - semantic search runs only when a model is provided and embedding/search succeeds
//! - structural search runs when structural signals can be computed from the
//!   lexical cohort and dependency graph

use crate::fusion::scoring::rrf_fuse;
use crate::semantic::{
    SemanticModel, SemanticSearchResult, knn_search, sync_projection_embeddings,
};
use crate::structural::structural_similarity;
use anyhow::{Context, Result};
use bones_core::db::fts::search_bm25;
use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::warn;

const MAX_STRUCTURAL_SEEDS: usize = 16;
const MAX_STRUCTURAL_CANDIDATES: usize = 128;
const MIN_SEMANTIC_SCORE: f32 = 0.60;
const MIN_SEMANTIC_TOP_SCORE_NO_LEXICAL: f32 = 0.62;
static SEMANTIC_DEGRADED_WARNED: AtomicBool = AtomicBool::new(false);

/// Unified fused result with per-layer explanation fields.
#[derive(Debug, Clone, PartialEq)]
pub struct HybridSearchResult {
    pub item_id: String,
    pub score: f32,
    pub lexical_score: f32,
    pub semantic_score: f32,
    pub structural_score: f32,
    pub lexical_rank: usize,
    pub semantic_rank: usize,
    pub structural_rank: usize,
}

/// Run hybrid search for a free-text query and fuse ranked lists with RRF.
pub fn hybrid_search(
    query: &str,
    db: &Connection,
    model: Option<&SemanticModel>,
    limit: usize,
    rrf_k: usize,
) -> Result<Vec<HybridSearchResult>> {
    hybrid_search_inner(query, db, model, None, limit, rrf_k)
}

/// Run hybrid search using a caller-provided dependency graph for structural scoring.
pub fn hybrid_search_with_graph(
    query: &str,
    db: &Connection,
    model: Option<&SemanticModel>,
    graph: &DiGraph<String, ()>,
    limit: usize,
    rrf_k: usize,
) -> Result<Vec<HybridSearchResult>> {
    hybrid_search_inner(query, db, model, Some(graph), limit, rrf_k)
}

fn hybrid_search_inner(
    query: &str,
    db: &Connection,
    model: Option<&SemanticModel>,
    structural_graph: Option<&DiGraph<String, ()>>,
    limit: usize,
    rrf_k: usize,
) -> Result<Vec<HybridSearchResult>> {
    let limit = limit.min(1000);
    if limit == 0 {
        return Ok(Vec::new());
    }

    let lexical_hits = search_bm25(db, query, limit as u32).context("lexical search failed")?;
    let lexical_ranked_owned: Vec<String> = lexical_hits.into_iter().map(|h| h.item_id).collect();
    let lexical_ranked: Vec<&str> = lexical_ranked_owned.iter().map(String::as_str).collect();

    let semantic_ranked_owned = if let Some(model) = model {
        match sync_projection_embeddings(db, model)
            .and_then(|_| model.embed(query))
            .and_then(|embedding| knn_search(db, &embedding, limit))
        {
            Ok(hits) => semantic_ranked_items(hits, lexical_ranked_owned.is_empty()),
            Err(e) => {
                if !SEMANTIC_DEGRADED_WARNED.swap(true, Ordering::SeqCst) {
                    warn!("semantic layer unavailable; using lexical+structural ranking: {e}");
                }
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let semantic_ranked: Vec<&str> = semantic_ranked_owned.iter().map(String::as_str).collect();

    let owned_graph = if structural_graph.is_none() && !lexical_ranked_owned.is_empty() {
        match build_dependency_graph(db) {
            Ok(graph) => Some(graph),
            Err(e) => {
                warn!("structural layer unavailable, continuing without structural signal: {e}");
                None
            }
        }
    } else {
        None
    };

    let resolved_graph = structural_graph.or(owned_graph.as_ref());
    let structural_ranked_owned =
        derive_structural_ranked(db, &lexical_ranked_owned, resolved_graph, limit);
    let structural_ranked: Vec<&str> = structural_ranked_owned.iter().map(String::as_str).collect();

    let fused = rrf_fuse(&lexical_ranked, &semantic_ranked, &structural_ranked, rrf_k);

    let mut out = Vec::with_capacity(fused.len().min(limit));
    for (item_id, score) in fused.into_iter().take(limit) {
        let lexical_rank = find_rank(&lexical_ranked, &item_id);
        let semantic_rank = find_rank(&semantic_ranked, &item_id);
        let structural_rank = find_rank(&structural_ranked, &item_id);

        // Avoid returning graph-only expansions that have no textual relevance.
        if lexical_rank == usize::MAX && semantic_rank == usize::MAX {
            continue;
        }

        out.push(HybridSearchResult {
            item_id,
            score,
            lexical_score: rank_to_score(lexical_rank, rrf_k),
            semantic_score: rank_to_score(semantic_rank, rrf_k),
            structural_score: rank_to_score(structural_rank, rrf_k),
            lexical_rank,
            semantic_rank,
            structural_rank,
        });
    }

    Ok(out)
}

fn find_rank(layer: &[&str], item_id: &str) -> usize {
    layer
        .iter()
        .position(|id| *id == item_id)
        .map(|idx| idx + 1)
        .unwrap_or(usize::MAX)
}

fn rank_to_score(rank: usize, k: usize) -> f32 {
    if rank == usize::MAX {
        0.0
    } else {
        1.0 / (k as f32 + rank as f32)
    }
}

fn semantic_ranked_items(hits: Vec<SemanticSearchResult>, lexical_is_empty: bool) -> Vec<String> {
    if hits.is_empty() {
        return Vec::new();
    }

    if lexical_is_empty && hits[0].score < MIN_SEMANTIC_TOP_SCORE_NO_LEXICAL {
        return Vec::new();
    }

    hits.into_iter()
        .filter(|hit| hit.score >= MIN_SEMANTIC_SCORE)
        .map(|hit| hit.item_id)
        .collect()
}

fn derive_structural_ranked(
    db: &Connection,
    lexical_ranked: &[String],
    graph: Option<&DiGraph<String, ()>>,
    limit: usize,
) -> Vec<String> {
    if limit == 0 || lexical_ranked.is_empty() {
        return Vec::new();
    }

    let Some(graph) = graph else {
        return Vec::new();
    };

    let seed_ids: Vec<&str> = lexical_ranked
        .iter()
        .map(String::as_str)
        .take(MAX_STRUCTURAL_SEEDS)
        .collect();

    if seed_ids.is_empty() {
        return Vec::new();
    }

    let node_map: HashMap<&str, NodeIndex> = graph
        .node_indices()
        .filter_map(|idx| graph.node_weight(idx).map(|id| (id.as_str(), idx)))
        .collect();

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    for seed in &seed_ids {
        if seen.insert((*seed).to_owned()) {
            candidates.push((*seed).to_owned());
        }
    }

    for seed in &seed_ids {
        let Some(&seed_idx) = node_map.get(*seed) else {
            continue;
        };

        for neighbor_idx in graph
            .neighbors_directed(seed_idx, Direction::Outgoing)
            .chain(graph.neighbors_directed(seed_idx, Direction::Incoming))
        {
            let Some(neighbor_id) = graph.node_weight(neighbor_idx) else {
                continue;
            };

            if seen.insert(neighbor_id.clone()) {
                candidates.push(neighbor_id.clone());
                if candidates.len() >= MAX_STRUCTURAL_CANDIDATES {
                    break;
                }
            }
        }

        if candidates.len() >= MAX_STRUCTURAL_CANDIDATES {
            break;
        }
    }

    let mut scored = Vec::new();
    for candidate in candidates {
        let mut best = 0.0_f32;

        for seed in &seed_ids {
            if candidate == *seed {
                continue;
            }

            let Ok(score) = structural_similarity(seed, &candidate, db, graph) else {
                continue;
            };

            best = best.max(score.mean());
        }

        if best > 0.0 {
            scored.push((candidate, best));
        }
    }

    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    scored
        .into_iter()
        .take(limit)
        .map(|(item_id, _)| item_id)
        .collect()
}

fn build_dependency_graph(db: &Connection) -> Result<DiGraph<String, ()>> {
    let mut graph = DiGraph::<String, ()>::new();
    let mut node_map: HashMap<String, NodeIndex> = HashMap::new();

    let mut item_stmt = db
        .prepare("SELECT item_id FROM items WHERE is_deleted = 0")
        .context("prepare item id query for structural graph")?;
    let item_rows = item_stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("query item ids for structural graph")?;
    for row in item_rows {
        let item_id = row.context("read item id row for structural graph")?;
        let idx = graph.add_node(item_id.clone());
        node_map.insert(item_id, idx);
    }

    let mut edge_stmt = db
        .prepare(
            "SELECT depends_on_item_id, item_id
             FROM item_dependencies
             WHERE link_type IN ('blocks', 'blocked_by')",
        )
        .context("prepare dependency query for structural graph")?;
    let edge_rows = edge_stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("query dependencies for structural graph")?;

    for row in edge_rows {
        let (blocker, blocked) = row.context("read dependency row for structural graph")?;

        let blocker_idx = *node_map
            .entry(blocker.clone())
            .or_insert_with(|| graph.add_node(blocker));
        let blocked_idx = *node_map
            .entry(blocked.clone())
            .or_insert_with(|| graph.add_node(blocked));

        if !graph.contains_edge(blocker_idx, blocked_idx) {
            graph.add_edge(blocker_idx, blocked_idx, ());
        }
    }

    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db::migrations;
    use bones_core::db::project::{Projector, ensure_tracking_table};
    use bones_core::event::data::*;
    use bones_core::event::types::EventType;
    use bones_core::event::{Event, EventData};
    use bones_core::model::item::{Kind, Size, Urgency};
    use bones_core::model::item_id::ItemId;
    use rusqlite::Connection;
    use std::collections::BTreeMap;

    fn setup_db() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        ensure_tracking_table(&conn).expect("tracking table");

        let proj = Projector::new(&conn);
        proj.project_event(&make_create(
            "bn-001",
            "Authentication timeout regression",
            Some("Auth service fails after 30 seconds"),
            "h1",
        ))
        .unwrap();
        proj.project_event(&make_create(
            "bn-002",
            "Authentication service flaky",
            Some("Random auth timeouts in staging"),
            "h2",
        ))
        .unwrap();
        proj.project_event(&make_create(
            "bn-003",
            "README cleanup",
            Some("Fix typos in docs"),
            "h3",
        ))
        .unwrap();

        conn
    }

    fn make_create(id: &str, title: &str, desc: Option<&str>, hash: &str) -> Event {
        Event {
            wall_ts_us: 1000,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(id),
            data: EventData::Create(CreateData {
                title: title.into(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: desc.map(String::from),
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:{hash}"),
        }
    }

    fn make_create_with_labels(
        id: &str,
        title: &str,
        desc: Option<&str>,
        labels: &[&str],
        hash: &str,
    ) -> Event {
        Event {
            wall_ts_us: 1000,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(id),
            data: EventData::Create(CreateData {
                title: title.into(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: labels.iter().map(|label| (*label).to_owned()).collect(),
                parent: None,
                causation: None,
                description: desc.map(String::from),
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:{hash}"),
        }
    }

    fn setup_db_with_structural_overlap() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        ensure_tracking_table(&conn).expect("tracking table");

        let proj = Projector::new(&conn);
        proj.project_event(&make_create_with_labels(
            "bn-101",
            "Auth incident timeout while logging in",
            Some("OAuth callback intermittently times out"),
            &["auth", "backend"],
            "struct-a",
        ))
        .unwrap();
        proj.project_event(&make_create_with_labels(
            "bn-102",
            "Authentication incident timeout on mobile",
            Some("Token exchange fails under load"),
            &["auth", "mobile"],
            "struct-b",
        ))
        .unwrap();
        proj.project_event(&make_create_with_labels(
            "bn-103",
            "Docs typo cleanup",
            Some("Fix spelling in contributor guide"),
            &["docs"],
            "struct-c",
        ))
        .unwrap();

        conn
    }

    #[test]
    fn hybrid_search_returns_lexical_results_when_no_model() {
        let conn = setup_db();

        let results = hybrid_search("authentication", &conn, None, 10, 60).unwrap();

        assert!(!results.is_empty());
        assert!(results.iter().all(|r| r.semantic_score == 0.0));
        assert!(results.iter().all(|r| r.structural_score == 0.0));
        assert!(results.iter().all(|r| r.lexical_score > 0.0));
    }

    #[test]
    fn hybrid_search_respects_limit() {
        let conn = setup_db();

        let results = hybrid_search("auth", &conn, None, 1, 60).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn hybrid_search_zero_limit_returns_empty() {
        let conn = setup_db();
        let results = hybrid_search("auth", &conn, None, 0, 60).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn hybrid_search_populates_structural_scores_when_structure_exists() {
        let conn = setup_db_with_structural_overlap();
        let mut graph = DiGraph::new();
        let first = graph.add_node("bn-101".to_string());
        let second = graph.add_node("bn-102".to_string());
        graph.add_edge(first, second, ());

        let direct = structural_similarity("bn-101", "bn-102", &conn, &graph)
            .expect("direct structural similarity should compute");
        assert!(
            direct.mean() > 0.0,
            "expected positive direct structural similarity"
        );

        let structural_seeded =
            derive_structural_ranked(&conn, &["bn-101".to_string()], Some(&graph), 10);
        assert!(
            !structural_seeded.is_empty(),
            "expected structural ranking for an explicit seed"
        );

        let lexical_from_query: Vec<String> = search_bm25(&conn, "incident", 10)
            .expect("lexical search should succeed")
            .into_iter()
            .map(|hit| hit.item_id)
            .collect();
        assert!(
            !lexical_from_query.is_empty(),
            "expected lexical hits for incident query"
        );
        let structural_from_query =
            derive_structural_ranked(&conn, &lexical_from_query, Some(&graph), 10);
        assert!(
            !structural_from_query.is_empty(),
            "expected structural ranking for lexical cohort from query"
        );

        let results = hybrid_search_with_graph("incident", &conn, None, &graph, 10, 60).unwrap();

        assert!(!results.is_empty());
        assert!(
            results.iter().any(|r| r.structural_rank != usize::MAX),
            "expected at least one result to carry a structural rank"
        );
        assert!(
            results.iter().any(|r| r.structural_score > 0.0),
            "expected at least one non-zero structural score"
        );
    }

    #[test]
    fn semantic_ranked_items_drops_low_confidence_when_lexical_empty() {
        let hits = vec![
            SemanticSearchResult {
                item_id: "bn-001".to_string(),
                score: 0.55,
            },
            SemanticSearchResult {
                item_id: "bn-002".to_string(),
                score: 0.53,
            },
        ];

        let ranked = semantic_ranked_items(hits, true);
        assert!(ranked.is_empty());
    }

    #[test]
    fn semantic_ranked_items_keeps_high_confidence_hits() {
        let hits = vec![
            SemanticSearchResult {
                item_id: "bn-001".to_string(),
                score: 0.70,
            },
            SemanticSearchResult {
                item_id: "bn-002".to_string(),
                score: 0.61,
            },
            SemanticSearchResult {
                item_id: "bn-003".to_string(),
                score: 0.59,
            },
        ];

        let ranked = semantic_ranked_items(hits, true);
        assert_eq!(ranked, vec!["bn-001".to_string(), "bn-002".to_string()]);
    }

    #[test]
    fn hybrid_search_excludes_structural_only_results() {
        let conn = setup_db_with_structural_overlap();
        let mut graph = DiGraph::new();
        let n101 = graph.add_node("bn-101".to_string());
        let n102 = graph.add_node("bn-102".to_string());
        let n103 = graph.add_node("bn-103".to_string());
        graph.add_edge(n101, n102, ());
        graph.add_edge(n101, n103, ());

        let results = hybrid_search_with_graph("authentication", &conn, None, &graph, 10, 60)
            .expect("hybrid search should succeed");

        assert!(
            results
                .iter()
                .all(|row| { row.lexical_rank != usize::MAX || row.semantic_rank != usize::MAX }),
            "results should not include structural-only rows"
        );
        assert!(
            results.iter().all(|row| row.item_id != "bn-103"),
            "bn-103 should not appear as structural-only expansion"
        );
    }
}
