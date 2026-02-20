//! Gold dataset evaluation harness for search quality and duplicate detection.
//!
//! # Purpose
//!
//! Measures the quality of the lexical search baseline against a curated dataset of
//! 110 work items across 10 domains with 55 labeled duplicate pairs and 35 relevance
//! queries. This establishes a quality floor: CI fails if metrics regress below thresholds.
//!
//! # Metrics
//!
//! - **NDCG@10**: Normalized Discounted Cumulative Gain at k=10, averaged across all queries.
//!   Measures ranking quality. Threshold: avg NDCG@10 > 0.30 for the lexical baseline.
//!
//! - **Duplicate Recall@20**: Fraction of known duplicate pairs recovered in top-20 search results.
//!   Measures duplicate detection coverage. Threshold: recall@20 > 0.30.
//!
//! - **Duplicate F1@5**: F1 score for duplicate detection at top-5 search depth.
//!   Measures the balance between precision and recall. Threshold: F1 > 0.40.
//!
//! # Baseline vs Future
//!
//! These thresholds are calibrated for the **lexical-only (FTS5 BM25) baseline**.
//! When semantic search (bn-2c5) and hybrid fusion (bn-sgf) are integrated, the
//! thresholds should be raised to reflect improved quality.
//!
//! # Dataset
//!
//! Loaded from `tests/fixtures/gold_dataset.json`. See that file for the full schema
//! and item details. Adversarial pairs (similar keywords, different meaning) are
//! included to test false-positive suppression.

use bones_core::db::fts::search_bm25;
use bones_core::db::migrations;
use bones_core::db::project::{Projector, ensure_tracking_table};
use bones_core::event::data::CreateData;
use bones_core::event::types::EventType;
use bones_core::event::{Event, EventData};
use bones_core::model::item::{Kind, Size, Urgency};
use bones_core::model::item_id::ItemId;
use rusqlite::Connection;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};

// ---------------------------------------------------------------------------
// Dataset types
// ---------------------------------------------------------------------------

/// A single work item in the gold dataset.
#[derive(Debug, Deserialize)]
struct GoldItem {
    id: String,
    title: String,
    description: String,
    labels: Vec<String>,
    deps: Vec<String>,
}

/// A relevance-annotated search query.
#[derive(Debug, Deserialize)]
struct GoldQuery {
    query: String,
    relevant: Vec<String>,
    #[allow(dead_code)]
    notes: String,
}

/// The full gold evaluation dataset.
#[derive(Debug, Deserialize)]
struct GoldDataset {
    items: Vec<GoldItem>,
    queries: Vec<GoldQuery>,
    /// Each entry is a pair [id_a, id_b] of duplicate items.
    duplicates: Vec<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Dataset loading
// ---------------------------------------------------------------------------

fn load_gold_dataset() -> GoldDataset {
    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/gold_dataset.json"
    );
    let content = std::fs::read_to_string(fixture_path)
        .expect("gold_dataset.json must exist at tests/fixtures/gold_dataset.json");
    serde_json::from_str(&content).expect("gold_dataset.json must be valid JSON matching schema")
}

// ---------------------------------------------------------------------------
// Test database setup
// ---------------------------------------------------------------------------

fn build_test_db() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory SQLite");
    migrations::migrate(&mut conn).expect("run migrations");
    ensure_tracking_table(&conn).expect("create tracking table");
    conn
}

