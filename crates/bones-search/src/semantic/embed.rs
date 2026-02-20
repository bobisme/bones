use crate::semantic::model::SemanticModel;
use anyhow::{Context, Result, bail};
use bones_core::model::item::WorkItemFields;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

const EMBEDDING_DIM: usize = 384;

/// Manages embedding computation and vector storage.
/// Holds both the ONNX model and a database connection.
pub struct EmbeddingPipeline {
    /// The loaded semantic model.
    model: SemanticModel,
    /// SQLite connection with sqlite-vec extension loaded.
    db: Connection,
}

impl EmbeddingPipeline {
    /// Construct a new embedding pipeline and ensure vector tables exist.
    pub fn new(model: SemanticModel, db: Connection) -> Result<Self> {
        ensure_vec_schema(&db)?;
        Ok(Self { model, db })
    }

    /// Embed a single item and upsert its vector if content changed.
    ///
    /// Returns `Ok(true)` if an embedding was written, `Ok(false)` when skipped.
    pub fn embed_item(&self, item: &WorkItemFields) -> Result<bool> {
        let content = item_content(item);
        let content_hash = content_hash_hex(&content);

        if has_same_hash(&self.db, &item.id, &content_hash)? {
            return Ok(false);
        }

        let embedding = self
            .model
            .embed(&content)
            .with_context(|| format!("embedding inference failed for item {}", item.id))?;

        upsert_embedding(&self.db, &item.id, &content_hash, &embedding)
    }

    /// Batch-embed multiple items.
    ///
    /// Returns count of items actually embedded (unchanged items are skipped).
    pub fn embed_all(&self, items: &[WorkItemFields]) -> Result<usize> {
        let mut pending = Vec::new();

        for item in items {
            let content = item_content(item);
            let content_hash = content_hash_hex(&content);
            if has_same_hash(&self.db, &item.id, &content_hash)? {
                continue;
            }
            pending.push((item.id.clone(), content_hash, content));
        }

        if pending.is_empty() {
            return Ok(0);
        }

        let texts: Vec<&str> = pending.iter().map(|(_, _, text)| text.as_str()).collect();
        let embeddings = self
            .model
            .embed_batch(&texts)
            .context("batch embedding inference failed")?;

        if embeddings.len() != pending.len() {
            bail!(
                "embedding batch length mismatch: expected {}, got {}",
                pending.len(),
                embeddings.len()
            );
        }

        for ((item_id, hash, _), embedding) in pending.iter().zip(embeddings) {
            upsert_embedding(&self.db, item_id, hash, &embedding)?;
        }

        Ok(pending.len())
    }

    /// Consume the pipeline and return the underlying SQLite connection.
    pub fn into_connection(self) -> Connection {
        self.db
    }
}

fn ensure_vec_schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS vec_items USING vec0(
            embedding float[384]
        );

        CREATE TABLE IF NOT EXISTS vec_item_map (
            rowid INTEGER PRIMARY KEY,
            item_id TEXT NOT NULL UNIQUE,
            content_hash TEXT NOT NULL
        );
        ",
    )
    .context("failed to create sqlite-vec schema (is vec0 extension available?)")?;

    Ok(())
}

