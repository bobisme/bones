//! Integration tests for the TSJSON parser and writer.
//!
//! Covers:
//! - Round-trip tests (write_event → parse_line) for all 11 event types
//! - Malformed input edge cases producing clear errors (not panics)
//! - Unicode edge cases: emoji, RTL text, zero-width chars, multi-byte sequences
//! - Boundary values: timestamp 0, timestamp i64::MAX, 10,000-char title
//! - Golden-file tests: 20+ representative events written to fixture file

use std::collections::BTreeMap;
use std::path::PathBuf;

use bones_core::event::data::{
    AssignAction, AssignData, CommentData, CompactData, CreateData, DeleteData, EventData,
    LinkData, MoveData, RedactData, SnapshotData, UnlinkData, UpdateData,
};
use bones_core::event::types::EventType;
use bones_core::event::writer::{compute_event_hash, write_event};
use bones_core::event::{parse_line, parse_lines, ParseError, ParsedLine};
use bones_core::event::Event;
use bones_core::model::item::{Kind, Size, State, Urgency};
use bones_core::model::item_id::ItemId;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn item_id(raw: &str) -> ItemId {
    ItemId::new_unchecked(raw)
}

/// Build a base Event with no hash set (use write_event to fill it).
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

/// Write an Event to a TSJSON line (computing hash), then parse it back.
/// Asserts the round-trip produces an Event equal to the written event.
fn roundtrip(mut event: Event) -> Event {
    let line = write_event(&mut event).expect("write_event failed");
    let trimmed = line.trim_end_matches('\n');
    match parse_line(trimmed).expect("parse_line failed") {
        ParsedLine::Event(parsed) => {
            assert_eq!(*parsed, event, "round-trip produced different event");
            *parsed
        }
        other => panic!("expected ParsedLine::Event, got {other:?}"),
    }
}

fn create_data(title: &str) -> EventData {
    EventData::Create(CreateData {
        title: title.into(),
        kind: Kind::Task,
        size: None,
        urgency: Urgency::Default,
        labels: vec![],
        parent: None,
        causation: None,
        description: None,
        extra: BTreeMap::new(),
    })
}

fn comment_data(body: &str) -> EventData {
    EventData::Comment(CommentData {
        body: body.into(),
        extra: BTreeMap::new(),
    })
}

fn move_data(state: State) -> EventData {
    EventData::Move(MoveData {
        state,
        reason: None,
        extra: BTreeMap::new(),
    })
}

// ---------------------------------------------------------------------------
// Round-trip tests — one per event type (write_event → parse_line)
// ---------------------------------------------------------------------------