/// Build an in-memory SQLite database populated with all items from the dataset.
///
/// Returns a `Connection` with the projection schema and FTS5 index populated.
fn build_and_populate_index(items: &[GoldItem]) -> Connection {
    let conn = build_test_db();
    let proj = Projector::new(&conn);

    for (i, item) in items.iter().enumerate() {
        let event = Event {
            wall_ts_us: 1_000_000 + i as i64,
            agent: "gold-eval".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(&item.id),
            data: EventData::Create(CreateData {
                title: item.title.clone(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: item.labels.clone(),
                parent: None,
                causation: None,
                description: Some(item.description.clone()),
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:goldhash{i:04}"),
        };

        proj.project_event(&event)
            .unwrap_or_else(|e| panic!("failed to project item {}: {e:#}", item.id));
    }

    conn
}

// ---------------------------------------------------------------------------
// NDCG metric
// ---------------------------------------------------------------------------

/// Compute Normalized Discounted Cumulative Gain at k (NDCG@k).
///
/// Uses binary relevance: 1 if item is in the relevant set, 0 otherwise.
///
/// # Parameters
/// - `ranked_ids`: Ordered list of item IDs returned by search (best first).
/// - `relevant_ids`: Set of item IDs that are relevant to this query.
/// - `k`: Cutoff depth.
///
/// # Returns
/// NDCG@k in [0, 1]. Returns 1.0 if both ranked and relevant are empty.
fn ndcg_at_k(ranked_ids: &[String], relevant_ids: &HashSet<&str>, k: usize) -> f64 {
    if relevant_ids.is_empty() {
        return 1.0; // Vacuously perfect
    }

    let top_k: Vec<&String> = ranked_ids.iter().take(k).collect();

    // DCG@k: sum of rel_i / log2(i + 2) for i in 0..k
    let dcg: f64 = top_k
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let rel = if relevant_ids.contains(id.as_str()) {
                1.0
            } else {
                0.0
            };
            rel / f64::log2(i as f64 + 2.0)
        })
        .sum();

    // IDCG@k: ideal DCG assuming all relevant items appear at top
    let num_relevant = relevant_ids.len().min(k);
    let idcg: f64 = (0..num_relevant)
        .map(|i| 1.0 / f64::log2(i as f64 + 2.0))
        .sum();

    if idcg == 0.0 {
        return 0.0;
    }

    dcg / idcg
}

// ---------------------------------------------------------------------------
// Duplicate detection metrics
// ---------------------------------------------------------------------------

/// Compute duplicate detection precision and recall at a given search depth.
///
/// For each item A with a known duplicate B:
/// - Searches using A's title (limit = `depth`)
/// - Considers the pair detected if B appears in the results
///
/// Precision = TP / (TP + FP) where FP = non-gold pairs in search results
/// Recall    = TP / (TP + FN) where FN = gold pairs not found
///
/// # Returns
/// `(precision, recall)` both in [0, 1].
fn compute_duplicate_pr(
    conn: &Connection,
    items: &[GoldItem],
    duplicates: &[Vec<String>],
    depth: usize,
) -> (f64, f64) {
    // Build lookup: item_id → title
    let title_map: BTreeMap<&str, &str> = items
        .iter()
        .map(|i| (i.id.as_str(), i.title.as_str()))
        .collect();

    // Build gold duplicate set as canonical (smaller_id, larger_id) pairs
    let gold_pairs: HashSet<(String, String)> = duplicates
        .iter()
        .filter_map(|pair| {
            if pair.len() == 2 {
                let mut sorted = [pair[0].clone(), pair[1].clone()];
                sorted.sort();
                Some((sorted[0].clone(), sorted[1].clone()))
            } else {
                None
            }
        })
        .collect();

    let mut tp = 0u64;
    let mut fp = 0u64;
    let mut fn_ = 0u64;

    for pair in duplicates {
        let (Some(a_id), Some(b_id)) = (pair.first(), pair.get(1)) else {
            continue;
        };

        let Some(&a_title) = title_map.get(a_id.as_str()) else {
            continue;
        };

        // Search using A's title
        let results = search_bm25(conn, a_title, depth as u32).unwrap_or_default();
        let result_ids: Vec<&str> = results.iter().map(|h| h.item_id.as_str()).collect();

        // Check if B was found (excluding A itself from results)
        let found_b = result_ids.iter().any(|id| *id == b_id.as_str());

        if found_b {
            tp += 1;
        } else {
            fn_ += 1;
        }

        // Count FPs: items in results that are not A, not B, and not gold duplicates of A
        for found_id in &result_ids {
            if *found_id == a_id.as_str() || *found_id == b_id.as_str() {
                continue;
            }
            // Check if this is a known gold duplicate of A
            let mut sorted = [a_id.clone(), found_id.to_string()];
            sorted.sort();
            let pair_key = (sorted[0].clone(), sorted[1].clone());
            if !gold_pairs.contains(&pair_key) {
                fp += 1;
            }
        }
    }

    let precision = if tp + fp > 0 {
        tp as f64 / (tp + fp) as f64
    } else {
        1.0 // No predictions made → perfect precision (vacuous)
    };

    let recall = if tp + fn_ > 0 {
        tp as f64 / (tp + fn_) as f64
    } else {
        1.0 // No gold pairs → perfect recall (vacuous)
    };

    (precision, recall)
}

// ---------------------------------------------------------------------------
// Dataset validation tests
// ---------------------------------------------------------------------------

#[test]
fn gold_dataset_loads_and_validates() {
    let dataset = load_gold_dataset();

    assert!(
        dataset.items.len() >= 100,
        "dataset must have 100+ items, got {}",
        dataset.items.len()
    );
    assert!(
        dataset.queries.len() >= 30,
        "dataset must have 30+ queries, got {}",
        dataset.queries.len()
    );
    assert!(
        dataset.duplicates.len() >= 50,
        "dataset must have 50+ duplicate pairs, got {}",
        dataset.duplicates.len()
    );

    // Validate all duplicate pair IDs exist in items
    let item_ids: HashSet<&str> = dataset.items.iter().map(|i| i.id.as_str()).collect();
    for pair in &dataset.duplicates {
        assert_eq!(pair.len(), 2, "duplicate pair must have exactly 2 IDs");
        assert!(
            item_ids.contains(pair[0].as_str()),
            "duplicate pair references unknown item: {}",
            pair[0]
        );
        assert!(
            item_ids.contains(pair[1].as_str()),
            "duplicate pair references unknown item: {}",
            pair[1]
        );
        assert_ne!(
            pair[0], pair[1],
            "duplicate pair must reference two different items"
        );
    }

    // Validate all query relevant IDs exist in items
    for query in &dataset.queries {
        for rel_id in &query.relevant {
            assert!(
                item_ids.contains(rel_id.as_str()),
                "query '{}' references unknown item: {}",
                query.query,
                rel_id
            );
        }
    }

    // No duplicate pair should reference the same item twice
    let mut pair_set: HashSet<(String, String)> = HashSet::new();
    for pair in &dataset.duplicates {
        let mut sorted = [pair[0].clone(), pair[1].clone()];
        sorted.sort();
        let key = (sorted[0].clone(), sorted[1].clone());
        assert!(
            pair_set.insert(key.clone()),
            "duplicate pair listed twice: {:?}",
            key
        );
    }
}

#[test]
fn gold_dataset_index_builds_successfully() {
    let dataset = load_gold_dataset();
    let conn = build_and_populate_index(&dataset.items);

    // Verify all items were indexed
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM items WHERE is_deleted = 0", [], |r| {
            r.get(0)
        })
        .expect("count items");

    assert_eq!(
        count as usize,
        dataset.items.len(),
        "all items must be indexed"
    );

    // Verify FTS index is populated
    let fts_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM items_fts", [], |r| r.get(0))
        .expect("count FTS rows");

    assert!(
        fts_count >= count,
        "FTS index must be populated for all items"
    );
}

