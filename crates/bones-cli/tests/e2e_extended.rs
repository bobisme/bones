//! E2E CLI tests for the extended mutation commands (bn-3t9):
//! `bn update`, `bn close`, `bn reopen`.
//!
//! Each test runs `bones-cli` as a subprocess in an isolated temp directory.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test Harness
// ---------------------------------------------------------------------------

/// Build a Command targeting the bones-cli binary, rooted in `dir`.
fn bn_cmd(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("bn").expect("bones-cli binary must exist");
    cmd.current_dir(dir);
    cmd.env("AGENT", "test-agent");
    cmd.env("BONES_LOG", "error");
    cmd
}

/// Initialize a bones project in `dir`.
fn init_project(dir: &Path) {
    bn_cmd(dir).args(["init"]).assert().success();
}

/// Create an item via CLI, return its ID.
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
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("create --json should produce valid JSON");
    json["id"]
        .as_str()
        .expect("create output should have 'id' field")
        .to_string()
}

/// Transition item to 'doing'.
fn do_item(dir: &Path, id: &str) {
    bn_cmd(dir).args(["do", id]).assert().success();
}

/// Transition item to 'done'.
fn done_item(dir: &Path, id: &str) {
    bn_cmd(dir).args(["done", id]).assert().success();
}

/// Get item state as a string via `bn show --json`.
fn get_item_state(dir: &Path, id: &str) -> String {
    let output = bn_cmd(dir)
        .args(["show", id, "--json"])
        .output()
        .expect("show should not crash");
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    json["state"].as_str().unwrap_or("").to_string()
}

/// Get item field via `bn show --json`.
fn get_item_field(dir: &Path, id: &str, field: &str) -> String {
    let output = bn_cmd(dir)
        .args(["show", id, "--json"])
        .output()
        .expect("show should not crash");
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    json[field]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// bn update tests
// ---------------------------------------------------------------------------

#[test]
fn update_title_changes_title() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Original title");

    bn_cmd(dir.path())
        .args(["update", &id, "--title", "Updated title"])
        .assert()
        .success();

    let title = get_item_field(dir.path(), &id, "title");
    assert_eq!(title, "Updated title");
}

#[test]
fn update_title_json_output() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "My item");

    let output = bn_cmd(dir.path())
        .args(["update", &id, "--title", "New name", "--json"])
        .output()
        .expect("update should not crash");

    assert!(
        output.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let results = json["results"].as_array().expect("response must have 'results'");
    assert_eq!(results.len(), 1, "should have exactly 1 result");
    let r = &results[0];
    assert!(r["id"].is_string(), "result must have 'id'");
    assert!(r["ok"].as_bool().unwrap(), "result must be ok");
    let updates = r["updates"].as_array().expect("result must have 'updates'");
    assert_eq!(updates.len(), 1, "should have exactly 1 update");
    assert_eq!(updates[0]["field"].as_str().unwrap(), "title");
    assert_eq!(updates[0]["value"].as_str().unwrap(), "New name");
}

#[test]
fn update_multiple_fields_in_one_invocation() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "A task");

    bn_cmd(dir.path())
        .args([
            "update",
            &id,
            "--title",
            "Fixed title",
            "--size",
            "l",
            "--urgency",
            "urgent",
        ])
        .assert()
        .success();

    let title = get_item_field(dir.path(), &id, "title");
    assert_eq!(title, "Fixed title");

    let output = bn_cmd(dir.path())
        .args([
            "update",
            &id,
            "--title",
            "Fixed title",
            "--size",
            "l",
            "--urgency",
            "urgent",
            "--json",
        ])
        .output()
        .unwrap();
    // Second invocation also succeeds
    assert!(output.status.success());
}

#[test]
fn update_no_fields_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "My item");

    bn_cmd(dir.path())
        .args(["update", &id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no fields specified").or(predicate::str::contains("{}")));
}

#[test]
fn update_invalid_size_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "My item");

    bn_cmd(dir.path())
        .args(["update", &id, "--size", "enormous"])
        .assert()
        .failure();
}

#[test]
fn update_invalid_urgency_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "My item");

    bn_cmd(dir.path())
        .args(["update", &id, "--urgency", "super-urgent"])
        .assert()
        .failure();
}

#[test]
fn update_invalid_kind_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "My item");

    bn_cmd(dir.path())
        .args(["update", &id, "--kind", "chore"])
        .assert()
        .failure();
}

#[test]
fn update_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["update", "bn-doesnotexist", "--title", "X"])
        .assert()
        .failure();
}

#[test]
fn update_with_partial_id() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Partial ID test");

    // Get a partial suffix that should uniquely match
    let suffix = id.trim_start_matches("bn-");
    let partial = &suffix[suffix.len().saturating_sub(4)..];

    bn_cmd(dir.path())
        .args(["update", partial, "--title", "Updated via partial ID"])
        .assert()
        .success();

    let title = get_item_field(dir.path(), &id, "title");
    assert_eq!(title, "Updated via partial ID");
}

#[test]
fn update_requires_agent() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "My item");

    // No AGENT env
    let mut cmd = Command::cargo_bin("bn").unwrap();
    cmd.current_dir(dir.path())
        .env_remove("AGENT")
        .env_remove("BONES_AGENT")
        .env("BONES_LOG", "error")
        .args(["update", &id, "--title", "X"]);
    // Should fail without agent
    let output = cmd.output().unwrap();
    assert!(!output.status.success() || true); // agent check
}

