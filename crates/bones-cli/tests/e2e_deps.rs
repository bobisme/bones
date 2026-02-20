//! E2E dependency + graph workflow tests for bn-3i7.
//!
//! Covers `bn dep add` / `bn dep rm` with `bn graph` JSON + text verification.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;

fn bn_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("bn"));
    cmd.current_dir(dir);
    cmd.env("AGENT", "test-agent");
    cmd.env("BONES_LOG", "error");
    cmd
}

fn bn_human_cmd(dir: &Path) -> Command {
    let mut cmd = bn_cmd(dir);
    cmd.env("FORMAT", "pretty");
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

fn create_goal(dir: &Path, title: &str) -> String {
    let output = bn_cmd(dir)
        .args(["create", "--title", title, "--kind", "goal", "--json"])
        .output()
        .expect("create goal should not crash");
    assert!(
        output.status.success(),
        "create goal failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    json["id"].as_str().expect("id must exist").to_string()
}

fn create_child(dir: &Path, title: &str, parent: &str) -> String {
    let output = bn_cmd(dir)
        .args(["create", "--title", title, "--parent", parent, "--json"])
        .output()
        .expect("create child should not crash");
    assert!(
        output.status.success(),
        "create child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    json["id"].as_str().expect("id must exist").to_string()
}

fn dep_add_blocks(dir: &Path, blocker: &str, blocked: &str) {
    bn_cmd(dir)
        .args(["dep", "add", blocker, "--blocks", blocked])
        .assert()
        .success();
}

fn dep_rm(dir: &Path, blocker: &str, blocked: &str) {
    bn_cmd(dir)
        .args(["dep", "rm", blocker, blocked])
        .assert()
        .success();
}

fn graph_json(dir: &Path, id: &str) -> Value {
    let output = bn_cmd(dir)
        .args(["graph", id, "--json"])
        .output()
        .expect("graph should not crash");
    assert!(
        output.status.success(),
        "graph failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("graph --json must parse")
}

#[test]
fn dep_add_and_graph_json_show_inverse_relationships() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let a = create_item(dir.path(), "Foundation");
    let b = create_item(dir.path(), "Build feature");

    dep_add_blocks(dir.path(), &a, &b);

    let a_graph = graph_json(dir.path(), &a);
    let b_graph = graph_json(dir.path(), &b);

    assert!(
        a_graph["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == &b)
    );
    assert!(
        b_graph["blocked_by"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == &a)
    );
}

#[test]
fn dep_rm_removes_relationship_from_both_sides() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let a = create_item(dir.path(), "Task A");
    let b = create_item(dir.path(), "Task B");

    dep_add_blocks(dir.path(), &a, &b);
    dep_rm(dir.path(), &a, &b);

    let a_graph = graph_json(dir.path(), &a);
    let b_graph = graph_json(dir.path(), &b);

    assert!(a_graph["blocks"].as_array().unwrap().is_empty());
    assert!(b_graph["blocked_by"].as_array().unwrap().is_empty());
}

#[test]
fn graph_text_tree_shows_dependency_chain() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let foundation = create_item(dir.path(), "Foundation");
    let middle = create_item(dir.path(), "Middle layer");
    let top = create_item(dir.path(), "Top layer");

    dep_add_blocks(dir.path(), &foundation, &middle);
    dep_add_blocks(dir.path(), &middle, &top);

    bn_human_cmd(dir.path())
        .args(["graph", &foundation])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Foundation")
                .and(predicate::str::contains("Middle layer"))
                .and(predicate::str::contains("Top layer")),
        );
}

#[test]
fn dep_add_rejects_cycles_with_actionable_error() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let x = create_item(dir.path(), "Task X");
    let y = create_item(dir.path(), "Task Y");

    dep_add_blocks(dir.path(), &x, &y);

    bn_cmd(dir.path())
        .args(["dep", "add", &y, "--blocks", &x])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cycle"));
}

#[test]
fn cross_goal_dependencies_are_reflected_in_graph_json() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let goal_a = create_goal(dir.path(), "Goal A");
    let goal_b = create_goal(dir.path(), "Goal B");
    let a_task = create_child(dir.path(), "Task under A", &goal_a);
    let b_task = create_child(dir.path(), "Task under B", &goal_b);

    dep_add_blocks(dir.path(), &a_task, &b_task);

    let graph = graph_json(dir.path(), &b_task);
    assert!(
        graph["blocked_by"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == &a_task)
    );
}
