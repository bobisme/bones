//! `bn search` — lexical full-text search over items using SQLite FTS5/BM25.
//!
//! Searches the FTS5 index with BM25 ranking (title 3×, description 2×,
//! labels 1×). Results are sorted by relevance (best match first).
//!
//! Supports FTS5 query syntax: stemming, prefix search (`auth*`), boolean ops.

use crate::output::{CliError, OutputMode, render, render_error};
use bones_core::db::fts;
use bones_core::db::query;
use clap::Args;
use serde::Serialize;
use std::io::Write;

#[derive(Args, Debug)]
#[command(
    about = "Search items using full-text search",
    long_about = "Search work items using SQLite FTS5 lexical full-text search with BM25 ranking.\n\n\
                  Column weights: title 3×, description 2×, labels 1×.\n\n\
                  FTS5 syntax is supported: stemming ('run' matches 'running'), \
                  prefix search ('auth*'), boolean operators (AND, OR, NOT).",
    after_help = "EXAMPLES:\n    # Search for items about authentication\n    bn search authentication\n\n\
                  # Prefix search\n    bn search 'auth*'\n\n\
                  # Limit results\n    bn search timeout -n 5\n\n\
                  # Machine-readable output\n    bn search authentication --json"
)]
pub struct SearchArgs {
    /// Search query. FTS5 syntax supported (stemming, prefix `auth*`, AND/OR/NOT).
    pub query: String,

    /// Maximum number of results to return.
    #[arg(short = 'n', long, default_value = "10")]
    pub limit: usize,
}

/// A single search result row.
#[derive(Debug, Serialize)]
pub struct SearchResult {
    /// Item ID.
    pub id: String,
    /// Item title.
    pub title: String,
    /// BM25 relevance score (more negative = better match).
    pub score: f64,
    /// Lifecycle state of the item.
    pub state: String,
}

/// JSON envelope for search output.
#[derive(Debug, Serialize)]
pub struct SearchOutput {
    /// The original query string.
    pub query: String,
    /// Total number of results returned.
    pub count: usize,
    /// Ordered list of results (best match first).
    pub results: Vec<SearchResult>,
}

