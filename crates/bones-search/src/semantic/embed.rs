use crate::semantic::model::SemanticModel;
use anyhow::{Context, Result, bail};
use bones_core::model::item::WorkItemFields;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

const EMBEDDING_DIM: usize = 384;
const SEMANTIC_META_ID: i64 = 1;

/// Summary of semantic index synchronization work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncStats {
    pub embedded: usize,
    pub removed: usize,
}

/// Manages embedding computation and semantic index storage.
pub struct EmbeddingPipeline<'a> {
    model: &'a SemanticModel,
    db: &'a Connection,
}

impl<'a> EmbeddingPipeline<'a> {
    /// Construct a pipeline and ensure semantic tables exist.
    pub fn new(model: &'a SemanticModel, db: &'a Connection) -> Result<Self> {
        ensure_embedding_schema(db)?;
        Ok(Self { model, db })
    }

    /// Embed a single item and upsert its vector if content changed.
    pub fn embed_item(&self, item: &WorkItemFields) -> Result<bool> {
        let content = item_content(item);
        let content_hash = content_hash_hex(&content);

        if has_same_hash(self.db, &item.id, &content_hash)? {
            return Ok(false);
        }

        let embedding = self
            .model
            .embed(&content)
            .with_context(|| format!("embedding inference failed for item {}", item.id))?;

        upsert_embedding(self.db, &item.id, &content_hash, &embedding)
    }

    /// Batch-embed multiple items.
    pub fn embed_all(&self, items: &[WorkItemFields]) -> Result<usize> {
        let mut pending = Vec::new();

        for item in items {
            let content = item_content(item);
            let content_hash = content_hash_hex(&content);
            if has_same_hash(self.db, &item.id, &content_hash)? {
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
            upsert_embedding(self.db, item_id, hash, &embedding)?;
        }

        Ok(pending.len())
    }
}

/// Ensure semantic embeddings are synchronized with the current projection.
///
/// This is safe to call before every semantic search request: when no new
/// events were projected, it returns quickly without recomputing embeddings.
pub fn sync_projection_embeddings(db: &Connection, model: &SemanticModel) -> Result<SyncStats> {
    ensure_embedding_schema(db)?;

    let projection_cursor = projection_cursor(db)?;
    let indexed_cursor = semantic_cursor(db)?;
    let active_items = active_item_count(db)?;
    let embedded_items = embedding_count(db)?;
    if should_skip_sync(
        &indexed_cursor,
        &projection_cursor,
        active_items,
        embedded_items,
    ) {
        return Ok(SyncStats::default());
    }

    let items = load_items_for_embedding(db)?;
    let live_ids: HashSet<String> = items.iter().map(|(id, _, _)| id.clone()).collect();
    let existing_hashes = load_existing_hashes(db)?;

    let mut pending = Vec::new();
    for (item_id, content_hash, content) in &items {
        if existing_hashes.get(item_id) == Some(content_hash) {
            continue;
        }
        pending.push((item_id.clone(), content_hash.clone(), content.clone()));
    }

    let embedded = if pending.is_empty() {
        0
    } else {
        let texts: Vec<&str> = pending
            .iter()
            .map(|(_, _, content)| content.as_str())
            .collect();
        let embeddings = model
            .embed_batch(&texts)
            .context("semantic index sync failed during embedding inference")?;

        if embeddings.len() != pending.len() {
            bail!(
                "semantic index sync embedding count mismatch: expected {}, got {}",
                pending.len(),
                embeddings.len()
            );
        }

        for ((item_id, content_hash, _), embedding) in pending.iter().zip(embeddings.iter()) {
            upsert_embedding(db, item_id, content_hash, embedding)?;
        }
        pending.len()
    };

    let removed = remove_stale_embeddings(db, &live_ids)?;
    set_semantic_cursor(db, projection_cursor.0, projection_cursor.1.as_deref())?;

    Ok(SyncStats { embedded, removed })
}

/// Ensure semantic index tables exist without running embedding inference.
///
/// This is useful for maintenance flows that want predictable schema state
/// (for example after a projection rebuild) while deferring embedding work
/// until a semantic query is actually executed.
pub fn ensure_semantic_index_schema(db: &Connection) -> Result<()> {
    ensure_embedding_schema(db)
}

fn ensure_embedding_schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS item_embeddings (
            item_id TEXT PRIMARY KEY,
            content_hash TEXT NOT NULL,
            embedding_json TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS semantic_meta (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            last_event_offset INTEGER NOT NULL DEFAULT 0,
            last_event_hash TEXT
        );

        INSERT OR IGNORE INTO semantic_meta (id, last_event_offset, last_event_hash)
        VALUES (1, 0, NULL);
        ",
    )
    .context("failed to create semantic index tables")?;

