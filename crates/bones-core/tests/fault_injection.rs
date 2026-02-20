//! Fault-injection integration tests for crash-recovery paths.
//!
//! These tests exercise realistic failure modes at the project level:
//! - process crash mid-append (torn TSJSON line)
//! - deterministic shard corruption with stale projection rebuild
//! - missing projection DB auto-rebuild on startup
//! - write-path permission failures surfacing clear errors

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use bones_core::db;
use bones_core::db::query::{self, ItemFilter};
use bones_core::event::Event;
use bones_core::event::data::{CreateData, EventData};
use bones_core::event::parser;
use bones_core::event::types::EventType;
use bones_core::event::writer;
use bones_core::model::item::{Kind, Size, Urgency};
use bones_core::model::item_id::ItemId;
use bones_core::recovery::{
    RecoveryAction, auto_recover, recover_corrupt_shard, recover_missing_db, recover_partial_write,
};
use bones_core::shard::{ShardError, ShardManager};
use tempfile::TempDir;

const BASE_TS_US: i64 = 1_708_012_200_000_000;
const CORRUPTION_SEED: u64 = 0x5EED_F00D;

fn item_id_for(seq: usize) -> String {
    if seq == 0 {
        "bn-a7x".to_string()
    } else {
        format!("bn-a7x.{seq}")
    }
}

fn make_create_event(seq: usize) -> Event {
    let mut event = Event {
        wall_ts_us: BASE_TS_US + seq as i64,
        agent: "fault-bot".to_string(),
        itc: format!("itc:AQ.{seq}"),
        parents: Vec::new(),
        event_type: EventType::Create,
        item_id: ItemId::new_unchecked(item_id_for(seq)),
        data: EventData::Create(CreateData {
            title: format!("Fault fixture item {seq}"),
            kind: Kind::Task,
            size: Some(Size::M),
            urgency: Urgency::Default,
            labels: vec!["fault-fixture".to_string()],
            parent: None,
            causation: None,
            description: Some("deterministic recovery fixture".to_string()),
            extra: BTreeMap::new(),
        }),
        event_hash: String::new(),
    };

    writer::write_event(&mut event).expect("compute event hash");
    event
}

fn setup_project_with_events(event_count: usize) -> (TempDir, ShardManager, PathBuf) {
    let temp_dir = TempDir::new().expect("create temp dir");
    let bones_dir = temp_dir.path().join(".bones");

    let shard_mgr = ShardManager::new(&bones_dir);
    shard_mgr.ensure_dirs().expect("create bones dirs");
    shard_mgr.init().expect("init shard manager");

    let (year, month) = shard_mgr
        .active_shard()
        .expect("resolve active shard")
        .expect("active shard exists after init");

    for seq in 0..event_count {
        let line = writer::write_line(&make_create_event(seq)).expect("serialize event line");
        shard_mgr
            .append_raw(year, month, &line)
            .expect("append fixture event");
    }

    let shard_path = shard_mgr.shard_path(year, month);
    (temp_dir, shard_mgr, shard_path)
}

fn shard_diagnostics(path: &Path) -> String {
    let shard = fs::read_to_string(path)
        .unwrap_or_else(|err| format!("<failed to read {}: {err}>", path.display()));
    let backup_path = path.with_extension("corrupt");
    let backup = fs::read_to_string(&backup_path)
        .unwrap_or_else(|err| format!("<failed to read {}: {err}>", backup_path.display()));

    format!(
        "shard={}\n--- shard ---\n{shard}\n--- backup ({}) ---\n{backup}",
        path.display(),
        backup_path.display(),
    )
}