#[test]
fn roundtrip_item_create() {
    let event = base_event(
        EventType::Create,
        "bn-a7x",
        EventData::Create(CreateData {
            title: "Fix auth retry".into(),
            kind: Kind::Task,
            size: Some(Size::M),
            urgency: Urgency::Default,
            labels: vec!["backend".into(), "auth".into()],
            parent: None,
            causation: None,
            description: Some("Retry logic broken after token expiry".into()),
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Create(d) => {
            assert_eq!(d.title, "Fix auth retry");
            assert_eq!(d.kind, Kind::Task);
            assert_eq!(d.size, Some(Size::M));
            assert_eq!(d.labels, vec!["backend", "auth"]);
            assert_eq!(
                d.description.as_deref(),
                Some("Retry logic broken after token expiry")
            );
        }
        other => panic!("expected Create data, got {other:?}"),
    }
}

#[test]
fn roundtrip_item_update() {
    let event = base_event(
        EventType::Update,
        "bn-b8y",
        EventData::Update(UpdateData {
            field: "title".into(),
            value: serde_json::json!("New title after review"),
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Update(d) => {
            assert_eq!(d.field, "title");
            assert_eq!(d.value, serde_json::json!("New title after review"));
        }
        other => panic!("expected Update data, got {other:?}"),
    }
}

#[test]
fn roundtrip_item_move() {
    let event = base_event(
        EventType::Move,
        "bn-c9z",
        EventData::Move(MoveData {
            state: State::Doing,
            reason: Some("Starting implementation".into()),
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Move(d) => {
            assert_eq!(d.state, State::Doing);
            assert_eq!(d.reason.as_deref(), Some("Starting implementation"));
        }
        other => panic!("expected Move data, got {other:?}"),
    }
}

#[test]
fn roundtrip_item_assign() {
    let event = base_event(
        EventType::Assign,
        "bn-d1x",
        EventData::Assign(AssignData {
            agent: "gemini-flash".into(),
            action: AssignAction::Assign,
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Assign(d) => {
            assert_eq!(d.agent, "gemini-flash");
            assert_eq!(d.action, AssignAction::Assign);
        }
        other => panic!("expected Assign data, got {other:?}"),
    }
}

#[test]
fn roundtrip_item_comment() {
    let event = base_event(
        EventType::Comment,
        "bn-e2y",
        comment_data("Root cause found in token refresh logic."),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Comment(d) => {
            assert_eq!(d.body, "Root cause found in token refresh logic.");
        }
        other => panic!("expected Comment data, got {other:?}"),
    }
}

#[test]
fn roundtrip_item_link() {
    let event = base_event(
        EventType::Link,
        "bn-f3z",
        EventData::Link(LinkData {
            target: "bn-g4x".into(),
            link_type: "blocks".into(),
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Link(d) => {
            assert_eq!(d.target, "bn-g4x");
            assert_eq!(d.link_type, "blocks");
        }
        other => panic!("expected Link data, got {other:?}"),
    }
}

#[test]
fn roundtrip_item_unlink() {
    let event = base_event(
        EventType::Unlink,
        "bn-h5y",
        EventData::Unlink(UnlinkData {
            target: "bn-g4x".into(),
            link_type: Some("blocks".into()),
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Unlink(d) => {
            assert_eq!(d.target, "bn-g4x");
            assert_eq!(d.link_type.as_deref(), Some("blocks"));
        }
        other => panic!("expected Unlink data, got {other:?}"),
    }
}

#[test]
fn roundtrip_item_delete() {
    let event = base_event(
        EventType::Delete,
        "bn-i6z",
        EventData::Delete(DeleteData {
            reason: Some("Duplicate of bn-a7x".into()),
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Delete(d) => {
            assert_eq!(d.reason.as_deref(), Some("Duplicate of bn-a7x"));
        }
        other => panic!("expected Delete data, got {other:?}"),
    }
}

#[test]
fn roundtrip_item_compact() {
    let event = base_event(
        EventType::Compact,
        "bn-j7x",
        EventData::Compact(CompactData {
            summary: "Auth token refresh race fixed in session store.".into(),
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Compact(d) => {
            assert_eq!(d.summary, "Auth token refresh race fixed in session store.");
        }
        other => panic!("expected Compact data, got {other:?}"),
    }
}

#[test]
fn roundtrip_item_snapshot() {
    let state = serde_json::json!({
        "id": "bn-k8y",
        "title": "Full system scan",
        "kind": "task",
        "state": "done",
        "urgency": "default",
        "labels": ["infra"],
        "assignees": ["claude-abc"]
    });
    let event = base_event(
        EventType::Snapshot,
        "bn-k8y",
        EventData::Snapshot(SnapshotData {
            state: state.clone(),
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Snapshot(d) => {
            assert_eq!(d.state, state);
        }
        other => panic!("expected Snapshot data, got {other:?}"),
    }
}

#[test]
fn roundtrip_item_redact() {
    let event = base_event(
        EventType::Redact,
        "bn-l9z",
        EventData::Redact(RedactData {
            target_hash: "blake3:a1b2c3d4e5f6a1b2c3d4e5f6".into(),
            reason: "Accidental API key exposure".into(),
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Redact(d) => {
            assert_eq!(d.target_hash, "blake3:a1b2c3d4e5f6a1b2c3d4e5f6");
            assert_eq!(d.reason, "Accidental API key exposure");
        }
        other => panic!("expected Redact data, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Round-trip with parents field
// ---------------------------------------------------------------------------

#[test]
fn roundtrip_with_single_parent() {
    let parent_hash = "blake3:deadbeef01234567deadbeef01234567";
    let mut event = base_event(EventType::Comment, "bn-a7x", comment_data("Follow-up"));
    event.parents = vec![parent_hash.to_string()];

    let rt = roundtrip(event);
    assert_eq!(rt.parents, vec![parent_hash]);
}

#[test]
fn roundtrip_with_multiple_parents() {
    let p1 = "blake3:aaaa0000";
    let p2 = "blake3:bbbb1111";
    let mut event = base_event(EventType::Comment, "bn-a7x", comment_data("Merge"));
    event.parents = vec![p1.to_string(), p2.to_string()];

    let rt = roundtrip(event);
    assert_eq!(rt.parents, vec![p1, p2]);
}

// ---------------------------------------------------------------------------
// Unicode edge cases
// ---------------------------------------------------------------------------

#[test]
fn roundtrip_unicode_agent_name_cjk() {
    let mut event = base_event(EventType::Comment, "bn-a7x", comment_data("Unicode agent test"));
    event.agent = "gemini-日本語".into();
    let rt = roundtrip(event);
    assert_eq!(rt.agent, "gemini-日本語");
}

#[test]
fn roundtrip_unicode_agent_name_emoji() {
    let mut event = base_event(EventType::Comment, "bn-a7x", comment_data("Emoji agent"));
    event.agent = "agent-🦀".into();
    let rt = roundtrip(event);
    assert_eq!(rt.agent, "agent-🦀");
}

#[test]
fn roundtrip_emoji_in_comment_body() {
    let body = "🎉 Deployed to prod! 🚀 All systems go. ✅";
    let event = base_event(EventType::Comment, "bn-a7x", comment_data(body));
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Comment(d) => assert_eq!(d.body, body),
        other => panic!("expected Comment, got {other:?}"),
    }
}

#[test]
fn roundtrip_rtl_text_in_comment() {
    // Arabic right-to-left text
    let body = "هذا نص عربي لاختبار الاتجاه من اليمين إلى اليسار";
    let event = base_event(EventType::Comment, "bn-a7x", comment_data(body));
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Comment(d) => assert_eq!(d.body, body),
        other => panic!("expected Comment, got {other:?}"),
    }
}

#[test]
fn roundtrip_zero_width_chars_in_title() {
    // Zero-width joiner and zero-width non-joiner
    let title = "Fix\u{200B}auth\u{200C}retry\u{200D}logic";
    let event = base_event(EventType::Create, "bn-a7x", create_data(title));
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Create(d) => assert_eq!(d.title, title),
        other => panic!("expected Create, got {other:?}"),
    }
}

#[test]
fn roundtrip_unicode_title_mixed_scripts() {
    // Mixed Unicode scripts: Latin, CJK, Arabic, Cyrillic, Emoji
    let title = "Fix: 認証 retry / إعادة المحاولة / Повтор 🔐";
    let event = base_event(EventType::Create, "bn-a7x", create_data(title));
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Create(d) => assert_eq!(d.title, title),
        other => panic!("expected Create, got {other:?}"),
    }
}

#[test]
fn roundtrip_unicode_in_description() {
    let desc = "Description with 日本語, Ελληνικά, العربية, and emoji 🎯";
    let event = base_event(
        EventType::Create,
        "bn-a7x",
        EventData::Create(CreateData {
            title: "Unicode description test".into(),
            kind: Kind::Bug,
            size: None,
            urgency: Urgency::Urgent,
            labels: vec![],
            parent: None,
            causation: None,
            description: Some(desc.into()),
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Create(d) => {
            assert_eq!(d.description.as_deref(), Some(desc));
        }
        other => panic!("expected Create, got {other:?}"),
    }
}

#[test]
fn roundtrip_unicode_label() {
    let event = base_event(
        EventType::Create,
        "bn-a7x",
        EventData::Create(CreateData {
            title: "Unicode label test".into(),
            kind: Kind::Task,
            size: None,
            urgency: Urgency::Default,
            labels: vec!["긴급".into(), "バグ修正".into()],
            parent: None,
            causation: None,
            description: None,
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Create(d) => {
            assert_eq!(d.labels, vec!["긴급", "バグ修正"]);
        }
        other => panic!("expected Create, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Boundary value tests
// ---------------------------------------------------------------------------

#[test]
fn roundtrip_timestamp_zero() {
    let mut event = base_event(EventType::Comment, "bn-a7x", comment_data("Epoch zero"));
    event.wall_ts_us = 0;
    let rt = roundtrip(event);
    assert_eq!(rt.wall_ts_us, 0);
}

#[test]
fn roundtrip_timestamp_one_microsecond() {
    let mut event = base_event(EventType::Comment, "bn-a7x", comment_data("One microsecond"));
    event.wall_ts_us = 1;
    let rt = roundtrip(event);
    assert_eq!(rt.wall_ts_us, 1);
}

#[test]
fn roundtrip_timestamp_i64_max() {
    let mut event = base_event(EventType::Comment, "bn-a7x", comment_data("Max timestamp"));
    event.wall_ts_us = i64::MAX;
    let rt = roundtrip(event);
    assert_eq!(rt.wall_ts_us, i64::MAX);
}

#[test]
fn roundtrip_timestamp_negative() {
    // Negative timestamps may represent pre-epoch events or test data
    let mut event = base_event(EventType::Comment, "bn-a7x", comment_data("Pre-epoch"));
    event.wall_ts_us = -1_000_000;
    let rt = roundtrip(event);
    assert_eq!(rt.wall_ts_us, -1_000_000);
}

#[test]
fn roundtrip_max_length_title() {
    // 10,000 character title — verifies no truncation in parser/writer
    let title = "A".repeat(10_000);
    let event = base_event(EventType::Create, "bn-a7x", create_data(&title));
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Create(d) => {
            assert_eq!(d.title.len(), 10_000);
            assert_eq!(d.title, title);
        }
        other => panic!("expected Create, got {other:?}"),
    }
}

#[test]
fn roundtrip_empty_string_body() {
    // Empty comment body — should parse successfully (JSON allows empty strings)
    let event = base_event(EventType::Comment, "bn-a7x", comment_data(""));
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Comment(d) => assert_eq!(d.body, ""),
        other => panic!("expected Comment, got {other:?}"),
    }
}

#[test]
fn roundtrip_json_special_chars_in_body() {
    // JSON special characters that need escaping
    let body = r#"Error: "null" != null; backslash: \; newline encoded: \\n"#;
    let event = base_event(EventType::Comment, "bn-a7x", comment_data(body));
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Comment(d) => assert_eq!(d.body, body),
        other => panic!("expected Comment, got {other:?}"),
    }
}

#[test]
fn roundtrip_large_json_snapshot() {
    // Snapshot with a large, deeply nested state
    let state = serde_json::json!({
        "id": "bn-a7x",
        "title": "Large snapshot test",
        "kind": "task",
        "state": "done",
        "urgency": "default",
        "labels": ["l1","l2","l3","l4","l5","l6","l7","l8","l9","l10"],
        "assignees": ["agent-1","agent-2","agent-3"],
        "description": "A".repeat(1000),
        "metadata": {
            "created_at": 1_708_012_200_000_000i64,
            "updated_at": 1_708_012_300_000_000i64,
            "version": 42
        },
        "history_count": 100
    });
    let event = base_event(
        EventType::Snapshot,
        "bn-a7x",
        EventData::Snapshot(SnapshotData {
            state: state.clone(),
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Snapshot(d) => {
            assert_eq!(d.state["id"], serde_json::json!("bn-a7x"));
            assert_eq!(d.state["history_count"], serde_json::json!(100));
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[test]
fn roundtrip_update_with_array_value() {
    // Update event with an array value (labels field)
    let event = base_event(
        EventType::Update,
        "bn-a7x",
        EventData::Update(UpdateData {
            field: "labels".into(),
            value: serde_json::json!(["security", "urgent", "p0"]),
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Update(d) => {
            assert_eq!(d.field, "labels");
            assert_eq!(
                d.value,
                serde_json::json!(["security", "urgent", "p0"])
            );
        }
        other => panic!("expected Update, got {other:?}"),
    }
}

#[test]
fn roundtrip_unassign_action() {
    // AssignAction::Unassign — complementary to assign roundtrip test
    let event = base_event(
        EventType::Assign,
        "bn-a7x",
        EventData::Assign(AssignData {
            agent: "gemini-flash".into(),
            action: AssignAction::Unassign,
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Assign(d) => {
            assert_eq!(d.action, AssignAction::Unassign);
        }
        other => panic!("expected Assign, got {other:?}"),
    }
}

#[test]
fn roundtrip_delete_no_reason() {
    let event = base_event(
        EventType::Delete,
        "bn-a7x",
        EventData::Delete(DeleteData {
            reason: None,
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Delete(d) => assert!(d.reason.is_none()),
        other => panic!("expected Delete, got {other:?}"),
    }
}

#[test]
fn roundtrip_move_to_done() {
    let event = base_event(EventType::Move, "bn-a7x", move_data(State::Done));
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Move(d) => assert_eq!(d.state, State::Done),
        other => panic!("expected Move, got {other:?}"),
    }
}

#[test]
fn roundtrip_move_to_archived() {
    let event = base_event(EventType::Move, "bn-a7x", move_data(State::Archived));
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Move(d) => assert_eq!(d.state, State::Archived),
        other => panic!("expected Move, got {other:?}"),
    }
}

#[test]
fn roundtrip_create_all_kinds() {
    for kind in [Kind::Task, Kind::Goal, Kind::Bug] {
        let event = base_event(
            EventType::Create,
            "bn-a7x",
            EventData::Create(CreateData {
                title: format!("Test {kind}"),
                kind,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
        );
        let rt = roundtrip(event);
        match &rt.data {
            EventData::Create(d) => assert_eq!(d.kind, kind, "kind mismatch for {kind}"),
            other => panic!("expected Create, got {other:?}"),
        }
    }
}

#[test]
fn roundtrip_create_all_sizes() {
    for size in [Size::Xs, Size::S, Size::M, Size::L, Size::Xl, Size::Xxl] {
        let event = base_event(
            EventType::Create,
            "bn-a7x",
            EventData::Create(CreateData {
                title: format!("Size test {size}"),
                kind: Kind::Task,
                size: Some(size),
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
        );
        let rt = roundtrip(event);
        match &rt.data {
            EventData::Create(d) => assert_eq!(d.size, Some(size), "size mismatch for {size}"),
            other => panic!("expected Create, got {other:?}"),
        }
    }
}

#[test]
fn roundtrip_create_all_urgencies() {
    for urgency in [Urgency::Urgent, Urgency::Default, Urgency::Punt] {
        let event = base_event(
            EventType::Create,
            "bn-a7x",
            EventData::Create(CreateData {
                title: format!("Urgency test {urgency}"),
                kind: Kind::Task,
                size: None,
                urgency,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
        );
        let rt = roundtrip(event);
        match &rt.data {
            EventData::Create(d) => {
                assert_eq!(d.urgency, urgency, "urgency mismatch for {urgency}")
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }
}

#[test]
fn roundtrip_with_causation() {
    let event = base_event(
        EventType::Create,
        "bn-a7x",
        EventData::Create(CreateData {
            title: "Follow-up task".into(),
            kind: Kind::Task,
            size: None,
            urgency: Urgency::Default,
            labels: vec![],
            parent: Some("bn-root".into()),
            causation: Some("bn-parent".into()),
            description: None,
            extra: BTreeMap::new(),
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Create(d) => {
            assert_eq!(d.parent.as_deref(), Some("bn-root"));
            assert_eq!(d.causation.as_deref(), Some("bn-parent"));
        }
        other => panic!("expected Create, got {other:?}"),
    }
}

#[test]
fn roundtrip_extra_fields_preserved() {
    // Forward-compatibility: extra fields survive round-trip
    let mut extra = BTreeMap::new();
    extra.insert(
        "future_field".to_string(),
        serde_json::json!("some_future_value"),
    );
    extra.insert("priority_v2".to_string(), serde_json::json!(42));

    let event = base_event(
        EventType::Comment,
        "bn-a7x",
        EventData::Comment(CommentData {
            body: "Comment with extra fields".into(),
            extra,
        }),
    );
    let rt = roundtrip(event);
    match &rt.data {
        EventData::Comment(d) => {
            assert_eq!(
                d.extra.get("future_field"),
                Some(&serde_json::json!("some_future_value"))
            );
            assert_eq!(d.extra.get("priority_v2"), Some(&serde_json::json!(42)));
        }
        other => panic!("expected Comment, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Malformed input — each error variant with NO panics
// ---------------------------------------------------------------------------

#[test]
fn malformed_missing_tab() {
    // Remove one tab separator from an otherwise valid line
    // This gives wrong field count
    let err = parse_line("1000\tagent\titc:A\t\titem.create\tbn-a7x\t{\"title\":\"T\",\"kind\":\"task\"}")
        .expect_err("should fail with field count error");
    assert!(
        matches!(err, ParseError::FieldCount { .. }),
        "expected FieldCount, got {err:?}"
    );
}

#[test]
fn malformed_bad_json_payload() {
    // Valid fixed fields but invalid JSON in payload
    let line = "1000\tagent\titc:A\t\titem.comment\tbn-a7x\t{invalid json here}\tblake3:aaa";
    let err = parse_line(line).expect_err("should fail with JSON error");
    assert!(
        matches!(err, ParseError::InvalidDataJson(_)),
        "expected InvalidDataJson, got {err:?}"
    );
}

#[test]
fn malformed_truncated_line() {
    // A valid line cut at ~50%
    let full_line = "1708012200000000\ttest-agent\titc:AQ\t\titem.comment\tbn-a7x\t{\"body\":\"hello\"}\tblake3:abcdef";
    let truncated = &full_line[..full_line.len() / 2];
    // Should not panic, should return an error
    let result = parse_line(truncated);
    assert!(result.is_err(), "truncated line should fail");
}

#[test]
fn malformed_wrong_field_count_3() {
    let err = parse_line("a\tb\tc").expect_err("should fail");
    assert!(matches!(
        err,
        ParseError::FieldCount {
            found: 3,
            expected: 8
        }
    ));
}

#[test]
fn malformed_wrong_field_count_9() {
    let err = parse_line("1\t2\t3\t4\t5\t6\t7\t8\t9").expect_err("should fail");
    assert!(matches!(
        err,
        ParseError::FieldCount {
            found: 9,
            expected: 8
        }
    ));
}

#[test]
fn malformed_empty_line_no_panic() {
    // Empty line should return Blank (not an error, not a panic)
    let result = parse_line("").expect("should parse as Blank");
    assert_eq!(result, ParsedLine::Blank);
}

#[test]
fn malformed_whitespace_only_no_panic() {
    // Whitespace-only line should return Blank
    let result = parse_line("   \t  ").expect("should parse as Blank");
    assert_eq!(result, ParsedLine::Blank);
}

#[test]
fn malformed_schema_mismatch_missing_title() {
    // Valid JSON but missing required `title` for item.create
    let line = "1000\tagent\titc:A\t\titem.create\tbn-a7x\t{\"kind\":\"task\"}\tblake3:aaa";
    let err = parse_line(line).expect_err("should fail");
    assert!(
        matches!(err, ParseError::DataSchemaMismatch { .. }),
        "expected DataSchemaMismatch, got {err:?}"
    );
}

#[test]
fn malformed_invalid_event_type() {
    let line = "1000\tagent\titc:A\t\titem.unknown\tbn-a7x\t{}\tblake3:aaa";
    let err = parse_line(line).expect_err("should fail");
    assert!(
        matches!(err, ParseError::InvalidEventType(_)),
        "expected InvalidEventType, got {err:?}"
    );
}

#[test]
fn malformed_invalid_timestamp_string() {
    let line = "notanumber\tagent\titc:A\t\titem.comment\tbn-a7x\t{}\tblake3:aaa";
    let err = parse_line(line).expect_err("should fail");
    assert!(
        matches!(err, ParseError::InvalidTimestamp(_)),
        "expected InvalidTimestamp, got {err:?}"
    );
}

#[test]
fn malformed_hash_mismatch() {
    // Correct format but wrong hash value
    let line = "1000\tagent\titc:A\t\titem.comment\tbn-a7x\t{\"body\":\"hi\"}\tblake3:0000000000000000000000000000000000000000000000000000000000000000";
    let err = parse_line(line).expect_err("should fail");
    assert!(
        matches!(err, ParseError::HashMismatch { .. }),
        "expected HashMismatch, got {err:?}"
    );
}

#[test]
fn malformed_no_panic_on_garbage_inputs() {
    // None of these should panic
    let long_string = "a".repeat(100_000);
    let many_tabs = "\t".repeat(100);
    let adversarial_inputs: Vec<&str> = vec![
        "",
        "\t",
        "\t\t\t\t\t\t\t",
        "\t\t\t\t\t\t\t\t",
        "🎉🎉🎉",
        "\0\0\0",
        &long_string,
        "1\t2\t3\t4\t5\t6\t7\t8",
        "\n\n\n",
        "# comment with 🦀 emoji",
        &many_tabs,
        "null\tnull\tnull\tnull\tnull\tnull\tnull\tnull",
    ];

    for input in adversarial_inputs {
        let _ = parse_line(input); // Must not panic
    }
}

// ---------------------------------------------------------------------------
// parse_lines integration
// ---------------------------------------------------------------------------

#[test]
fn parse_lines_skips_comments_and_blanks() {
    // Generate two valid event lines
    let mut e1 = base_event(EventType::Comment, "bn-a7x", comment_data("First"));
    e1.wall_ts_us = 1_000;
    let mut e2 = base_event(EventType::Comment, "bn-a7x", comment_data("Second"));
    e2.wall_ts_us = 2_000;

    let line1 = write_event(&mut e1).expect("write e1");
    let line2 = write_event(&mut e2).expect("write e2");

    let input = format!(
        "# bones event log v1\n# fields: ...\n\n{line1}\n{line2}\n"
    );

    let events = parse_lines(&input).expect("should parse all");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].wall_ts_us, 1_000);
    assert_eq!(events[1].wall_ts_us, 2_000);
}

#[test]
fn parse_lines_empty_input_returns_empty() {
    let events = parse_lines("").expect("should parse");
    assert!(events.is_empty());
}

#[test]
fn parse_lines_comments_only_returns_empty() {
    let input = "# header\n# another comment\n# yet another\n";
    let events = parse_lines(input).expect("should parse");
    assert!(events.is_empty());
}

#[test]
fn parse_lines_error_includes_line_number() {
    let mut good = base_event(EventType::Comment, "bn-a7x", comment_data("Good"));
    good.wall_ts_us = 100;
    let good_line = write_event(&mut good).expect("write");

    // Line 1: header comment, Line 2: good event, Line 3: bad line
    let input = format!("# header\n{good_line}bad\n");
    let (line_num, _) = parse_lines(&input).expect_err("should fail");
    assert_eq!(line_num, 3, "error should be on line 3");
}

// ---------------------------------------------------------------------------
// compute_event_hash consistency
// ---------------------------------------------------------------------------

#[test]
fn compute_hash_consistent_with_parse_line() {
    // The hash computed by write_event must match what parse_line verifies
    let mut event = base_event(EventType::Create, "bn-a7x", create_data("Hash test"));
    let line = write_event(&mut event).expect("write");
    let trimmed = line.trim_end_matches('\n');

    // Parse must succeed (hash verified internally)
    parse_line(trimmed).expect("parse should succeed — hash must match");
}

#[test]
fn compute_hash_different_events_different_hashes() {
    let mut e1 = base_event(EventType::Comment, "bn-a7x", comment_data("Event 1"));
    e1.wall_ts_us = 1_000;
    let mut e2 = base_event(EventType::Comment, "bn-a7x", comment_data("Event 2"));
    e2.wall_ts_us = 2_000;

    let h1 = compute_event_hash(&e1).expect("hash 1");
    let h2 = compute_event_hash(&e2).expect("hash 2");
    assert_ne!(h1, h2, "different events should have different hashes");
}

// ---------------------------------------------------------------------------
// Golden-file tests: 20+ representative events
// ---------------------------------------------------------------------------

/// Build a corpus of 20+ diverse events covering all types and edge cases.
fn build_golden_corpus() -> Vec<Event> {
    let item_ids = ["bn-a7x", "bn-b8y", "bn-c9z", "bn-d1x", "bn-e2y"];

    let mut events = vec![
        // 1: item.create — simple task
        base_event(
            EventType::Create,
            item_ids[0],
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
        // 2: item.create — bug
        base_event(
            EventType::Create,
            item_ids[1],
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
        // 3: item.create — goal with XL size
        base_event(
            EventType::Create,
            item_ids[2],
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
        // 4: item.update — title change
        base_event(
            EventType::Update,
            item_ids[0],
            EventData::Update(UpdateData {
                field: "title".into(),
                value: serde_json::json!("Fix auth retry logic (v2)"),
                extra: BTreeMap::new(),
            }),
        ),
        // 5: item.update — labels array
        base_event(
            EventType::Update,
            item_ids[0],
            EventData::Update(UpdateData {
                field: "labels".into(),
                value: serde_json::json!(["backend", "auth", "security"]),
                extra: BTreeMap::new(),
            }),
        ),
        // 6: item.move — open → doing
        base_event(
            EventType::Move,
            item_ids[0],
            EventData::Move(MoveData {
                state: State::Doing,
                reason: Some("Starting implementation".into()),
                extra: BTreeMap::new(),
            }),
        ),
        // 7: item.move — doing → done
        base_event(
            EventType::Move,
            item_ids[0],
            EventData::Move(MoveData {
                state: State::Done,
                reason: Some("Shipped in commit 9f3a2b1".into()),
                extra: BTreeMap::new(),
            }),
        ),
        // 8: item.move — done → archived
        base_event(
            EventType::Move,
            item_ids[0],
            EventData::Move(MoveData {
                state: State::Archived,
                reason: None,
                extra: BTreeMap::new(),
            }),
        ),
        // 9: item.assign — assign agent
        base_event(
            EventType::Assign,
            item_ids[0],
            EventData::Assign(AssignData {
                agent: "claude-sonnet".into(),
                action: AssignAction::Assign,
                extra: BTreeMap::new(),
            }),
        ),
        // 10: item.assign — unassign agent
        base_event(
            EventType::Assign,
            item_ids[0],
            EventData::Assign(AssignData {
                agent: "claude-sonnet".into(),
                action: AssignAction::Unassign,
                extra: BTreeMap::new(),
            }),
        ),
        // 11: item.comment — simple
        base_event(
            EventType::Comment,
            item_ids[0],
            comment_data("Root cause: race in token expiry check."),
        ),
        // 12: item.comment — unicode body
        base_event(
            EventType::Comment,
            item_ids[1],
            comment_data("バグ発見: セッション初期化の競合状態 🐛"),
        ),
        // 13: item.comment — RTL text
        base_event(
            EventType::Comment,
            item_ids[1],
            comment_data("تم إيجاد السبب الجذري في منطق المصادقة"),
        ),
        // 14: item.link — blocks
        base_event(
            EventType::Link,
            item_ids[0],
            EventData::Link(LinkData {
                target: item_ids[1].into(),
                link_type: "blocks".into(),
                extra: BTreeMap::new(),
            }),
        ),
        // 15: item.link — related_to
        base_event(
            EventType::Link,
            item_ids[2],
            EventData::Link(LinkData {
                target: item_ids[0].into(),
                link_type: "related_to".into(),
                extra: BTreeMap::new(),
            }),
        ),
        // 16: item.unlink — with link_type
        base_event(
            EventType::Unlink,
            item_ids[0],
            EventData::Unlink(UnlinkData {
                target: item_ids[1].into(),
                link_type: Some("blocks".into()),
                extra: BTreeMap::new(),
            }),
        ),
        // 17: item.unlink — no link_type
        base_event(
            EventType::Unlink,
            item_ids[2],
            EventData::Unlink(UnlinkData {
                target: item_ids[0].into(),
                link_type: None,
                extra: BTreeMap::new(),
            }),
        ),
        // 18: item.delete — with reason
        base_event(
            EventType::Delete,
            item_ids[3],
            EventData::Delete(DeleteData {
                reason: Some("Duplicate of bn-a7x".into()),
                extra: BTreeMap::new(),
            }),
        ),
        // 19: item.delete — no reason
        base_event(
            EventType::Delete,
            item_ids[4],
            EventData::Delete(DeleteData {
                reason: None,
                extra: BTreeMap::new(),
            }),
        ),
        // 20: item.compact — memory decay
        base_event(
            EventType::Compact,
            item_ids[0],
            EventData::Compact(CompactData {
                summary: "Auth token refresh race condition fixed in commit 9f3a2b1. Root cause was missing mutex in session store. Fixed with read-write lock.".into(),
                extra: BTreeMap::new(),
            }),
        ),
        // 21: item.snapshot — full state
        base_event(
            EventType::Snapshot,
            item_ids[0],
            EventData::Snapshot(SnapshotData {
                state: serde_json::json!({
                    "id": item_ids[0],
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
        // 22: item.redact
        base_event(
            EventType::Redact,
            item_ids[0],
            EventData::Redact(RedactData {
                target_hash: "blake3:a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6".into(),
                reason: "Accidental credential exposure in comment".into(),
                extra: BTreeMap::new(),
            }),
        ),
        // 23: item.create — max-effort edge case: max-length title
        base_event(
            EventType::Create,
            item_ids[0],
            EventData::Create(CreateData {
                title: "X".repeat(1_000),
                kind: Kind::Task,
                size: Some(Size::Xxl),
                urgency: Urgency::Urgent,
                labels: (0..10).map(|i| format!("label-{i}")).collect(),
                parent: None,
                causation: None,
                description: Some("Y".repeat(1_000)),
                extra: BTreeMap::new(),
            }),
        ),
        // 24: item.comment — emoji-rich
        base_event(
            EventType::Comment,
            item_ids[0],
            comment_data("🚀 Deployed! ✅ Tests pass. 🎉 Congrats team! 🦀 Rust FTW."),
        ),
        // 25: item.create — unicode mixed scripts
        base_event(
            EventType::Create,
            item_ids[1],
            EventData::Create(CreateData {
                title: "修复 auth retry / إعادة المحاولة / Повтор 🔐".into(),
                kind: Kind::Bug,
                size: Some(Size::S),
                urgency: Urgency::Urgent,
                labels: vec!["internationalization".into()],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
        ),
    ];

    // Assign distinct timestamps to maintain ordering
    for (i, event) in events.iter_mut().enumerate() {
        event.wall_ts_us = 1_708_012_200_000_000 + (i as i64) * 1_000_000;
    }

    events
}

/// Path to the golden events fixture file.
fn golden_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("golden_events.tsjson")
}

/// Generate golden file content: TSJSON lines for all corpus events.
fn generate_golden_content(corpus: &mut Vec<Event>) -> String {
    let mut lines = vec![
        "# bones event log v1".to_string(),
        "# Golden events fixture — generated by tsjson_parse tests".to_string(),
        "# DO NOT EDIT MANUALLY — run tests to regenerate".to_string(),
        String::new(), // blank line after header
    ];

    for event in corpus.iter_mut() {
        let line = write_event(event).expect("write_event in golden generation");
        // write_event returns line with trailing newline; trim it for the vec
        lines.push(line.trim_end_matches('\n').to_string());
    }

    lines.join("\n") + "\n"
}

#[test]
fn golden_file_all_events_parse_correctly() {
    let mut corpus = build_golden_corpus();
    assert!(corpus.len() >= 20, "golden corpus must have 20+ events");

    let content = generate_golden_content(&mut corpus);

    // Write (or update) the golden fixture file for reference
    let path = golden_fixture_path();
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    std::fs::write(&path, &content).ok(); // best-effort; don't fail if read-only

    // Parse all lines from the generated content
    let events = parse_lines(&content).expect("all golden events should parse");

    // Verify event count matches corpus
    assert_eq!(
        events.len(),
        corpus.len(),
        "parsed count should match corpus size"
    );

    // Spot-check specific events
    assert_eq!(events[0].event_type, EventType::Create);
    assert_eq!(events[0].item_id.as_str(), "bn-a7x");
    match &events[0].data {
        EventData::Create(d) => assert_eq!(d.title, "Fix auth retry logic"),
        other => panic!("expected Create, got {other:?}"),
    }

    // Verify event 6 (index 5) is a Move to Doing
    assert_eq!(events[5].event_type, EventType::Move);
    match &events[5].data {
        EventData::Move(d) => assert_eq!(d.state, State::Doing),
        other => panic!("expected Move, got {other:?}"),
    }

    // Verify the last event is present
    let _last = events.last().expect("non-empty");
    // The last is item 25 (unicode create) since we have 25 items (indices 0..24)
    assert_eq!(events.len(), corpus.len());

    // Verify timestamps are distinct
    let timestamps: Vec<i64> = events.iter().map(|e| e.wall_ts_us).collect();
    let mut sorted = timestamps.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        timestamps.len(),
        "all events should have distinct timestamps"
    );

    // Verify hashes are present and start with blake3:
    for (i, event) in events.iter().enumerate() {
        assert!(
            event.event_hash.starts_with("blake3:"),
            "event {i} has invalid hash: {}",
            event.event_hash
        );
    }
}

#[test]
fn golden_file_if_exists_is_still_parseable() {
    let path = golden_fixture_path();
    if !path.exists() {
        // File not yet generated; skip silently
        return;
    }

    let content = std::fs::read_to_string(&path).expect("read golden file");
    let events = parse_lines(&content)
        .expect("golden file should parse — if this fails, the format may have changed");

    assert!(
        !events.is_empty(),
        "golden file should contain at least one event"
    );

    // Every hash must be valid blake3
    for (i, event) in events.iter().enumerate() {
        assert!(
            event.event_hash.starts_with("blake3:"),
            "golden event {i} has invalid hash"
        );
    }
}

// ---------------------------------------------------------------------------
// Partial parse integration tests
// ---------------------------------------------------------------------------

#[test]
fn partial_parse_roundtrip_preserves_fixed_fields() {
    use bones_core::event::{parse_line_partial, PartialParsedLine};

    let mut event = base_event(EventType::Create, "bn-a7x", create_data("Partial parse test"));
    event.wall_ts_us = 9_999_999;
    event.agent = "my-agent".into();
    event.itc = "itc:ZZZ".into();

    let line = write_event(&mut event).expect("write");
    let trimmed = line.trim_end_matches('\n');

    match parse_line_partial(trimmed).expect("partial parse") {
        PartialParsedLine::Event(pe) => {
            assert_eq!(pe.wall_ts_us, 9_999_999);
            assert_eq!(pe.agent, "my-agent");
            assert_eq!(pe.itc, "itc:ZZZ");
            assert_eq!(pe.event_type, EventType::Create);
            assert_eq!(pe.item_id_raw, "bn-a7x");
        }
        other => panic!("expected Event, got {other:?}"),
    }
}

#[test]
fn partial_parse_all_11_event_types() {
    use bones_core::event::{parse_line_partial, PartialParsedLine};

    let all_events = vec![
        base_event(EventType::Create, "bn-a7x", create_data("T")),
        base_event(
            EventType::Update,
            "bn-a7x",
            EventData::Update(UpdateData {
                field: "title".into(),
                value: serde_json::json!("New"),
                extra: BTreeMap::new(),
            }),
        ),
        base_event(EventType::Move, "bn-a7x", move_data(State::Doing)),
        base_event(
            EventType::Assign,
            "bn-a7x",
            EventData::Assign(AssignData {
                agent: "a".into(),
                action: AssignAction::Assign,
                extra: BTreeMap::new(),
            }),
        ),
        base_event(EventType::Comment, "bn-a7x", comment_data("Hi")),
        base_event(
            EventType::Link,
            "bn-a7x",
            EventData::Link(LinkData {
                target: "bn-b8y".into(),
                link_type: "blocks".into(),
                extra: BTreeMap::new(),
            }),
        ),
        base_event(
            EventType::Unlink,
            "bn-a7x",
            EventData::Unlink(UnlinkData {
                target: "bn-b8y".into(),
                link_type: None,
                extra: BTreeMap::new(),
            }),
        ),
        base_event(
            EventType::Delete,
            "bn-a7x",
            EventData::Delete(DeleteData {
                reason: None,
                extra: BTreeMap::new(),
            }),
        ),
        base_event(
            EventType::Compact,
            "bn-a7x",
            EventData::Compact(CompactData {
                summary: "S".into(),
                extra: BTreeMap::new(),
            }),
        ),
        base_event(
            EventType::Snapshot,
            "bn-a7x",
            EventData::Snapshot(SnapshotData {
                state: serde_json::json!({}),
                extra: BTreeMap::new(),
            }),
        ),
        base_event(
            EventType::Redact,
            "bn-a7x",
            EventData::Redact(RedactData {
                target_hash: "blake3:abc".into(),
                reason: "test".into(),
                extra: BTreeMap::new(),
            }),
        ),
    ];

    assert_eq!(all_events.len(), 11);

    for mut event in all_events {
        let expected_type = event.event_type;
        let line = write_event(&mut event).expect("write");
        let trimmed = line.trim_end_matches('\n');

        match parse_line_partial(trimmed).expect("partial parse") {
            PartialParsedLine::Event(pe) => {
                assert_eq!(
                    pe.event_type, expected_type,
                    "event type mismatch for {expected_type}"
                );
                assert_eq!(pe.item_id_raw, "bn-a7x");
            }
            other => panic!("expected Event for {expected_type}, got {other:?}"),
        }
    }
}