    Ok(())
}

fn projection_cursor(db: &Connection) -> Result<(i64, Option<String>)> {
    db.query_row(
        "SELECT last_event_offset, last_event_hash FROM projection_meta WHERE id = 1",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
    )
    .context("failed to read projection cursor for semantic sync")
}

fn semantic_cursor(db: &Connection) -> Result<(i64, Option<String>)> {
    db.query_row(
        "SELECT last_event_offset, last_event_hash FROM semantic_meta WHERE id = ?1",
        params![SEMANTIC_META_ID],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
    )
    .context("failed to read semantic index cursor")
}

fn active_item_count(db: &Connection) -> Result<usize> {
    let count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM items WHERE is_deleted = 0",
            [],
            |row| row.get(0),
        )
        .context("failed to count active items for semantic sync")?;
    Ok(usize::try_from(count).unwrap_or(0))
}

fn embedding_count(db: &Connection) -> Result<usize> {
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM item_embeddings", [], |row| row.get(0))
        .context("failed to count semantic embeddings")?;
    Ok(usize::try_from(count).unwrap_or(0))
}

fn should_skip_sync(
    indexed_cursor: &(i64, Option<String>),
    projection_cursor: &(i64, Option<String>),
    active_items: usize,
    embedded_items: usize,
) -> bool {
    indexed_cursor == projection_cursor && active_items == embedded_items
}

fn set_semantic_cursor(db: &Connection, offset: i64, hash: Option<&str>) -> Result<()> {
    db.execute(
        "UPDATE semantic_meta
         SET last_event_offset = ?1, last_event_hash = ?2
         WHERE id = ?3",
        params![offset, hash, SEMANTIC_META_ID],
    )
    .context("failed to update semantic index cursor")?;

    Ok(())
}

fn load_items_for_embedding(db: &Connection) -> Result<Vec<(String, String, String)>> {
    let mut stmt = db
        .prepare(
            "SELECT item_id, title, description
             FROM items
             WHERE is_deleted = 0",
        )
        .context("failed to prepare item query for semantic sync")?;

    let rows = stmt
        .query_map([], |row| {
            let item_id = row.get::<_, String>(0)?;
            let title = row.get::<_, String>(1)?;
            let description = row.get::<_, Option<String>>(2)?;
            Ok((item_id, title, description))
        })
        .context("failed to execute item query for semantic sync")?;

    let mut items = Vec::new();
    for row in rows {
        let (item_id, title, description) =
            row.context("failed to read item row for semantic sync")?;
        let content = content_from_title_description(&title, description.as_deref());
        let content_hash = content_hash_hex(&content);
        items.push((item_id, content_hash, content));
    }

    Ok(items)
}

fn load_existing_hashes(db: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = db
        .prepare("SELECT item_id, content_hash FROM item_embeddings")
        .context("failed to prepare semantic hash query")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("failed to query semantic hash table")?;

    let mut out = HashMap::new();
    for row in rows {
        let (item_id, hash) = row.context("failed to read semantic hash row")?;
        out.insert(item_id, hash);
    }
    Ok(out)
}

