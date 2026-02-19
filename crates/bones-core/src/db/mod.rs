//! SQLite projection database utilities.
//!
//! Runtime defaults are intentionally conservative:
//! - `journal_mode = WAL` to allow concurrent readers while writers append
//! - `busy_timeout = 5s` to reduce transient lock failures under contention
//! - `foreign_keys = ON` to protect relational integrity in projection tables

pub mod fts;
pub mod migrations;
pub mod project;
pub mod query;
pub mod rebuild;
pub mod schema;

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::{path::Path, time::Duration};

/// Busy timeout used for projection DB connections.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Open (or create) the projection SQLite database, apply runtime pragmas,
/// and migrate schema to the latest version.
///
/// # Errors
///
/// Returns an error if opening/configuring/migrating the database fails.
pub fn open_projection(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create projection db directory {}", parent.display()))?;
    }

    let mut conn = Connection::open(path)
        .with_context(|| format!("open projection database {}", path.display()))?;

    configure_connection(&conn).context("configure sqlite pragmas")?;
    migrations::migrate(&mut conn).context("apply projection migrations")?;

    Ok(conn)
}

fn configure_connection(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    let _journal_mode: String =
        conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    conn.busy_timeout(DEFAULT_BUSY_TIMEOUT)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_BUSY_TIMEOUT, open_projection};
    use crate::db::migrations;
    use tempfile::TempDir;

    fn temp_db_path() -> (TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("bones-projection.sqlite3");
        (dir, path)
    }

    #[test]
    fn open_projection_sets_wal_busy_timeout_and_fk() {
        let (_dir, path) = temp_db_path();
        let conn = open_projection(&path).expect("open projection db");

        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("query journal_mode");
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

        let busy_timeout_ms: u64 = conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .expect("query busy_timeout");
        assert_eq!(
            u128::from(busy_timeout_ms),
            DEFAULT_BUSY_TIMEOUT.as_millis()
        );

        let foreign_keys: i64 = conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("query foreign_keys");
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn open_projection_runs_migrations() {
        let (_dir, path) = temp_db_path();
        let conn = open_projection(&path).expect("open projection db");

        let version = migrations::current_schema_version(&conn).expect("schema version query");
        assert_eq!(version, migrations::LATEST_SCHEMA_VERSION);

        let projection_version: i64 = conn
            .query_row(
                "SELECT schema_version FROM projection_meta WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .expect("projection_meta schema version");
        assert_eq!(
            projection_version,
            i64::from(migrations::LATEST_SCHEMA_VERSION)
        );
    }
}
