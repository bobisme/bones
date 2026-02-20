//! Runtime capability detection for optional bones subsystems.
//!
//! This module probes the active SQLite database and filesystem to determine
//! which optional features are available at startup. The result is a
//! [`Capabilities`] struct that callers use to choose between full-featured
//! and gracefully-degraded code paths — without panicking on missing deps.
//!
//! # Design
//!
//! Every probe is infallible from the caller's perspective: it returns a
//! `bool`, logs the outcome at `debug!` level, and never propagates errors.
//! This ensures the CLI remains usable even when subsystems are broken or not
//! yet initialised.
//!
//! # Usage
//!
//! ```rust,no_run
//! use bones_core::capabilities::{detect_capabilities, describe_capabilities};
//! use bones_core::db::open_projection;
//! use std::path::Path;
//!
//! let conn = open_projection(Path::new(".bones/bones-projection.sqlite3")).unwrap();
//! let caps = detect_capabilities(&conn);
//! if !caps.fts5 {
//!     eprintln!("FTS5 not available — falling back to LIKE queries");
//! }
//! for status in describe_capabilities(&caps) {
//!     if !status.available {
//!         eprintln!("[{}] degraded: {}", status.name, status.fallback);
//!     }
//! }
//! ```

use std::io::Read as _;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use tracing::debug;

use crate::cache::CACHE_MAGIC;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Runtime capability flags detected at startup.
///
/// Each flag indicates whether a specific optional subsystem is functional.
/// Consumers should check the relevant flag before calling into the subsystem
/// and fall back gracefully when it is `false`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// SQLite FTS5 extension is available and index (`items_fts`) is built.
    pub fts5: bool,
    /// Semantic search model (MiniLM) is loaded and vectors table exists.
    pub semantic: bool,
    /// sqlite-vec extension is available for vector operations.
    pub vectors: bool,
    /// Binary columnar cache (`events.bin`) exists and has a valid header.
    pub binary_cache: bool,
    /// Triage engine dependencies (petgraph, items table) are available.
    pub triage: bool,
}

/// Status of a single capability for user-visible display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityStatus {
    /// Short machine-readable name of the capability.
    pub name: &'static str,
    /// Whether the capability is currently available.
    pub available: bool,
    /// Human-readable description of what the system does when this
    /// capability is missing.
    pub fallback: &'static str,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Detect available capabilities by probing the database and filesystem.
///
/// Checks performed:
/// - **fts5**: `items_fts` virtual table present in `sqlite_master`
/// - **vectors**: `vec_version()` SQL function is callable (sqlite-vec loaded)
/// - **semantic**: MiniLM model+tokenizer files available on disk
/// - **binary_cache**: `.bones/cache/events.bin` exists with valid `BNCH` magic
/// - **triage**: `items` table queryable (petgraph is always compiled in)
///
/// All probes are infallible — errors are logged at `debug!` and treated as
/// capability absent.
///
/// # Arguments
///
/// * `db` — An open SQLite projection database connection.
#[must_use]
pub fn detect_capabilities(db: &Connection) -> Capabilities {
    let fts5 = probe_fts5(db);
    let vectors = probe_vectors(db);
    let semantic = probe_semantic_model();
    let bones_dir = bones_dir_from_db(db);
    let binary_cache = bones_dir
        .as_deref()
        .map(|d| probe_binary_cache(&d.join("cache").join("events.bin")))
        .unwrap_or_else(|| {
            debug!("binary_cache probe: cannot determine .bones dir from connection, reporting unavailable");
            false
        });
    let triage = probe_triage(db);

    let caps = Capabilities {
        fts5,
        semantic,
        vectors,
        binary_cache,
        triage,
    };
    debug!(?caps, "capability detection complete");
    caps
}