/// Corrupt one event hash deterministically and return the expected corruption
/// offset used by `recover_corrupt_shard`.
fn corrupt_nth_event_hash(path: &Path, nth_event: usize, seed: u64) -> u64 {
    let original = fs::read_to_string(path).expect("read shard before corruption");
    let mut lines: Vec<String> = original.lines().map(ToOwned::to_owned).collect();

    let event_line_indexes: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                None
            } else {
                Some(idx)
            }
        })
        .collect();

    let target = event_line_indexes[nth_event];
    let mut fields: Vec<String> = lines[target].split('\t').map(ToOwned::to_owned).collect();
    assert_eq!(
        fields.len(),
        8,
        "fixture line must be valid TSJSON before fault"
    );

    let mut hash = fields[7].as_bytes().to_vec();
    let hash_prefix_len = b"blake3:".len();
    assert!(
        hash.len() > hash_prefix_len,
        "event hash should have blake3 payload"
    );
    let payload_len = hash.len() - hash_prefix_len;
    let target_pos = hash_prefix_len + (seed as usize % payload_len);
    hash[target_pos] = if hash[target_pos] == b'0' { b'1' } else { b'0' };
    fields[7] = String::from_utf8(hash).expect("hash stays UTF-8");

    lines[target] = fields.join("\t");
    let rewritten = format!("{}\n", lines.join("\n"));
    fs::write(path, &rewritten).expect("persist corrupted shard");

    rewritten
        .lines()
        .take(target)
        .map(|line| line.len() as u64 + 1)
        .sum()
}

#[test]
fn truncated_event_line_recovery() {
    let (_temp_dir, _shard_mgr, shard_path) = setup_project_with_events(5);

    // Inject a deterministic torn write: partial TSJSON line with no newline.
    let torn_line = writer::write_line(&make_create_event(99)).expect("serialize torn fixture");
    let truncated = &torn_line[..torn_line.len() / 2];
    let mut file = OpenOptions::new()
        .append(true)
        .open(&shard_path)
        .expect("open shard for fault injection");
    file.write_all(truncated.as_bytes())
        .expect("append partial line");
    file.flush().expect("flush partial line");

    let removed = recover_partial_write(&shard_path).expect("recover torn write");
    assert!(
        removed > 0,
        "expected truncated bytes > 0 after torn-write recovery\n{}",
        shard_diagnostics(&shard_path)
    );

    let repaired = fs::read_to_string(&shard_path).expect("read repaired shard");
    let parsed = parser::parse_lines(&repaired).unwrap_or_else(|(line, err)| {
        panic!(
            "repaired shard should parse (failed at line {line}: {err})\n{}",
            shard_diagnostics(&shard_path)
        )
    });

    assert_eq!(
        parsed.len(),
        5,
        "recovery must preserve the 5 valid events exactly\n{}",
        shard_diagnostics(&shard_path)
    );
    assert!(
        repaired.ends_with('\n'),
        "repaired shard must end at a full-line boundary\n{}",
        shard_diagnostics(&shard_path)
    );
}

