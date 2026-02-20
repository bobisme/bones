//! Duplicate risk scoring via Reciprocal Rank Fusion (RRF).
//!
//! This module implements RRF to fuse ranked lists from multiple search signals
//! (lexical, semantic, structural) into a composite duplicate risk score, then
//! maps that score to a discrete risk classification.
//!
//! # Algorithm Overview
//!
//! **Reciprocal Rank Fusion (RRF)** combines multiple ranked lists by scoring
//! each item based on its position in each list:
//!
//! ```text
//! RRF score = sum over all lists of: 1 / (k + rank_in_list)
//! ```
//!
//! Where:
//! - `k` is a constant (default 60) that reduces the impact of high ranks.
//! - Items absent from a list contribute 0 to the sum.
//! - Results are sorted by composite score descending.
//!
//! # Risk Classification
//!
//! The composite RRF score is mapped to a categorical risk level:
//!
//! | Score Range       | Classification     |
//! |-------------------|--------------------|
//! | >= 0.90           | LikelyDuplicate    |
//! | 0.70..0.89        | PossiblyRelated    |
//! | 0.50..0.69        | MaybeRelated       |
//! | < 0.50            | None               |
//!
//! Thresholds are configurable via project config (`.bones/config.toml`).
//!
//! # Example
//!
//! ```ignore
//! use bones_search::fusion::scoring::{rrf_fuse, classify_risk, DuplicateRisk};
//! use bones_core::config::SearchConfig;
//!
//! let lexical_ranked = vec!["bn-001", "bn-002"];
//! let semantic_ranked = vec!["bn-002", "bn-001", "bn-003"];
//! let structural_ranked = vec!["bn-001"];
//!
//! // Fuse into composite scores
//! let fused = rrf_fuse(&lexical_ranked, &semantic_ranked, &structural_ranked, 60);
//!
//! // Classify a score
//! let config = SearchConfig::default();
//! let risk = classify_risk(0.85, &config);
//! assert_eq!(risk, DuplicateRisk::PossiblyRelated);
//! ```

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Risk classification for a candidate duplicate pair.
///
/// Based on the composite RRF score, candidates are classified into one of
/// four risk levels. These determine how prominently the candidate is
/// presented to the user and whether automated warnings are triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DuplicateRisk {
    /// Fused score >= 0.90 — almost certainly the same item.
    ///
    /// Automatic warnings on create, suggestion to merge.
    LikelyDuplicate,

    /// Fused score 0.70..0.89 — strong overlap, worth reviewing.
    ///
    /// Shown prominently in search results.
    PossiblyRelated,

    /// Fused score 0.50..0.69 — some similarity, lower confidence.
    ///
    /// Shown in extended results.
    MaybeRelated,

    /// Fused score < 0.50 — not considered a duplicate.
    ///
    /// Not displayed in duplicate context, though may appear in other searches.
    None,
}

/// A single duplicate candidate with full scoring breakdown.
///
/// Includes the composite RRF-fused score, per-layer rank positions for
/// explainability, and the final risk classification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DupCandidate {
    /// The item ID of the candidate (e.g., `"bn-001"`).
    pub item_id: String,

    /// Final RRF-fused composite score in [0, 1] range.
    ///
    /// Higher scores indicate greater confidence in duplicate/relatedness.
    pub composite_score: f32,

    /// Rank position in the lexical (FTS5) search results (1-indexed).
    ///
    /// `usize::MAX` if the item did not appear in lexical results.
    pub lexical_rank: usize,

    /// Rank position in the semantic (KNN) search results (1-indexed).
    ///
    /// `usize::MAX` if the item did not appear in semantic results.
    pub semantic_rank: usize,

    /// Rank position in the structural similarity results (1-indexed).
    ///
    /// `usize::MAX` if the item did not appear in structural results.
    pub structural_rank: usize,

    /// Classification based on `composite_score` vs configured thresholds.
    pub risk: DuplicateRisk,
}

// ---------------------------------------------------------------------------
// RRF Fusion
// ---------------------------------------------------------------------------

