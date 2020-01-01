//! Graceful degradation integration tests for search.
//!
//! Verifies that the search system degrades gracefully when components are
//! unavailable — missing ONNX model, missing/corrupt vector table, or
//! semantic layer not compiled in.
//!
//! # Scenarios covered
//!
//! 1. **FTS5-only mode when model file missing** — `hybrid_search` with
//!    `model = None` falls back to lexical-only, returns correct results,
//!    no panic.
//! 2. **Semantic unavailability reported** — `is_semantic_available()` returns
//!    `false` when the `semantic-ort` feature is not compiled in.
//! 3. **KNN search fails gracefully on missing semantic index table** —
//!    `knn_search` returns an error (not a panic) when `item_embeddings` is absent.
//! 4. **Hybrid search does not crash when model unavailable** — no panics
//!    on a range of query types in degraded mode.
//! 5. **find_duplicates works without semantic layer** — end-to-end duplicate
//!    detection uses FTS5-only when `semantic_enabled = false`.
//! 6. **find_duplicates gracefully handles model load failure** — when
//!    `semantic_enabled = true` but the model cannot be loaded, the system
//!    falls back to lexical-only without crashing.

use bones_core::db::migrations;
use bones_core::db::project::{Projector, ensure_tracking_table};
use bones_core::event::data::CreateData;
use bones_core::event::types::EventType;
use bones_core::event::{Event, EventData};
use bones_core::model::item::{Kind, Size, Urgency};
use bones_core::model::item_id::ItemId;
use bones_search::find_duplicates;
use bones_search::fusion::{SearchConfig, hybrid_search};
use bones_search::semantic::{is_semantic_available, knn_search};
use petgraph::graph::DiGraph;
use rusqlite::Connection;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Build an in-memory SQLite database with three items spanning different topics.
fn build_db_with_items() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    migrations::migrate(&mut conn).expect("run migrations");
    ensure_tracking_table(&conn).expect("create tracking table");

    let proj = Projector::new(&conn);

    let items = [
        (
            "bn-001",
            "Fix authentication timeout",
            "OAuth service fails after 30 seconds under load",
        ),
        (
            "bn-002",
            "Database connection pool exhaustion",
            "Pool exhausts under sustained write load",
        ),
        (
            "bn-003",
            "README cleanup and typo fixes",
            "Documentation improvements and spelling corrections",
        ),
    ];

    for (i, (id, title, desc)) in items.iter().enumerate() {
        let event = Event {
            wall_ts_us: 1_000_000 + i as i64,
            agent: "degradation-test".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(*id),
            data: EventData::Create(CreateData {
                title: (*title).into(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: Some((*desc).into()),
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:degradhash{i:04}"),
        };
        proj.project_event(&event)
            .unwrap_or_else(|e| panic!("failed to project {id}: {e:#}"));
    }

    conn
}

// ---------------------------------------------------------------------------
// Scenario 1: FTS5-only mode when model is not provided
// ---------------------------------------------------------------------------

/// When no model is provided (`model = None`), `hybrid_search` must fall back
/// to FTS5-only and still return relevant results.  This simulates: model file
/// missing, model not downloaded, or explicit opt-out of semantic search.
#[test]
fn fts5_only_search_returns_results_when_no_model() {
    let conn = build_db_with_items();

    let results = hybrid_search("authentication", &conn, None, 10, 60)
        .expect("hybrid_search must not fail in FTS5-only mode");

    assert!(
        !results.is_empty(),
        "FTS5 search should still return results when no model is provided"
    );
    assert!(
        results.iter().any(|r| r.item_id == "bn-001"),
        "should find the authentication item via FTS5 lexical match"
    );
}

/// Without a model, all results must carry zero semantic score and non-zero
/// lexical score.  This confirms the result is purely from the FTS5 layer.
#[test]
fn results_are_lexical_only_when_no_model() {
    let conn = build_db_with_items();

    let results =
        hybrid_search("authentication", &conn, None, 10, 60).expect("hybrid_search must succeed");

    assert!(!results.is_empty(), "should have at least one result");

    for r in &results {
        assert_eq!(
            r.semantic_score, 0.0,
            "semantic score must be 0.0 without model (item {})",
            r.item_id
        );
        assert_eq!(
            r.structural_score, 0.0,
            "structural score must be 0.0 for this dataset (item {})",
            r.item_id
        );
        assert!(
            r.lexical_score > 0.0,
            "lexical score must be positive for matching items (item {})",
            r.item_id
        );
    }
}

/// Rank assignments make sense in FTS5-only mode: lexical rank is set,
/// semantic rank is `usize::MAX` (absent from semantic layer).
#[test]
fn rank_fields_correct_in_fts5_only_mode() {
    let conn = build_db_with_items();

    let results =
        hybrid_search("database", &conn, None, 10, 60).expect("hybrid_search must succeed");

    assert!(!results.is_empty());

    for r in &results {
        assert!(
            r.lexical_rank < usize::MAX,
            "lexical_rank should be set (item {})",
            r.item_id
        );
        assert_eq!(
            r.semantic_rank,
            usize::MAX,
            "semantic_rank should be MAX when no model (item {})",
            r.item_id
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 2: Semantic unavailability detection
// ---------------------------------------------------------------------------

/// Without the `semantic-ort` feature, `is_semantic_available()` must report
/// the semantic layer as unavailable.  This is the expected state during tests
/// (no ONNX runtime compiled in, no bundled model bytes).
#[cfg(not(feature = "semantic-ort"))]
#[test]
fn semantic_unavailable_without_ort_feature() {
    assert!(
        !is_semantic_available(),
        "is_semantic_available() must return false when semantic-ort feature is absent"
    );
}

// ---------------------------------------------------------------------------
// Scenario 3: KNN search fails gracefully when semantic index table is missing
// ---------------------------------------------------------------------------

/// When the `item_embeddings` table does not exist (e.g. semantic index not
/// initialized), `knn_search` must return an `Err`, not panic.
///
/// The error message should reference the missing table so callers can
/// diagnose the problem.
#[test]
fn knn_search_errors_gracefully_when_vec_table_missing() {
    let conn = build_db_with_items(); // has items but no item_embeddings table

    let embedding = vec![0.1_f32; 384]; // valid 384-dim embedding
    let result = knn_search(&conn, &embedding, 10);

    assert!(
        result.is_err(),
        "knn_search must return Err when semantic index table is absent"
    );

    let err_msg = result.unwrap_err().to_string();
    // The error should mention the missing semantic index table.
    assert!(
        err_msg.contains("item_embeddings")
            || err_msg.contains("no such table")
            || err_msg.contains("prepare"),
        "error should mention missing semantic index table, got: {err_msg}"
    );
}

/// Wrong-dimension embedding always causes a clear error regardless of table state.
#[test]
fn knn_search_rejects_wrong_dimension_embedding() {
    let conn = build_db_with_items();

    let bad_embedding = vec![0.1_f32; 128]; // wrong dimension (not 384)
    let result = knn_search(&conn, &bad_embedding, 10);

    assert!(result.is_err(), "wrong-dimension embedding must return Err");
    assert!(
        result.unwrap_err().to_string().contains("dimension"),
        "error should mention dimension mismatch"
    );
}

// ---------------------------------------------------------------------------
// Scenario 4: No panics across a range of query types
// ---------------------------------------------------------------------------

/// `hybrid_search` in FTS5-only mode must not panic for any reasonable input,
/// including empty queries, multi-word queries, and no-match queries.
#[test]
fn no_panic_for_varied_queries_in_fts5_only_mode() {
    let conn = build_db_with_items();

    let queries = [
        "authentication",
        "database connection",
        "nonexistent_term_zzz_xyz",
        "fix auth timeout",
        "README", // single capitalized token
        "",       // empty query
    ];

    for query in queries {
        // We don't assert Ok/Err because some queries (especially empty) may
        // legitimately error in FTS5 — the critical invariant is no panic.
        let _ = hybrid_search(query, &conn, None, 10, 60);
    }
}

/// Zero limit returns empty results without crashing.
#[test]
fn zero_limit_returns_empty_no_crash() {
    let conn = build_db_with_items();

    let results =
        hybrid_search("authentication", &conn, None, 0, 60).expect("zero limit must not fail");

    assert!(results.is_empty(), "zero limit should return empty results");
}

/// An empty database (no indexed items) must not crash in FTS5-only mode.
#[test]
fn no_crash_with_empty_index() {
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    migrations::migrate(&mut conn).expect("run migrations");
    ensure_tracking_table(&conn).expect("create tracking table");
    // No items indexed.

    let results = hybrid_search("authentication", &conn, None, 10, 60)
        .expect("search on empty index must not fail");

    assert!(results.is_empty(), "empty index should return no results");
}

// ---------------------------------------------------------------------------
// Scenario 5: find_duplicates without semantic layer
// ---------------------------------------------------------------------------

/// `find_duplicates` with `semantic_enabled = false` runs FTS5-only and must
/// return plausible candidates for a title that matches indexed items.
#[test]
fn find_duplicates_fts5_only_returns_candidates() {
    let conn = build_db_with_items();
    let config = SearchConfig::default();
    let graph: DiGraph<String, ()> = DiGraph::new();

    let candidates = find_duplicates("authentication timeout", &conn, &graph, &config, false, 10)
        .expect("find_duplicates must not fail in FTS5-only mode");

    // Should find at least one candidate via lexical match on "authentication".
    assert!(
        !candidates.is_empty(),
        "should find duplicate candidates via FTS5 for authentication timeout"
    );
    assert!(
        candidates.iter().any(|c| c.item_id == "bn-001"),
        "bn-001 (authentication) should appear as a candidate"
    );
}

/// `find_duplicates` returns an empty list (not an error) when no items match.
#[test]
fn find_duplicates_fts5_only_returns_empty_for_no_match() {
    let conn = build_db_with_items();
    let config = SearchConfig::default();
    let graph: DiGraph<String, ()> = DiGraph::new();

    let candidates = find_duplicates(
        "totally_unrelated_term_xyz_zzz",
        &conn,
        &graph,
        &config,
        false,
        10,
    )
    .expect("find_duplicates must not fail even with no matches");

    // No items match this query, so no candidates expected.
    assert!(
        candidates.is_empty(),
        "unmatched query should produce no candidates"
    );
}

// ---------------------------------------------------------------------------
// Scenario 6: find_duplicates with semantic_enabled=true but model unavailable
// ---------------------------------------------------------------------------

/// When `semantic_enabled = true` but the model cannot be loaded (no bundled
/// model, no ort feature), `find_duplicates` must fall back gracefully to
/// FTS5-only and still return results.  This mirrors the production scenario
/// where the model file is missing or corrupt.
#[test]
fn find_duplicates_falls_back_when_model_load_fails() {
    let conn = build_db_with_items();
    let config = SearchConfig::default();
    let graph: DiGraph<String, ()> = DiGraph::new();

    // semantic_enabled = true, but SemanticModel::load() will fail (no ort, no bundled model).
    // The implementation uses `.ok()` to convert the error to None, then passes None to
    // hybrid_search, which degrades to FTS5-only.
    let candidates = find_duplicates("authentication timeout", &conn, &graph, &config, true, 10)
        .expect("find_duplicates must not fail when model load fails");

    // Lexical match should still work.
    assert!(
        !candidates.is_empty(),
        "find_duplicates should still find candidates via FTS5 when model is unavailable"
    );
}

/// No panics in `find_duplicates` with semantic_enabled=true across various inputs.
#[test]
fn no_panic_in_find_duplicates_semantic_enabled_model_unavailable() {
    let conn = build_db_with_items();
    let config = SearchConfig::default();
    let graph: DiGraph<String, ()> = DiGraph::new();

    let queries = [
        "authentication",
        "database pool",
        "nonexistent_xyz_zzz",
        "README",
    ];

    for query in queries {
        let _ = find_duplicates(query, &conn, &graph, &config, true, 10);
        // Critical: no panic, regardless of error or empty result.
    }
}

// ---------------------------------------------------------------------------
// Scenario 7: Missing semantic index table does not break lexical search
// ---------------------------------------------------------------------------

/// Simulate absent semantic index storage by dropping `item_embeddings` (if it
/// exists), and verify lexical search still works.
#[test]
fn lexical_search_unaffected_by_absent_vector_store() {
    let conn = build_db_with_items();

    // Drop semantic index table if it exists (idempotent in tests).
    let _ = conn.execute_batch("DROP TABLE IF EXISTS item_embeddings");

    // Lexical search with model=None must still work after dropping vec tables.
    let results = hybrid_search("database", &conn, None, 10, 60)
        .expect("lexical search must work without vector tables");

    assert!(
        !results.is_empty(),
        "should find 'database' items via FTS5 even when semantic index table is absent"
    );
    assert!(
        results.iter().any(|r| r.item_id == "bn-002"),
        "database pool item should appear via FTS5"
    );
}
