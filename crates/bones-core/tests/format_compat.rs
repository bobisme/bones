//! Backward compatibility tests for TSJSON event format evolution.
//!
//! These tests verify the policy defined in the versioning spec:
//! all events from prior format versions must always parse with the
//! current parser.
//!
//! # Golden File Rules
//!
//! - `tests/fixtures/v1_events.events` is the golden file for the v1 format.
//! - Once committed, golden files are **NEVER** modified.
//! - Each format version gets its own golden file.
//! - Golden files contain all event types with edge cases.
//!
//! # Tests
//!
//! - [`v1_events_parse_with_current_parser`] — v1 golden events still parse
//! - [`unknown_event_type_warns_not_errors`] — forward-compat: skip unknowns
//! - [`unknown_json_fields_ignored`] — extra JSON fields captured in `extra`
//! - [`future_version_produces_upgrade_error`] — actionable error for newer format
//! - [`missing_header_produces_clear_error`] — clear error when header is absent
//! - [`v1_roundtrip_through_current_writer`] — parse v1 → write → parse again

use std::collections::BTreeMap;
use std::path::PathBuf;

use bones_core::event::Event;
use bones_core::event::data::{
    AssignAction, AssignData, CommentData, CompactData, CreateData, DeleteData, EventData,
    LinkData, MoveData, RedactData, SnapshotData, UnlinkData, UpdateData,
};
use bones_core::event::types::EventType;
use bones_core::event::writer::write_event;
use bones_core::event::{ParseError, ParsedLine, detect_version, parse_line, parse_lines};
use bones_core::model::item::{Kind, Size, State, Urgency};
use bones_core::model::item_id::ItemId;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn item_id(raw: &str) -> ItemId {
    ItemId::new_unchecked(raw)
}

fn base_event(event_type: EventType, item_id_raw: &str, data: EventData) -> Event {
    Event {
        wall_ts_us: 1_708_012_200_000_000,
        agent: "test-agent".into(),
        itc: "itc:AQ".into(),
        parents: vec![],
        event_type,
        item_id: item_id(item_id_raw),
        data,
        event_hash: String::new(),
    }
}

/// Path to the v1 golden fixture file.
fn v1_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("v1_events.events")
}