/// Describe which capabilities are active or missing for user display.
///
/// Returns a [`Vec`] of [`CapabilityStatus`] entries in a stable order.
/// Each entry contains the capability name, availability flag, and the
/// fallback behaviour description used when the capability is absent.
#[must_use]
pub fn describe_capabilities(caps: &Capabilities) -> Vec<CapabilityStatus> {
    vec![
        CapabilityStatus {
            name: "fts5",
            available: caps.fts5,
            fallback: "`bn search` uses LIKE queries (slower, no ranking)",
        },
        CapabilityStatus {
            name: "semantic",
            available: caps.semantic,
            fallback: "`bn search` uses lexical only, warns user",
        },
        CapabilityStatus {
            name: "vectors",
            available: caps.vectors,
            fallback: "semantic search disabled",
        },
        CapabilityStatus {
            name: "binary_cache",
            available: caps.binary_cache,
            fallback: "event replay reads .events files directly (slower)",
        },
        CapabilityStatus {
            name: "triage",
            available: caps.triage,
            fallback: "`bn next` uses simple heuristic (urgency + age)",
        },
    ]
}

// ---------------------------------------------------------------------------
// Internal probes
// ---------------------------------------------------------------------------

/// Returns `true` if the `items_fts` FTS5 virtual table exists in `sqlite_master`.
fn probe_fts5(db: &Connection) -> bool {
    let result = db.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'items_fts'",
        [],
        |row| row.get::<_, i64>(0),
    );
    match result {
        Ok(count) => {
            let available = count > 0;
            debug!(available, "fts5 probe");
            available
        }
        Err(e) => {
            debug!(error = %e, "fts5 probe failed");
            false
        }
    }
}

/// Returns `true` if the sqlite-vec extension is loaded.
///
/// Probes by calling `vec_version()` — a function exported only by sqlite-vec.
fn probe_vectors(db: &Connection) -> bool {
    let result = db.query_row("SELECT vec_version()", [], |row| row.get::<_, String>(0));
    let available = result.is_ok();
    debug!(available, "vectors probe");
    available
}

/// Returns `true` if the MiniLM-L6-v2 ONNX model file exists on disk.
///
/// Uses the same cache path convention as `SemanticModel::model_cache_path()`:
/// `<os-cache-dir>/bones/models/minilm-l6-v2-int8.onnx`.
fn probe_semantic_model() -> bool {
    let available = dirs::cache_dir()
        .map(|mut p| {
            p.push("bones");
            p.push("models");
            let model = p.join("minilm-l6-v2-int8.onnx");
            let tokenizer = p.join("minilm-l6-v2-tokenizer.json");
            model.is_file() && tokenizer.is_file()
        })
        .unwrap_or(false);
    debug!(available, "semantic model probe");
    available
}

/// Returns `true` if `events.bin` exists and begins with the `BNCH` magic bytes.
fn probe_binary_cache(events_bin: &Path) -> bool {
    if !events_bin.exists() {
        debug!(path = %events_bin.display(), "binary_cache probe: file absent");
        return false;
    }
    let available = match std::fs::File::open(events_bin) {
        Ok(mut f) => {
            let mut magic = [0u8; 4];
            f.read_exact(&mut magic)
                .map(|_| magic == CACHE_MAGIC)
                .unwrap_or(false)
        }
        Err(e) => {
            debug!(error = %e, "binary_cache probe: cannot open file");
            false
        }
    };
    debug!(available, path = %events_bin.display(), "binary_cache probe");
    available
}

/// Returns `true` if the triage engine can operate.
///
/// Petgraph is a compile-time dependency and is always available. This probe
/// verifies that the underlying `items` table is queryable, which is the
/// runtime precondition for building the dependency graph.
fn probe_triage(db: &Connection) -> bool {
    let result = db.query_row(
        "SELECT COUNT(*) FROM items WHERE is_deleted = 0",
        [],
        |row| row.get::<_, i64>(0),
    );
    let available = result.is_ok();
    debug!(available, "triage probe");
    available
}