/// Reciprocal Rank Fusion: merge ranked lists from multiple signals.
///
/// Fuses lexical, semantic, and structural ranked lists using RRF to produce
/// a composite score for each item. Items are scored based on their positions
/// in the input lists; items absent from a list contribute 0.
///
/// # Parameters
///
/// - `lexical` — Ranked item IDs from lexical (FTS5) search, best first.
/// - `semantic` — Ranked item IDs from semantic (KNN) search.
/// - `structural` — Ranked item IDs from structural similarity search.
/// - `k` — RRF constant (e.g., 60). Higher values reduce rank impact.
///
/// # Returns
///
/// A vector of `(item_id, composite_score)` sorted by score descending.
///
/// # Algorithm
///
/// For each unique item across all lists:
/// ```text
/// rrf_score = sum(1 / (k + rank_in_list) for each list where item appears)
/// ```
/// Ranks are 1-indexed; absent items contribute 0.
///
/// # Example
///
/// ```
/// use bones_search::fusion::scoring::rrf_fuse;
///
/// let lex = vec!["bn-001", "bn-002"];
/// let sem = vec!["bn-002", "bn-001"];
/// let str = vec!["bn-001"];
/// let result = rrf_fuse(&lex, &sem, &str, 60);
/// assert!(!result.is_empty());
/// ```
pub fn rrf_fuse(
    lexical: &[&str],
    semantic: &[&str],
    structural: &[&str],
    k: usize,
) -> Vec<(String, f32)> {
    let mut scores: BTreeMap<String, f32> = BTreeMap::new();

    // Process lexical ranks
    for (idx, item_id) in lexical.iter().enumerate() {
        let rank = idx + 1; // 1-indexed
        let contribution = 1.0 / (k as f32 + rank as f32);
        scores
            .entry(item_id.to_string())
            .and_modify(|s| *s += contribution)
            .or_insert(contribution);
    }

    // Process semantic ranks
    for (idx, item_id) in semantic.iter().enumerate() {
        let rank = idx + 1;
        let contribution = 1.0 / (k as f32 + rank as f32);
        scores
            .entry(item_id.to_string())
            .and_modify(|s| *s += contribution)
            .or_insert(contribution);
    }

    // Process structural ranks
    for (idx, item_id) in structural.iter().enumerate() {
        let rank = idx + 1;
        let contribution = 1.0 / (k as f32 + rank as f32);
        scores
            .entry(item_id.to_string())
            .and_modify(|s| *s += contribution)
            .or_insert(contribution);
    }

    // Sort by score descending, then by item_id for stability
    let mut result: Vec<_> = scores.into_iter().collect();
    result.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    result
}

/// Build a ranked list of `DupCandidate` items with classification and rank metadata.
///
/// Takes the fused RRF scores and wraps them in `DupCandidate` structs that include
/// per-layer rank positions for explainability. The input lists are searched to find
/// each item's rank in each layer.
///
/// # Parameters
///
/// - `fused` — Pre-sorted list of `(item_id, composite_score)` from `rrf_fuse`.
/// - `lexical` — Original ranked list from lexical search (for rank lookup).
/// - `semantic` — Original ranked list from semantic search.
/// - `structural` — Original ranked list from structural search.
/// - `config` — Search configuration with threshold values.
///
/// # Returns
///
/// A vector of `DupCandidate` structs in the same order as `fused` (descending score).
///
/// # Example
///
/// ```ignore
/// use bones_search::fusion::scoring::{rrf_fuse, build_dup_candidates};
/// use bones_core::config::SearchConfig;
///
/// let lex = vec!["bn-001", "bn-002"];
/// let sem = vec!["bn-002", "bn-001"];
/// let str = vec!["bn-001"];
/// let config = SearchConfig::default();
///
/// let fused = rrf_fuse(&lex, &sem, &str, config.rrf_k);
/// let candidates = build_dup_candidates(&fused, &lex, &sem, &str, &config);
///
/// for cand in candidates {
///     println!("{}: {} ({})", cand.item_id, cand.composite_score, cand.risk);
/// }
/// ```
pub fn build_dup_candidates(
    fused: &[(String, f32)],
    lexical: &[&str],
    semantic: &[&str],
    structural: &[&str],
    config: &SearchConfig,
) -> Vec<DupCandidate> {
    let mut candidates = Vec::with_capacity(fused.len());

    for (item_id, composite_score) in fused {
        // Find ranks in each layer (1-indexed; usize::MAX if absent)
        let lexical_rank = lexical
            .iter()
            .position(|id| id == item_id)
            .map(|idx| idx + 1)
            .unwrap_or(usize::MAX);

        let semantic_rank = semantic
            .iter()
            .position(|id| id == item_id)
            .map(|idx| idx + 1)
            .unwrap_or(usize::MAX);

        let structural_rank = structural
            .iter()
            .position(|id| id == item_id)
            .map(|idx| idx + 1)
            .unwrap_or(usize::MAX);

        let risk = classify_risk(*composite_score, config);

        candidates.push(DupCandidate {
            item_id: item_id.clone(),
            composite_score: *composite_score,
            lexical_rank,
            semantic_rank,
            structural_rank,
            risk,
        });
    }

    candidates
}

