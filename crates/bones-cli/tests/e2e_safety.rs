//! E2E coverage for high-risk lifecycle and maintenance commands:
//! `bn archive`, `bn undo`, `bn admin diagnose`, and `bn admin doctor`.

use assert_cmd::Command;
use bones_core::event::parser::{ParsedLine, parse_line};
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn bn_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("bn"));
    cmd.current_dir(dir);
    cmd.env("AGENT", "test-agent");
    cmd.env("BONES_LOG", "error");
    cmd
}

fn init_project(dir: &Path) {
    bn_cmd(dir).args(["init"]).assert().success();
}

fn create_item(dir: &Path, title: &str) -> String {
    let output = bn_cmd(dir)
        .args(["create", "--title", title, "--json"])
        .output()
        .expect("create should not crash");
    assert!(
        output.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    json["id"].as_str().expect("id must exist").to_string()
}

fn done_item(dir: &Path, id: &str) {
    bn_cmd(dir).args(["done", id]).assert().success();
}

fn do_item(dir: &Path, id: &str) {
    bn_cmd(dir).args(["do", id]).assert().success();
}

fn rebuild(dir: &Path) {
    bn_cmd(dir).args(["admin", "rebuild"]).assert().success();
}

fn get_item_state(dir: &Path, id: &str) -> String {
    let output = bn_cmd(dir)
        .args(["show", id, "--json"])
        .output()
        .expect("show should not crash");
    assert!(
        output.status.success(),
        "show failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    json["state"].as_str().unwrap_or("").to_string()
}

fn event_shards(root: &Path) -> Vec<std::path::PathBuf> {
    fn visit(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some("events") {
                    out.push(path);
                }
            }
        }
    }

    let mut shards = Vec::new();
    visit(&root.join(".bones/events"), &mut shards);
    shards.sort();
    shards
}

fn latest_event_hash_for_item_type(root: &Path, item_id: &str, event_type: &str) -> Option<String> {
    let mut latest = None;
    for shard in event_shards(root) {
        let content = fs::read_to_string(shard).ok()?;
        for line in content.lines() {
            let Ok(parsed) = parse_line(line) else {
                continue;
            };
            if let ParsedLine::Event(event) = parsed
                && event.item_id.as_str() == item_id
                && event.event_type.as_str() == event_type
            {
                latest = Some(event.event_hash.clone());
            }
        }
    }
    latest
}

#[test]
fn archive_done_item_and_reopen_from_archived() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Archive me");
    done_item(dir.path(), &id);

    let archive_output = bn_cmd(dir.path())
        .args(["archive", &id, "--json"])
        .output()
        .unwrap();
    assert!(
        archive_output.status.success(),
        "archive failed: {}",
        String::from_utf8_lossy(&archive_output.stderr)
    );

    let archive_json: Value = serde_json::from_slice(&archive_output.stdout).unwrap();
    assert_eq!(archive_json["id"].as_str().unwrap(), id);
    assert_eq!(archive_json["new_state"].as_str().unwrap(), "archived");
    assert!(archive_json["event_hash"].as_str().is_some());
    assert_eq!(get_item_state(dir.path(), &id), "archived");

    bn_cmd(dir.path()).args(["reopen", &id]).assert().success();
    assert_eq!(get_item_state(dir.path(), &id), "open");
}

#[test]
fn archive_open_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Still open");
    let output = bn_cmd(dir.path()).args(["archive", &id]).output().unwrap();

    assert!(
        !output.status.success(),
        "archive should fail for open item"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot transition") || stderr.contains("only valid for done"),
        "stderr should explain invalid transition; got: {stderr}"
    );
}

