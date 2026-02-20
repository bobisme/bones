//! `bn diagnose` — repository health diagnostics for event-log and projection integrity.
//!
//! JSON output schema is intentionally stable for automation. Fields are added
//! in a backward-compatible way; existing field names and types should not
//! change.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use bones_core::db::{incremental, query};
use bones_core::event::parser::{ParseError, ParsedLine, parse_line};
use bones_core::event::types::EventType;
use bones_core::shard::ShardManager;
use chrono::Utc;
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::output::{OutputMode, render};

const MAX_PARSE_ERROR_SAMPLES: usize = 20;
const MAX_UNKNOWN_PARENT_SAMPLES: usize = 20;
const MAX_ORPHAN_ITEM_SAMPLES: usize = 20;

/// Execute `bn diagnose`.
///
/// Produces a health report over raw `.events` shards and compares that state
/// against the SQLite projection cursor/tracking tables.
pub fn run_diagnose(output: OutputMode, project_root: &Path) -> Result<()> {
    let report = build_report(project_root)?;
    render(output, &report, |r, w| render_human(r, w))
}

#[derive(Debug, Serialize)]
pub struct DiagnoseReport {
    pub generated_at_us: i64,
    pub shard_inventory: ShardInventory,
    pub event_stats: EventStats,
    pub integrity: IntegrityReport,
    pub projection: ProjectionReport,
    pub remediation_hints: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ShardInventory {
    pub shard_count: usize,
    pub total_bytes: u64,
    pub shards: Vec<ShardSummary>,
}

#[derive(Debug, Serialize)]
pub struct ShardSummary {
    pub shard_name: String,
    pub path: String,
    pub byte_size: u64,
    pub event_count: usize,
    pub parse_error_count: usize,
    pub time_range: TimeRange,
    pub read_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EventStats {
    pub total_events: usize,
    pub unique_event_hashes: usize,
    pub duplicate_event_hashes: usize,
    pub unique_items: usize,
    pub events_by_type: BTreeMap<String, usize>,
    pub events_by_agent: BTreeMap<String, usize>,
    pub time_range: TimeRange,
}

#[derive(Debug, Serialize)]
pub struct IntegrityReport {
    pub parse_error_count: usize,
    pub parse_error_samples: Vec<ParseErrorSample>,
    pub hash_anomalies: HashAnomalies,
    pub orphan_events: OrphanEventStats,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ParseErrorSample {
    pub shard_name: String,
    pub line_number: usize,
    pub error: String,
}

#[derive(Debug, Default, Serialize)]
pub struct HashAnomalies {
    pub invalid_event_hash_lines: usize,
    pub hash_mismatch_lines: usize,
    pub invalid_parent_hash_lines: usize,
    pub unknown_parent_refs: usize,
    pub unknown_parent_samples: Vec<UnknownParentSample>,
}

#[derive(Debug, Serialize)]
pub struct UnknownParentSample {
    pub event_hash: String,
    pub item_id: String,
    pub missing_parent_hash: String,
}

#[derive(Debug, Serialize)]
pub struct OrphanEventStats {
    pub orphan_event_count: usize,
    pub orphan_item_count: usize,
    pub orphan_item_samples: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ProjectionReport {
    pub status: String,
    pub db_path: String,
    pub expected_offset: u64,
    pub expected_last_hash: Option<String>,
    pub cursor_offset: Option<i64>,
    pub cursor_hash: Option<String>,
    pub cursor_offset_matches_log: Option<bool>,
    pub cursor_hash_matches_log: Option<bool>,
    pub projected_events_table_present: bool,
    pub projected_event_count: Option<usize>,
    pub projected_events_match_log: Option<bool>,
    pub item_count: Option<usize>,
    pub placeholder_item_count: Option<usize>,
    pub incremental_safety_error: Option<String>,
    pub drift_indicators: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TimeRange {
    pub earliest_wall_ts_us: Option<i64>,
    pub latest_wall_ts_us: Option<i64>,
}

impl TimeRange {
    fn observe(&mut self, ts: i64) {
        self.earliest_wall_ts_us = Some(
            self.earliest_wall_ts_us
                .map_or(ts, |existing| existing.min(ts)),
        );
        self.latest_wall_ts_us = Some(
            self.latest_wall_ts_us
                .map_or(ts, |existing| existing.max(ts)),
        );
    }
}

#[derive(Debug)]
struct EventMeta {
    hash: String,
    item_id: String,
    parents: Vec<String>,
}

fn build_report(project_root: &Path) -> Result<DiagnoseReport> {
    let generated_at_us = Utc::now().timestamp_micros();
    let bones_dir = project_root.join(".bones");
    let events_dir = bones_dir.join("events");
    let db_path = bones_dir.join("bones.db");

    let shard_manager = ShardManager::new(&bones_dir);
    let shards = shard_manager
        .list_shards()
        .map_err(|e| anyhow::anyhow!("list shards: {e}"))?;

    let mut shard_summaries = Vec::new();
    let mut total_bytes = 0_u64;

    let mut total_events = 0_usize;
    let mut seen_hashes: HashSet<String> = HashSet::new();
    let mut duplicate_hashes = 0_usize;
    let mut unique_items: BTreeSet<String> = BTreeSet::new();
    let mut events_by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut events_by_agent: BTreeMap<String, usize> = BTreeMap::new();
    let mut global_time_range = TimeRange::default();

    let mut parse_error_count = 0_usize;
    let mut parse_error_samples = Vec::new();
    let mut hash_anomalies = HashAnomalies::default();
    let mut warnings = Vec::new();

    let mut event_metas = Vec::new();
    let mut created_items: HashSet<String> = HashSet::new();
    let mut non_create_event_counts: BTreeMap<String, usize> = BTreeMap::new();

    for (year, month) in shards.iter().copied() {
        let shard_name = ShardManager::shard_filename(year, month);
        let shard_path = shard_manager.shard_path(year, month);

        let mut shard_summary = ShardSummary {
            shard_name: shard_name.clone(),
            path: shard_path.display().to_string(),
            byte_size: 0,
            event_count: 0,
            parse_error_count: 0,
            time_range: TimeRange::default(),
            read_error: None,
        };

        let bytes = match fs::read(&shard_path) {
            Ok(bytes) => bytes,
            Err(err) => {
                shard_summary.read_error = Some(err.to_string());
                warnings.push(format!("failed to read shard {shard_name}: {err}"));
                shard_summaries.push(shard_summary);
                continue;
            }
        };

        shard_summary.byte_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        total_bytes = total_bytes.saturating_add(shard_summary.byte_size);

        let content = String::from_utf8_lossy(&bytes);
        for (line_no, line) in content.lines().enumerate() {
            match parse_line(line) {
                Ok(ParsedLine::Event(event)) => {
                    let event = *event;

                    total_events += 1;
                    shard_summary.event_count += 1;
                    unique_items.insert(event.item_id.as_str().to_string());

                    let type_key = event.event_type.to_string();
                    *events_by_type.entry(type_key).or_insert(0) += 1;
                    *events_by_agent.entry(event.agent.clone()).or_insert(0) += 1;

                    shard_summary.time_range.observe(event.wall_ts_us);
                    global_time_range.observe(event.wall_ts_us);

                    if !seen_hashes.insert(event.event_hash.clone()) {
                        duplicate_hashes += 1;
                    }

                    if event.event_type == EventType::Create {
                        created_items.insert(event.item_id.as_str().to_string());
                    } else {
                        *non_create_event_counts
                            .entry(event.item_id.as_str().to_string())
                            .or_insert(0) += 1;
                    }

                    event_metas.push(EventMeta {
                        hash: event.event_hash,
                        item_id: event.item_id.as_str().to_string(),
                        parents: event.parents,
                    });
                }
                Ok(ParsedLine::Comment(_) | ParsedLine::Blank) => {}
                Err(err) => {
                    parse_error_count += 1;
                    shard_summary.parse_error_count += 1;
                    classify_parse_error(&err, &mut hash_anomalies);

                    if parse_error_samples.len() < MAX_PARSE_ERROR_SAMPLES {
                        parse_error_samples.push(ParseErrorSample {
                            shard_name: shard_name.clone(),
                            line_number: line_no + 1,
                            error: err.to_string(),
                        });
                    }
                }
            }
        }

        shard_summaries.push(shard_summary);
    }

    for meta in &event_metas {
        for parent in &meta.parents {
            if !seen_hashes.contains(parent) {
                hash_anomalies.unknown_parent_refs += 1;
                if hash_anomalies.unknown_parent_samples.len() < MAX_UNKNOWN_PARENT_SAMPLES {
                    hash_anomalies
                        .unknown_parent_samples
                        .push(UnknownParentSample {
                            event_hash: meta.hash.clone(),
                            item_id: meta.item_id.clone(),
                            missing_parent_hash: parent.clone(),
                        });
                }
            }
        }
    }

    let orphan_item_ids: Vec<String> = non_create_event_counts
        .iter()
        .filter_map(|(item_id, _)| {
            if created_items.contains(item_id) {
                None
            } else {
                Some(item_id.clone())
            }
        })
        .collect();

    let orphan_event_count = orphan_item_ids
        .iter()
        .map(|item_id| non_create_event_counts.get(item_id).copied().unwrap_or(0))
        .sum();

    if parse_error_count > 0 {
        warnings.push(format!(
            "{} malformed line(s) were skipped while computing diagnostics",
            parse_error_count
        ));
    }
    if hash_anomalies.unknown_parent_refs > 0 {
        warnings.push(format!(
            "{} parent reference(s) point to hashes missing from scanned events",
            hash_anomalies.unknown_parent_refs
        ));
    }
    if orphan_event_count > 0 {
        warnings.push(format!(
            "{} event(s) reference item IDs without any item.create event",
            orphan_event_count
        ));
    }

    let shard_inventory = ShardInventory {
        shard_count: shards.len(),
        total_bytes,
        shards: shard_summaries,
    };

    let event_stats = EventStats {
        total_events,
        unique_event_hashes: seen_hashes.len(),
        duplicate_event_hashes: duplicate_hashes,
        unique_items: unique_items.len(),
        events_by_type,
        events_by_agent,
        time_range: global_time_range,
    };

    let integrity = IntegrityReport {
        parse_error_count,
        parse_error_samples,
        hash_anomalies,
        orphan_events: OrphanEventStats {
            orphan_event_count,
            orphan_item_count: orphan_item_ids.len(),
            orphan_item_samples: orphan_item_ids
                .into_iter()
                .take(MAX_ORPHAN_ITEM_SAMPLES)
                .collect(),
        },
        warnings,
    };

    let expected_last_hash = event_metas.last().map(|m| m.hash.clone());
    let projection = collect_projection_report(
        &db_path,
        &events_dir,
        total_bytes,
        expected_last_hash.clone(),
        event_stats.unique_event_hashes,
    );

    let remediation_hints = remediation_hints(&integrity, &projection);

    Ok(DiagnoseReport {
        generated_at_us,
        shard_inventory,
        event_stats,
        integrity,
        projection,
        remediation_hints,
    })
}

fn classify_parse_error(error: &ParseError, anomalies: &mut HashAnomalies) {
    match error {
        ParseError::InvalidEventHash(_) => anomalies.invalid_event_hash_lines += 1,
        ParseError::HashMismatch { .. } => anomalies.hash_mismatch_lines += 1,
        ParseError::InvalidParentHash(_) => anomalies.invalid_parent_hash_lines += 1,
        _ => {}
    }
}

fn collect_projection_report(
    db_path: &Path,
    events_dir: &Path,
    expected_offset: u64,
    expected_last_hash: Option<String>,
    unique_event_hashes: usize,
) -> ProjectionReport {
    let mut report = ProjectionReport {
        status: "unknown".to_string(),
        db_path: db_path.display().to_string(),
        expected_offset,
        expected_last_hash,
        cursor_offset: None,
        cursor_hash: None,
        cursor_offset_matches_log: None,
        cursor_hash_matches_log: None,
        projected_events_table_present: false,
        projected_event_count: None,
        projected_events_match_log: None,
        item_count: None,
        placeholder_item_count: None,
        incremental_safety_error: None,
        drift_indicators: Vec::new(),
    };

    let conn = match query::try_open_projection(db_path) {
        Ok(Some(conn)) => {
            report.status = "ok".to_string();
            conn
        }
        Ok(None) => {
            report.status = "missing_or_corrupt".to_string();
            report
                .drift_indicators
                .push("projection database missing or corrupt".to_string());
            return report;
        }
        Err(err) => {
            report.status = "open_error".to_string();
            report
                .drift_indicators
                .push(format!("failed to open projection database: {err}"));
            return report;
        }
    };

    match query::get_projection_cursor(&conn) {
        Ok((offset, hash)) => {
            report.cursor_offset = Some(offset);
            report.cursor_hash = hash;

            let expected_offset_i64 = i64::try_from(expected_offset).unwrap_or(i64::MAX);
            let offset_matches = offset == expected_offset_i64;
            report.cursor_offset_matches_log = Some(offset_matches);
            if !offset_matches {
                report.drift_indicators.push(format!(
                    "projection cursor offset {} differs from log byte size {}",
                    offset, expected_offset
                ));
            }

            let hash_matches = report.cursor_hash == report.expected_last_hash;
            report.cursor_hash_matches_log = Some(hash_matches);
            if !hash_matches {
                report.drift_indicators.push(format!(
                    "projection cursor hash {:?} differs from last log hash {:?}",
                    report.cursor_hash, report.expected_last_hash
                ));
            }
        }
        Err(err) => {
            report.status = "cursor_error".to_string();
            report
                .drift_indicators
                .push(format!("failed to read projection cursor: {err}"));
        }
    }

    if let Err(reason) = incremental::check_incremental_safety(&conn, events_dir) {
        report.incremental_safety_error = Some(reason.clone());
        report
            .drift_indicators
            .push(format!("incremental safety check failed: {reason}"));
    }

    report.projected_events_table_present = table_exists(&conn, "projected_events");
    if report.projected_events_table_present {
        if let Some(count) = query_count_usize(&conn, "SELECT COUNT(*) FROM projected_events") {
            report.projected_event_count = Some(count);
            let matches = count == unique_event_hashes;
            report.projected_events_match_log = Some(matches);
            if !matches {
                report.drift_indicators.push(format!(
                    "projected_events has {count} row(s) but log has {} unique hash(es)",
                    unique_event_hashes
                ));
            }
        }
    } else {
        report
            .drift_indicators
            .push("projection is missing projected_events tracking table".to_string());
    }

    report.item_count = query_count_usize(&conn, "SELECT COUNT(*) FROM items");
    report.placeholder_item_count =
        query_count_usize(&conn, "SELECT COUNT(*) FROM items WHERE title = ''");

    if let Some(placeholder_count) = report.placeholder_item_count
        && placeholder_count > 0
    {
        report.drift_indicators.push(format!(
            "projection contains {placeholder_count} placeholder item(s) with empty title"
        ));
    }

    if report.drift_indicators.is_empty() {
        report.status = "ok".to_string();
    }

    report
}

fn table_exists(conn: &Connection, table_name: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1)",
        params![table_name],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

fn query_count_usize(conn: &Connection, sql: &str) -> Option<usize> {
    let raw: i64 = conn.query_row(sql, [], |row| row.get(0)).ok()?;
    usize::try_from(raw).ok()
}

fn remediation_hints(integrity: &IntegrityReport, projection: &ProjectionReport) -> Vec<String> {
    let mut hints = Vec::new();

    if integrity.parse_error_count > 0
        || integrity.hash_anomalies.hash_mismatch_lines > 0
        || integrity.hash_anomalies.invalid_event_hash_lines > 0
        || integrity.hash_anomalies.invalid_parent_hash_lines > 0
    {
        hints.push(
            "Run `bn admin verify` to pinpoint malformed or tampered event-log lines before syncing."
                .to_string(),
        );
    }

    if !projection.drift_indicators.is_empty() || projection.status != "ok" {
        hints.push(
            "Run `bn admin rebuild` to regenerate the projection database from append-only events."
                .to_string(),
        );
    }

    if integrity.orphan_events.orphan_event_count > 0 {
        hints.push(
            "Investigate import/migration history for missing `item.create` events on orphaned item IDs."
                .to_string(),
        );
    }

    if hints.is_empty() {
        hints.push("No immediate remediation required; health signals look normal.".to_string());
    }

    hints
}

fn render_human(report: &DiagnoseReport, w: &mut dyn Write) -> std::io::Result<()> {
    writeln!(w, "Repository diagnostics")?;
    writeln!(
        w,
        "- shards: {} ({} bytes)",
        report.shard_inventory.shard_count, report.shard_inventory.total_bytes
    )?;
    writeln!(
        w,
        "- events: total={} unique_hashes={} duplicates={} unique_items={}",
        report.event_stats.total_events,
        report.event_stats.unique_event_hashes,
        report.event_stats.duplicate_event_hashes,
        report.event_stats.unique_items,
    )?;
    writeln!(
        w,
        "- time range: {:?} .. {:?}",
        report.event_stats.time_range.earliest_wall_ts_us,
        report.event_stats.time_range.latest_wall_ts_us
    )?;

    writeln!(w)?;
    writeln!(w, "Event type distribution:")?;
    for (event_type, count) in top_counts(&report.event_stats.events_by_type, 10) {
        writeln!(w, "  - {event_type}: {count}")?;
    }

    writeln!(w)?;
    writeln!(w, "Top agents:")?;
    for (agent, count) in top_counts(&report.event_stats.events_by_agent, 10) {
        writeln!(w, "  - {agent}: {count}")?;
    }

    writeln!(w)?;
    writeln!(w, "Integrity:")?;
    writeln!(
        w,
        "  - parse_errors={} hash_mismatch={} invalid_hash={} invalid_parent_hash={} unknown_parents={} orphan_events={}",
        report.integrity.parse_error_count,
        report.integrity.hash_anomalies.hash_mismatch_lines,
        report.integrity.hash_anomalies.invalid_event_hash_lines,
        report.integrity.hash_anomalies.invalid_parent_hash_lines,
        report.integrity.hash_anomalies.unknown_parent_refs,
        report.integrity.orphan_events.orphan_event_count,
    )?;
    if !report.integrity.warnings.is_empty() {
        writeln!(w, "  warnings:")?;
        for warning in report.integrity.warnings.iter().take(6) {
            writeln!(w, "    - {warning}")?;
        }
    }

    writeln!(w)?;
    writeln!(w, "Projection:")?;
    writeln!(w, "  - status: {}", report.projection.status)?;
    writeln!(
        w,
        "  - cursor_offset={:?} expected_offset={} match={:?}",
        report.projection.cursor_offset,
        report.projection.expected_offset,
        report.projection.cursor_offset_matches_log,
    )?;
    writeln!(
        w,
        "  - cursor_hash={:?} expected_last_hash={:?} match={:?}",
        report.projection.cursor_hash,
        report.projection.expected_last_hash,
        report.projection.cursor_hash_matches_log,
    )?;
    writeln!(
        w,
        "  - projected_events={:?} matches_log={:?} placeholders={:?}",
        report.projection.projected_event_count,
        report.projection.projected_events_match_log,
        report.projection.placeholder_item_count,
    )?;
    if !report.projection.drift_indicators.is_empty() {
        writeln!(w, "  drift indicators:")?;
        for indicator in report.projection.drift_indicators.iter().take(6) {
            writeln!(w, "    - {indicator}")?;
        }
    }

    writeln!(w)?;
    writeln!(w, "Remediation hints:")?;
    for hint in &report.remediation_hints {
        writeln!(w, "  - {hint}")?;
    }

    Ok(())
}

fn top_counts(counts: &BTreeMap<String, usize>, limit: usize) -> Vec<(&str, usize)> {
    let mut rows: Vec<(&str, usize)> = counts.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    rows.sort_by(|(ka, va), (kb, vb)| vb.cmp(va).then_with(|| ka.cmp(kb)));
    rows.truncate(limit);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db::rebuild;
    use bones_core::event::Event;
    use bones_core::event::data::{CreateData, EventData, MoveData};
    use bones_core::event::types::EventType;
    use bones_core::event::writer;
    use bones_core::model::item::{Kind, State, Urgency};
    use bones_core::model::item_id::ItemId;
    use rusqlite::Connection;
    use std::collections::BTreeMap;

    fn make_create(item_id: &str, ts: i64, agent: &str) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: agent.to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Create(CreateData {
                title: format!("title-{item_id}"),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        writer::write_event(&mut event).expect("write create event");
        event
    }

    fn make_move(item_id: &str, parent: &str, ts: i64, agent: &str) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: agent.to_string(),
            itc: "itc:AQ.1".to_string(),
            parents: vec![parent.to_string()],
            event_type: EventType::Move,
            item_id: ItemId::new_unchecked(item_id),
            data: EventData::Move(MoveData {
                state: State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        writer::write_event(&mut event).expect("write move event");
        event
    }

    fn write_events_shard(root: &Path, events: &[Event]) -> std::path::PathBuf {
        let events_dir = root.join(".bones/events");
        fs::create_dir_all(&events_dir).expect("create events dir");
        let path = events_dir.join("2026-02.events");

        let mut content = writer::shard_header();
        for event in events {
            content.push_str(&writer::write_line(event).expect("line"));
        }

        fs::write(&path, content).expect("write shard");
        path
    }

    #[test]
    fn diagnose_detects_unknown_parent_and_orphan_events() {
        let dir = tempfile::tempdir().expect("tempdir");

        let create = make_create("bn-aaa1", 1_700_000_001_000_000, "alice");
        let orphan_move = make_move(
            "bn-bb22",
            "blake3:abcdef0123456789",
            1_700_000_001_500_000,
            "bob",
        );
        write_events_shard(dir.path(), &[create, orphan_move]);

        let report = build_report(dir.path()).expect("diagnose report");

        assert_eq!(report.shard_inventory.shard_count, 1);
        assert_eq!(report.event_stats.total_events, 2);
        assert_eq!(report.integrity.hash_anomalies.unknown_parent_refs, 1);
        assert_eq!(report.integrity.orphan_events.orphan_event_count, 1);
        assert_eq!(
            report.projection.status, "missing_or_corrupt",
            "no projection db should be reported"
        );
    }

    #[test]
    fn diagnose_detects_projection_cursor_drift() {
        let dir = tempfile::tempdir().expect("tempdir");

        let create = make_create("bn-aaa1", 1_700_000_001_000_000, "alice");
        let move_event = make_move(
            "bn-aaa1",
            &create.event_hash,
            1_700_000_001_500_000,
            "alice",
        );
        write_events_shard(dir.path(), &[create, move_event]);

        let events_dir = dir.path().join(".bones/events");
        let db_path = dir.path().join(".bones/bones.db");
        rebuild::rebuild(&events_dir, &db_path).expect("rebuild projection");

        let conn = Connection::open(&db_path).expect("open db");
        conn.execute(
            "UPDATE projection_meta SET last_event_offset = 1 WHERE id = 1",
            [],
        )
        .expect("tamper cursor");

        let report = build_report(dir.path()).expect("diagnose report");

        assert_eq!(report.projection.status, "ok");
        assert_eq!(report.projection.cursor_offset_matches_log, Some(false));
        assert!(
            !report.projection.drift_indicators.is_empty(),
            "tampered cursor should surface drift indicator"
        );
    }

    #[test]
    fn top_counts_orders_by_count_desc_then_key() {
        let mut counts = BTreeMap::new();
        counts.insert("z".to_string(), 2);
        counts.insert("a".to_string(), 2);
        counts.insert("m".to_string(), 5);

        let rows = top_counts(&counts, 3);
        assert_eq!(rows[0], ("m", 5));
        assert_eq!(rows[1], ("a", 2));
        assert_eq!(rows[2], ("z", 2));
    }
}
