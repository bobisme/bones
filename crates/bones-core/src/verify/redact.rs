//! Redaction completeness verification.
//!
//! Verifies that all `item.redact` events have been fully applied:
//! redacted content must be absent from projection rows, FTS5 index,
//! and comment bodies.
//!
//! # Approach
//!
//! 1. Replay the event log to find all `item.redact` events and their targets.
//! 2. For each redaction, look up the original event content.
//! 3. Check every query surface (projection, FTS5, comments) for residual content.
//! 4. Report any failures with precise location information.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::event::Event;
use crate::event::data::EventData;
use crate::event::parser::parse_lines;
use crate::event::types::EventType;
use crate::shard::ShardManager;

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

/// Report from verifying redaction completeness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedactionReport {
    /// Number of redaction events checked.
    pub redactions_checked: usize,
    /// Number that passed (content confirmed absent from all surfaces).
    pub passed: usize,
    /// Number that failed (residual content found somewhere).
    pub failed: usize,
    /// Details of each failure.
    pub failures: Vec<RedactionFailure>,
}

impl RedactionReport {
    /// Returns `true` if all redactions passed verification.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        self.failed == 0
    }
}

/// Details of one failed redaction check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedactionFailure {
    /// The item ID whose redaction is incomplete.
    pub item_id: String,
    /// The original event hash that was supposed to be redacted.
    pub event_hash: String,
    /// Where residual content was found.
    pub residual_locations: Vec<ResidualLocation>,
}

/// A location where residual (un-redacted) content was found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ResidualLocation {
    /// Redaction record missing from `event_redactions` table.
    MissingRedactionRecord,
    /// Comment body not replaced with `[redacted]`.
    CommentNotRedacted {
        /// The comment ID in the projection.
        comment_id: i64,
    },
    /// Content found in FTS5 index (the redacted text is still searchable).
    Fts5Index {
        /// The search term that matched.
        matched_term: String,
    },
}

// ---------------------------------------------------------------------------
// Parsed redaction context
// ---------------------------------------------------------------------------

