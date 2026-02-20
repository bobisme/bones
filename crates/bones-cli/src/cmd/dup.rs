//! `bn dup` — find potential duplicate work items using FTS5 lexical search.
//!
//! Uses the source item's title (and optionally description) as an FTS5 query
//! to find similar items. BM25 scores are normalized to [0, 1] using the
//! source item's own score as the reference maximum.
//!
//! Match types are classified using thresholds from `.bones/config.toml`:
//! - `search.duplicate_threshold` (default 0.85) → `likely_duplicate`
//! - `search.related_threshold` (default 0.65) → `possibly_related`
//! - Below `related_threshold` → `maybe_related`

use crate::cmd::show::resolve_item_id;
use crate::output::{CliError, OutputMode, render, render_error};
use bones_core::config;
use bones_core::db::fts;
use bones_core::db::query;
use clap::Args;
use serde::Serialize;
use std::io::Write;

#[derive(Args, Debug)]
#[command(
    about = "Find potential duplicate work items",
    long_about = "Find work items that may be duplicates of the given item.\n\n\
                  Uses SQLite FTS5 lexical search with BM25 ranking. The item's \
                  title and description are used as a query against all other items. \
                  Results are classified by similarity score using thresholds from \
                  `.bones/config.toml` (search.duplicate_threshold, search.related_threshold).",
    after_help = "EXAMPLES:\n    # Find duplicates of an item\n    bn dup bn-abc\n\n\
                  # Use a custom threshold\n    bn dup bn-abc --threshold 0.75\n\n\
                  # Machine-readable output\n    bn dup bn-abc --json"
)]
pub struct DupArgs {
    /// Item ID to check for duplicates. Supports partial IDs.
    pub id: String,

    /// Override similarity threshold (0.0–1.0). Below this, items are excluded.
    /// Defaults to `search.related_threshold` from config (0.65).
    #[arg(long)]
    pub threshold: Option<f64>,
}

/// Similarity classification for a candidate duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchType {
    /// Normalized score ≥ duplicate_threshold (default 0.85).
    LikelyDuplicate,
    /// Normalized score ≥ related_threshold (default 0.65).
    PossiblyRelated,
    /// Normalized score below related_threshold but above the display cutoff.
    MaybeRelated,
}

impl MatchType {
    fn label(self) -> &'static str {
        match self {
            Self::LikelyDuplicate => "likely_duplicate",
            Self::PossiblyRelated => "possibly_related",
            Self::MaybeRelated => "maybe_related",
        }
    }
}

/// A candidate duplicate item with similarity score and classification.
#[derive(Debug, Serialize)]
pub struct DupCandidate {
    /// Item ID of the candidate.
    pub id: String,
    /// Item title.
    pub title: String,
    /// Normalized similarity score in [0, 1] (1.0 = identical match to self).
    pub score: f64,
    /// Lifecycle state.
    pub state: String,
    /// Match type classification.
    pub match_type: MatchType,
}

/// JSON envelope for dup output.
#[derive(Debug, Serialize)]
pub struct DupOutput {
    /// Source item ID (canonicalized).
    pub source_id: String,
    /// Source item title.
    pub source_title: String,
    /// Duplicate threshold used.
    pub duplicate_threshold: f64,
    /// Related threshold used.
    pub related_threshold: f64,
    /// Number of candidates found.
    pub count: usize,
    /// Ordered list of candidates (highest score first).
    pub candidates: Vec<DupCandidate>,
}

