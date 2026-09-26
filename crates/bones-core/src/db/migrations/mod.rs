//! `SQLite` schema migrations for the disposable projection database.

use super::schema;
use rusqlite::{Connection, TransactionBehavior, types::Type};

/// Latest schema version understood by this binary.
pub const LATEST_SCHEMA_VERSION: u32 = 6;

/// A projection created before this schema version must be rebuilt.
///
/// v3 adds per-field winner keys that only a replay of the event log can
/// fill in, v5 changes the format of link keys, and v6 changes how
/// `created_at_us` treats events at time 0.
pub const REBUILD_REQUIRED_BELOW: u32 = 6;

const MIGRATIONS: &[(u32, &str)] = &[
    (1, schema::MIGRATION_V1_SQL),
    (2, schema::MIGRATION_V2_SQL),
    (3, schema::MIGRATION_V3_SQL),
    (4, schema::MIGRATION_V4_SQL),
    (5, schema::MIGRATION_V5_SQL),
    (6, schema::MIGRATION_V6_SQL),
];

/// Read `PRAGMA user_version` and convert it to a Rust `u32`.
///
/// # Errors
///
/// Returns an error if querying `SQLite` fails or the version value cannot be
/// represented as `u32`.
pub fn current_schema_version(conn: &Connection) -> rusqlite::Result<u32> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    u32::try_from(version).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, Type::Integer, Box::new(error))
    })
}

