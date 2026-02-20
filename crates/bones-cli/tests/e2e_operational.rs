//! E2E tests for operational commands: search, dup, verify, history/log.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
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

fn rebuild(dir: &Path) {
    bn_cmd(dir).args(["rebuild"]).assert().success();
}

fn first_event_shard(root: &Path) -> PathBuf {
    fn visit(dir: &Path) -> Option<PathBuf> {
        for entry in fs::read_dir(dir).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = visit(&path) {
                    return Some(found);
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("events") {
                return Some(path);
            }
        }
        None
    }

    visit(&root.join(".bones/events")).expect("no .events file found")
}

#[test]
fn search_json_returns_expected_hits() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    create_item(dir.path(), "Fix authentication timeout in login");
    create_item(dir.path(), "Authentication fails after 30 seconds");
    create_item(dir.path(), "Dark mode theme");
    rebuild(dir.path());

    let output = bn_cmd(dir.path())
        .args(["search", "authentication", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).expect("search --json must parse");
    assert_eq!(json["query"], "authentication");
    assert!(json["count"].as_u64().unwrap_or(0) >= 2);
    assert!(json["results"].is_array());
}

#[test]
fn dup_json_returns_candidates_for_similar_items() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let source = create_item(dir.path(), "Authentication timeout regression");
    create_item(
        dir.path(),
        "Authentication timeout regression in API gateway",
    );
    rebuild(dir.path());

    let output = bn_cmd(dir.path())
        .args(["dup", &source, "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).expect("dup --json must parse");
    assert_eq!(json["source_id"], source);
    assert!(json["candidates"].is_array());
}

#[test]
fn history_json_schema_is_stable() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Tracked item");
    bn_cmd(dir.path()).args(["do", &id]).assert().success();

    let output = bn_cmd(dir.path())
        .args(["history", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).expect("history --json must parse");
    let rows = json.as_array().expect("history output should be array");
    assert!(!rows.is_empty());
    let row = &rows[0];
    assert!(row["item_id"].is_string());
    assert!(row["event_type"].is_string());
    assert!(row["timestamp_us"].is_number());
}

#[test]
fn verify_json_schema_is_stable() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    create_item(dir.path(), "Verify me");

    let output = bn_cmd(dir.path())
        .args(["verify", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).expect("verify --json must parse");
    assert!(json["ok"].is_boolean());
    assert!(json["active_shard_parse_ok"].is_boolean());
    assert!(json["shards"].is_array());
}

#[test]
fn verify_succeeds_with_missing_projection_db() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    create_item(dir.path(), "Projection stale test");

    let db = dir.path().join(".bones/bones.db");
    if db.exists() {
        fs::remove_file(&db).unwrap();
    }

    bn_cmd(dir.path())
        .args(["admin", "verify"])
        .assert()
        .success();
}

#[test]
fn history_fails_on_corrupted_shard_with_actionable_error() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    create_item(dir.path(), "Corruption target");

    let shard = first_event_shard(dir.path());
    let mut content = fs::read_to_string(&shard).unwrap();
    content.push_str("\nthis is not tsjson\n");
    fs::write(&shard, content).unwrap();

    bn_cmd(dir.path())
        .args(["history"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("bn admin verify").or(predicate::str::contains("corruption")),
        );
}