/// Execute `bn dup <id>`.
///
/// Resolves the item, builds an FTS5 query from its title and description,
/// searches for similar items, normalizes BM25 scores, applies thresholds,
/// and renders results.
///
/// # Errors
///
/// Returns an error if the projection is missing, item not found, or output
/// rendering fails.
pub fn run_dup(
    args: &DupArgs,
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
                    "run `bn rebuild` to initialize the projection",
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

    // Load config for thresholds
    let cfg = config::load_project_config(project_root).unwrap_or_default();
    let dup_threshold = cfg.search.duplicate_threshold;
    let related_threshold = cfg.search.related_threshold;

    // The effective display cutoff: user override or related_threshold
    let display_threshold = args
        .threshold
        .map(|t| t.clamp(0.0, 1.0))
        .unwrap_or(related_threshold);

    // Build FTS5 query from title (and description if available)
    let fts_query = build_fts_query(&source.title, source.description.as_deref());

    if fts_query.is_empty() {
        let dup_output = DupOutput {
            source_id: resolved_id.clone(),
            source_title: source.title.clone(),
            duplicate_threshold: dup_threshold,
            related_threshold,
            count: 0,
            candidates: vec![],
        };
        return render(output, &dup_output, |out, w| render_dup_human(out, w));
    }

    // Search — fetch more than needed so we can find source item's own rank
    let search_limit = 51_u32; // source + up to 50 candidates
    let hits = fts::search_bm25(&conn, &fts_query, search_limit).unwrap_or_default();

    if hits.is_empty() {
        let dup_output = DupOutput {
            source_id: resolved_id.clone(),
            source_title: source.title.clone(),
            duplicate_threshold: dup_threshold,
            related_threshold,
            count: 0,
            candidates: vec![],
        };
        return render(output, &dup_output, |out, w| render_dup_human(out, w));
    }

    // Find the source item's own BM25 rank to use as the normalization reference.
    // The source should be the best match (most negative rank) since we're querying
    // with its own title. Use its rank as the denominator for normalization.
    let source_rank = hits
        .iter()
        .find(|h| h.item_id == resolved_id)
        .map(|h| h.rank)
        .unwrap_or_else(|| {
            // Fallback: use the best rank from all hits
            hits.iter().map(|h| h.rank).fold(f64::INFINITY, f64::min)
        });

    // BM25 ranks are negative; source_rank should be the most negative.
    // If source_rank is 0 or positive (degenerate), skip normalization.
    let can_normalize = source_rank < 0.0;

    // Build candidates (excluding the source item itself)
    let mut candidates: Vec<DupCandidate> = Vec::new();

    for hit in &hits {
        if hit.item_id == resolved_id {
            continue; // skip self
        }

        // Normalize: candidate_rank / source_rank (both negative → 0-1)
        // Higher = more similar.
        let normalized_score = if can_normalize {
            (hit.rank / source_rank).clamp(0.0, 1.0)
        } else {
            // Cannot normalize; treat all as maybe_related
            0.5
        };

        // Apply display threshold cutoff
        if normalized_score < display_threshold {
            continue;
        }

        let match_type = classify_match(normalized_score, dup_threshold, related_threshold);

        // Fetch state
        let state = conn
            .query_row(
                "SELECT state FROM items WHERE item_id = ?1",
                rusqlite::params![hit.item_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_else(|_| "unknown".to_string());

        candidates.push(DupCandidate {
            id: hit.item_id.clone(),
            title: hit.title.clone(),
            score: normalized_score,
            state,
            match_type,
        });
    }

    // Sort by score descending (highest similarity first), then by id for stability
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });

    let dup_output = DupOutput {
        source_id: resolved_id,
        source_title: source.title,
        duplicate_threshold: dup_threshold,
        related_threshold,
        count: candidates.len(),
        candidates,
    };

    render(output, &dup_output, |out, w| render_dup_human(out, w))
}

/// Classify a normalized similarity score into a match type.
fn classify_match(score: f64, dup_threshold: f64, related_threshold: f64) -> MatchType {
    if score >= dup_threshold {
        MatchType::LikelyDuplicate
    } else if score >= related_threshold {
        MatchType::PossiblyRelated
    } else {
        MatchType::MaybeRelated
    }
}