#[test]
fn archive_auto_days_zero_archives_done_items() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let done_a = create_item(dir.path(), "done-a");
    let done_b = create_item(dir.path(), "done-b");
    let open_id = create_item(dir.path(), "still-open");
    done_item(dir.path(), &done_a);
    done_item(dir.path(), &done_b);

    let output = bn_cmd(dir.path())
        .args(["archive", "--auto", "--days", "0", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "archive --auto failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["days"].as_u64().unwrap(), 0);
    assert!(json["archived_count"].as_u64().unwrap() >= 2);
    let archived_ids = json["archived_ids"].as_array().unwrap();
    assert!(
        archived_ids
            .iter()
            .any(|v| v.as_str() == Some(done_a.as_str()))
    );
    assert!(
        archived_ids
            .iter()
            .any(|v| v.as_str() == Some(done_b.as_str()))
    );

    assert_eq!(get_item_state(dir.path(), &done_a), "archived");
    assert_eq!(get_item_state(dir.path(), &done_b), "archived");
    assert_eq!(get_item_state(dir.path(), &open_id), "open");
}

#[test]
fn undo_last_two_events_restore_previous_state() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Undo state test");
    do_item(dir.path(), &id);
    assert_eq!(get_item_state(dir.path(), &id), "doing");

    let output = bn_cmd(dir.path())
        .args(["undo", &id, "--last", "2", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "undo failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["item_id"].as_str().unwrap(), id);
    assert_eq!(json["dry_run"].as_bool().unwrap(), false);
    let results = json["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    assert!(
        results
            .iter()
            .all(|r| !r["skipped"].as_bool().unwrap_or(true)),
        "none of the undone events should be skipped"
    );
    assert!(
        results
            .iter()
            .any(|r| r["compensating_type"] == "item.move"),
        "expected one compensating move event"
    );

    // Undo emits a compensating event; rebuild ensures projection reflects it.
    rebuild(dir.path());
    assert_eq!(get_item_state(dir.path(), &id), "open");
}

#[test]
fn undo_dry_run_does_not_change_state() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Undo dry run");
    do_item(dir.path(), &id);

    let output = bn_cmd(dir.path())
        .args(["undo", &id, "--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "undo --dry-run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["dry_run"].as_bool().unwrap(), true);
    let results = json["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["dry_run"].as_bool().unwrap(), true);
    assert!(results[0]["compensating_hash"].as_str().is_some());

    assert_eq!(get_item_state(dir.path(), &id), "doing");
}

#[test]
fn undo_specific_event_hash_restores_state() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Undo by hash");
    do_item(dir.path(), &id);
    assert_eq!(get_item_state(dir.path(), &id), "doing");

    let move_hash = latest_event_hash_for_item_type(dir.path(), &id, "item.move")
        .expect("expected to find move event hash in shard");

    let output = bn_cmd(dir.path())
        .args(["undo", "--event", &move_hash, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "undo --event failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let results = json["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["original_hash"].as_str().unwrap(), move_hash);
    assert_eq!(results[0]["skipped"].as_bool().unwrap(), false);

    rebuild(dir.path());
    assert_eq!(get_item_state(dir.path(), &id), "open");
}

#[test]
fn undo_write_failure_returns_nonzero_status() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Undo lock failure");
    do_item(dir.path(), &id);

    let lock_path = dir.path().join(".bones/lock");
    fs::remove_file(&lock_path).expect("remove lock file");
    fs::create_dir(&lock_path).expect("replace lock file with directory");

    let output = bn_cmd(dir.path())
        .args(["undo", &id, "--json"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "undo should fail when it cannot acquire the shard lock"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to acquire lock") || stderr.contains("Is a directory"),
        "stderr should report the write-path failure; got: {stderr}"
    );
}

#[test]
fn diagnose_json_schema_is_stable() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Diagnose schema");
    do_item(dir.path(), &id);

    let output = bn_cmd(dir.path())
        .args(["admin", "diagnose", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "diagnose failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json["generated_at_us"].is_number());
    assert!(json["shard_inventory"].is_object());
    assert!(json["event_stats"].is_object());
    assert!(json["integrity"].is_object());
    assert!(json["projection"].is_object());
    assert!(json["remediation_hints"].is_array());
    assert!(json["projection"]["status"].is_string());
}

#[test]
fn doctor_json_schema_is_stable_for_healthy_repo() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    create_item(dir.path(), "Doctor schema");

    let output = bn_cmd(dir.path())
        .args(["admin", "doctor", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "doctor failed unexpectedly: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["ok"].as_bool().unwrap(), true);
    assert!(json["sections"].is_array());
    assert!(json["fixes_applied"].is_array());

    let sections = json["sections"].as_array().unwrap();
    assert!(!sections.is_empty(), "doctor should report sections");
    for section in sections {
        assert!(section["name"].is_string());
        assert!(section["status"].is_string());
        assert!(section["details"].is_array());
    }
}

#[test]
fn doctor_fix_removes_stale_current_events() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    create_item(dir.path(), "Doctor fix");

    let stale_path = dir.path().join(".bones/events/current.events");
    fs::write(&stale_path, "stale").unwrap();
    assert!(stale_path.exists());

    let unhealthy = bn_cmd(dir.path())
        .args(["admin", "doctor", "--json"])
        .output()
        .unwrap();
    assert!(
        unhealthy.status.success(),
        "doctor should succeed with WARN-only sections"
    );
    let unhealthy_json: Value = serde_json::from_slice(&unhealthy.stdout).unwrap();
    let stale_section = unhealthy_json["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "stale_symlink")
        .expect("stale_symlink section should be present");
    assert_eq!(stale_section["status"].as_str().unwrap(), "WARN");

    let fixed = bn_cmd(dir.path())
        .args(["admin", "doctor", "--fix", "--json"])
        .output()
        .unwrap();
    assert!(
        fixed.status.success(),
        "doctor --fix failed: {}",
        String::from_utf8_lossy(&fixed.stderr)
    );
    let fixed_json: Value = serde_json::from_slice(&fixed.stdout).unwrap();
    assert_eq!(fixed_json["ok"].as_bool().unwrap(), true);
    let fixes = fixed_json["fixes_applied"].as_array().unwrap();
    assert!(
        fixes
            .iter()
            .filter_map(|v| v.as_str())
            .any(|s| s.contains("current.events")),
        "expected stale current.events fix in fixes_applied"
    );
    assert!(
        !stale_path.exists(),
        "--fix should remove stale current.events"
    );
}

#[test]
fn doctor_reports_projection_drift_as_fail() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    create_item(dir.path(), "Drift me");

    let db_path = dir.path().join(".bones/bones.db");
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE projection_meta SET last_event_offset = 0 WHERE id = 1",
        [],
    )
    .unwrap();

    let output = bn_cmd(dir.path())
        .args(["admin", "doctor", "--json"])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "doctor should fail on projection drift"
    );

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["ok"].as_bool().unwrap(), false);
    let drift = json["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "projection_drift")
        .expect("projection_drift section should be present");
    assert_eq!(drift["status"].as_str().unwrap(), "FAIL");
}

#[test]
fn doctor_reports_projection_hash_drift_as_fail() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    create_item(dir.path(), "Hash drift me");

    let db_path = dir.path().join(".bones/bones.db");
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE projection_meta SET last_event_hash = 'blake3:not-the-log-tail' WHERE id = 1",
        [],
    )
    .unwrap();

    let output = bn_cmd(dir.path())
        .args(["admin", "doctor", "--json"])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "doctor should fail on projection hash drift"
    );

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["ok"].as_bool().unwrap(), false);
    let drift = json["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "projection_drift")
        .expect("projection_drift section should be present");
    assert_eq!(drift["status"].as_str().unwrap(), "FAIL");
    let details = drift["details"].as_array().unwrap();
    assert!(
        details
            .iter()
            .any(|detail| detail.as_str() == Some("cursor_hash_match=false")),
        "doctor details should report cursor_hash_match=false"
    );
}

#[test]
fn unstart_reverts_doing_to_open() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Abandoned mid-flight");
    do_item(dir.path(), &id);
    assert_eq!(get_item_state(dir.path(), &id), "doing");

    let output = bn_cmd(dir.path())
        .args(["unstart", &id, "--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "unstart failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(get_item_state(dir.path(), &id), "open");

    // Round-trip should work — agent can pick the bone back up.
    do_item(dir.path(), &id);
    assert_eq!(get_item_state(dir.path(), &id), "doing");
}

#[test]
fn unstart_rejects_done_with_reopen_hint() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Already done");
    done_item(dir.path(), &id);

    let output = bn_cmd(dir.path()).args(["unstart", &id]).output().unwrap();
    assert!(!output.status.success(), "unstart should fail on done item");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bn reopen"),
        "stderr should hint at bn reopen; got: {stderr}"
    );
}

#[test]
fn unstart_rejects_already_open() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Still open");
    let output = bn_cmd(dir.path()).args(["unstart", &id]).output().unwrap();

    assert!(!output.status.success(), "unstart should fail on open item");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already open"),
        "stderr should explain already-open state; got: {stderr}"
    );
}