/// Execute `bn search <query>`.
///
/// Opens the projection database, runs an FTS5 BM25 search, and renders
/// results in the requested output format.
///
/// # Errors
///
/// Returns an error if the projection database is missing, the FTS5 query
/// is malformed, or output rendering fails.
pub fn run_search(
    args: &SearchArgs,
    output: OutputMode,
    project_root: &std::path::Path,
) -> anyhow::Result<()> {
    if args.query.trim().is_empty() {
        render_error(
            output,
            &CliError::with_details(
                "search query must not be empty",
                "provide a non-empty query string",
                "empty_query",
            ),
        )?;
        anyhow::bail!("empty search query");
    }

    let db_path = project_root.join(".bones/bones.db");

    let conn = match query::try_open_projection(&db_path)? {
        Some(c) => c,
        None => {
            render_error(
                output,
                &CliError::with_details(
                    "projection database not found",
                    "run `bn rebuild` to initialize the projection",
                    "projection_missing",
                ),
            )?;
            anyhow::bail!("projection not found");
        }
    };

    let limit = u32::try_from(args.limit.min(1000)).unwrap_or(1000);
    let hits = fts::search_bm25(&conn, &args.query, limit).map_err(|e| {
        anyhow::anyhow!("FTS5 search error: {e}. Check query syntax (use 'auth*' for prefix, AND/OR/NOT for boolean).")
    })?;

    // Enrich hits with item state
    let mut results: Vec<SearchResult> = Vec::with_capacity(hits.len());
    for hit in hits {
        // Fetch state from items table
        let state = conn
            .query_row(
                "SELECT state FROM items WHERE item_id = ?1",
                rusqlite::params![hit.item_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_else(|_| "unknown".to_string());

        results.push(SearchResult {
            id: hit.item_id,
            title: hit.title,
            score: hit.rank,
            state,
        });
    }

    let search_output = SearchOutput {
        query: args.query.clone(),
        count: results.len(),
        results,
    };

    render(output, &search_output, |out, w| render_search_human(out, w))
}

/// Render search results in human-readable format.
fn render_search_human(out: &SearchOutput, w: &mut dyn Write) -> std::io::Result<()> {
    if out.results.is_empty() {
        writeln!(w, "No results for '{}'", out.query)?;
        return Ok(());
    }

    writeln!(w, "{} result(s) for '{}':", out.count, out.query)?;
    writeln!(w, "{:-<60}", "")?;

    for result in &out.results {
        writeln!(w, "  {}  [{}]  {}", result.id, result.state, result.title)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db::migrations;
    use bones_core::db::project::{Projector, ensure_tracking_table};
    use bones_core::event::data::*;
    use bones_core::event::types::EventType;
    use bones_core::event::{Event, EventData};
    use bones_core::model::item::{Kind, Size, Urgency};
    use bones_core::model::item_id::ItemId;
    use rusqlite::Connection;
    use std::collections::BTreeMap;

    fn test_db() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        migrations::migrate(&mut conn).expect("migrate");
        ensure_tracking_table(&conn).expect("tracking table");
        conn
    }

    fn make_create(
        id: &str,
        title: &str,
        desc: Option<&str>,
        labels: &[&str],
        hash: &str,
    ) -> Event {
        Event {
            wall_ts_us: 1000,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(id),
            data: EventData::Create(CreateData {
                title: title.into(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: labels.iter().map(|s| s.to_string()).collect(),
                parent: None,
                causation: None,
                description: desc.map(String::from),
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:{hash}"),
        }
    }

    // -----------------------------------------------------------------------
    // SearchArgs parsing
    // -----------------------------------------------------------------------

    #[test]
    fn search_args_parse_query() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: SearchArgs,
        }

        let w = Wrapper::parse_from(["test", "authentication"]);
        assert_eq!(w.args.query, "authentication");
        assert_eq!(w.args.limit, 10);
    }

    #[test]
    fn search_args_parse_limit() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: SearchArgs,
        }

        let w = Wrapper::parse_from(["test", "auth*", "-n", "5"]);
        assert_eq!(w.args.query, "auth*");
        assert_eq!(w.args.limit, 5);
    }

    // -----------------------------------------------------------------------
    // render_search_human
    // -----------------------------------------------------------------------

    #[test]
    fn render_search_human_no_results() {
        let out = SearchOutput {
            query: "nonexistent".into(),
            count: 0,
            results: vec![],
        };
        let mut buf = Vec::new();
        render_search_human(&out, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("No results"));
        assert!(text.contains("nonexistent"));
    }

    #[test]
    fn render_search_human_with_results() {
        let out = SearchOutput {
            query: "auth".into(),
            count: 2,
            results: vec![
                SearchResult {
                    id: "bn-001".into(),
                    title: "Authentication timeout".into(),
                    score: -3.5,
                    state: "open".into(),
                },
                SearchResult {
                    id: "bn-002".into(),
                    title: "Auth service broken".into(),
                    score: -2.1,
                    state: "doing".into(),
                },
            ],
        };
        let mut buf = Vec::new();
        render_search_human(&out, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("2 result(s)"));
        assert!(text.contains("bn-001"));
        assert!(text.contains("Authentication timeout"));
        assert!(text.contains("open"));
        assert!(text.contains("bn-002"));
        assert!(text.contains("doing"));
    }

    // -----------------------------------------------------------------------
    // run_search integration
    // -----------------------------------------------------------------------

    fn setup_test_dir() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(&bones_dir).unwrap();
        let db_path = bones_dir.join("bones.db");

        let mut conn = Connection::open(&db_path).expect("open db");
        migrations::migrate(&mut conn).expect("migrate");
        ensure_tracking_table(&conn).expect("tracking");

        let proj = Projector::new(&conn);
        proj.project_event(&make_create(
            "bn-001",
            "Authentication timeout regression",
            Some("Auth service fails after 30 seconds"),
            &["auth", "backend"],
            "h1",
        ))
        .unwrap();
        proj.project_event(&make_create(
            "bn-002",
            "Update documentation",
            Some("Fix typos in README"),
            &["docs"],
            "h2",
        ))
        .unwrap();

        let root = dir.path().to_path_buf();
        (dir, root)
    }

    #[test]
    fn run_search_finds_results() {
        let (_dir, root) = setup_test_dir();
        let args = SearchArgs {
            query: "authentication".into(),
            limit: 10,
        };
        run_search(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_search_json_output() {
        let (_dir, root) = setup_test_dir();
        let args = SearchArgs {
            query: "auth".into(),
            limit: 10,
        };
        run_search(&args, OutputMode::Json, &root).unwrap();
    }

    #[test]
    fn run_search_no_results() {
        let (_dir, root) = setup_test_dir();
        let args = SearchArgs {
            query: "zzznomatch".into(),
            limit: 10,
        };
        // Should succeed (not error) even with no results
        run_search(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_search_prefix_query() {
        let (_dir, root) = setup_test_dir();
        let args = SearchArgs {
            query: "auth*".into(),
            limit: 10,
        };
        run_search(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_search_missing_projection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = SearchArgs {
            query: "test".into(),
            limit: 10,
        };
        assert!(run_search(&args, OutputMode::Human, dir.path()).is_err());
    }

    #[test]
    fn run_search_empty_query_errors() {
        let (_dir, root) = setup_test_dir();
        let args = SearchArgs {
            query: "   ".into(),
            limit: 10,
        };
        assert!(run_search(&args, OutputMode::Human, &root).is_err());
    }

    #[test]
    fn search_output_json_serializable() {
        let out = SearchOutput {
            query: "auth".into(),
            count: 1,
            results: vec![SearchResult {
                id: "bn-001".into(),
                title: "Auth bug".into(),
                score: -2.5,
                state: "open".into(),
            }],
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("bn-001"));
        assert!(json.contains("auth"));
        assert!(json.contains("Auth bug"));
    }
}