/// Build a sanitized FTS5 query from an item's title and optional description.
///
/// Extracts word tokens (alphanumeric + hyphens), de-duplicates, and joins
/// them with spaces for FTS5 OR semantics. Special FTS5 characters are stripped
/// to prevent syntax errors.
pub fn build_fts_query(title: &str, description: Option<&str>) -> String {
    let combined = match description {
        Some(desc) => format!("{title} {desc}"),
        None => title.to_string(),
    };

    // Extract word tokens: alphanumeric and hyphens, minimum 3 chars to reduce noise
    let mut tokens: Vec<String> = combined
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter_map(|tok| {
            let t = tok.trim_matches('-');
            if t.len() >= 3 {
                Some(t.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect();

    // De-duplicate while preserving order
    let mut seen = std::collections::HashSet::new();
    tokens.retain(|t| seen.insert(t.clone()));

    tokens.join(" ")
}

/// Render duplicate candidates in human-readable format.
fn render_dup_human(out: &DupOutput, w: &mut dyn Write) -> std::io::Result<()> {
    writeln!(
        w,
        "Duplicate check for: {} — {}",
        out.source_id, out.source_title
    )?;
    writeln!(
        w,
        "Thresholds: likely_duplicate ≥ {:.0}%, possibly_related ≥ {:.0}%",
        out.duplicate_threshold * 100.0,
        out.related_threshold * 100.0,
    )?;

    if out.candidates.is_empty() {
        writeln!(w, "No duplicates or related items found.")?;
        return Ok(());
    }

    writeln!(w, "{:-<70}", "")?;

    for candidate in &out.candidates {
        writeln!(
            w,
            "  {:14}  {:.0}%  [{}]  {}  ({})",
            candidate.match_type.label(),
            candidate.score * 100.0,
            candidate.state,
            candidate.id,
            candidate.title,
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

    // -----------------------------------------------------------------------
    // build_fts_query
    // -----------------------------------------------------------------------

    #[test]
    fn build_fts_query_from_title_only() {
        let q = build_fts_query("Authentication timeout regression", None);
        assert!(q.contains("authentication"));
        assert!(q.contains("timeout"));
        assert!(q.contains("regression"));
    }

    #[test]
    fn build_fts_query_deduplicates_tokens() {
        let q = build_fts_query("auth auth authentication", None);
        let parts: Vec<&str> = q.split_whitespace().collect();
        // "auth" and "authentication" should appear once each
        let auth_count = parts.iter().filter(|&&p| p == "auth").count();
        assert_eq!(auth_count, 1, "auth should appear once, got: {q}");
    }

    #[test]
    fn build_fts_query_strips_short_tokens() {
        let q = build_fts_query("a is auth", None);
        // "a" (1 char) and "is" (2 chars) are below min length (3) and should be excluded
        let parts: Vec<&str> = q.split_whitespace().collect();
        assert!(!parts.contains(&"a"), "single char 'a' should be excluded");
        assert!(!parts.contains(&"is"), "two-char 'is' should be excluded");
        // "auth" (4 chars) should be included
        assert!(q.contains("auth"));
    }

    #[test]
    fn build_fts_query_includes_description() {
        let q = build_fts_query("Fix bug", Some("authentication service broken"));
        assert!(q.contains("authentication"));
        assert!(q.contains("service"));
        assert!(q.contains("broken"));
    }

    #[test]
    fn build_fts_query_empty_title_no_desc() {
        let q = build_fts_query("", None);
        assert!(q.is_empty());
    }

    #[test]
    fn build_fts_query_strips_specials() {
        let q = build_fts_query("auth* \"quoted\" (paren) {brace}", None);
        // Special chars stripped — should not contain FTS5 syntax chars
        assert!(!q.contains('"'));
        assert!(!q.contains('*'));
        assert!(!q.contains('('));
        assert!(!q.contains('{'));
    }

    // -----------------------------------------------------------------------
    // classify_match
    // -----------------------------------------------------------------------

    #[test]
    fn classify_match_likely_duplicate() {
        assert_eq!(classify_match(0.90, 0.85, 0.65), MatchType::LikelyDuplicate);
        assert_eq!(classify_match(0.85, 0.85, 0.65), MatchType::LikelyDuplicate);
        assert_eq!(classify_match(1.00, 0.85, 0.65), MatchType::LikelyDuplicate);
    }

    #[test]
    fn classify_match_possibly_related() {
        assert_eq!(classify_match(0.75, 0.85, 0.65), MatchType::PossiblyRelated);
        assert_eq!(classify_match(0.65, 0.85, 0.65), MatchType::PossiblyRelated);
    }

    #[test]
    fn classify_match_maybe_related() {
        assert_eq!(classify_match(0.50, 0.85, 0.65), MatchType::MaybeRelated);
        assert_eq!(classify_match(0.00, 0.85, 0.65), MatchType::MaybeRelated);
    }

    // -----------------------------------------------------------------------
    // MatchType serialization
    // -----------------------------------------------------------------------

    #[test]
    fn match_type_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&MatchType::LikelyDuplicate).unwrap(),
            "\"likely_duplicate\""
        );
        assert_eq!(
            serde_json::to_string(&MatchType::PossiblyRelated).unwrap(),
            "\"possibly_related\""
        );
        assert_eq!(
            serde_json::to_string(&MatchType::MaybeRelated).unwrap(),
            "\"maybe_related\""
        );
    }

    // -----------------------------------------------------------------------
    // render_dup_human
    // -----------------------------------------------------------------------

    #[test]
    fn render_dup_human_no_candidates() {
        let out = DupOutput {
            source_id: "bn-001".into(),
            source_title: "Fix auth".into(),
            duplicate_threshold: 0.85,
            related_threshold: 0.65,
            count: 0,
            candidates: vec![],
        };
        let mut buf = Vec::new();
        render_dup_human(&out, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("bn-001"));
        assert!(text.contains("Fix auth"));
        assert!(text.contains("No duplicates"));
    }

    #[test]
    fn render_dup_human_with_candidates() {
        let out = DupOutput {
            source_id: "bn-001".into(),
            source_title: "Fix auth timeout".into(),
            duplicate_threshold: 0.85,
            related_threshold: 0.65,
            count: 2,
            candidates: vec![
                DupCandidate {
                    id: "bn-002".into(),
                    title: "Authentication timeout bug".into(),
                    score: 0.92,
                    state: "open".into(),
                    match_type: MatchType::LikelyDuplicate,
                },
                DupCandidate {
                    id: "bn-003".into(),
                    title: "Auth service slow".into(),
                    score: 0.70,
                    state: "doing".into(),
                    match_type: MatchType::PossiblyRelated,
                },
            ],
        };
        let mut buf = Vec::new();
        render_dup_human(&out, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("bn-002"));
        assert!(text.contains("likely_duplicate"));
        assert!(text.contains("bn-003"));
        assert!(text.contains("possibly_related"));
        assert!(text.contains("92%"));
        assert!(text.contains("70%"));
    }

    // -----------------------------------------------------------------------
    // DupArgs parsing
    // -----------------------------------------------------------------------

    #[test]
    fn dup_args_parse_id() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: DupArgs,
        }

        let w = Wrapper::parse_from(["test", "bn-001"]);
        assert_eq!(w.args.id, "bn-001");
        assert!(w.args.threshold.is_none());
    }

    #[test]
    fn dup_args_parse_threshold() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: DupArgs,
        }

        let w = Wrapper::parse_from(["test", "bn-001", "--threshold", "0.75"]);
        assert_eq!(w.args.threshold, Some(0.75));
    }

    // -----------------------------------------------------------------------
    // run_dup integration
    // -----------------------------------------------------------------------

    fn setup_test_dir_with_duplicates() -> (tempfile::TempDir, std::path::PathBuf) {
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

        // Near-duplicate
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

    #[test]
    fn run_dup_finds_near_duplicate() {
        let (_dir, root) = setup_test_dir_with_duplicates();
        let args = DupArgs {
            id: "bn-001".into(),
            threshold: None,
        };
        // Should succeed without error
        run_dup(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_dup_json_output() {
        let (_dir, root) = setup_test_dir_with_duplicates();
        let args = DupArgs {
            id: "bn-001".into(),
            threshold: None,
        };
        run_dup(&args, OutputMode::Json, &root).unwrap();
    }

    #[test]
    fn run_dup_missing_item_errors() {
        let (_dir, root) = setup_test_dir_with_duplicates();
        let args = DupArgs {
            id: "nonexistent".into(),
            threshold: None,
        };
        assert!(run_dup(&args, OutputMode::Human, &root).is_err());
    }

    #[test]
    fn run_dup_missing_projection_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = DupArgs {
            id: "bn-001".into(),
            threshold: None,
        };
        assert!(run_dup(&args, OutputMode::Human, dir.path()).is_err());
    }

    #[test]
    fn run_dup_partial_id() {
        let (_dir, root) = setup_test_dir_with_duplicates();
        // "001" → "bn-001" via resolution
        let args = DupArgs {
            id: "001".into(),
            threshold: None,
        };
        run_dup(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn run_dup_custom_threshold_excludes_low_matches() {
        let (_dir, root) = setup_test_dir_with_duplicates();
        // Very high threshold — likely nothing passes
        let args = DupArgs {
            id: "bn-001".into(),
            threshold: Some(0.99),
        };
        // Should succeed (just might return 0 candidates)
        run_dup(&args, OutputMode::Human, &root).unwrap();
    }

    #[test]
    fn dup_output_json_serializable() {
        let out = DupOutput {
            source_id: "bn-001".into(),
            source_title: "Auth bug".into(),
            duplicate_threshold: 0.85,
            related_threshold: 0.65,
            count: 1,
            candidates: vec![DupCandidate {
                id: "bn-002".into(),
                title: "Auth timeout".into(),
                score: 0.88,
                state: "open".into(),
                match_type: MatchType::LikelyDuplicate,
            }],
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("bn-001"));
        assert!(json.contains("likely_duplicate"));
        assert!(json.contains("0.88"));
    }
}
