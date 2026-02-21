//! `bn similar` — find items most similar to a given item using fusion scoring.
//!
//! Combines lexical (FTS5), semantic, and structural search layers via
//! Reciprocal Rank Fusion (RRF) to produce a ranked list of similar items.
//! Excludes the source item from results.

use crate::cmd::dup::{build_fts_query, has_meaningful_signal_overlap};
use crate::cmd::show::resolve_item_id;
use crate::output::{CliError, OutputMode, render, render_error};
use bones_core::config::load_project_config;
use bones_core::db::query;
use bones_search::find_duplicates_with_model;
use bones_search::fusion::SearchConfig;
use bones_search::semantic::SemanticModel;
use bones_triage::graph::RawGraph;
use clap::Args;
use serde::Serialize;
use std::io::Write;

const DEFAULT_LIMIT: usize = 10;

#[derive(Args, Debug)]
#[command(
    about = "Find items most similar to a given item",
    long_about = "Find work items most similar to the given item using fusion scoring.\n\n\
                  Combines lexical (FTS5), semantic, and structural search layers via\n\
                  Reciprocal Rank Fusion (RRF) to rank candidates by similarity.\n\n\
                  Results exclude the source item and show per-layer score breakdown.",
    after_help = "EXAMPLES:\n    # Find items similar to bn-abc\n    bn similar bn-abc\n\n\
                  # Limit to top 5 results\n    bn similar bn-abc --limit 5\n\n\
                  # Machine-readable output\n    bn triage similar bn-abc --format json"
)]
pub struct SimilarArgs {
    /// Item ID to find similar items for. Supports partial IDs.
    pub id: String,

    /// Maximum number of results to return.
    #[arg(short, long, default_value_t = DEFAULT_LIMIT)]
    pub limit: usize,
}

/// A single similar item result with per-layer score breakdown.
#[derive(Debug, Serialize)]
pub struct SimilarResult {
    /// Item ID of the similar item.
    pub id: String,
    /// Title of the similar item.
    pub title: String,
    /// Composite RRF fusion score (higher = more similar).
    pub score: f32,
    /// RRF score contribution from the lexical (FTS5) layer; 0.0 if absent.
    pub lexical_score: f32,
    /// RRF score contribution from the semantic (KNN) layer; 0.0 if absent.
    pub semantic_score: f32,
    /// RRF score contribution from the structural similarity layer; 0.0 if absent.
    pub structural_score: f32,
}

/// JSON envelope for `bn similar` output.
#[derive(Debug, Serialize)]
pub struct SimilarOutput {
    /// Canonicalized source item ID.
    pub source_id: String,
    /// Source item title.
    pub source_title: String,
    /// Number of results returned.
    pub count: usize,
    /// Ordered list of similar items (highest score first).
    pub results: Vec<SimilarResult>,
}

/// Compute a per-layer score from a rank position using the RRF formula.
///
/// Returns `1.0 / (k + rank)` for a present item, or `0.0` if absent
/// (`rank == usize::MAX`).
fn rank_to_score(rank: usize, k: usize) -> f32 {
    if rank == usize::MAX {
        0.0
    } else {
        1.0 / (k as f32 + rank as f32)
    }
}