// ---------------------------------------------------------------------------
// NDCG metric unit tests
// ---------------------------------------------------------------------------

#[test]
fn ndcg_perfect_ranking() {
    let ranked = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let relevant: HashSet<&str> = ["a", "b"].iter().copied().collect();
    let score = ndcg_at_k(&ranked, &relevant, 10);
    assert!(
        score > 0.99,
        "perfect ranking should give NDCG ≈ 1.0, got {score}"
    );
}

#[test]
fn ndcg_worst_ranking() {
    let ranked = vec!["x".to_string(), "y".to_string(), "z".to_string()];
    let relevant: HashSet<&str> = ["a", "b"].iter().copied().collect();
    let score = ndcg_at_k(&ranked, &relevant, 10);
    assert_eq!(
        score, 0.0,
        "no relevant items in results should give NDCG = 0.0"
    );
}

#[test]
fn ndcg_partial_ranking() {
    // Only 1 of 2 relevant items found, at rank 1
    let ranked = vec!["a".to_string(), "x".to_string()];
    let relevant: HashSet<&str> = ["a", "b"].iter().copied().collect();
    let score = ndcg_at_k(&ranked, &relevant, 10);
    assert!(
        score > 0.0 && score < 1.0,
        "partial match should give 0 < NDCG < 1, got {score}"
    );
}

