//! E2E onboarding workflow tests for `bn init` + first-item flow.

use assert_cmd::Command;
use predicates::prelude::*;
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

fn list_items_json(dir: &Path) -> Vec<Value> {
    let output = bn_cmd(dir)
        .args(["list", "--json"])
        .output()
        .expect("list should not crash");
    assert!(
        output.status.success(),
        "list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    response["items"].as_array().cloned().unwrap_or_default()
}

#[test]
fn init_create_list_show_first_item_flow_succeeds() {
    let dir = TempDir::new().unwrap();

    // No `.git` exists: init should still work in standalone mode.
    bn_cmd(dir.path())
        .args(["init"])
        .assert()
        .success()
        .stderr(predicate::str::contains("No git repository detected"));

    assert!(dir.path().join(".bones").is_dir());
    assert!(dir.path().join(".bones/events").is_dir());
    assert!(dir.path().join(".bones/config.toml").is_file());

    let create_out = bn_cmd(dir.path())
        .args(["create", "--title", "First task", "--json"])
        .output()
        .unwrap();
    assert!(
        create_out.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&create_out.stderr)
    );

    let created: Value = serde_json::from_slice(&create_out.stdout).expect("valid create JSON");
    let id = created["id"]
        .as_str()
        .expect("id must be present")
        .to_string();

    let items = list_items_json(dir.path());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], id);
    assert_eq!(items[0]["title"], "First task");

    bn_cmd(dir.path()).args(["show", &id]).assert().success();
}

#[test]
fn init_creates_expected_event_structure() {
    let dir = TempDir::new().unwrap();
    bn_cmd(dir.path()).args(["init"]).assert().success();

    let events_dir = dir.path().join(".bones/events");
    assert!(events_dir.join("current.events").is_symlink());

    let entries: Vec<_> = fs::read_dir(&events_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        entries.len() >= 2,
        "expected shard + current.events symlink"
    );
}

#[test]
fn first_item_appends_event_to_active_shard() {
    let dir = TempDir::new().unwrap();
    bn_cmd(dir.path()).args(["init"]).assert().success();

    bn_cmd(dir.path())
        .args(["create", "--title", "Shard write check"])
        .assert()
        .success();

    let current = dir.path().join(".bones/events/current.events");
    let content = fs::read_to_string(&current).expect("current.events should be readable");
    let lines: Vec<_> = content.lines().collect();
    assert!(
        lines.len() >= 3,
        "expected header + at least one event line"
    );
    assert!(
        content.contains("item.create"),
        "expected item.create event in active shard"
    );
}

#[test]
fn reinit_without_force_fails_with_actionable_message() {
    let dir = TempDir::new().unwrap();
    bn_cmd(dir.path()).args(["init"]).assert().success();

    bn_cmd(dir.path())
        .args(["init"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bn init --force"));
}

#[test]
fn init_hooks_requires_git_repo() {
    let dir = TempDir::new().unwrap();

    bn_cmd(dir.path())
        .args(["init", "--hooks"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("initialize git first"));
}

#[test]
fn create_without_agent_fails_with_actionable_message() {
    let dir = TempDir::new().unwrap();
    bn_cmd(dir.path()).args(["init"]).assert().success();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("bn"));
    cmd.current_dir(dir.path())
        .env_remove("AGENT")
        .env_remove("BONES_AGENT")
        .env("BONES_LOG", "error")
        .args(["create", "--title", "Needs agent"]);

    cmd.assert().failure().stderr(predicate::str::contains(
        "Set --agent, BONES_AGENT, or AGENT",
    ));
}
