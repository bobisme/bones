//! SQLite schema migrations for the disposable projection database.

use super::schema;
use rusqlite::{Connection, types::Type};

/// Latest schema version understood by this binary.
pub const LATEST_SCHEMA_VERSION: u32 = 2;

const MIGRATIONS: &[(u32, &str)] = &[(1, schema::MIGRATION_V1_SQL), (2, schema::MIGRATION_V2_SQL)];

/// Read `PRAGMA user_version` and convert it to a Rust `u32`.
///
/// # Errors
///
/// Returns an error if querying SQLite fails or the version value cannot be
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

        let tx = conn.transaction()?;
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
}