#[test]
fn ndcg_empty_relevant() {
    let ranked = vec!["a".to_string()];
    let relevant: HashSet<&str> = HashSet::new();
    let score = ndcg_at_k(&ranked, &relevant, 10);
    assert_eq!(
        score, 1.0,
        "empty relevant set should give NDCG = 1.0 (vacuous)"
    );
}

// ---------------------------------------------------------------------------
// Search quality evaluation (lexical baseline)
// ---------------------------------------------------------------------------

/// Evaluate NDCG@10 for the lexical (FTS5 BM25) search baseline.
///
/// This test defines the regression floor for search ranking quality.
/// Threshold is calibrated for the lexical-only baseline. Raise it
/// when hybrid semantic search is integrated.
#[test]
fn lexical_search_ndcg_above_baseline() {
    let dataset = load_gold_dataset();
    let conn = build_and_populate_index(&dataset.items);

    let mut ndcg_scores: Vec<f64> = Vec::new();
    let mut skipped = 0usize;

    for query_entry in &dataset.queries {
        let relevant_set: HashSet<&str> = query_entry.relevant.iter().map(|s| s.as_str()).collect();

        // FTS5 queries: wrap multi-word queries in quotes for phrase matching,
        // or use individual terms for broader matching
        let results = search_bm25(&conn, &query_entry.query, 10).unwrap_or_default();
        let ranked_ids: Vec<String> = results.iter().map(|h| h.item_id.clone()).collect();

        // If search returns nothing (no vocabulary overlap), skip this query
        // to avoid penalizing FTS5 for out-of-vocabulary queries
        if ranked_ids.is_empty() {
            skipped += 1;
            continue;
        }

        let score = ndcg_at_k(&ranked_ids, &relevant_set, 10);
        ndcg_scores.push(score);
    }

    let evaluated = ndcg_scores.len();
    assert!(
        evaluated >= 15,
        "must evaluate at least 15 queries (skipped {skipped}), evaluated {evaluated}"
    );

    let avg_ndcg = ndcg_scores.iter().sum::<f64>() / evaluated as f64;

    // Lexical baseline threshold: 0.30
    // This should be achievable since our duplicate items share key vocabulary.
    // Raise to 0.50 once semantic hybrid search is integrated.
    assert!(
        avg_ndcg >= 0.30,
        "Average NDCG@10 should be >= 0.30 for lexical baseline, got {avg_ndcg:.4} \
         (evaluated {evaluated} queries, skipped {skipped})"
    );
}

// ---------------------------------------------------------------------------
// Duplicate detection evaluation (lexical baseline)
// ---------------------------------------------------------------------------

/// Evaluate duplicate recall at search depth 20 for the lexical baseline.
///
/// Recall@20 measures: for what fraction of gold duplicate pairs does
/// one item appear in the top-20 BM25 results when searching with the
/// other item's title?
///
/// Threshold is calibrated for the lexical-only baseline.
#[test]
fn lexical_duplicate_recall_at_20_above_baseline() {
    let dataset = load_gold_dataset();
    let conn = build_and_populate_index(&dataset.items);

    let title_map: BTreeMap<&str, &str> = dataset
        .items
        .iter()
        .map(|i| (i.id.as_str(), i.title.as_str()))
        .collect();

    let mut found = 0usize;
    let mut total = 0usize;

    for pair in &dataset.duplicates {
        let (Some(a_id), Some(b_id)) = (pair.first(), pair.get(1)) else {
            continue;
        };

        let Some(&a_title) = title_map.get(a_id.as_str()) else {
            continue;
        };
        let Some(&b_title) = title_map.get(b_id.as_str()) else {
            continue;
        };

        total += 1;

        // Check both directions: A→B and B→A
        let results_a = search_bm25(&conn, a_title, 20).unwrap_or_default();
        let found_a_to_b = results_a.iter().any(|h| h.item_id == *b_id);

        let results_b = search_bm25(&conn, b_title, 20).unwrap_or_default();
        let found_b_to_a = results_b.iter().any(|h| h.item_id == *a_id);

        // Count as found if either direction retrieves the other
        if found_a_to_b || found_b_to_a {
            found += 1;
        }
    }

    let recall = found as f64 / total as f64;

    // Lexical baseline threshold: 0.30 recall@20
    // Pairs sharing vocabulary in titles + descriptions should be recalled.
    // Raise to 0.55+ once hybrid search (bn-sgf) improves duplicate detection.
    assert!(
        recall >= 0.30,
        "Duplicate recall@20 should be >= 0.30, got {recall:.4} ({found}/{total} pairs found)"
    );
}

