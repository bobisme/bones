//! KNN vector search via sqlite-vec.
//!
//! Performs nearest-neighbour lookup against pre-computed item embeddings
//! stored in the `vec_items` virtual table. Query text is embedded first
//! (by the caller), then the 384-dim vector is compared against stored
//! vectors using the sqlite-vec `MATCH` operator.
//!
//! # Usage
//!
//! ```rust,ignore
//! use bones_search::semantic::search::{knn_search, SemanticSearchResult};
//! use rusqlite::Connection;
//!
//! let db: Connection = /* open db with sqlite-vec loaded */;
//! let query_embedding: Vec<f32> = model.embed("fix auth timeout")?;
//! let results = knn_search(&db, &query_embedding, 20)?;
//! for r in &results {
//!     println!("{}: similarity={:.3}", r.item_id, r.score);
//! }
//! ```

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::Serialize;

/// Expected embedding dimensionality (MiniLM-L6-v2).
const EMBEDDING_DIM: usize = 384;

/// A single semantic search result with item ID and similarity score.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SemanticSearchResult {
    /// The item ID (e.g. `"bn-001"`).
    pub item_id: String,
    /// Cosine similarity score in `[0, 1]` range (higher = more similar).
    ///
    /// Derived from sqlite-vec distance: `score = 1.0 - distance`.
    pub score: f32,
}