/// Derive the `.bones` directory path from the database connection.
///
/// Uses `PRAGMA database_list` to find the on-disk path of the `main` database,
/// then returns its parent directory (which is expected to be `.bones`).
///
/// Returns `None` for in-memory connections or when the path cannot be
/// determined.
fn bones_dir_from_db(db: &Connection) -> Option<PathBuf> {
    let mut stmt = db.prepare("PRAGMA database_list").ok()?;
    let mut rows = stmt.query([]).ok()?;
    while let Ok(Some(row)) = rows.next() {
        let name: String = row.get(1).unwrap_or_default();
        let file: String = row.get(2).unwrap_or_default();
        if name == "main" && !file.is_empty() {
            return PathBuf::from(file).parent().map(ToOwned::to_owned);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::db::{migrations, open_projection};

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn migrated_db() -> (TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bones-projection.sqlite3");
        let conn = open_projection(&path).expect("open projection db");
        (dir, conn)
    }

    fn bare_db() -> Connection {
        // In-memory DB with no schema at all.
        Connection::open_in_memory().expect("open in-memory db")
    }

    // -----------------------------------------------------------------------
    // probe_fts5
    // -----------------------------------------------------------------------

    #[test]
    fn fts5_is_false_on_bare_db() {
        let conn = bare_db();
        assert!(!probe_fts5(&conn));
    }

    #[test]
    fn fts5_is_true_after_migration() {
        let (_dir, conn) = migrated_db();
        // Migration v2 creates items_fts.
        assert!(
            migrations::current_schema_version(&conn).expect("version") >= 2,
            "test assumes migration v2+ is applied"
        );
        assert!(probe_fts5(&conn));
    }

    // -----------------------------------------------------------------------
    // probe_vectors
    // -----------------------------------------------------------------------

    #[test]
    fn vectors_is_false_without_extension() {
        // sqlite-vec is not loaded in the test environment.
        let conn = bare_db();
        assert!(!probe_vectors(&conn));
    }

    // -----------------------------------------------------------------------
    // probe_semantic_model
    // -----------------------------------------------------------------------

    #[test]
    fn semantic_model_is_false_in_ci() {
        // The MiniLM model is never present in the CI environment.
        // This test documents the expected degradation path.
        let result = probe_semantic_model();
        // Either true (dev machine with model) or false (CI/fresh checkout).
        // We just verify the probe doesn't panic.
        let _ = result;
    }

    // -----------------------------------------------------------------------
    // probe_binary_cache
    // -----------------------------------------------------------------------

    #[test]
    fn binary_cache_false_for_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.bin");
        assert!(!probe_binary_cache(&path));
    }

    #[test]
    fn binary_cache_true_for_valid_magic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.bin");
        // Write a file with valid BNCH magic followed by padding.
        let mut content = Vec::from(CACHE_MAGIC);
        content.extend_from_slice(&[0u8; 28]); // pad to HEADER_SIZE
        std::fs::write(&path, &content).expect("write cache");
        assert!(probe_binary_cache(&path));
    }

    #[test]
    fn binary_cache_false_for_wrong_magic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.bin");
        std::fs::write(&path, b"WXYZ\x00\x00\x00\x00").expect("write bad magic");
        assert!(!probe_binary_cache(&path));
    }

    #[test]
    fn binary_cache_false_for_truncated_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.bin");
        // File too short to read 4 magic bytes.
        std::fs::write(&path, b"BN").expect("write truncated");
        assert!(!probe_binary_cache(&path));
    }

    // -----------------------------------------------------------------------
    // probe_triage
    // -----------------------------------------------------------------------

    #[test]
    fn triage_false_on_bare_db_no_items_table() {
        let conn = bare_db();
        assert!(!probe_triage(&conn));
    }

    #[test]
    fn triage_true_after_migration_zero_items() {
        let (_dir, conn) = migrated_db();
        // No items inserted — but the query succeeds, so triage = true.
        assert!(probe_triage(&conn));
    }

    // -----------------------------------------------------------------------
    // bones_dir_from_db
    // -----------------------------------------------------------------------

    #[test]
    fn bones_dir_none_for_in_memory_db() {
        let conn = bare_db();
        assert!(bones_dir_from_db(&conn).is_none());
    }

    #[test]
    fn bones_dir_is_parent_of_db_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("bones-projection.sqlite3");
        let conn = Connection::open(&db_path).expect("open");
        let bones_dir = bones_dir_from_db(&conn);
        assert_eq!(bones_dir.as_deref(), Some(dir.path()));
    }

    // -----------------------------------------------------------------------
    // detect_capabilities (integration)
    // -----------------------------------------------------------------------

    #[test]
    fn detect_on_migrated_db_has_fts5() {
        let (_dir, conn) = migrated_db();
        let caps = detect_capabilities(&conn);
        // FTS5 index is built by migration v2.
        assert!(caps.fts5, "FTS5 should be available after migration");
    }

    #[test]
    fn detect_on_bare_db_has_no_capabilities() {
        let conn = bare_db();
        let caps = detect_capabilities(&conn);
        assert!(!caps.fts5, "no FTS5 on bare db");
        assert!(!caps.vectors, "no vectors on bare db");
        // semantic may be true on developer machines where model assets are
        // present in cache; this test only asserts DB-derived capabilities.
        assert!(!caps.binary_cache, "no binary_cache (in-memory db)");
        assert!(!caps.triage, "no triage on bare db (no items table)");
    }

    #[test]
    fn detect_triage_true_on_migrated_db() {
        let (_dir, conn) = migrated_db();
        let caps = detect_capabilities(&conn);
        assert!(caps.triage, "triage should be true after migration");
    }

    #[test]
    fn detect_with_valid_binary_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        // DB lives inside dir so bones_dir_from_db returns dir.path().
        let db_path = dir.path().join("bones-projection.sqlite3");
        let conn = open_projection(&db_path).expect("open projection");

        // Create cache dir and write a valid events.bin.
        let cache_dir = dir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).expect("create cache dir");
        let mut content = Vec::from(CACHE_MAGIC);
        content.extend_from_slice(&[0u8; 28]);
        std::fs::write(cache_dir.join("events.bin"), &content).expect("write cache");

        let caps = detect_capabilities(&conn);
        assert!(
            caps.binary_cache,
            "binary_cache should be true with valid events.bin"
        );
    }

    #[test]
    fn detect_binary_cache_false_with_bad_magic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("bones-projection.sqlite3");
        let conn = open_projection(&db_path).expect("open projection");

        let cache_dir = dir.path().join("cache");
        std::fs::create_dir_all(&cache_dir).expect("create cache dir");
        std::fs::write(cache_dir.join("events.bin"), b"BADMAGIC").expect("write");

        let caps = detect_capabilities(&conn);
        assert!(
            !caps.binary_cache,
            "binary_cache should be false with bad magic"
        );
    }

    // -----------------------------------------------------------------------
    // describe_capabilities
    // -----------------------------------------------------------------------

    #[test]
    fn describe_returns_five_entries() {
        let caps = Capabilities::default();
        let statuses = describe_capabilities(&caps);
        assert_eq!(statuses.len(), 5);
    }

    #[test]
    fn describe_names_are_stable() {
        let caps = Capabilities::default();
        let statuses = describe_capabilities(&caps);
        let names: Vec<_> = statuses.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            &["fts5", "semantic", "vectors", "binary_cache", "triage"]
        );
    }

    #[test]
    fn describe_available_flags_match_capabilities() {
        let caps = Capabilities {
            fts5: true,
            semantic: false,
            vectors: true,
            binary_cache: false,
            triage: true,
        };
        let statuses = describe_capabilities(&caps);
        let map: std::collections::HashMap<_, _> =
            statuses.iter().map(|s| (s.name, s.available)).collect();
        assert!(map["fts5"]);
        assert!(!map["semantic"]);
        assert!(map["vectors"]);
        assert!(!map["binary_cache"]);
        assert!(map["triage"]);
    }

    #[test]
    fn describe_fallbacks_are_non_empty() {
        let caps = Capabilities::default();
        let statuses = describe_capabilities(&caps);
        for status in &statuses {
            assert!(
                !status.fallback.is_empty(),
                "fallback for {} is empty",
                status.name
            );
        }
    }

    #[test]
    fn capabilities_default_is_all_false() {
        let caps = Capabilities::default();
        assert!(!caps.fts5);
        assert!(!caps.semantic);
        assert!(!caps.vectors);
        assert!(!caps.binary_cache);
        assert!(!caps.triage);
    }
}
