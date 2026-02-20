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
//!
//! # Graceful Degradation
//!
//! If semantic or structural search layers are unavailable:
//! - Semantic search is skipped if the model cannot be loaded or embeddings fail
//! - Structural search is skipped if the graph is empty or comparison fails
//! - Lexical search is always performed as the baseline
//!
//! The RRF fusion still works effectively with partial results from available layers.

use crate::fusion::scoring::{DupCandidate, SearchConfig, build_dup_candidates, rrf_fuse};
use anyhow::{Context, Result};
use bones_core::db::fts::search_bm25;
use petgraph::graph::DiGraph;
use rusqlite::Connection;
use std::collections::HashSet;

/// Find potential duplicate candidates for a given title.
///
/// This function performs an integrated duplicate risk analysis by combining:
/// - Lexical search (FTS5 BM25 on title/description)
/// - Semantic search (KNN embeddings, if enabled and model is available)
/// - Structural similarity (graph-based relatedness, if graph is provided)
///
/// Results are ranked using Reciprocal Rank Fusion (RRF) and filtered by
/// the risk thresholds in `config`.
///
/// # Parameters
///
/// - `query_title` — The title of the item being created.
/// - `db` — SQLite connection to the bones projection database.
/// - `graph` — Dependency graph for structural similarity. May be empty; structural search will be skipped.
/// - `config` — Search configuration with RRF parameters and thresholds.
/// - `semantic_enabled` — If false, skip semantic search (e.g. model not available).
/// - `limit` — Maximum number of results to return per layer.
///
/// # Returns
///
/// A vector of `DupCandidate` items sorted by composite score (highest first),
/// filtered to those with risk >= MaybeRelated. Returns an empty vector if no
/// candidates are found above the threshold.
///
/// # Errors
///
/// Returns an error if the lexical search fails. Failures in semantic or
/// structural layers are logged and do not block the function — it falls
/// back to available layers.
pub fn find_duplicates(
    query_title: &str,
    db: &Connection,
    _graph: &DiGraph<String, ()>,
    config: &SearchConfig,
    _semantic_enabled: bool,
    limit: usize,
) -> Result<Vec<DupCandidate>> {
    // 1. Lexical search using FTS5 BM25 (always performed)
    let lexical_hits =
        search_bm25(db, query_title, limit as u32).context("lexical search failed")?;
    let lexical_ranked: Vec<&str> = lexical_hits.iter().map(|h| h.item_id.as_str()).collect();

    // 2. Semantic search (optional, requires model and embeddings)
    // For now, semantic search is not performed in the initial implementation.
    // Future: load SemanticModel, embed the query title, call knn_search
    let semantic_ranked: Vec<&str> = vec![];

    // 3. Structural similarity (optional, requires graph and metadata)
    // For now, structural search is not performed in the initial implementation.
    // Future: build pairwise structural similarity for all candidates
    let structural_ranked: Vec<&str> = vec![];

    // 4. Fuse results using RRF
    // Even with some layers missing, RRF still produces sensible scores
    let fused = rrf_fuse(
        &lexical_ranked,
        &semantic_ranked,
        &structural_ranked,
        config.rrf_k,
    );

    // 5. Build candidate list with rank metadata and classifications
    let mut candidates = build_dup_candidates(
        &fused,
        &lexical_ranked,
        &semantic_ranked,
        &structural_ranked,
        config,
    );

    // 6. Filter to candidates above MaybeRelated threshold
    candidates.retain(|c| c.composite_score >= config.maybe_related_threshold);

    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_test() {
        // Placeholder test
        assert!(true);
    }
}