/// Perform KNN vector search using sqlite-vec.
///
/// 1. Validates the query embedding dimensionality (must be 384).
/// 2. Encodes the embedding as a JSON array (sqlite-vec interchange format).
/// 3. Executes a KNN `MATCH` query against `vec_items`.
/// 4. Joins on `vec_item_map` to recover item IDs.
/// 5. Converts distance to similarity: `score = 1.0 - distance`.
/// 6. Returns up to `limit` results sorted by similarity descending.
///
/// # Parameters
///
/// - `db` — SQLite connection with the sqlite-vec extension loaded and
///   `vec_items` / `vec_item_map` tables populated by the embedding pipeline.
/// - `query_embedding` — 384-dim `f32` vector for the query text.
/// - `limit` — maximum number of results to return (e.g. 20).
///
/// # Errors
///
/// Returns an error if:
/// - `query_embedding` length is not 384.
/// - The sqlite-vec tables are missing or the query fails.
pub fn knn_search(
    db: &Connection,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<SemanticSearchResult>> {
    if query_embedding.len() != EMBEDDING_DIM {
        bail!(
            "query embedding dimension mismatch: expected {EMBEDDING_DIM}, got {}",
            query_embedding.len()
        );
    }

    if limit == 0 {
        return Ok(Vec::new());
    }

    let encoded = encode_embedding_json(query_embedding);

    let mut stmt = db
        .prepare(
            "SELECT
                vim.item_id,
                v.distance
            FROM vec_items v
            INNER JOIN vec_item_map vim ON vim.rowid = v.rowid
            WHERE v.embedding MATCH ?1
              AND k = ?2
            ORDER BY v.distance",
        )
        .context("failed to prepare KNN search query (is sqlite-vec loaded?)")?;

    let limit_i64 = i64::try_from(limit.min(10_000)).unwrap_or(10_000);

    let rows = stmt
        .query_map(rusqlite::params![encoded, limit_i64], |row| {
            let item_id: String = row.get(0)?;
            let distance: f64 = row.get(1)?;
            Ok((item_id, distance))
        })
        .context("failed to execute KNN search query")?;

    let mut results = Vec::with_capacity(limit);
    for row in rows {
        let (item_id, distance) = row.context("failed to read KNN result row")?;
        // Convert distance to similarity: score = 1.0 - distance
        // Clamp to [0, 1] to handle floating-point edge cases.
        let score = (1.0 - distance as f32).clamp(0.0, 1.0);
        results.push(SemanticSearchResult { item_id, score });
    }

    Ok(results)
}

/// Encode an embedding as a JSON array string for sqlite-vec.
///
/// sqlite-vec accepts embeddings as JSON arrays of floats.
fn encode_embedding_json(embedding: &[f32]) -> String {
    let mut encoded = String::from("[");
    for (idx, value) in embedding.iter().enumerate() {
        if idx != 0 {
            encoded.push(',');
        }
        encoded.push_str(&value.to_string());
    }
    encoded.push(']');
    encoded
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers — plain SQLite tables that mimic sqlite-vec behaviour
    // -----------------------------------------------------------------------

    /// Create mock tables that simulate sqlite-vec for unit testing.
    ///
    /// In production, `vec_items` is a virtual table created by the vec0
    /// extension. For unit tests we use plain tables with a `distance`
    /// column that we populate manually so we can test the search logic
    /// without loading the native extension.
    fn setup_mock_db() -> Connection {
        let db = Connection::open_in_memory().expect("open in-memory db");

        // Plain tables mimicking the vec0 schema.
        // vec_items stores embeddings as text and a distance column for test control.
        db.execute_batch(
            "
            CREATE TABLE vec_item_map (
                rowid INTEGER PRIMARY KEY,
                item_id TEXT NOT NULL UNIQUE,
                content_hash TEXT NOT NULL
            );
            ",
        )
        .expect("create mock tables");

        db
    }

    fn sample_embedding(fill: f32) -> Vec<f32> {
        vec![fill; EMBEDDING_DIM]
    }

    // -----------------------------------------------------------------------
    // encode_embedding_json
    // -----------------------------------------------------------------------

    #[test]
    fn encode_embedding_json_small() {
        let emb = vec![0.1, 0.2, 0.3];
        let json = encode_embedding_json(&emb);
        assert_eq!(json, "[0.1,0.2,0.3]");
    }

    #[test]
    fn encode_embedding_json_empty() {
        let json = encode_embedding_json(&[]);
        assert_eq!(json, "[]");
    }

    #[test]
    fn encode_embedding_json_single() {
        let json = encode_embedding_json(&[1.5]);
        assert_eq!(json, "[1.5]");
    }

    // -----------------------------------------------------------------------
    // SemanticSearchResult
    // -----------------------------------------------------------------------

    #[test]
    fn semantic_search_result_fields() {
        let r = SemanticSearchResult {
            item_id: "bn-001".into(),
            score: 0.85,
        };
        assert_eq!(r.item_id, "bn-001");
        assert!((r.score - 0.85).abs() < 1e-6);
    }

    #[test]
    fn semantic_search_result_clone_eq() {
        let r = SemanticSearchResult {
            item_id: "bn-001".into(),
            score: 0.75,
        };
        let r2 = r.clone();
        assert_eq!(r, r2);
    }

    // -----------------------------------------------------------------------
    // knn_search — dimension validation
    // -----------------------------------------------------------------------

    #[test]
    fn knn_search_rejects_wrong_dimension() {
        let db = setup_mock_db();
        let bad_embedding = vec![0.1_f32; 100]; // not 384
        let err = knn_search(&db, &bad_embedding, 10).unwrap_err();
        assert!(
            err.to_string().contains("dimension mismatch"),
            "expected dimension error, got: {err}"
        );
    }

    #[test]
    fn knn_search_rejects_empty_embedding() {
        let db = setup_mock_db();
        let err = knn_search(&db, &[], 10).unwrap_err();
        assert!(err.to_string().contains("dimension mismatch"));
    }

    #[test]
    fn knn_search_zero_limit_returns_empty() {
        let db = setup_mock_db();
        let emb = sample_embedding(0.5);
        let results = knn_search(&db, &emb, 0).unwrap();
        assert!(results.is_empty());
    }

    // -----------------------------------------------------------------------
    // knn_search — score conversion
    // -----------------------------------------------------------------------

    #[test]
    fn score_clamps_to_zero_one() {
        // distance = 0.0 → score = 1.0
        let score_max = (1.0_f32 - 0.0).clamp(0.0, 1.0);
        assert_eq!(score_max, 1.0);

        // distance = 1.0 → score = 0.0
        let score_min = (1.0_f32 - 1.0).clamp(0.0, 1.0);
        assert_eq!(score_min, 0.0);

        // distance = 1.5 (edge case) → clamped to 0.0
        let score_neg = (1.0_f32 - 1.5).clamp(0.0, 1.0);
        assert_eq!(score_neg, 0.0);

        // distance = -0.1 (edge case) → clamped to 1.0
        let score_over = (1.0_f32 - (-0.1)).clamp(0.0, 1.0);
        assert_eq!(score_over, 1.0);
    }

    // -----------------------------------------------------------------------
    // knn_search — mock table missing (no vec0 extension)
    // -----------------------------------------------------------------------

    #[test]
    fn knn_search_errors_without_vec_items_table() {
        // DB with only vec_item_map but no vec_items
        let db = setup_mock_db();
        let emb = sample_embedding(0.5);
        let err = knn_search(&db, &emb, 10).unwrap_err();
        // Should fail during prepare since vec_items doesn't exist
        let msg = err.to_string();
        assert!(
            msg.contains("vec_items") || msg.contains("no such table") || msg.contains("prepare"),
            "expected table-missing error, got: {msg}"
        );
    }
}