// ---------------------------------------------------------------------------
// Risk Classification
// ---------------------------------------------------------------------------

/// Map a fused RRF score to a `DuplicateRisk` classification.
///
/// Classification boundaries are configurable via `SearchConfig` but typically:
/// - >= 0.90: `LikelyDuplicate`
/// - 0.70..0.89: `PossiblyRelated`
/// - 0.50..0.69: `MaybeRelated`
/// - < 0.50: `None`
///
/// # Parameters
///
/// - `score` — Composite RRF score (typically in [0, 1], but unbounded in principle).
/// - `config` — Search configuration with threshold values.
///
/// # Returns
///
/// The appropriate `DuplicateRisk` variant for this score.
///
/// # Example
///
/// ```ignore
/// use bones_search::fusion::scoring::classify_risk;
/// use bones_core::config::SearchConfig;
///
/// let config = SearchConfig::default();
/// assert_eq!(classify_risk(0.95, &config), DuplicateRisk::LikelyDuplicate);
/// assert_eq!(classify_risk(0.75, &config), DuplicateRisk::PossiblyRelated);
/// ```
pub fn classify_risk(score: f32, config: &SearchConfig) -> DuplicateRisk {
    if score >= config.likely_duplicate_threshold {
        DuplicateRisk::LikelyDuplicate
    } else if score >= config.possibly_related_threshold {
        DuplicateRisk::PossiblyRelated
    } else if score >= config.maybe_related_threshold {
        DuplicateRisk::MaybeRelated
    } else {
        DuplicateRisk::None
    }
}

/// Configuration for search/fusion thresholds.
///
/// Loaded from `.bones/config.toml` under the `[search]` section.
/// All threshold values are in [0, 1] range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchConfig {
    /// RRF constant; higher values reduce the impact of high ranks.
    #[serde(default = "default_rrf_k")]
    pub rrf_k: usize,

    /// Score threshold for LikelyDuplicate classification (default 0.90).
    #[serde(default = "default_likely_duplicate_threshold")]
    pub likely_duplicate_threshold: f32,

    /// Score threshold for PossiblyRelated classification (default 0.70).
    #[serde(default = "default_possibly_related_threshold")]
    pub possibly_related_threshold: f32,

    /// Score threshold for MaybeRelated classification (default 0.50).
    #[serde(default = "default_maybe_related_threshold")]
    pub maybe_related_threshold: f32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            rrf_k: default_rrf_k(),
            likely_duplicate_threshold: default_likely_duplicate_threshold(),
            possibly_related_threshold: default_possibly_related_threshold(),
            maybe_related_threshold: default_maybe_related_threshold(),
        }
    }
}

const fn default_rrf_k() -> usize {
    60
}

const fn default_likely_duplicate_threshold() -> f32 {
    0.90
}

const fn default_possibly_related_threshold() -> f32 {
    0.70
}