#[test]
fn corrupt_shard_and_stale_projection_recover_deterministically() {
    let (temp_dir, _shard_mgr, shard_path) = setup_project_with_events(3);
    let bones_dir = temp_dir.path().join(".bones");
    let events_dir = bones_dir.join("events");
    let db_path = bones_dir.join("bones.db");

    db::rebuild::rebuild(&events_dir, &db_path).expect("build initial projection");
    let conn = db::open_projection(&db_path).expect("open initial projection");
    let before = query::count_items(&conn, &ItemFilter::default()).expect("count pre-fault items");
    assert_eq!(
        before, 3,
        "fixture sanity: projection should include 3 items"
    );

    let expected_offset = corrupt_nth_event_hash(&shard_path, 1, CORRUPTION_SEED);
    let report = recover_corrupt_shard(&shard_path).expect("recover corrupt shard");

    assert_eq!(
        report.events_preserved,
        1,
        "corrupt line should truncate replayable prefix to 1 event\n{}",
        shard_diagnostics(&shard_path)
    );
    assert_eq!(
        report.events_discarded,
        2,
        "corrupt line and trailing events should be quarantined\n{}",
        shard_diagnostics(&shard_path)
    );
    assert_eq!(
        report.corruption_offset,
        Some(expected_offset),
        "corruption offset should be deterministic for fixed fixture + seed\n{}",
        shard_diagnostics(&shard_path)
    );

    match &report.action_taken {
        RecoveryAction::Quarantined { backup_path } => {
            assert!(
                backup_path.exists(),
                "corrupt tail should be quarantined to a backup file\n{}",
                shard_diagnostics(&shard_path)
            );
        }
        other => panic!("expected quarantined recovery action, got {other:?}"),
    }

    let rebuild_report = recover_missing_db(&events_dir, &db_path)
        .expect("rebuild stale projection against repaired shard");
    assert_eq!(
        rebuild_report.events_preserved, 1,
        "projection rebuild should ingest only preserved events"
    );
    match rebuild_report.action_taken {
        RecoveryAction::Quarantined { .. } => {}
        other => panic!("expected corrupt/stale DB to be quarantined, got {other:?}"),
    }

    let conn = db::open_projection(&db_path).expect("open rebuilt projection");
    let after = query::count_items(&conn, &ItemFilter::default()).expect("count rebuilt items");
    assert_eq!(
        after, 1,
        "rebuilt projection should deterministically match repaired shard"
    );
}

#[test]
fn missing_db_triggers_auto_rebuild() {
    let (temp_dir, _shard_mgr, _shard_path) = setup_project_with_events(2);
    let bones_dir = temp_dir.path().join(".bones");
    let events_dir = bones_dir.join("events");
    let db_path = bones_dir.join("bones.db");

    db::rebuild::rebuild(&events_dir, &db_path).expect("create initial projection");
    assert!(
        db_path.exists(),
        "fixture sanity: projection DB should exist"
    );
    fs::remove_file(&db_path).expect("delete projection DB to simulate crash/loss");

    let health = auto_recover(&bones_dir).expect("auto-recover missing projection DB");
    assert!(
        health.project_valid,
        "fixture should be recognized as a bones project"
    );
    assert!(
        health.db_rebuilt,
        "auto_recover should rebuild missing DB on startup"
    );
    assert!(
        db_path.exists(),
        "startup recovery should recreate bones.db"
    );

    let conn = db::open_projection(&db_path).expect("open rebuilt projection");
    let item = query::get_item(&conn, "bn-a7x", false)
        .expect("query rebuilt item")
        .expect("root fixture item must exist after rebuild");
    assert_eq!(item.title, "Fault fixture item 0");
}

#[test]
fn write_failure_surfaces_clear_permission_error() {
    let (_temp_dir, shard_mgr, shard_path) = setup_project_with_events(1);

    let original_perms = fs::metadata(&shard_path)
        .expect("read shard metadata")
        .permissions();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut readonly = original_perms.clone();
        readonly.set_mode(0o444);
        fs::set_permissions(&shard_path, readonly).expect("make shard read-only");
    }

    #[cfg(not(unix))]
    {
        let mut readonly = original_perms.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&shard_path, readonly).expect("make shard read-only");
    }

    let (year, month) = shard_mgr
        .active_shard()
        .expect("resolve active shard")
        .expect("active shard exists");

    let append_result = shard_mgr.append_raw(year, month, "injected failure\n");

    // Restore permissions so tempdir cleanup remains reliable across platforms.
    fs::set_permissions(&shard_path, original_perms).expect("restore shard permissions");

    let err = append_result.expect_err("append should fail when shard is read-only");
    match &err {
        ShardError::Io(io_err) => {
            assert_eq!(
                io_err.kind(),
                std::io::ErrorKind::PermissionDenied,
                "write failure should surface PermissionDenied, got {io_err:?}"
            );
        }
        other => panic!("expected ShardError::Io, got {other:?}"),
    }

    let rendered = err.to_string();
    assert!(
        !rendered.trim().is_empty(),
        "write-path failure should include a non-empty diagnostic message"
    );
}