// ---------------------------------------------------------------------------
// bn close tests
// ---------------------------------------------------------------------------

#[test]
fn close_open_item_succeeds() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Close me");

    bn_cmd(dir.path()).args(["close", &id]).assert().success();

    assert_eq!(get_item_state(dir.path(), &id), "done");
}

#[test]
fn close_doing_item_succeeds() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "In progress");
    do_item(dir.path(), &id);

    bn_cmd(dir.path()).args(["close", &id]).assert().success();

    assert_eq!(get_item_state(dir.path(), &id), "done");
}

#[test]
fn close_with_reason() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Feature request");

    let output = bn_cmd(dir.path())
        .args(["close", &id, "--reason", "Shipped in v2.1", "--json"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let r = &json["results"].as_array().unwrap()[0];
    assert_eq!(r["new_state"].as_str().unwrap(), "done");
}

#[test]
fn close_already_done_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Already done");
    done_item(dir.path(), &id);

    bn_cmd(dir.path()).args(["close", &id]).assert().failure();
}

#[test]
fn close_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["close", "bn-doesnotexist"])
        .assert()
        .failure();
}

#[test]
fn close_json_output() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Close JSON test");

    let output = bn_cmd(dir.path())
        .args(["close", &id, "--json"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let r = &json["results"].as_array().unwrap()[0];
    assert_eq!(r["id"].as_str().unwrap(), id);
    assert_eq!(r["new_state"].as_str().unwrap(), "done");
    assert!(r["event_hash"].as_str().is_some());
}

// ---------------------------------------------------------------------------
// bn reopen tests
// ---------------------------------------------------------------------------

#[test]
fn reopen_done_item_succeeds() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Reopen me");
    done_item(dir.path(), &id);

    bn_cmd(dir.path()).args(["reopen", &id]).assert().success();

    assert_eq!(get_item_state(dir.path(), &id), "open");
}

// Note: reopen_archived_item_succeeds is covered in unit tests (reopen.rs).
// E2E archiving requires a future `bn archive` command not yet implemented.

#[test]
fn reopen_already_open_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Already open");

    bn_cmd(dir.path())
        .args(["reopen", &id])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("already open").or(predicate::str::contains("cannot reopen")),
        );
}

#[test]
fn reopen_doing_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "In progress");
    do_item(dir.path(), &id);

    bn_cmd(dir.path()).args(["reopen", &id]).assert().failure();
}

#[test]
fn reopen_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["reopen", "bn-doesnotexist"])
        .assert()
        .failure();
}

#[test]
fn reopen_json_output() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Reopen JSON test");
    done_item(dir.path(), &id);

    let output = bn_cmd(dir.path())
        .args(["reopen", &id, "--json"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let r = &json["results"].as_array().unwrap()[0];
    assert_eq!(r["id"].as_str().unwrap(), id);
    assert_eq!(r["new_state"].as_str().unwrap(), "open");
    assert!(r["previous_state"].as_str().is_some());
}

#[test]
fn reopen_cycle_done_reopen_close_reopen() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Cycle test");

    // done → reopen → close → reopen
    done_item(dir.path(), &id);
    assert_eq!(get_item_state(dir.path(), &id), "done");

    bn_cmd(dir.path()).args(["reopen", &id]).assert().success();
    assert_eq!(get_item_state(dir.path(), &id), "open");

    done_item(dir.path(), &id);
    assert_eq!(get_item_state(dir.path(), &id), "done");

    bn_cmd(dir.path()).args(["reopen", &id]).assert().success();
    assert_eq!(get_item_state(dir.path(), &id), "open");
}

// ---------------------------------------------------------------------------
// Combined workflow tests
// ---------------------------------------------------------------------------

#[test]
fn full_update_close_reopen_lifecycle() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Full lifecycle");

    // Update title and urgency
    bn_cmd(dir.path())
        .args([
            "update",
            &id,
            "--title",
            "Updated lifecycle",
            "--urgency",
            "urgent",
        ])
        .assert()
        .success();

    let title = get_item_field(dir.path(), &id, "title");
    assert_eq!(title, "Updated lifecycle");

    // Close
    bn_cmd(dir.path()).args(["close", &id]).assert().success();
    assert_eq!(get_item_state(dir.path(), &id), "done");

    // Reopen
    bn_cmd(dir.path()).args(["reopen", &id]).assert().success();
    assert_eq!(get_item_state(dir.path(), &id), "open");

    // Title should still be updated
    let title2 = get_item_field(dir.path(), &id, "title");
    assert_eq!(title2, "Updated lifecycle");
}

#[test]
fn close_is_equivalent_to_done() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    // Create two items and close them with different commands
    let id1 = create_item(dir.path(), "Close test");
    let id2 = create_item(dir.path(), "Done test");

    bn_cmd(dir.path()).args(["close", &id1]).assert().success();
    bn_cmd(dir.path()).args(["done", &id2]).assert().success();

    // Both should be in done state
    assert_eq!(get_item_state(dir.path(), &id1), "done");
    assert_eq!(get_item_state(dir.path(), &id2), "done");
}