/// Build a corpus of events covering all 11 event types with edge cases.
///
/// This corpus is the basis for the v1 golden fixture. Once generated and
/// committed, the fixture file becomes a frozen artifact that proves v1 events
/// parse correctly on every future build.
fn build_v1_corpus() -> Vec<Event> {
    let mut events = vec![
        // --- item.create variants ---
        // 1: task, all fields
        base_event(
            EventType::Create,
            "bn-a7x",
            EventData::Create(CreateData {
                title: "Fix auth retry logic".into(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: vec!["backend".into(), "auth".into()],
                parent: None,
                causation: None,
                description: Some("Token refresh race condition in session store".into()),
                extra: BTreeMap::new(),
            }),
        ),
        // 2: bug, urgent, small
        base_event(
            EventType::Create,
            "bn-b8y",
            EventData::Create(CreateData {
                title: "Null pointer in parser".into(),
                kind: Kind::Bug,
                size: Some(Size::S),
                urgency: Urgency::Urgent,
                labels: vec!["crash".into()],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
        ),
        // 3: goal, XL, punt
        base_event(
            EventType::Create,
            "bn-c9z",
            EventData::Create(CreateData {
                title: "Q2 reliability milestone".into(),
                kind: Kind::Goal,
                size: Some(Size::Xl),
                urgency: Urgency::Punt,
                labels: vec!["milestone".into(), "q2".into()],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
        ),
        // 4: with parent and causation
        base_event(
            EventType::Create,
            "bn-d1a",
            EventData::Create(CreateData {
                title: "Follow-up: retry improvements".into(),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: Some("bn-a7x".into()),
                causation: Some("bn-a7x".into()),
                description: None,
                extra: BTreeMap::new(),
            }),
        ),
        // 5: unicode title (CJK + RTL + emoji)
        base_event(
            EventType::Create,
            "bn-e2b",
            EventData::Create(CreateData {
                title: "修复 auth retry / إعادة المحاولة 🔐".into(),
                kind: Kind::Bug,
                size: Some(Size::S),
                urgency: Urgency::Urgent,
                labels: vec!["i18n".into()],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
        ),
        // --- item.update ---
        // 6: update title (string value)
        base_event(
            EventType::Update,
            "bn-a7x",
            EventData::Update(UpdateData {
                field: "title".into(),
                value: serde_json::json!("Fix auth retry logic (v2)"),
                extra: BTreeMap::new(),
            }),
        ),
        // 7: update labels (array value)
        base_event(
            EventType::Update,
            "bn-a7x",
            EventData::Update(UpdateData {
                field: "labels".into(),
                value: serde_json::json!(["backend", "auth", "security"]),
                extra: BTreeMap::new(),
            }),
        ),
        // --- item.move ---
        // 8: open → doing with reason
        base_event(
            EventType::Move,
            "bn-a7x",
            EventData::Move(MoveData {
                state: State::Doing,
                reason: Some("Starting implementation".into()),
                extra: BTreeMap::new(),
            }),
        ),
        // 9: doing → done
        base_event(
            EventType::Move,
            "bn-a7x",
            EventData::Move(MoveData {
                state: State::Done,
                reason: Some("Shipped in commit 9f3a2b1".into()),
                extra: BTreeMap::new(),
            }),
        ),
        // 10: done → archived, no reason
        base_event(
            EventType::Move,
            "bn-a7x",
            EventData::Move(MoveData {
                state: State::Archived,
                reason: None,
                extra: BTreeMap::new(),
            }),
        ),
        // --- item.assign ---
        // 11: assign
        base_event(
            EventType::Assign,
            "bn-a7x",
            EventData::Assign(AssignData {
                agent: "claude-sonnet".into(),
                action: AssignAction::Assign,
                extra: BTreeMap::new(),
            }),
        ),
        // 12: unassign
        base_event(
            EventType::Assign,
            "bn-a7x",
            EventData::Assign(AssignData {
                agent: "claude-sonnet".into(),
                action: AssignAction::Unassign,
                extra: BTreeMap::new(),
            }),
        ),
        // --- item.comment ---
        // 13: ASCII comment
        base_event(
            EventType::Comment,
            "bn-a7x",
            EventData::Comment(CommentData {
                body: "Root cause: race in token expiry check.".into(),
                extra: BTreeMap::new(),
            }),
        ),
        // 14: CJK comment
        base_event(
            EventType::Comment,
            "bn-b8y",
            EventData::Comment(CommentData {
                body: "バグ発見: セッション初期化の競合状態 🐛".into(),
                extra: BTreeMap::new(),
            }),
        ),
        // 15: RTL comment
        base_event(
            EventType::Comment,
            "bn-b8y",
            EventData::Comment(CommentData {
                body: "تم إيجاد السبب الجذري في منطق المصادقة".into(),
                extra: BTreeMap::new(),
            }),
        ),
        // 16: emoji-rich comment
        base_event(
            EventType::Comment,
            "bn-a7x",
            EventData::Comment(CommentData {
                body: "🚀 Deployed! ✅ Tests pass. 🎉 Congrats team! 🦀 Rust FTW.".into(),
                extra: BTreeMap::new(),
            }),
        ),
        // --- item.link ---
        // 17: blocks link
        base_event(
            EventType::Link,
            "bn-a7x",
            EventData::Link(LinkData {
                target: "bn-b8y".into(),
                link_type: "blocks".into(),
                extra: BTreeMap::new(),
            }),
        ),
        // 18: related_to link
        base_event(
            EventType::Link,
            "bn-c9z",
            EventData::Link(LinkData {
                target: "bn-a7x".into(),
                link_type: "related_to".into(),
                extra: BTreeMap::new(),
            }),
        ),
        // --- item.unlink ---
        // 19: with link_type
        base_event(
            EventType::Unlink,
            "bn-a7x",
            EventData::Unlink(UnlinkData {
                target: "bn-b8y".into(),
                link_type: Some("blocks".into()),
                extra: BTreeMap::new(),
            }),
        ),
        // 20: without link_type
        base_event(
            EventType::Unlink,
            "bn-c9z",
            EventData::Unlink(UnlinkData {
                target: "bn-a7x".into(),
                link_type: None,
                extra: BTreeMap::new(),
            }),
        ),
        // --- item.delete ---
        // 21: with reason
        base_event(
            EventType::Delete,
            "bn-d1a",
            EventData::Delete(DeleteData {
                reason: Some("Duplicate of bn-a7x".into()),
                extra: BTreeMap::new(),
            }),
        ),
        // 22: no reason
        base_event(
            EventType::Delete,
            "bn-e2b",
            EventData::Delete(DeleteData {
                reason: None,
                extra: BTreeMap::new(),
            }),
        ),
        // --- item.compact ---
        // 23
        base_event(
            EventType::Compact,
            "bn-a7x",
            EventData::Compact(CompactData {
                summary: "Auth token refresh race condition fixed in commit 9f3a2b1. Root cause: missing mutex in session store. Fixed with read-write lock.".into(),
                extra: BTreeMap::new(),
            }),
        ),
        // --- item.snapshot ---
        // 24: full state snapshot
        base_event(
            EventType::Snapshot,
            "bn-a7x",
            EventData::Snapshot(SnapshotData {
                state: serde_json::json!({
                    "id": "bn-a7x",
                    "title": "Fix auth retry logic (v2)",
                    "kind": "task",
                    "state": "archived",
                    "urgency": "default",
                    "size": "m",
                    "labels": ["backend", "auth", "security"],
                    "assignees": []
                }),
                extra: BTreeMap::new(),
            }),
        ),
        // --- item.redact ---
        // 25
        base_event(
            EventType::Redact,
            "bn-a7x",
            EventData::Redact(RedactData {
                target_hash: "blake3:a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4".into(),
                reason: "Accidental credential exposure in comment".into(),
                extra: BTreeMap::new(),
            }),
        ),
    ];

    // Assign distinct, monotonically increasing timestamps.
    for (i, event) in events.iter_mut().enumerate() {
        event.wall_ts_us = 1_708_012_200_000_000 + (i as i64) * 1_000_000;
    }

    events
}

/// Generate the TSJSON file content for the v1 corpus.
fn generate_v1_content(corpus: &mut Vec<Event>) -> String {
    let mut lines = vec![
        "# bones event log v1".to_string(),
        "# V1 golden fixture — all 11 event types, edge cases, unicode".to_string(),
        "# DO NOT MODIFY — this is a frozen backward-compatibility artifact".to_string(),
        "# Generated by format_compat tests. Covers: all EventType variants,".to_string(),
        "# unicode (CJK/RTL/emoji), optional fields, array values in Update.".to_string(),
        String::new(), // blank separator
    ];

    for event in corpus.iter_mut() {
        let line = write_event(event).expect("write_event in v1 fixture generation");
        lines.push(line.trim_end_matches('\n').to_string());
    }

    lines.join("\n") + "\n"
}

// ---------------------------------------------------------------------------
// TEST: v1_events_parse_with_current_parser
// ---------------------------------------------------------------------------

/// V1 events must parse correctly with the current (and all future) parsers.
///
/// This test reads `tests/fixtures/v1_events.events` — a frozen artifact
/// committed at project inception. As the parser evolves, this file NEVER
/// changes; the test will fail if a parser change breaks v1 backward compat.
///
/// On first run (fixture absent), the file is generated and written so it can
/// be committed. Subsequent runs read the fixed file.
#[test]
fn v1_events_parse_with_current_parser() {
    let path = v1_fixture_path();

    // Generate corpus and content (always, to have a known-good baseline).
    let mut corpus = build_v1_corpus();
    let generated_content = generate_v1_content(&mut corpus);

    // Write the fixture if it doesn't exist (first run only).
    // Use the generated content for verification regardless of file state.
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).expect("create fixtures directory");
        // Best-effort write: if two test threads race here, both produce the
        // same deterministic content, so a partial overwrite is harmless.
        let _ = std::fs::write(&path, &generated_content);
        eprintln!("Generated v1_events.events fixture at {}", path.display());
    }

    // Read from file if available; fall back to generated content.
    // This avoids a race condition on first run when the file is being written.
    let content = std::fs::read_to_string(&path).unwrap_or_else(|_| generated_content.clone());

    // The file must begin with the v1 header.
    let first_line = content.lines().next().unwrap_or("");
    assert_eq!(
        first_line, "# bones event log v1",
        "v1 fixture must start with v1 header"
    );

    // All lines must parse without error.
    let events = parse_lines(&content)
        .expect("v1 events must parse with current parser — backward compat broken!");

    // Must contain at least 25 events (all types represented).
    assert!(
        events.len() >= 25,
        "v1 fixture should have at least 25 events, got {}",
        events.len()
    );

    // Every event must have a valid blake3 hash.
    for (i, event) in events.iter().enumerate() {
        assert!(
            event.event_hash.starts_with("blake3:"),
            "v1 event {i} has invalid hash: {}",
            event.event_hash
        );
    }

    // All 11 event types must be represented.
    let types_found: std::collections::HashSet<EventType> =
        events.iter().map(|e| e.event_type).collect();
    let all_types = [
        EventType::Create,
        EventType::Update,
        EventType::Move,
        EventType::Assign,
        EventType::Comment,
        EventType::Link,
        EventType::Unlink,
        EventType::Delete,
        EventType::Compact,
        EventType::Snapshot,
        EventType::Redact,
    ];
    for expected_type in all_types {
        assert!(
            types_found.contains(&expected_type),
            "v1 fixture missing event type: {expected_type}"
        );
    }

    // Spot-check the first event (item.create, bn-a7x).
    assert_eq!(events[0].event_type, EventType::Create);
    assert_eq!(events[0].item_id.as_str(), "bn-a7x");
    match &events[0].data {
        EventData::Create(d) => {
            assert_eq!(d.title, "Fix auth retry logic");
            assert_eq!(d.kind, Kind::Task);
            assert_eq!(d.size, Some(Size::M));
            assert_eq!(d.labels, vec!["backend", "auth"]);
        }
        other => panic!("expected Create data in first event, got {other:?}"),
    }

    // Timestamps are distinct (events are ordered, not duplicated).
    let timestamps: Vec<i64> = events.iter().map(|e| e.wall_ts_us).collect();
    let mut sorted_unique = timestamps.clone();
    sorted_unique.sort_unstable();
    sorted_unique.dedup();
    assert_eq!(
        sorted_unique.len(),
        timestamps.len(),
        "all v1 events should have distinct timestamps"
    );
}

// ---------------------------------------------------------------------------
// TEST: unknown_event_type_warns_not_errors
// ---------------------------------------------------------------------------

/// Unknown event types (from newer format versions or extensions) must be
/// skipped with a warning — they must NOT cause a parse error.
///
/// This is the forward-compatibility policy: new event types can be added
/// without a format version bump, and older readers simply skip them.
#[test]
fn unknown_event_type_warns_not_errors() {
    use bones_core::event::canonical::canonicalize_json;

    // Build one known-good event for context.
    let mut known = base_event(
        EventType::Comment,
        "bn-a7x",
        EventData::Comment(CommentData {
            body: "Known event".into(),
            extra: BTreeMap::new(),
        }),
    );
    known.wall_ts_us = 1_000;
    let known_line = write_event(&mut known).expect("write known event");

    // Construct a raw TSJSON line with a future event type.
    // We must compute the hash ourselves since write_event only handles known types.
    let unknown_data_json = canonicalize_json(&serde_json::json!({"action":"future_action"}));
    let hash_input =
        format!("2000\ttest-agent\titc:AQ\t\titem.future_v2_type\tbn-a7x\t{unknown_data_json}\n");
    let hash = blake3::hash(hash_input.as_bytes());
    let unknown_line = format!(
        "2000\ttest-agent\titc:AQ\t\titem.future_v2_type\tbn-a7x\t{unknown_data_json}\tblake3:{}",
        hash.to_hex()
    );

    // Another future event type in the same file.
    let unknown_data2 = canonicalize_json(&serde_json::json!({"priority":42,"tags":["x"]}));
    let hash_input2 =
        format!("3000\ttest-agent\titc:AQ\t\titem.another_future_type\tbn-b8y\t{unknown_data2}\n");
    let hash2 = blake3::hash(hash_input2.as_bytes());
    let unknown_line2 = format!(
        "3000\ttest-agent\titc:AQ\t\titem.another_future_type\tbn-b8y\t{unknown_data2}\tblake3:{}",
        hash2.to_hex()
    );

    let mut known2 = base_event(
        EventType::Comment,
        "bn-b8y",
        EventData::Comment(CommentData {
            body: "Another known event".into(),
            extra: BTreeMap::new(),
        }),
    );
    known2.wall_ts_us = 4_000;
    let known2_line = write_event(&mut known2).expect("write known2");

    // Interleave known and unknown event types.
    let input =
        format!("# bones event log v1\n{known_line}{unknown_line}\n{unknown_line2}\n{known2_line}");

    // parse_lines must succeed: unknown types are skipped, not errors.
    let events =
        parse_lines(&input).expect("unknown event types must be skipped, not cause errors");

    // Only the 2 known events should be returned; the 2 unknown lines skipped.
    assert_eq!(
        events.len(),
        2,
        "exactly 2 known events expected, got {}: {:?}",
        events.len(),
        events.iter().map(|e| e.wall_ts_us).collect::<Vec<_>>()
    );
    assert_eq!(events[0].wall_ts_us, 1_000, "first known event at ts=1000");
    assert_eq!(events[1].wall_ts_us, 4_000, "second known event at ts=4000");

    // Single-line parse of an unknown type also errors (parse_line is strict).
    // Only parse_lines does the skip-and-warn; parse_line returns the error.
    let single_err =
        parse_line(&unknown_line).expect_err("parse_line should error on unknown event type");
    assert!(
        matches!(single_err, ParseError::InvalidEventType(_)),
        "expected InvalidEventType, got {single_err:?}"
    );
    // The error message names the unknown type.
    assert!(
        single_err.to_string().contains("future_v2_type"),
        "error should name the unknown type: {single_err}"
    );
}

// ---------------------------------------------------------------------------
// TEST: unknown_json_fields_ignored
// ---------------------------------------------------------------------------

/// Unknown fields in the JSON payload of a known event type must be preserved
/// in the `extra` map — they must NOT cause a parse error.
///
/// This is the schema evolution policy: new optional fields can be added to
/// any event type's JSON payload without breaking older readers that don't
/// know about those fields.
#[test]
fn unknown_json_fields_ignored() {
    use bones_core::event::canonical::canonicalize_json;

    // Build a CreateData JSON with extra unknown fields that don't exist in v1.
    let data_with_extras = serde_json::json!({
        "title": "Task with future fields",
        "kind": "task",
        // Known optional fields
        "size": "m",
        "labels": ["backend"],
        // Unknown future fields — must be captured in extra, not rejected.
        "priority_v2": 10,
        "due_date": "2026-12-31",
        "affected_components": ["auth", "session"],
        "confidence": 0.9
    });
    let canonical = canonicalize_json(&data_with_extras);

    // Manually build a valid TSJSON line with extra JSON fields.
    let hash_input = format!("5000\ttest-agent\titc:AQ\t\titem.create\tbn-a7x\t{canonical}\n");
    let hash = blake3::hash(hash_input.as_bytes());
    let line = format!(
        "5000\ttest-agent\titc:AQ\t\titem.create\tbn-a7x\t{canonical}\tblake3:{}",
        hash.to_hex()
    );

    // Must parse without error.
    let parsed =
        parse_line(&line).expect("event with unknown JSON fields must parse without error");

    let event = match parsed {
        ParsedLine::Event(e) => *e,
        other => panic!("expected Event, got {other:?}"),
    };

    // Core known fields are correctly parsed.
    match &event.data {
        EventData::Create(d) => {
            assert_eq!(d.title, "Task with future fields");
            assert_eq!(d.kind, Kind::Task);
            assert_eq!(d.size, Some(Size::M));
            assert_eq!(d.labels, vec!["backend"]);

            // Unknown fields must be captured in `extra`.
            assert_eq!(
                d.extra.get("priority_v2"),
                Some(&serde_json::json!(10)),
                "priority_v2 should be in extra"
            );
            assert_eq!(
                d.extra.get("due_date"),
                Some(&serde_json::json!("2026-12-31")),
                "due_date should be in extra"
            );
            assert_eq!(
                d.extra.get("affected_components"),
                Some(&serde_json::json!(["auth", "session"])),
                "affected_components should be in extra"
            );
            assert_eq!(
                d.extra.get("confidence"),
                Some(&serde_json::json!(0.9)),
                "confidence should be in extra"
            );
        }
        other => panic!("expected Create data, got {other:?}"),
    }

    // Verify the same behavior works via parse_lines.
    let input = format!("# bones event log v1\n{line}\n");
    let events = parse_lines(&input).expect("parse_lines with extra JSON fields should succeed");
    assert_eq!(events.len(), 1);
    // Confirm the extra fields survive parse_lines too.
    match &events[0].data {
        EventData::Create(d) => {
            assert!(
                d.extra.contains_key("priority_v2"),
                "extra fields should survive parse_lines"
            );
        }
        other => panic!("expected Create data, got {other:?}"),
    }

    // Also test a Comment event with extra fields.
    let comment_with_extras = serde_json::json!({
        "body": "Comment with future fields",
        "sentiment": "positive",
        "auto_generated": true,
        "model_version": "gpt-5-turbo"
    });
    let comment_canonical = canonicalize_json(&comment_with_extras);
    let comment_hash_input =
        format!("6000\ttest-agent\titc:AQ\t\titem.comment\tbn-a7x\t{comment_canonical}\n");
    let comment_hash = blake3::hash(comment_hash_input.as_bytes());
    let comment_line = format!(
        "6000\ttest-agent\titc:AQ\t\titem.comment\tbn-a7x\t{comment_canonical}\tblake3:{}",
        comment_hash.to_hex()
    );

    let parsed_comment = parse_line(&comment_line).expect("comment with extra fields must parse");
    match parsed_comment {
        ParsedLine::Event(e) => match &e.data {
            EventData::Comment(d) => {
                assert_eq!(d.body, "Comment with future fields");
                assert_eq!(
                    d.extra.get("sentiment"),
                    Some(&serde_json::json!("positive"))
                );
                assert_eq!(
                    d.extra.get("auto_generated"),
                    Some(&serde_json::json!(true))
                );
            }
            other => panic!("expected Comment data, got {other:?}"),
        },
        other => panic!("expected Event, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// TEST: future_version_produces_upgrade_error
// ---------------------------------------------------------------------------

/// A file with a version number higher than CURRENT_VERSION must produce a
/// clear, actionable upgrade error — not a silent parse failure or a panic.
///
/// The error must:
/// 1. Mention the version number found.
/// 2. Advise the user to upgrade.
/// 3. Be returned as `ParseError::VersionMismatch`.
#[test]
fn future_version_produces_upgrade_error() {
    // --- detect_version directly ---
    let future_headers = [
        "# bones event log v2",
        "# bones event log v10",
        "# bones event log v99",
        "# bones event log v999",
    ];

    for header in future_headers {
        let err = detect_version(header)
            .expect_err(&format!("'{header}' should be rejected as future version"));

        // Error must mention the version number found.
        let version_str = header.trim_start_matches("# bones event log v");
        assert!(
            err.contains(version_str),
            "error for '{header}' should mention version '{version_str}': {err}"
        );

        // Error must include upgrade advice.
        let lower = err.to_lowercase();
        assert!(
            lower.contains("upgrade") || lower.contains("install") || lower.contains("newer"),
            "error for '{header}' should give upgrade advice: {err}"
        );
    }

    // --- parse_lines with v2 header ---
    let (line_no, err) =
        parse_lines("# bones event log v2\n").expect_err("future version should fail");

    // Error must be on line 1 (the header line).
    assert_eq!(line_no, 1, "version error should be on line 1");

    // Error variant must be VersionMismatch.
    assert!(
        matches!(err, ParseError::VersionMismatch(_)),
        "expected VersionMismatch, got {err:?}"
    );

    // Error message must be actionable.
    let msg = err.to_string();
    assert!(
        msg.contains("2") || msg.contains("version"),
        "VersionMismatch message should be informative: {msg}"
    );
    let lower = msg.to_lowercase();
    assert!(
        lower.contains("upgrade")
            || lower.contains("install")
            || lower.contains("newer")
            || lower.contains("mismatch"),
        "VersionMismatch message should guide user: {msg}"
    );

    // --- parse_lines with v999 header (extreme future version) ---
    let (_, err999) = parse_lines("# bones event log v999\n1000\tagent\titc\t\titem.comment\tbn-a7x\t{\"body\":\"hi\"}\tblake3:aaa\n")
        .expect_err("v999 should fail");
    assert!(
        matches!(err999, ParseError::VersionMismatch(_)),
        "v999 should also be VersionMismatch: {err999:?}"
    );
    let msg999 = err999.to_string();
    assert!(
        msg999.contains("999"),
        "error for v999 should mention 999: {msg999}"
    );
}

// ---------------------------------------------------------------------------
// TEST: missing_header_produces_clear_error
// ---------------------------------------------------------------------------

/// When the shard header is missing or malformed, `detect_version` must return
/// a clear, descriptive error — not a cryptic message or a panic.
#[test]
fn missing_header_produces_clear_error() {
    let bad_headers = [
        // Completely absent header (first line is a comment but wrong format)
        "",
        "not a valid header",
        "# this is not the bones header",
        "bones event log v1",    // missing #
        "# bones event log",     // missing version number
        "# bones event log v",   // version number is empty
        "# bones event log vX",  // version number is not numeric
        "# BONES EVENT LOG V1",  // wrong case
        "## bones event log v1", // double hash
    ];

    for header in bad_headers {
        let err =
            detect_version(header).expect_err(&format!("'{header}' should fail version detection"));

        // Error must be non-empty and descriptive.
        assert!(!err.is_empty(), "error for '{header}' must be non-empty");

        // Should mention what was found or expected, not just "error".
        // (At minimum, the error must be human-readable.)
        assert!(
            err.len() > 10,
            "error for '{header}' must be descriptive, got: {err}"
        );
    }

    // --- parse_lines with data but no header ---
    // A file without a version header is not automatically rejected
    // (parse_lines only checks the header if it finds the HEADER_PREFIX pattern).
    // This is fine — files may legitimately lack the header (legacy or partial).
    // The test here verifies parse_lines does not panic on header-less files.
    let result = parse_lines("1000\tagent\titc\t\titem.comment\tbn-a7x\t{\"body\":\"hi\"}\t\n");
    // May succeed (empty events if hash is wrong) or fail with a parse error.
    // The critical invariant is: NO PANIC.
    let _ = result;

    // A file where the first line looks like a header but has a wrong format
    // should produce a clear error when detect_version is called directly.
    let err =
        detect_version("# bones event log vNaN").expect_err("non-numeric version should fail");
    assert!(!err.is_empty());
    assert!(
        err.len() > 10,
        "error for bad version format must be descriptive: {err}"
    );

    // An empty file's "first line" is an empty string — should fail clearly.
    let err = detect_version("").expect_err("empty header should fail");
    assert!(
        !err.is_empty(),
        "empty header should produce descriptive error"
    );
}

// ---------------------------------------------------------------------------
// TEST: v1_roundtrip_through_current_writer
// ---------------------------------------------------------------------------

/// Parse v1 events, write them with the current writer, parse again, compare.
///
/// This verifies that the v1 → current-writer → parser pipeline is lossless:
/// events read from a v1 file are byte-for-byte (semantically) identical after
/// a write+parse round-trip.
#[test]
fn v1_roundtrip_through_current_writer() {
    let path = v1_fixture_path();

    // Generate corpus and content to ensure we always have a valid baseline.
    let mut corpus = build_v1_corpus();
    let generated_content = generate_v1_content(&mut corpus);

    // Write the fixture if it doesn't exist (first run only, best-effort).
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).expect("create fixtures directory");
        let _ = std::fs::write(&path, &generated_content);
    }

    // Read from file if available; fall back to the generated content.
    let content = std::fs::read_to_string(&path).unwrap_or_else(|_| generated_content.clone());

    // Parse all v1 events.
    let v1_events = parse_lines(&content).expect("v1 events must parse for roundtrip test");
    assert!(!v1_events.is_empty(), "no events to round-trip");

    for (i, original) in v1_events.iter().enumerate() {
        // Write the v1 event using the current writer.
        let mut event_copy = original.clone();
        let written_line = write_event(&mut event_copy)
            .unwrap_or_else(|e| panic!("write_event failed for v1 event {i}: {e}"));

        // Parse the written line.
        let trimmed = written_line.trim_end_matches('\n');
        let reparsed =
            parse_line(trimmed).unwrap_or_else(|e| panic!("re-parse of v1 event {i} failed: {e}"));

        let reparsed_event = match reparsed {
            ParsedLine::Event(e) => *e,
            other => panic!("expected Event for v1 event {i}, got {other:?}"),
        };

        // Semantic equality: all fields except event_hash must match.
        // (The hash is recomputed by write_event, so it will be the same
        // deterministic value — we verify it matches.)
        assert_eq!(
            original.wall_ts_us, reparsed_event.wall_ts_us,
            "v1 event {i}: wall_ts_us mismatch after roundtrip"
        );
        assert_eq!(
            original.agent, reparsed_event.agent,
            "v1 event {i}: agent mismatch after roundtrip"
        );
        assert_eq!(
            original.itc, reparsed_event.itc,
            "v1 event {i}: itc mismatch after roundtrip"
        );
        assert_eq!(
            original.parents, reparsed_event.parents,
            "v1 event {i}: parents mismatch after roundtrip"
        );
        assert_eq!(
            original.event_type, reparsed_event.event_type,
            "v1 event {i}: event_type mismatch after roundtrip"
        );
        assert_eq!(
            original.item_id, reparsed_event.item_id,
            "v1 event {i}: item_id mismatch after roundtrip"
        );
        assert_eq!(
            original.data, reparsed_event.data,
            "v1 event {i}: data mismatch after roundtrip"
        );
        assert_eq!(
            original.event_hash, reparsed_event.event_hash,
            "v1 event {i}: event_hash mismatch after roundtrip (hash not deterministic?)"
        );
    }

    // Verify the write+parse pipeline preserves event count.
    // (No events should be dropped or duplicated.)
    let mut all_written = String::new();
    all_written.push_str("# bones event log v1\n");
    for original in &v1_events {
        let mut event_copy = original.clone();
        let line = write_event(&mut event_copy).expect("write_event in bulk roundtrip");
        all_written.push_str(&line);
    }

    let reparsed_all = parse_lines(&all_written).expect("bulk roundtrip should parse cleanly");
    assert_eq!(
        v1_events.len(),
        reparsed_all.len(),
        "event count must be preserved in roundtrip: {} v1 events, {} after roundtrip",
        v1_events.len(),
        reparsed_all.len()
    );
}