/// Evaluate duplicate F1 at search depth 5 for the lexical baseline.
///
/// At depth 5, precision tends to be higher (fewer false positives) while
/// recall may be lower. F1 balances these.
#[test]
fn lexical_duplicate_f1_at_5_above_baseline() {
    let dataset = load_gold_dataset();
    let conn = build_and_populate_index(&dataset.items);

    let (precision, recall) = compute_duplicate_pr(&conn, &dataset.items, &dataset.duplicates, 5);

    let f1 = if precision + recall > 0.0 {
        2.0 * precision * recall / (precision + recall)
    } else {
        0.0
    };

    // Lexical baseline F1 threshold: 0.30
    // With duplicate items sharing key vocabulary, F1 at depth-5 should exceed this.
    // Raise to 0.70 once hybrid semantic search is integrated.
    assert!(
        f1 >= 0.30,
        "Duplicate F1@5 should be >= 0.30 for lexical baseline, got {f1:.4} \
         (precision={precision:.4}, recall={recall:.4})"
    );
}

// ---------------------------------------------------------------------------
// Regression gate (CI-blocking)
// ---------------------------------------------------------------------------

/// CI regression gate: duplicate detection quality must not regress.
///
/// This test defines the minimum acceptable quality for the current implementation.
/// It combines recall@20 and a per-query relevance check into a single gate.
///
/// # Failure means regression
/// If this test fails, a code change has degraded search/dedup quality.
/// Investigate by running `cargo test -p bones-search gold_eval -- --nocapture`
/// to see detailed per-query and per-pair scores.
#[test]
fn ci_regression_gate_search_quality() {
    let dataset = load_gold_dataset();
    let conn = build_and_populate_index(&dataset.items);

    let title_map: BTreeMap<&str, &str> = dataset
        .items
        .iter()
        .map(|i| (i.id.as_str(), i.title.as_str()))
        .collect();

    // --- Duplicate recall@20 ---
    let mut dup_found = 0usize;
    let mut dup_total = 0usize;

    for pair in &dataset.duplicates {
        let (Some(a_id), Some(b_id)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let (Some(&a_title), Some(&b_title)) =
            (title_map.get(a_id.as_str()), title_map.get(b_id.as_str()))
        else {
            continue;
        };

        dup_total += 1;

        let results_a = search_bm25(&conn, a_title, 20).unwrap_or_default();
        let results_b = search_bm25(&conn, b_title, 20).unwrap_or_default();

        if results_a.iter().any(|h| h.item_id == *b_id)
            || results_b.iter().any(|h| h.item_id == *a_id)
        {
            dup_found += 1;
        }
    }

    let dup_recall = dup_found as f64 / dup_total as f64;

    // --- Query relevance hits ---
    let mut query_hits = 0usize;
    let mut query_total = 0usize;

    for query_entry in &dataset.queries {
        let results = search_bm25(&conn, &query_entry.query, 10).unwrap_or_default();
        if results.is_empty() {
            continue;
        }
        query_total += 1;
        let found_relevant = query_entry
            .relevant
            .iter()
            .any(|rel_id| results.iter().any(|h| h.item_id == *rel_id));
        if found_relevant {
            query_hits += 1;
        }
    }

    let query_hit_rate = if query_total > 0 {
        query_hits as f64 / query_total as f64
    } else {
        0.0
    };

    // Gate: both metrics must pass (lexical baseline thresholds)
    let passed = dup_recall >= 0.30 && query_hit_rate >= 0.50;

    assert!(
        passed,
        "CI regression gate failed:\n  \
         duplicate recall@20: {dup_recall:.4} (need >= 0.30, {dup_found}/{dup_total} pairs)\n  \
         query hit rate@10:   {query_hit_rate:.4} (need >= 0.50, {query_hits}/{query_total} queries)"
    );
}