fn remove_stale_embeddings(db: &Connection, live_ids: &HashSet<String>) -> Result<usize> {
    let mut stmt = db
        .prepare("SELECT item_id FROM item_embeddings")
        .context("failed to prepare stale semantic row query")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("failed to query semantic rows for stale cleanup")?;

    let mut stale = Vec::new();
    for row in rows {
        let item_id = row.context("failed to read semantic row id")?;
        if !live_ids.contains(&item_id) {
            stale.push(item_id);
        }
    }

    for item_id in &stale {
        db.execute(
            "DELETE FROM item_embeddings WHERE item_id = ?1",
            params![item_id],
        )
        .with_context(|| format!("failed to delete stale semantic row for {item_id}"))?;
    }

    Ok(stale.len())
}

fn has_same_hash(db: &Connection, item_id: &str, content_hash: &str) -> Result<bool> {
    let existing = db
        .query_row(
            "SELECT content_hash FROM item_embeddings WHERE item_id = ?1",
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

    let existing_hash = db
        .query_row(
            "SELECT content_hash FROM item_embeddings WHERE item_id = ?1",
            params![item_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .with_context(|| format!("failed to lookup semantic row for item {item_id}"))?;

    if existing_hash.as_deref() == Some(content_hash) {
        return Ok(false);
    }

    let encoded_vector = encode_embedding_json(embedding);
    db.execute(
        "INSERT INTO item_embeddings (item_id, content_hash, embedding_json)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(item_id)
         DO UPDATE SET content_hash = excluded.content_hash,
                       embedding_json = excluded.embedding_json",
        params![item_id, content_hash, encoded_vector],
    )
    .with_context(|| format!("failed to upsert semantic embedding for item {item_id}"))?;

    Ok(true)
}

fn item_content(item: &WorkItemFields) -> String {
    content_from_title_description(&item.title, item.description.as_deref())
}

fn content_from_title_description(title: &str, description: Option<&str>) -> String {
    match description {
        Some(description) if !description.trim().is_empty() => {
            format!("{} {}", title.trim(), description.trim())
        }
        _ => title.trim().to_owned(),
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
            CREATE TABLE items (
                item_id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                description TEXT,
                is_deleted INTEGER NOT NULL DEFAULT 0,
                updated_at_us INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE projection_meta (
                id INTEGER PRIMARY KEY,
                last_event_offset INTEGER NOT NULL,
                last_event_hash TEXT
            );

            INSERT INTO projection_meta (id, last_event_offset, last_event_hash)
            VALUES (1, 0, NULL);
            ",
        )?;

        ensure_embedding_schema(db)?;
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

        let count: i64 =
            db.query_row("SELECT COUNT(*) FROM item_embeddings", [], |row| row.get(0))?;
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
            "SELECT content_hash FROM item_embeddings WHERE item_id = ?1",
            params![item_id],
            |row| row.get(0),
        )?;
        assert_eq!(stored_hash, second_hash);

        Ok(())
    }

    #[test]
    fn sync_projection_embeddings_short_circuits_when_cursor_matches() -> Result<()> {
        let db = Connection::open_in_memory()?;
        seed_schema_for_unit_tests(&db)?;

        db.execute(
            "UPDATE semantic_meta SET last_event_offset = 7, last_event_hash = 'h7' WHERE id = 1",
            [],
        )?;
        db.execute(
            "UPDATE projection_meta SET last_event_offset = 7, last_event_hash = 'h7' WHERE id = 1",
            [],
        )?;

        let model = SemanticModel::load();
        if let Ok(model) = model {
            let stats = sync_projection_embeddings(&db, &model)?;
            assert_eq!(stats, SyncStats::default());
        }

        Ok(())
    }

    #[test]
    fn should_skip_sync_requires_cardinality_match() {
        let cursor = (7, Some("h7".to_string()));
        assert!(should_skip_sync(&cursor, &cursor, 0, 0));
        assert!(should_skip_sync(&cursor, &cursor, 3, 3));
        assert!(!should_skip_sync(&cursor, &cursor, 3, 0));
        assert!(!should_skip_sync(&cursor, &cursor, 0, 2));
        assert!(!should_skip_sync(
            &cursor,
            &(8, Some("h8".to_string())),
            3,
            3
        ));
    }
}