fn has_same_hash(db: &Connection, item_id: &str, content_hash: &str) -> Result<bool> {
    let existing = db
        .query_row(
            "SELECT content_hash FROM vec_item_map WHERE item_id = ?1",
            params![item_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .with_context(|| format!("failed to query content hash for item {item_id}"))?;

    Ok(existing.as_deref() == Some(content_hash))
}

fn upsert_embedding(
    db: &Connection,
    item_id: &str,
    content_hash: &str,
    embedding: &[f32],
) -> Result<bool> {
    if embedding.len() != EMBEDDING_DIM {
        bail!(
            "invalid embedding dimension for item {item_id}: expected {EMBEDDING_DIM}, got {}",
            embedding.len()
        );
    }

    let row_meta = db
        .query_row(
            "SELECT rowid, content_hash FROM vec_item_map WHERE item_id = ?1",
            params![item_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .with_context(|| format!("failed to lookup vector row mapping for item {item_id}"))?;

    if let Some((_, existing_hash)) = &row_meta
        && existing_hash == content_hash
    {
        return Ok(false);
    }

    let encoded_vector = encode_embedding_json(embedding);

    match row_meta {
        Some((rowid, _)) => {
            db.execute(
                "UPDATE vec_items SET embedding = ?1 WHERE rowid = ?2",
                params![encoded_vector, rowid],
            )
            .with_context(|| {
                format!("failed to update embedding row {rowid} for item {item_id}")
            })?;

            db.execute(
                "UPDATE vec_item_map SET content_hash = ?1 WHERE item_id = ?2",
                params![content_hash, item_id],
            )
            .with_context(|| format!("failed to update content hash for item {item_id}"))?;
        }
        None => {
            db.execute(
                "INSERT INTO vec_items (embedding) VALUES (?1)",
                params![encoded_vector],
            )
            .with_context(|| format!("failed to insert embedding for item {item_id}"))?;

            let rowid = db.last_insert_rowid();
            db.execute(
                "INSERT INTO vec_item_map (rowid, item_id, content_hash) VALUES (?1, ?2, ?3)",
                params![rowid, item_id, content_hash],
            )
            .with_context(|| format!("failed to insert mapping for item {item_id}"))?;
        }
    }

    Ok(true)
}

fn item_content(item: &WorkItemFields) -> String {
    match item.description.as_deref() {
        Some(description) if !description.trim().is_empty() => {
            format!("{} {}", item.title.trim(), description.trim())
        }
        _ => item.title.trim().to_owned(),
    }
}

fn content_hash_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_schema_for_unit_tests(db: &Connection) -> Result<()> {
        db.execute_batch(
            "
            CREATE TABLE vec_items (
                rowid INTEGER PRIMARY KEY,
                embedding TEXT NOT NULL
            );

            CREATE TABLE vec_item_map (
                rowid INTEGER PRIMARY KEY,
                item_id TEXT NOT NULL UNIQUE,
                content_hash TEXT NOT NULL
            );
            ",
        )?;
        Ok(())
    }

    fn sample_embedding() -> Vec<f32> {
        vec![0.25_f32; EMBEDDING_DIM]
    }

    #[test]
    fn content_hash_changes_with_content() {
        let left = content_hash_hex("alpha");
        let right = content_hash_hex("beta");
        assert_ne!(left, right);
    }

    #[test]
    fn item_content_concatenates_title_and_description() {
        let item = WorkItemFields {
            title: "Title".to_string(),
            description: Some("Description".to_string()),
            ..WorkItemFields::default()
        };

        assert_eq!(item_content(&item), "Title Description");
    }

    #[test]
    fn upsert_embedding_skips_when_hash_matches() -> Result<()> {
        let db = Connection::open_in_memory()?;
        seed_schema_for_unit_tests(&db)?;

        let item_id = "bn-abc";
        let hash = content_hash_hex("same-content");
        let embedding = sample_embedding();

        let inserted = upsert_embedding(&db, item_id, &hash, &embedding)?;
        let skipped = upsert_embedding(&db, item_id, &hash, &embedding)?;

        assert!(inserted);
        assert!(!skipped);

        let count: i64 = db.query_row("SELECT COUNT(*) FROM vec_item_map", [], |row| row.get(0))?;
        assert_eq!(count, 1);

        Ok(())
    }

    #[test]
    fn upsert_embedding_updates_hash_when_content_changes() -> Result<()> {
        let db = Connection::open_in_memory()?;
        seed_schema_for_unit_tests(&db)?;

        let item_id = "bn-def";
        let first_hash = content_hash_hex("old");
        let second_hash = content_hash_hex("new");

        upsert_embedding(&db, item_id, &first_hash, &sample_embedding())?;
        let written = upsert_embedding(&db, item_id, &second_hash, &sample_embedding())?;

        assert!(written);

        let stored_hash: String = db.query_row(
            "SELECT content_hash FROM vec_item_map WHERE item_id = ?1",
            params![item_id],
            |row| row.get(0),
        )?;
        assert_eq!(stored_hash, second_hash);

        Ok(())
    }
}