const fn default_maybe_related_threshold() -> f32 {
    0.50
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // DuplicateRisk
    // -----------------------------------------------------------------------

    #[test]
    fn duplicate_risk_eq() {
        assert_eq!(
            DuplicateRisk::LikelyDuplicate,
            DuplicateRisk::LikelyDuplicate
        );
        assert_ne!(
            DuplicateRisk::LikelyDuplicate,
            DuplicateRisk::PossiblyRelated
        );
    }

    // -----------------------------------------------------------------------
    // DupCandidate
    // -----------------------------------------------------------------------

    #[test]
    fn dup_candidate_fields() {
        let cand = DupCandidate {
            item_id: "bn-001".into(),
            composite_score: 0.85,
            lexical_rank: 1,
            semantic_rank: 2,
            structural_rank: usize::MAX,
            risk: DuplicateRisk::PossiblyRelated,
        };

        assert_eq!(cand.item_id, "bn-001");
        assert!((cand.composite_score - 0.85).abs() < 1e-6);
        assert_eq!(cand.lexical_rank, 1);
        assert_eq!(cand.semantic_rank, 2);
        assert_eq!(cand.structural_rank, usize::MAX);
        assert_eq!(cand.risk, DuplicateRisk::PossiblyRelated);
    }

    #[test]
    fn dup_candidate_clone_eq() {
        let cand = DupCandidate {
            item_id: "bn-001".into(),
            composite_score: 0.75,
            lexical_rank: 1,
            semantic_rank: usize::MAX,
            structural_rank: 3,
            risk: DuplicateRisk::MaybeRelated,
        };

        let cand2 = cand.clone();
        assert_eq!(cand, cand2);
    }

    // -----------------------------------------------------------------------
    // rrf_fuse
    // -----------------------------------------------------------------------

    #[test]
    fn rrf_fuse_empty_lists() {
        let result = rrf_fuse(&[], &[], &[], 60);
        assert!(result.is_empty());
    }

    #[test]
    fn rrf_fuse_single_item_all_lists() {
        let lex = vec!["bn-001"];
        let sem = vec!["bn-001"];
        let str = vec!["bn-001"];
        let result = rrf_fuse(&lex, &sem, &str, 60);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "bn-001");

        // Score = 1/(60+1) + 1/(60+1) + 1/(60+1) = 3/61 ≈ 0.0492
        let expected_score = 3.0 / 61.0;
        assert!((result[0].1 - expected_score).abs() < 1e-6);
    }

    #[test]
    fn rrf_fuse_different_lists() {
        // Lexical: [bn-001 (rank 1), bn-002 (rank 2)]
        // Semantic: [bn-002 (rank 1), bn-001 (rank 2)]
        // Structural: [bn-001 (rank 1)]
        let lex = vec!["bn-001", "bn-002"];
        let sem = vec!["bn-002", "bn-001"];
        let str = vec!["bn-001"];

        let result = rrf_fuse(&lex, &sem, &str, 60);

        // Both items should be present
        assert_eq!(result.len(), 2);

        // bn-001: 1/61 + 1/62 + 1/61 ≈ 0.0493 + 0.0161 + 0.0164 ≈ 0.0818
        let bn001_idx = result.iter().position(|(id, _)| id == "bn-001").unwrap();
        let bn001_score = result[bn001_idx].1;

        // bn-002: 1/62 + 1/61 ≈ 0.0161 + 0.0164 ≈ 0.0325
        let bn002_idx = result.iter().position(|(id, _)| id == "bn-002").unwrap();
        let bn002_score = result[bn002_idx].1;

        // bn-001 should have higher score
        assert!(bn001_score > bn002_score);

        // Sorted descending by score
        assert!(result[0].1 >= result[1].1);
    }

    #[test]
    fn rrf_fuse_disjoint_lists() {
        let lex = vec!["bn-001"];
        let sem = vec!["bn-002"];
        let str = vec!["bn-003"];

        let result = rrf_fuse(&lex, &sem, &str, 60);

        assert_eq!(result.len(), 3);

        // All should have the same score (1/61 each)
        for (_, score) in &result {
            assert!((score - 1.0 / 61.0).abs() < 1e-6);
        }
    }

    #[test]
    fn rrf_fuse_stability_by_item_id() {
        // Two items with identical RRF scores should be sorted by item_id
        let lex = vec!["bn-002", "bn-001"];
        let sem = vec!["bn-001", "bn-002"];
        let str = vec![];

        let result = rrf_fuse(&lex, &sem, &str, 60);

        // Both have score 1/61 + 1/62, but bn-001 < bn-002 lexically
        assert_eq!(result[0].0, "bn-001");
        assert_eq!(result[1].0, "bn-002");
    }

    #[test]
    fn rrf_fuse_respects_k() {
        // Lower k increases the score impact of ranks
        let lex = vec!["bn-001"];
        let sem = vec![];
        let str = vec![];

        let k60 = rrf_fuse(&lex, &sem, &str, 60);
        let k10 = rrf_fuse(&lex, &sem, &str, 10);

        // With k=10: score = 1/11 ≈ 0.0909
        // With k=60: score = 1/61 ≈ 0.0164
        assert!(k10[0].1 > k60[0].1);
    }

    // -----------------------------------------------------------------------
    // classify_risk
    // -----------------------------------------------------------------------

    #[test]
    fn classify_risk_likely_duplicate() {
        let config = SearchConfig::default();

        assert_eq!(classify_risk(0.90, &config), DuplicateRisk::LikelyDuplicate);
        assert_eq!(classify_risk(0.95, &config), DuplicateRisk::LikelyDuplicate);
        assert_eq!(classify_risk(1.0, &config), DuplicateRisk::LikelyDuplicate);
    }

    #[test]
    fn classify_risk_possibly_related() {
        let config = SearchConfig::default();

        assert_eq!(classify_risk(0.70, &config), DuplicateRisk::PossiblyRelated);
        assert_eq!(classify_risk(0.80, &config), DuplicateRisk::PossiblyRelated);
        assert_eq!(classify_risk(0.89, &config), DuplicateRisk::PossiblyRelated);
    }

    #[test]
    fn classify_risk_maybe_related() {
        let config = SearchConfig::default();

        assert_eq!(classify_risk(0.50, &config), DuplicateRisk::MaybeRelated);
        assert_eq!(classify_risk(0.60, &config), DuplicateRisk::MaybeRelated);
        assert_eq!(classify_risk(0.69, &config), DuplicateRisk::MaybeRelated);
    }

    #[test]
    fn classify_risk_none() {
        let config = SearchConfig::default();

        assert_eq!(classify_risk(0.0, &config), DuplicateRisk::None);
        assert_eq!(classify_risk(0.25, &config), DuplicateRisk::None);
        assert_eq!(classify_risk(0.49, &config), DuplicateRisk::None);
    }

    #[test]
    fn classify_risk_boundary_values() {
        let config = SearchConfig::default();

        // Exactly at boundaries
        assert_eq!(classify_risk(0.90, &config), DuplicateRisk::LikelyDuplicate);
        assert_eq!(classify_risk(0.70, &config), DuplicateRisk::PossiblyRelated);
        assert_eq!(classify_risk(0.50, &config), DuplicateRisk::MaybeRelated);

        // Just below boundaries
        assert_eq!(
            classify_risk(0.89999, &config),
            DuplicateRisk::PossiblyRelated
        );
        assert_eq!(classify_risk(0.69999, &config), DuplicateRisk::MaybeRelated);
        assert_eq!(classify_risk(0.49999, &config), DuplicateRisk::None);
    }

    #[test]
    fn classify_risk_custom_thresholds() {
        let config = SearchConfig {
            rrf_k: 60,
            likely_duplicate_threshold: 0.95,
            possibly_related_threshold: 0.75,
            maybe_related_threshold: 0.55,
        };

        assert_eq!(classify_risk(0.95, &config), DuplicateRisk::LikelyDuplicate);
        assert_eq!(classify_risk(0.85, &config), DuplicateRisk::PossiblyRelated);
        assert_eq!(classify_risk(0.65, &config), DuplicateRisk::MaybeRelated);
        assert_eq!(classify_risk(0.45, &config), DuplicateRisk::None);
    }

    // -----------------------------------------------------------------------
    // SearchConfig
    // -----------------------------------------------------------------------

    #[test]
    fn search_config_defaults() {
        let config = SearchConfig::default();
        assert_eq!(config.rrf_k, 60);
        assert!((config.likely_duplicate_threshold - 0.90).abs() < 1e-6);
        assert!((config.possibly_related_threshold - 0.70).abs() < 1e-6);
        assert!((config.maybe_related_threshold - 0.50).abs() < 1e-6);
    }

    #[test]
    fn search_config_clone_eq() {
        let config = SearchConfig::default();
        let config2 = config.clone();
        assert_eq!(config, config2);
    }

    // -----------------------------------------------------------------------
    // build_dup_candidates
    // -----------------------------------------------------------------------

    #[test]
    fn build_dup_candidates_empty_fused() {
        let config = SearchConfig::default();
        let candidates = build_dup_candidates(&[], &[], &[], &[], &config);
        assert!(candidates.is_empty());
    }

    #[test]
    fn build_dup_candidates_rank_metadata() {
        let config = SearchConfig::default();
        let lex = vec!["bn-001", "bn-002"];
        let sem = vec!["bn-002", "bn-001"];
        let str = vec!["bn-001"];

        let fused = rrf_fuse(&lex, &sem, &str, config.rrf_k);
        let candidates = build_dup_candidates(&fused, &lex, &sem, &str, &config);

        // bn-001 should be in position 0 with rank 1 in lex, rank 2 in sem, rank 1 in str
        let bn001_idx = candidates
            .iter()
            .position(|c| c.item_id == "bn-001")
            .unwrap();
        let bn001 = &candidates[bn001_idx];
        assert_eq!(bn001.lexical_rank, 1);
        assert_eq!(bn001.semantic_rank, 2);
        assert_eq!(bn001.structural_rank, 1);

        // bn-002 should have rank 2 in lex, rank 1 in sem, absent in str
        let bn002_idx = candidates
            .iter()
            .position(|c| c.item_id == "bn-002")
            .unwrap();
        let bn002 = &candidates[bn002_idx];
        assert_eq!(bn002.lexical_rank, 2);
        assert_eq!(bn002.semantic_rank, 1);
        assert_eq!(bn002.structural_rank, usize::MAX);
    }

    #[test]
    fn build_dup_candidates_missing_from_all_lists() {
        let config = SearchConfig::default();
        let lex = vec!["bn-001"];
        let sem = vec!["bn-001"];
        let str = vec!["bn-001"];

        // Create a fused list with a synthetic item not in any input list
        let fused = vec![("bn-001".to_string(), 0.85), ("bn-999".to_string(), 0.15)];
        let candidates = build_dup_candidates(&fused, &lex, &sem, &str, &config);

        assert_eq!(candidates.len(), 2);

        // bn-999 should have usize::MAX for all ranks
        let bn999 = &candidates[1];
        assert_eq!(bn999.item_id, "bn-999");
        assert_eq!(bn999.lexical_rank, usize::MAX);
        assert_eq!(bn999.semantic_rank, usize::MAX);
        assert_eq!(bn999.structural_rank, usize::MAX);
    }

    #[test]
    fn build_dup_candidates_applies_risk_classification() {
        let config = SearchConfig::default();
        let lex = vec![];
        let sem = vec![];
        let str = vec![];

        let fused = vec![
            ("bn-likely".to_string(), 0.95),
            ("bn-possibly".to_string(), 0.75),
            ("bn-maybe".to_string(), 0.55),
            ("bn-none".to_string(), 0.25),
        ];

        let candidates = build_dup_candidates(&fused, &lex, &sem, &str, &config);

        assert_eq!(candidates[0].risk, DuplicateRisk::LikelyDuplicate);
        assert_eq!(candidates[1].risk, DuplicateRisk::PossiblyRelated);
        assert_eq!(candidates[2].risk, DuplicateRisk::MaybeRelated);
        assert_eq!(candidates[3].risk, DuplicateRisk::None);
    }

    #[test]
    fn build_dup_candidates_preserves_fused_order() {
        let config = SearchConfig::default();
        let lex = vec![];
        let sem = vec![];
        let str = vec![];

        let fused = vec![
            ("bn-a".to_string(), 0.9),
            ("bn-b".to_string(), 0.8),
            ("bn-c".to_string(), 0.7),
        ];

        let candidates = build_dup_candidates(&fused, &lex, &sem, &str, &config);

        assert_eq!(candidates[0].item_id, "bn-a");
        assert_eq!(candidates[1].item_id, "bn-b");
        assert_eq!(candidates[2].item_id, "bn-c");
    }
}