/// A redact event paired with the content it targets.
#[derive(Debug)]
struct RedactionTarget {
    /// The item ID being redacted.
    item_id: String,
    /// The target event hash being redacted.
    target_hash: String,
    /// The redaction reason.
    _reason: String,
    /// The original event's type (if found in the log).
    original_event_type: Option<EventType>,
    /// Searchable text from the original event (for FTS residual checks).
    original_text: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Verify that all `item.redact` events have been fully applied.
///
/// Replays the event log to find all `item.redact` events, then checks every
/// projection surface for residual content.
///
/// # Arguments
///
/// * `events_dir` — Path to `.bones/events/` directory
/// * `db` — Open `SQLite` connection to the projection database
///
/// # Errors
///
/// Returns an error if the event log cannot be read or parsed.
pub fn verify_redactions(events_dir: &Path, db: &Connection) -> Result<RedactionReport> {
    let targets = collect_redaction_targets(events_dir)?;
    let total = targets.len();

    let mut failures = Vec::new();
    for target in &targets {
        let locs = check_residuals(db, target)?;
        if !locs.is_empty() {
            failures.push(RedactionFailure {
                item_id: target.item_id.clone(),
                event_hash: target.target_hash.clone(),
                residual_locations: locs,
            });
        }
    }

    let failed = failures.len();
    Ok(RedactionReport {
        redactions_checked: total,
        passed: total - failed,
        failed,
        failures,
    })
}

/// Verify redaction for a single item.
///
/// Finds all `item.redact` events targeting the given `item_id` and checks
/// for residual content.
///
/// # Arguments
///
/// * `item_id` — The item ID to verify
/// * `events_dir` — Path to `.bones/events/` directory
/// * `db` — Open `SQLite` connection to the projection database
///
/// # Errors
///
/// Returns an error if the event log cannot be read or parsed.
pub fn verify_item_redaction(
    item_id: &str,
    events_dir: &Path,
    db: &Connection,
) -> Result<Vec<RedactionFailure>> {
    let all_targets = collect_redaction_targets(events_dir)?;
    let item_targets: Vec<_> = all_targets
        .into_iter()
        .filter(|t| t.item_id == item_id)
        .collect();

    let mut failures = Vec::new();
    for target in &item_targets {
        let locs = check_residuals(db, target)?;
        if !locs.is_empty() {
            failures.push(RedactionFailure {
                item_id: target.item_id.clone(),
                event_hash: target.target_hash.clone(),
                residual_locations: locs,
            });
        }
    }
    Ok(failures)
}

// ---------------------------------------------------------------------------
// Internal: collect redaction targets from the event log
// ---------------------------------------------------------------------------

/// Parse all events and build a map of redaction targets with their
/// original content.
fn collect_redaction_targets(events_dir: &Path) -> Result<Vec<RedactionTarget>> {
    let dot = Path::new(".");
    let bones_dir = events_dir.parent().unwrap_or(dot);
    let shard_mgr = ShardManager::new(bones_dir);

    let content = shard_mgr
        .replay()
        .map_err(|e| anyhow::anyhow!("replay shards: {e}"))?;

    if content.trim().is_empty() {
        return Ok(Vec::new());
    }

    let events = parse_lines(&content)
        .map_err(|(line_num, e)| anyhow::anyhow!("parse error at line {line_num}: {e}"))?;

    // Build a hash → event index for looking up original events.
    let events_by_hash: HashMap<&str, &Event> =
        events.iter().map(|e| (e.event_hash.as_str(), e)).collect();

    // Find all redact events and pair them with their targets.
    let mut targets = Vec::new();
    for event in &events {
        if event.event_type != EventType::Redact {
            continue;
        }
        let EventData::Redact(ref redact_data) = event.data else {
            continue;
        };

        let original = events_by_hash.get(redact_data.target_hash.as_str());
        let (original_event_type, original_text) = original.map_or((None, None), |orig| {
            (Some(orig.event_type), extract_searchable_text(orig))
        });

        targets.push(RedactionTarget {
            item_id: event.item_id.as_str().to_string(),
            target_hash: redact_data.target_hash.clone(),
            _reason: redact_data.reason.clone(),
            original_event_type,
            original_text,
        });
    }

    Ok(targets)
}

/// Extract searchable text from an event for FTS residual checking.
///
/// Returns the concatenation of meaningful text fields from the event
/// payload, which should NOT appear in any search index after redaction.
fn extract_searchable_text(event: &Event) -> Option<String> {
    match &event.data {
        EventData::Comment(d) => Some(d.body.clone()),
        EventData::Create(d) => {
            let mut parts = vec![d.title.clone()];
            if let Some(ref desc) = d.description {
                parts.push(desc.clone());
            }
            Some(parts.join(" "))
        }
        EventData::Update(d) => {
            // The value field may contain text content.
            Some(
                d.value
                    .as_str()
                    .map_or_else(|| d.value.to_string(), str::to_string),
            )
        }
        EventData::Compact(d) => Some(d.summary.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Internal: check residuals in each query surface
// ---------------------------------------------------------------------------

/// Check all projection surfaces for residual un-redacted content.
fn check_residuals(db: &Connection, target: &RedactionTarget) -> Result<Vec<ResidualLocation>> {
    let mut locations = Vec::new();

    // 1. Check that the redaction record exists in event_redactions table.
    check_redaction_record(db, target, &mut locations)?;

    // 2. If the target was a comment event, check that the comment body
    //    has been replaced with '[redacted]'.
    check_comment_redacted(db, target, &mut locations)?;

    // 3. If we have the original text, check FTS5 for residual content.
    check_fts5_residual(db, target, &mut locations)?;

    Ok(locations)
}

/// Verify that an `event_redactions` record exists for this target.
fn check_redaction_record(
    db: &Connection,
    target: &RedactionTarget,
    locations: &mut Vec<ResidualLocation>,
) -> Result<()> {
    let exists: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM event_redactions WHERE target_event_hash = ?1)",
            params![target.target_hash],
            |row| row.get(0),
        )
        .context("check event_redactions for target hash")?;

    if !exists {
        locations.push(ResidualLocation::MissingRedactionRecord);
    }
    Ok(())
}

/// If the targeted event was a comment, verify its body is `[redacted]`.
fn check_comment_redacted(
    db: &Connection,
    target: &RedactionTarget,
    locations: &mut Vec<ResidualLocation>,
) -> Result<()> {
    // Check if a comment with this event_hash exists and has un-redacted body.
    let mut stmt = db
        .prepare("SELECT comment_id, body FROM item_comments WHERE event_hash = ?1")
        .context("prepare comment redaction check")?;

    let rows: Vec<(i64, String)> = stmt
        .query_map(params![target.target_hash], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .context("query comment by event_hash")?
        .filter_map(Result::ok)
        .collect();

    for (comment_id, body) in rows {
        if body != "[redacted]" {
            locations.push(ResidualLocation::CommentNotRedacted { comment_id });
        }
    }
    Ok(())
}

/// If original text is available, check that it doesn't appear in FTS5 results.
///
/// Strategy: extract significant words from the original text (skip common
/// stop-words, require length ≥ 4), then search the FTS5 index. If the
/// redacted item's ID appears in results, there may be residual content.
///
/// Note: This is a heuristic check. Redaction of create events doesn't
/// necessarily mean the title/description should be removed from the
/// projection (only the targeted event's payload is redacted). We only
/// flag FTS hits when the original event was a comment (which the projection
/// stores directly).
fn check_fts5_residual(
    db: &Connection,
    target: &RedactionTarget,
    locations: &mut Vec<ResidualLocation>,
) -> Result<()> {
    // Only check FTS residual for comment events — other event types'
    // content may legitimately exist in the projection from non-redacted events.
    let is_comment = matches!(target.original_event_type, Some(EventType::Comment));
    if !is_comment {
        return Ok(());
    }

    let text = match &target.original_text {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(()),
    };

    // Extract significant words for FTS probing.
    let words = extract_probe_words(text);
    if words.is_empty() {
        return Ok(());
    }

    // Check if any of the probe words match in the FTS index for this item.
    // We use quoted phrases to avoid false positives from stemming.
    for word in &words {
        // FTS5 query: search for the exact word
        let fts_query = format!("\"{}\"", word.replace('"', ""));
        let hit_count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM items_fts \
                 WHERE items_fts MATCH ?1 AND item_id = ?2",
                params![fts_query, target.item_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if hit_count > 0 {
            locations.push(ResidualLocation::Fts5Index {
                matched_term: word.clone(),
            });
            // One FTS hit is enough to flag the issue.
            break;
        }
    }

    Ok(())
}

/// Extract significant words from text for FTS probing.
///
/// Filters out short words (< 4 chars) and common English stop-words
/// to reduce false positives.
fn extract_probe_words(text: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "the", "and", "for", "are", "but", "not", "you", "all", "can", "had", "her", "was", "one",
        "our", "out", "has", "have", "been", "from", "this", "that", "they", "with", "which",
        "their", "would", "there", "what", "about", "will", "make", "like", "just", "than", "them",
        "very", "when", "some", "could", "more", "also", "into", "other", "then", "these", "only",
        "after", "most",
    ];

    text.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| w.len() >= 4 && !STOP_WORDS.contains(&w.as_str()))
        .take(5) // Limit probe words to avoid excessive queries
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::project;
    use crate::event::data::*;
    use crate::event::writer::write_event;
    use crate::model::item::Kind;
    use crate::model::item_id::ItemId;
    use rusqlite::Connection;
    use std::collections::BTreeMap;
    use std::time::Duration;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Set up a bones project with shard infrastructure and projection DB.
    fn setup_test_project() -> (TempDir, Connection) {
        let dir = TempDir::new().expect("create tempdir");
        let bones_dir = dir.path().join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init shard");

        let db_path = bones_dir.join("bones.db");
        let conn = crate::db::open_projection(&db_path).expect("open projection");
        project::ensure_tracking_table(&conn).expect("tracking table");

        (dir, conn)
    }

    fn make_create_event(item_id: &str, title: &str, ts: i64) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: title.into(),
                kind: Kind::Task,
                size: None,
                urgency: crate::model::item::Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: Some(format!("Description for {title}")),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        write_event(&mut event).expect("compute hash");
        event
    }

    fn make_comment_event(item_id: &str, body: &str, ts: i64) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Comment,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Comment(CommentData {
                body: body.into(),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        write_event(&mut event).expect("compute hash");
        event
    }

    fn make_redact_event(item_id: &str, target_hash: &str, reason: &str, ts: i64) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: "test-agent".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Redact,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Redact(RedactData {
                target_hash: target_hash.into(),
                reason: reason.into(),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        write_event(&mut event).expect("compute hash");
        event
    }

    /// Write events to the shard and project them into the DB.
    fn write_and_project(dir: &TempDir, conn: &Connection, events: &[Event]) {
        let bones_dir = dir.path().join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        let projector = project::Projector::new(conn);

        for event in events {
            let mut event_clone = event.clone();
            let line = write_event(&mut event_clone).expect("serialize event");
            shard_mgr
                .append(&line, false, Duration::from_secs(5))
                .expect("append event");
            projector.project_event(event).expect("project event");
        }
    }

    // -----------------------------------------------------------------------
    // Unit tests: extract_probe_words
    // -----------------------------------------------------------------------

    #[test]
    fn probe_words_filters_short_and_stop_words() {
        let words = extract_probe_words("the quick brown fox jumps over the lazy dog");
        assert!(words.contains(&"quick".to_string()));
        assert!(words.contains(&"brown".to_string()));
        assert!(words.contains(&"jumps".to_string()));
        assert!(words.contains(&"lazy".to_string()));
        assert!(!words.contains(&"the".to_string()));
        assert!(!words.contains(&"fox".to_string())); // 3 chars
        assert!(!words.contains(&"dog".to_string())); // 3 chars
    }

    #[test]
    fn probe_words_limits_to_five() {
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliet";
        let words = extract_probe_words(text);
        assert!(words.len() <= 5);
    }

    #[test]
    fn probe_words_handles_empty_text() {
        assert!(extract_probe_words("").is_empty());
        assert!(extract_probe_words("   ").is_empty());
    }

    #[test]
    fn probe_words_strips_punctuation() {
        let words = extract_probe_words("hello! world? testing... (works)");
        assert!(words.contains(&"hello".to_string()));
        assert!(words.contains(&"world".to_string()));
        assert!(words.contains(&"testing".to_string()));
        assert!(words.contains(&"works".to_string()));
    }

    // -----------------------------------------------------------------------
    // Unit tests: extract_searchable_text
    // -----------------------------------------------------------------------

    #[test]
    fn searchable_text_from_comment() {
        let event = make_comment_event("bn-test", "Secret API key: abc123", 1000);
        let text = extract_searchable_text(&event);
        assert_eq!(text, Some("Secret API key: abc123".into()));
    }

    #[test]
    fn searchable_text_from_create() {
        let event = make_create_event("bn-test", "Fix auth timeout", 1000);
        let text = extract_searchable_text(&event);
        let t = text.unwrap();
        assert!(t.contains("Fix auth timeout"));
        assert!(t.contains("Description for Fix auth timeout"));
    }

    // -----------------------------------------------------------------------
    // Integration tests: verify_redactions
    // -----------------------------------------------------------------------

    #[test]
    fn verify_redactions_empty_log() {
        let (dir, conn) = setup_test_project();
        let events_dir = dir.path().join(".bones").join("events");

        let report = verify_redactions(&events_dir, &conn).expect("verify");
        assert_eq!(report.redactions_checked, 0);
        assert_eq!(report.passed, 0);
        assert_eq!(report.failed, 0);
        assert!(report.is_ok());
    }

    #[test]
    fn verify_redactions_no_redacts() {
        let (dir, conn) = setup_test_project();

        let create = make_create_event("bn-tst1", "Normal item", 1000);
        write_and_project(&dir, &conn, &[create]);

        let events_dir = dir.path().join(".bones").join("events");
        let report = verify_redactions(&events_dir, &conn).expect("verify");
        assert_eq!(report.redactions_checked, 0);
        assert!(report.is_ok());
    }

    #[test]
    fn verify_redactions_comment_properly_redacted() {
        let (dir, conn) = setup_test_project();

        let create = make_create_event("bn-tst1", "Test item", 1000);
        let comment = make_comment_event("bn-tst1", "Contains secret info", 2000);
        let redact = make_redact_event("bn-tst1", &comment.event_hash, "accidental secret", 3000);

        write_and_project(&dir, &conn, &[create, comment, redact]);

        let events_dir = dir.path().join(".bones").join("events");
        let report = verify_redactions(&events_dir, &conn).expect("verify");
        assert_eq!(report.redactions_checked, 1);
        assert_eq!(report.passed, 1);
        assert_eq!(report.failed, 0);
        assert!(report.is_ok());
    }

    #[test]
    fn verify_detects_missing_redaction_record() {
        let (dir, conn) = setup_test_project();

        let create = make_create_event("bn-tst1", "Test item", 1000);
        let comment = make_comment_event("bn-tst1", "Secret info", 2000);

        // Write create + comment to shard and project
        write_and_project(&dir, &conn, &[create, comment.clone()]);

        // Write redact event to shard ONLY (don't project it)
        let redact = make_redact_event("bn-tst1", &comment.event_hash, "accidental secret", 3000);
        let bones_dir = dir.path().join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        let mut redact_clone = redact.clone();
        let line = write_event(&mut redact_clone).expect("serialize");
        shard_mgr
            .append(&line, false, Duration::from_secs(5))
            .expect("append");

        let events_dir = bones_dir.join("events");
        let report = verify_redactions(&events_dir, &conn).expect("verify");
        assert_eq!(report.redactions_checked, 1);
        assert_eq!(report.failed, 1);

        let failure = &report.failures[0];
        assert_eq!(failure.item_id, "bn-tst1");
        assert!(
            failure
                .residual_locations
                .iter()
                .any(|l| matches!(l, ResidualLocation::MissingRedactionRecord))
        );
        assert!(
            failure
                .residual_locations
                .iter()
                .any(|l| matches!(l, ResidualLocation::CommentNotRedacted { .. }))
        );
    }

    #[test]
    fn verify_item_redaction_filters_by_item() {
        let (dir, conn) = setup_test_project();

        let create1 = make_create_event("bn-aaa", "Item A", 1000);
        let create2 = make_create_event("bn-bbb", "Item B", 1001);
        let comment1 = make_comment_event("bn-aaa", "Secret A", 2000);
        let comment2 = make_comment_event("bn-bbb", "Secret B", 2001);
        let redact1 = make_redact_event("bn-aaa", &comment1.event_hash, "reason A", 3000);
        let redact2 = make_redact_event("bn-bbb", &comment2.event_hash, "reason B", 3001);

        write_and_project(
            &dir,
            &conn,
            &[create1, create2, comment1, comment2, redact1, redact2],
        );

        let events_dir = dir.path().join(".bones").join("events");

        // Verify only item A
        let failures_a = verify_item_redaction("bn-aaa", &events_dir, &conn).expect("verify A");
        assert!(failures_a.is_empty(), "item A should pass");

        // Verify only item B
        let failures_b = verify_item_redaction("bn-bbb", &events_dir, &conn).expect("verify B");
        assert!(failures_b.is_empty(), "item B should pass");

        // Verify non-existent item
        let failures_none =
            verify_item_redaction("bn-zzz", &events_dir, &conn).expect("verify nonexistent");
        assert!(failures_none.is_empty());
    }

    #[test]
    fn verify_multiple_redactions_mixed_results() {
        let (dir, conn) = setup_test_project();

        let create = make_create_event("bn-mix1", "Mixed item", 1000);
        let comment_ok = make_comment_event("bn-mix1", "Safe comment", 2000);
        let comment_fail = make_comment_event("bn-mix1", "Dangerous secret", 2001);

        // Redact both comments
        let redact_ok = make_redact_event("bn-mix1", &comment_ok.event_hash, "reason 1", 3000);
        let redact_fail = make_redact_event("bn-mix1", &comment_fail.event_hash, "reason 2", 3001);

        // Write and project all events normally (both redactions applied)
        write_and_project(
            &dir,
            &conn,
            &[
                create,
                comment_ok,
                comment_fail.clone(),
                redact_ok,
                redact_fail,
            ],
        );

        let events_dir = dir.path().join(".bones").join("events");
        let report = verify_redactions(&events_dir, &conn).expect("verify");

        // Both should pass since both redactions were projected
        assert_eq!(report.redactions_checked, 2);
        assert_eq!(report.passed, 2);
        assert_eq!(report.failed, 0);
        assert!(report.is_ok());
    }

    #[test]
    fn report_serializes_to_json() {
        let report = RedactionReport {
            redactions_checked: 3,
            passed: 2,
            failed: 1,
            failures: vec![RedactionFailure {
                item_id: "bn-abc".into(),
                event_hash: "blake3:deadbeef".into(),
                residual_locations: vec![
                    ResidualLocation::MissingRedactionRecord,
                    ResidualLocation::CommentNotRedacted { comment_id: 42 },
                ],
            }],
        };

        let json = serde_json::to_string_pretty(&report).expect("serialize");
        assert!(json.contains("redactions_checked"));
        assert!(json.contains("bn-abc"));
        assert!(json.contains("MissingRedactionRecord"));
        assert!(json.contains("CommentNotRedacted"));
    }

    #[test]
    fn report_is_ok_when_no_failures() {
        let report = RedactionReport {
            redactions_checked: 5,
            passed: 5,
            failed: 0,
            failures: vec![],
        };
        assert!(report.is_ok());
    }

    #[test]
    fn report_not_ok_when_failures_exist() {
        let report = RedactionReport {
            redactions_checked: 5,
            passed: 4,
            failed: 1,
            failures: vec![RedactionFailure {
                item_id: "bn-x".into(),
                event_hash: "blake3:abc".into(),
                residual_locations: vec![ResidualLocation::MissingRedactionRecord],
            }],
        };
        assert!(!report.is_ok());
    }
}
