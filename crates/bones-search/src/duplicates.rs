//! Duplicate detection by orchestrating lexical, semantic, and structural search.
//!
//! This module provides a high-level API for finding potential duplicates of a
//! work item based on its title. It combines results from three search signals:
//!
//! 1. **Lexical search** (FTS5) — matches title text directly
//! 2. **Semantic search** (KNN) — matches items with similar meaning (optional)
//! 3. **Structural search** — matches items with similar metadata/graph structure (optional)
//!
//! Results are fused using Reciprocal Rank Fusion (RRF) and classified into
//! risk levels (likely_duplicate, possibly_related, maybe_related, none).

use crate::fusion::{DupCandidate, SearchConfig, classify_risk, hybrid_search_with_graph};
use crate::semantic::SemanticModel;
use anyhow::Result;
use petgraph::graph::DiGraph;
use rusqlite::Connection;

/// Find potential duplicate candidates for a given title.
///
/// Runs hybrid search and converts results into duplicate candidates with
/// risk classifications. Falls back gracefully to lexical-only when semantic
/// model loading or inference is unavailable.
pub fn find_duplicates(
    query_title: &str,
    db: &Connection,
    graph: &DiGraph<String, ()>,
    config: &SearchConfig,
    semantic_enabled: bool,
    limit: usize,
) -> Result<Vec<DupCandidate>> {
    let model = if semantic_enabled {
        SemanticModel::load().ok()
    } else {
        None
    };

    let fused =
        hybrid_search_with_graph(query_title, db, model.as_ref(), graph, limit, config.rrf_k)?;

    let mut candidates: Vec<DupCandidate> = fused
        .into_iter()
        .map(|r| {
            let risk = classify_risk(r.score, config);
            DupCandidate {
                item_id: r.item_id,
                composite_score: r.score,
                lexical_rank: r.lexical_rank,
                semantic_rank: r.semantic_rank,
                structural_rank: r.structural_rank,
                risk,
            }
        })
        .collect();

    let has_semantic = candidates.iter().any(|c| c.semantic_rank != usize::MAX);
    let has_structural = candidates.iter().any(|c| c.structural_rank != usize::MAX);
    let active_layers = 1 + usize::from(has_semantic) + usize::from(has_structural);
    let max_rrf = active_layers as f32 / (config.rrf_k as f32 + 1.0);
    let cutoff = config.maybe_related_threshold * max_rrf;

    candidates.retain(|c| c.composite_score >= cutoff);
    Ok(candidates)
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
            "Authentication timeout in staging",
            Some("Intermittent auth failures"),
            "h2",
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
    fn find_duplicates_returns_candidates() {
        let conn = setup_db();
        let config = SearchConfig::default();
        let graph: DiGraph<String, ()> = DiGraph::new();

        let out = find_duplicates("timeout", &conn, &graph, &config, false, 10)
            .expect("search should succeed");

        assert!(!out.is_empty());
        assert!(out.iter().all(|c| c.lexical_rank != usize::MAX));
    }

    #[test]
    fn find_duplicates_returns_empty_for_unmatched_query() {
        let conn = setup_db();
        let config = SearchConfig::default();
        let graph: DiGraph<String, ()> = DiGraph::new();

        let out = find_duplicates("totallyunrelatedtermxyz", &conn, &graph, &config, false, 10)
            .expect("search should succeed");

        assert!(out.is_empty());
    }

    #[test]
    fn find_duplicates_uses_structural_graph_when_available() {
        let conn = setup_db();
        let config = SearchConfig {
            maybe_related_threshold: 0.0,
            ..SearchConfig::default()
        };

        let mut graph: DiGraph<String, ()> = DiGraph::new();
        let auth_a = graph.add_node("bn-001".to_string());
        let auth_b = graph.add_node("bn-002".to_string());
        graph.add_edge(auth_a, auth_b, ());

        let direct = crate::structural::structural_similarity("bn-001", "bn-002", &conn, &graph)
            .expect("direct structural similarity should compute");
        assert!(
            direct.mean() > 0.0,
            "expected positive direct structural similarity"
        );

        let out = find_duplicates("timeout", &conn, &graph, &config, false, 10)
            .expect("search should succeed");

        assert!(
            out.iter().any(|c| c.structural_rank != usize::MAX),
            "expected structural ranks to be populated when graph is provided"
        );
    }
}
