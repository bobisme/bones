//! Hybrid search orchestration across lexical, semantic, and structural layers.
//!
//! The orchestrator intentionally degrades gracefully:
//! - lexical search always runs
//! - semantic search runs only when a model is provided and embedding/search succeeds
//! - structural search is currently a placeholder in free-text mode

use crate::fusion::scoring::rrf_fuse;
use crate::semantic::{SemanticModel, knn_search};
use anyhow::{Context, Result};
use bones_core::db::fts::search_bm25;
use rusqlite::Connection;
use tracing::warn;

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
    let limit = limit.min(1000);
    if limit == 0 {
        return Ok(Vec::new());
    }

    let lexical_hits = search_bm25(db, query, limit as u32).context("lexical search failed")?;
    let lexical_ranked: Vec<&str> = lexical_hits.iter().map(|h| h.item_id.as_str()).collect();

    let semantic_ranked_owned = if let Some(model) = model {
        match model
            .embed(query)
            .and_then(|embedding| knn_search(db, &embedding, limit))
        {
            Ok(hits) => hits.into_iter().map(|h| h.item_id).collect(),
            Err(e) => {
                warn!("semantic layer unavailable, falling back to lexical-only fusion: {e}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let semantic_ranked: Vec<&str> = semantic_ranked_owned.iter().map(String::as_str).collect();

    // Structural scoring for free-text search is not available yet.
    let structural_ranked: Vec<&str> = Vec::new();

    let fused = rrf_fuse(&lexical_ranked, &semantic_ranked, &structural_ranked, rrf_k);

    let mut out = Vec::with_capacity(fused.len().min(limit));
    for (item_id, score) in fused.into_iter().take(limit) {
        let lexical_rank = find_rank(&lexical_ranked, &item_id);
        let semantic_rank = find_rank(&semantic_ranked, &item_id);
        let structural_rank = usize::MAX;

        out.push(HybridSearchResult {
            item_id,
            score,
            lexical_score: rank_to_score(lexical_rank, rrf_k),
            semantic_score: rank_to_score(semantic_rank, rrf_k),
            structural_score: 0.0,
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
}