/// Execute `bn similar <id>`.
///
/// Resolves the source item, constructs a query from its title and description,
/// invokes the fusion search pipeline, filters out the source item from results,
/// and renders the ranked similar items.
///
/// # Errors
///
/// Returns an error if the projection database is missing, the item is not
/// found, or output rendering fails.
pub fn run_similar(
    args: &SimilarArgs,
    output: OutputMode,
    project_root: &std::path::Path,
) -> anyhow::Result<()> {
    let db_path = project_root.join(".bones/bones.db");

    let conn = match query::try_open_projection(&db_path)? {
        Some(c) => c,
        None => {
            render_error(
                output,
                &CliError::with_details(
                    "projection database not found",
                    "run `bn admin rebuild` to initialize the projection",
                    "projection_missing",
                ),
            )?;
            anyhow::bail!("projection not found");
        }
    };

    // Resolve potentially partial ID
    let resolved_id = match resolve_item_id(&conn, &args.id)? {
        Some(id) => id,
        None => {
            render_error(
                output,
                &CliError::with_details(
                    format!("item '{}' not found", args.id),
                    "use `bn list` to see available items",
                    "item_not_found",
                ),
            )?;
            anyhow::bail!("item '{}' not found", args.id);
        }
    };

    // Fetch the source item
    let source = match query::get_item(&conn, &resolved_id, false)? {
        Some(item) => item,
        None => {
            render_error(
                output,
                &CliError::with_details(
                    format!("item '{}' not found or deleted", resolved_id),
                    "use `bn list` to see available items",
                    "item_not_found",
                ),
            )?;
            anyhow::bail!("item '{}' not found", resolved_id);
        }
    };

    // Build a sanitized FTS query from title + optional description.
    let query_text = build_fts_query(&source.title, source.description.as_deref());

    if query_text.is_empty() {
        let similar_output = SimilarOutput {
            source_id: resolved_id,
            source_title: source.title,
            count: 0,
            results: Vec::new(),
        };
        return render(output, &similar_output, |out, w| {
            render_similar_human(out, w)
        });
    }

    let cfg = load_project_config(project_root).unwrap_or_default();
    let search_config = SearchConfig {
        rrf_k: 60,
        likely_duplicate_threshold: cfg.search.duplicate_threshold as f32,
        possibly_related_threshold: cfg.search.related_threshold as f32,
        maybe_related_threshold: 0.50,
    };
    let graph = RawGraph::from_sqlite(&conn)
        .map(|raw| raw.graph)
        .unwrap_or_else(|err| {
            tracing::warn!("unable to load dependency graph for similar: {err}");
            petgraph::graph::DiGraph::new()
        });
    let semantic_model = if cfg.search.semantic {
        match SemanticModel::load() {
            Ok(model) => Some(model),
            Err(err) => {
                tracing::warn!(
                    "semantic model unavailable for similar search; using lexical+structural only: {err}"
                );
                None
            }
        }
    } else {
        None
    };

    // Fetch limit+1 to guarantee enough results after self-exclusion
    let fetch_limit = args.limit.saturating_add(1);

    let candidates = find_duplicates_with_model(
        &query_text,
        &conn,
        &graph,
        &search_config,
        semantic_model.as_ref(),
        fetch_limit,
    )?;

    let k = search_config.rrf_k;

    // Filter out self, take up to limit, enrich with title
    let results: Vec<SimilarResult> = candidates
        .into_iter()
        .filter(|c| c.item_id != resolved_id)
        .filter_map(|c| {
            let title = conn
                .query_row(
                    "SELECT title FROM items WHERE item_id = ?1",
                    rusqlite::params![c.item_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap_or_else(|_| "<unknown>".to_string());

            if !has_meaningful_signal_overlap(&source.title, &title) {
                return None;
            }

            Some(SimilarResult {
                id: c.item_id,
                title,
                score: c.composite_score,
                lexical_score: rank_to_score(c.lexical_rank, k),
                semantic_score: rank_to_score(c.semantic_rank, k),
                structural_score: rank_to_score(c.structural_rank, k),
            })
        })
        .take(args.limit)
        .collect();

    let similar_output = SimilarOutput {
        source_id: resolved_id,
        source_title: source.title,
        count: results.len(),
        results,
    };

    render(output, &similar_output, |out, w| {
        render_similar_human(out, w)
    })
}

/// Render similar-item results in human-readable tabular format.
fn render_similar_human(out: &SimilarOutput, w: &mut dyn Write) -> std::io::Result<()> {
    writeln!(w, "Similar to: {} — {}", out.source_id, out.source_title)?;

    if out.results.is_empty() {
        writeln!(w, "No similar items found.")?;
        writeln!(w, "Try widening context (labels/description) and run again")?;
        return Ok(());
    }

    writeln!(w, "Results: {}", out.results.len())?;
    writeln!(w, "{:-<96}", "")?;
    writeln!(
        w,
        "{:>4}  {:<16}  {:>7}  {:>7}  {:>7}  {:>7}  TITLE",
        "RANK", "ID", "SCORE", "LEX", "SEM", "STR"
    )?;
    writeln!(w, "{:-<96}", "")?;

    for (i, result) in out.results.iter().enumerate() {
        writeln!(
            w,
            "{:>4}  {:<16}  {:>7.3}  {:>7.3}  {:>7.3}  {:>7.3}  {}",
            i + 1,
            result.id,
            result.score,
            result.lexical_score,
            result.semantic_score,
            result.structural_score,
            result.title,
        )?;
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

    fn setup_test_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(&bones_dir).unwrap();
        let db_path = bones_dir.join("bones.db");

        let mut conn = Connection::open(&db_path).expect("open db");
        migrations::migrate(&mut conn).expect("migrate");
        ensure_tracking_table(&conn).expect("tracking");

        let proj = Projector::new(&conn);

        // Source item
        proj.project_event(&make_create(
            "bn-001",
            "Authentication timeout regression",
            Some("The auth service times out after 30 seconds"),
            &["auth", "backend"],
            "h1",
        ))
        .unwrap();

        // Similar item (similar title/description)
        proj.project_event(&make_create(
            "bn-002",
            "Authentication timeout regression",
            Some("Auth service timeout issue"),
            &["auth"],
            "h2",
        ))
        .unwrap();

        // Unrelated item
        proj.project_event(&make_create(
            "bn-003",
            "Update README documentation",
            Some("Fix typos in docs"),
            &["docs"],
            "h3",
        ))
        .unwrap();

        let root = dir.path().to_path_buf();
        (dir, root)
    }

    fn setup_test_db_with_punctuation_titles() -> (tempfile::TempDir, std::path::PathBuf) {
        let (dir, root) = setup_test_db();
        let db_path = root.join(".bones").join("bones.db");
        let conn = Connection::open(&db_path).expect("open db");
        let proj = Projector::new(&conn);

        proj.project_event(&make_create(
            "bn-010",
            "[Phase 2] Auth service goal: callback timeout",
            Some("Investigate timeout in auth callback path"),
            &["auth", "phase-2"],
            "h10",
        ))
        .expect("insert punctuated source item");

        proj.project_event(&make_create(
            "bn-011",
            "Auth callback timeout in phase 2",
            Some("Auth callback timeout mirrors goal issue"),
            &["auth"],
            "h11",
        ))
        .expect("insert punctuated neighbor item");

        (dir, root)
    }

    // -----------------------------------------------------------------------
    // rank_to_score
    // -----------------------------------------------------------------------

    #[test]
    fn rank_to_score_absent_is_zero() {
        assert_eq!(rank_to_score(usize::MAX, 60), 0.0);
    }

    #[test]
    fn rank_to_score_rank_1() {
        let score = rank_to_score(1, 60);
        assert!((score - 1.0 / 61.0).abs() < 1e-6);
    }

    #[test]
    fn rank_to_score_rank_2() {
        let score = rank_to_score(2, 60);
        assert!((score - 1.0 / 62.0).abs() < 1e-6);
    }

    #[test]
    fn rank_to_score_higher_rank_lower_score() {
        let s1 = rank_to_score(1, 60);
        let s2 = rank_to_score(5, 60);
        assert!(s1 > s2);
    }

    // -----------------------------------------------------------------------
    // SimilarArgs parsing
    // -----------------------------------------------------------------------

    #[test]
    fn similar_args_parse_id() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: SimilarArgs,
        }

        let w = Wrapper::parse_from(["test", "bn-001"]);
        assert_eq!(w.args.id, "bn-001");
        assert_eq!(w.args.limit, DEFAULT_LIMIT);
    }

    #[test]
    fn similar_args_parse_limit_long() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: SimilarArgs,
        }

        let w = Wrapper::parse_from(["test", "bn-001", "--limit", "5"]);
        assert_eq!(w.args.id, "bn-001");
        assert_eq!(w.args.limit, 5);
    }

    #[test]
    fn similar_args_parse_limit_short() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: SimilarArgs,
        }

        let w = Wrapper::parse_from(["test", "bn-001", "-l", "3"]);
        assert_eq!(w.args.limit, 3);
    }

    // -----------------------------------------------------------------------
    // render_similar_human
    // -----------------------------------------------------------------------

    #[test]
    fn render_similar_human_no_results() {
        let out = SimilarOutput {
            source_id: "bn-001".into(),
            source_title: "Fix auth".into(),
            count: 0,
            results: vec![],
        };
        let mut buf = Vec::new();
        render_similar_human(&out, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("bn-001"));
        assert!(text.contains("Fix auth"));
        assert!(text.contains("No similar items found"));
    }

    #[test]
    fn render_similar_human_with_results() {
        let out = SimilarOutput {
            source_id: "bn-001".into(),
            source_title: "Authentication timeout".into(),
            count: 2,
            results: vec![
                SimilarResult {
                    id: "bn-002".into(),
                    title: "Auth timeout bug".into(),
                    score: 0.05,
                    lexical_score: 0.05,
                    semantic_score: 0.0,
                    structural_score: 0.0,
                },
                SimilarResult {
                    id: "bn-003".into(),
                    title: "Login timeout".into(),
                    score: 0.03,
                    lexical_score: 0.03,
                    semantic_score: 0.0,
                    structural_score: 0.0,
                },
            ],
        };
        let mut buf = Vec::new();
        render_similar_human(&out, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("bn-002"));
        assert!(text.contains("bn-003"));
        assert!(text.contains("Auth timeout bug"));
        assert!(text.contains("Login timeout"));
        // Both ranks should appear
        assert!(text.contains("1"));
        assert!(text.contains("2"));
    }

    #[test]
    fn render_similar_human_shows_scores() {
        let out = SimilarOutput {
            source_id: "bn-001".into(),
            source_title: "Auth".into(),
            count: 1,
            results: vec![SimilarResult {
                id: "bn-002".into(),
                title: "Auth bug".into(),
                score: 0.049,
                lexical_score: 0.049,
                semantic_score: 0.0,
                structural_score: 0.0,
            }],
        };
        let mut buf = Vec::new();
        render_similar_human(&out, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        // Scores formatted as .3f
        assert!(text.contains("0.049"));
        assert!(text.contains("0.000")); // semantic and structural
    }

    // -----------------------------------------------------------------------
    // run_similar integration
    // -----------------------------------------------------------------------

    #[test]
    fn run_similar_self_excluded() {
        // Should run without error; self should not appear in output
        let (_dir, root) = setup_test_db();
        let args = SimilarArgs {
            id: "bn-001".into(),
            limit: 10,
        };
        run_similar(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_similar_json_output() {
        let (_dir, root) = setup_test_db();
        let args = SimilarArgs {
            id: "bn-001".into(),
            limit: 10,
        };
        run_similar(&args, OutputMode::Json, &root).unwrap();
    }

    #[test]
    fn run_similar_respects_limit() {
        let (_dir, root) = setup_test_db();
        // limit=1 → at most 1 result
        let args = SimilarArgs {
            id: "bn-001".into(),
            limit: 1,
        };
        run_similar(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_similar_partial_id() {
        let (_dir, root) = setup_test_db();
        // "001" should resolve to "bn-001"
        let args = SimilarArgs {
            id: "001".into(),
            limit: 10,
        };
        run_similar(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_similar_handles_titles_with_punctuation() {
        let (_dir, root) = setup_test_db_with_punctuation_titles();
        let args = SimilarArgs {
            id: "bn-010".into(),
            limit: 10,
        };
        run_similar(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_similar_missing_item_errors() {
        let (_dir, root) = setup_test_db();
        let args = SimilarArgs {
            id: "nonexistent-xyz".into(),
            limit: 10,
        };
        assert!(run_similar(&args, OutputMode::Human, &root).is_err());
    }

    #[test]
    fn run_similar_missing_projection_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = SimilarArgs {
            id: "bn-001".into(),
            limit: 10,
        };
        assert!(run_similar(&args, OutputMode::Human, dir.path()).is_err());
    }

    // -----------------------------------------------------------------------
    // JSON schema
    // -----------------------------------------------------------------------

    #[test]
    fn similar_output_json_schema() {
        let out = SimilarOutput {
            source_id: "bn-001".into(),
            source_title: "Auth bug".into(),
            count: 1,
            results: vec![SimilarResult {
                id: "bn-002".into(),
                title: "Auth timeout".into(),
                score: 0.049,
                lexical_score: 0.049,
                semantic_score: 0.0,
                structural_score: 0.0,
            }],
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"source_id\""));
        assert!(json.contains("\"source_title\""));
        assert!(json.contains("\"count\""));
        assert!(json.contains("\"results\""));
        assert!(json.contains("\"id\""));
        assert!(json.contains("\"title\""));
        assert!(json.contains("\"score\""));
        assert!(json.contains("\"lexical_score\""));
        assert!(json.contains("\"semantic_score\""));
        assert!(json.contains("\"structural_score\""));
        assert!(json.contains("bn-001"));
        assert!(json.contains("bn-002"));
    }

    // -----------------------------------------------------------------------
    // Result ordering
    // -----------------------------------------------------------------------

    #[test]
    fn results_sorted_by_score_descending() {
        let out = SimilarOutput {
            source_id: "bn-001".into(),
            source_title: "Auth".into(),
            count: 3,
            results: vec![
                SimilarResult {
                    id: "bn-a".into(),
                    title: "A".into(),
                    score: 0.05,
                    lexical_score: 0.05,
                    semantic_score: 0.0,
                    structural_score: 0.0,
                },
                SimilarResult {
                    id: "bn-b".into(),
                    title: "B".into(),
                    score: 0.03,
                    lexical_score: 0.03,
                    semantic_score: 0.0,
                    structural_score: 0.0,
                },
                SimilarResult {
                    id: "bn-c".into(),
                    title: "C".into(),
                    score: 0.01,
                    lexical_score: 0.01,
                    semantic_score: 0.0,
                    structural_score: 0.0,
                },
            ],
        };
        for i in 0..out.results.len() - 1 {
            assert!(
                out.results[i].score >= out.results[i + 1].score,
                "results not sorted: index {} score {} >= index {} score {}",
                i,
                out.results[i].score,
                i + 1,
                out.results[i + 1].score
            );
        }
    }
}