/// Apply all pending migrations in ascending order.
///
/// Migrations are idempotent because:
/// - each migration only runs when `migration.version > user_version`
/// - migration SQL itself uses `IF NOT EXISTS` for DDL safety
///
/// # Errors
///
/// Returns an error if any migration fails.
pub fn migrate(conn: &mut Connection) -> rusqlite::Result<u32> {
    let mut current = current_schema_version(conn)?;

    for (version, sql) in MIGRATIONS {
        if *version <= current {
            continue;
        }

        // Take the write lock first, then re-read the version inside the
        // transaction: another process may have applied this migration since
        // the read above. Some migrations (ALTER TABLE ADD COLUMN) fail when
        // run twice (bn-3pnu).
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        current = current_schema_version(&tx)?;
        if *version <= current {
            continue;
        }
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i64::from(*version))?;
        tx.execute(
            "UPDATE projection_meta SET schema_version = ?1 WHERE id = 1",
            [i64::from(*version)],
        )?;
        tx.commit()?;
        current = *version;
    }

    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::{LATEST_SCHEMA_VERSION, current_schema_version, migrate};
    use crate::db::schema;
    use rusqlite::{Connection, params};

    fn sqlite_object_exists(
        conn: &Connection,
        object_type: &str,
        object_name: &str,
    ) -> rusqlite::Result<bool> {
        conn.query_row(
            "SELECT EXISTS(
                SELECT 1
                FROM sqlite_master
                WHERE type = ?1 AND name = ?2
            )",
            params![object_type, object_name],
            |row| row.get(0),
        )
    }

    #[test]
    fn migrate_empty_db_to_latest() -> rusqlite::Result<()> {
        let mut conn = Connection::open_in_memory()?;

        let applied = migrate(&mut conn)?;
        assert_eq!(applied, LATEST_SCHEMA_VERSION);
        assert_eq!(current_schema_version(&conn)?, LATEST_SCHEMA_VERSION);

        assert!(sqlite_object_exists(&conn, "table", "items")?);
        assert!(sqlite_object_exists(&conn, "table", "item_labels")?);
        assert!(sqlite_object_exists(&conn, "table", "item_assignees")?);
        assert!(sqlite_object_exists(&conn, "table", "item_dependencies")?);
        assert!(sqlite_object_exists(&conn, "table", "item_comments")?);
        assert!(sqlite_object_exists(&conn, "table", "event_redactions")?);
        assert!(sqlite_object_exists(&conn, "table", "projection_meta")?);
        assert!(sqlite_object_exists(&conn, "table", "items_fts")?);

        for index in schema::REQUIRED_INDEXES {
            assert!(
                sqlite_object_exists(&conn, "index", index)?,
                "missing expected index {index}"
            );
        }

        Ok(())
    }

    #[test]
    fn migrate_is_idempotent() -> rusqlite::Result<()> {
        let mut conn = Connection::open_in_memory()?;

        assert_eq!(migrate(&mut conn)?, LATEST_SCHEMA_VERSION);
        assert_eq!(migrate(&mut conn)?, LATEST_SCHEMA_VERSION);

        let meta_rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM projection_meta", [], |row| row.get(0))?;
        assert_eq!(meta_rows, 1);

        let schema_version: i64 = conn.query_row(
            "SELECT schema_version FROM projection_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(schema_version, i64::from(LATEST_SCHEMA_VERSION));

        Ok(())
    }

    #[test]
    fn migrate_upgrades_from_v1_and_backfills_fts() -> rusqlite::Result<()> {
        let mut conn = Connection::open_in_memory()?;

        conn.execute_batch(schema::MIGRATION_V1_SQL)?;
        conn.pragma_update(None, "user_version", 1_i64)?;
        conn.execute(
            "INSERT INTO items (
                item_id,
                title,
                description,
                kind,
                state,
                urgency,
                is_deleted,
                search_labels,
                created_at_us,
                updated_at_us
            ) VALUES (
                'bn-auth01',
                'Auth timeout in worker sync',
                'Retries fail after 30 seconds',
                'task',
                'open',
                'urgent',
                0,
                'auth backend',
                1,
                2
            )",
            [],
        )?;

        let applied = migrate(&mut conn)?;
        assert_eq!(applied, LATEST_SCHEMA_VERSION);

        let fts_hits: i64 = conn.query_row(
            "SELECT COUNT(*)
             FROM items_fts
             WHERE items_fts MATCH 'auth'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(fts_hits, 1);

        let projected_version: i64 = conn.query_row(
            "SELECT schema_version FROM projection_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(projected_version, i64::from(LATEST_SCHEMA_VERSION));

        Ok(())
    }

    #[test]
    fn concurrent_migrations_all_succeed() {
        // Several processes may open an old projection at once. Each must
        // either apply a migration or see that another one did (bn-3pnu).
        for round in 0..20 {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("bones.db");
            {
                let conn = Connection::open(&path).expect("open");
                conn.execute_batch(schema::MIGRATION_V1_SQL).expect("v1");
                conn.execute_batch(schema::MIGRATION_V2_SQL).expect("v2");
                conn.execute_batch(schema::MIGRATION_V3_SQL).expect("v3");
                conn.pragma_update(None, "user_version", 3_i64)
                    .expect("version");
            }
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let path = path.clone();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        let mut conn = Connection::open(&path).expect("open");
                        conn.busy_timeout(std::time::Duration::from_secs(10))
                            .expect("busy_timeout");
                        barrier.wait();
                        migrate(&mut conn)
                    })
                })
                .collect();
            for handle in handles {
                let result = handle.join().expect("thread");
                assert_eq!(
                    result.expect("migrate"),
                    LATEST_SCHEMA_VERSION,
                    "round {round}"
                );
            }
        }
    }

    /// Rows projected before v3 have no field keys, so the upgrade must
    /// clear the cursor, which makes the next incremental apply rebuild.
    #[test]
    fn migrate_to_v3_clears_cursor_to_force_rebuild() -> rusqlite::Result<()> {
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch(schema::MIGRATION_V1_SQL)?;
        conn.execute_batch(schema::MIGRATION_V2_SQL)?;
        conn.pragma_update(None, "user_version", 2_i64)?;
        conn.execute(
            "UPDATE projection_meta SET last_event_offset = 4096, last_event_hash = 'blake3:abc'
             WHERE id = 1",
            [],
        )?;

        assert_eq!(migrate(&mut conn)?, LATEST_SCHEMA_VERSION);

        let (offset, hash): (i64, Option<String>) = conn.query_row(
            "SELECT last_event_offset, last_event_hash FROM projection_meta WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!((offset, hash), (0, None));

        let clocks: i64 =
            conn.query_row("SELECT COUNT(*) FROM field_clocks", [], |row| row.get(0))?;
        assert_eq!(clocks, 0);

        Ok(())
    }
}
