//! Semantic KNN search over stored item embeddings.
//!
//! Query text is embedded by the caller, then compared against vectors stored
//! in `item_embeddings.embedding_json`.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::Serialize;
use tracing::debug;

/// Expected embedding dimensionality (MiniLM-L6-v2).
const EMBEDDING_DIM: usize = 384;

/// A single semantic search result with item ID and similarity score.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SemanticSearchResult {
    /// The item ID (e.g. `"bn-001"`).
    pub item_id: String,
    /// Similarity score in `[0, 1]` (higher = more similar).
    pub score: f32,
}

/// Perform semantic KNN search over `item_embeddings`.
///
/// The function computes cosine similarity between the query embedding and each
/// stored item embedding, then returns the top `limit` items by score.
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

    if let Some(results) = try_knn_search_sqlite_vec(db, query_embedding, limit)? {
        return Ok(results);
    }

    let mut stmt = db
        .prepare("SELECT item_id, embedding_json FROM item_embeddings")
        .context("failed to prepare semantic KNN query (semantic index missing?)")?;

    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("failed to execute semantic KNN query")?;

    let mut scored = Vec::new();
    for row in rows {
        let (item_id, embedding_json) = row.context("failed to read semantic KNN row")?;
        let embedding: Vec<f32> = match serde_json::from_str(&embedding_json) {
            Ok(value) => value,
            Err(err) => {
                debug!(
                    "skipping malformed semantic embedding row for {}: {}",
                    item_id, err
                );
                continue;
            }
        };

        if embedding.len() != EMBEDDING_DIM {
            debug!(
                "skipping semantic embedding row for {} due to dimension {}",
                item_id,
                embedding.len()
            );
            continue;
        }

        let Some(cosine) = cosine_similarity(query_embedding, &embedding) else {
            continue;
        };
        // Map cosine [-1, 1] to [0, 1] for consistent scoring with the rest of
        // the fusion pipeline.
        let score = ((cosine + 1.0) * 0.5).clamp(0.0, 1.0);
        scored.push(SemanticSearchResult { item_id, score });
    }

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.item_id.cmp(&b.item_id))
    });
    scored.truncate(limit);

    Ok(scored)
}

fn try_knn_search_sqlite_vec(
    db: &Connection,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Option<Vec<SemanticSearchResult>>> {
    let vec_available = db
        .query_row("SELECT vec_version()", [], |row| row.get::<_, String>(0))
        .is_ok();
    if !vec_available {
        return Ok(None);
    }

    let query_json = encode_embedding_json(query_embedding);
    let mut stmt = match db.prepare(
        "SELECT item_id,
                vec_distance_cosine(vec_f32(embedding_json), vec_f32(?1)) AS distance
         FROM item_embeddings
         ORDER BY distance ASC
         LIMIT ?2",
    ) {
        Ok(stmt) => stmt,
        Err(err) => {
            debug!("sqlite-vec KNN unavailable, falling back to Rust KNN: {err}");
            return Ok(None);
        }
    };

    let rows = match stmt.query_map(rusqlite::params![query_json, limit as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
    }) {
        Ok(rows) => rows,
        Err(err) => {
            debug!("sqlite-vec KNN query failed, falling back to Rust KNN: {err}");
            return Ok(None);
        }
    };

    let mut out = Vec::new();
    for row in rows {
        let (item_id, distance) = row.context("failed to read sqlite-vec KNN row")?;
        let distance = distance as f32;
        let score = if distance <= 1.0 {
            1.0 - distance
        } else {
            1.0 - (distance / 2.0)
        }
        .clamp(0.0, 1.0);
        out.push(SemanticSearchResult { item_id, score });
    }

    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.item_id.cmp(&b.item_id))
    });

    Ok(Some(out))
}

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

fn cosine_similarity(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }

    let mut dot = 0.0_f32;
    let mut left_norm_sq = 0.0_f32;
    let mut right_norm_sq = 0.0_f32;

    for (a, b) in left.iter().zip(right.iter()) {
        dot += a * b;
        left_norm_sq += a * a;
        right_norm_sq += b * b;
    }

    let denom = left_norm_sq.sqrt() * right_norm_sq.sqrt();
    if denom <= f32::EPSILON {
        return None;
    }

    Some((dot / denom).clamp(-1.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_mock_db() -> Connection {
        let db = Connection::open_in_memory().expect("open in-memory db");
        db.execute_batch(
            "
            CREATE TABLE item_embeddings (
                item_id TEXT PRIMARY KEY,
                content_hash TEXT NOT NULL,
                embedding_json TEXT NOT NULL
            );
            ",
        )
        .expect("create mock table");
        db
    }

    fn sample_embedding(fill: f32) -> Vec<f32> {
        vec![fill; EMBEDDING_DIM]
    }

    fn insert_embedding(db: &Connection, item_id: &str, embedding: &[f32]) {
        db.execute(
            "INSERT INTO item_embeddings (item_id, content_hash, embedding_json)
             VALUES (?1, 'h', ?2)",
            rusqlite::params![item_id, serde_json::to_string(embedding).unwrap()],
        )
        .expect("insert embedding");
    }

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

    #[test]
    fn knn_search_rejects_wrong_dimension() {
        let db = setup_mock_db();
        let bad_embedding = vec![0.1_f32; 100];
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

    #[test]
    fn knn_search_errors_without_embedding_table() {
        let db = Connection::open_in_memory().expect("open in-memory db");
        let emb = sample_embedding(0.5);
        let err = knn_search(&db, &emb, 10).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("semantic index")
                || msg.contains("item_embeddings")
                || msg.contains("table"),
            "expected semantic-index error, got: {msg}"
        );
    }

    #[test]
    fn knn_search_returns_ranked_results() {
        let db = setup_mock_db();
        let mut near = vec![0.0_f32; EMBEDDING_DIM];
        near[0] = 1.0;
        let mut far = vec![0.0_f32; EMBEDDING_DIM];
        far[0] = -1.0;

        insert_embedding(&db, "bn-near", &near);
        insert_embedding(&db, "bn-far", &far);

        let query = near.clone();
        let results = knn_search(&db, &query, 10).expect("knn search should succeed");

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].item_id, "bn-near");
        assert!(results[0].score >= results[1].score);
    }
}
