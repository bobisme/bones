//! E2E CLI lifecycle workflow tests for bn-yot.4.
//!
//! Tests validate the core lifecycle surface delivered by bn-2da:
//! create -> do -> done, goals, tags, and JSON contract checks.
//!
//! Each test runs `bones-cli` as a subprocess in an isolated temp directory.

use assert_cmd::Command;
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test Harness
// ---------------------------------------------------------------------------

/// Build a Command targeting the bones-cli binary, rooted in `dir`.
fn bn_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("bn"));
    cmd.current_dir(dir);
    // Provide a default agent so mutating commands don't fail
    cmd.env("AGENT", "test-agent");
    // Suppress tracing output that goes to stderr
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

/// Create an item with a specific kind.
fn create_item_kind(dir: &Path, title: &str, kind: &str) -> String {
    let output = bn_cmd(dir)
        .args(["create", "--title", title, "--kind", kind, "--json"])
        .output()
        .expect("create should not crash");
    assert!(
        output.status.success(),
        "create kind={kind} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    json["id"].as_str().expect("id field").to_string()
}

/// Create an item as a child of a parent.
fn create_child(dir: &Path, title: &str, parent_id: &str) -> String {
    let output = bn_cmd(dir)
        .args(["create", "--title", title, "--parent", parent_id, "--json"])
        .output()
        .expect("create child should not crash");
    assert!(
        output.status.success(),
        "create child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    json["id"].as_str().expect("id field").to_string()
}

/// Run `bn show <id> --json` and return the parsed JSON.
fn show_item_json(dir: &Path, id: &str) -> Value {
    let output = bn_cmd(dir)
        .args(["show", id, "--json"])
        .output()
        .expect("show should not crash");
    assert!(
        output.status.success(),
        "show {} failed: {}",
        id,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("show --json should produce valid JSON")
}

/// Rebuild the projection database so that all events are reflected in queries.
/// Some commands (tag, untag, move) don't inline-project to SQLite, so we need
/// this after those operations.
fn rebuild(dir: &Path) {
    bn_cmd(dir).args(["rebuild"]).assert().success();
}

/// Run `bn list --json` and return the parsed JSON array.
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
    let response: Value =
        serde_json::from_slice(&output.stdout).expect("list --json should produce valid JSON");
    // list --json returns { items: [...], total, limit, offset, has_more }
    response["items"].as_array().cloned().unwrap_or_default()
}

/// Run `bn list --json` with extra args and return parsed JSON array.
fn list_items_filtered(dir: &Path, args: &[&str]) -> Vec<Value> {
    let mut full_args = vec!["list", "--json"];
    full_args.extend_from_slice(args);
    let output = bn_cmd(dir)
        .args(&full_args)
        .output()
        .expect("list should not crash");
    assert!(
        output.status.success(),
        "list filtered failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    response["items"].as_array().cloned().unwrap_or_default()
}

// ===========================================================================
// Test 1: Create and List
// ===========================================================================

#[test]
fn create_and_list_single_item() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Test Item");
    assert!(id.starts_with("bn-"), "ID should start with bn- prefix");

    let items = list_items_json(dir.path());
    assert_eq!(items.len(), 1, "should have exactly 1 item");
    assert_eq!(items[0]["title"], "Test Item");
    assert_eq!(items[0]["id"], id);
    assert_eq!(items[0]["state"], "open");
    assert_eq!(items[0]["kind"], "task");
}

#[test]
fn create_multiple_items_all_listed() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id1 = create_item(dir.path(), "First");
    let id2 = create_item(dir.path(), "Second");
    let id3 = create_item(dir.path(), "Third");

    let items = list_items_json(dir.path());
    assert_eq!(items.len(), 3);

    let ids: Vec<&str> = items.iter().filter_map(|i| i["id"].as_str()).collect();
    assert!(ids.contains(&id1.as_str()));
    assert!(ids.contains(&id2.as_str()));
    assert!(ids.contains(&id3.as_str()));
}

// ===========================================================================
// Test 2: Full Lifecycle (create -> do -> done)
// ===========================================================================

#[test]
fn full_lifecycle_create_do_done() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    // Create
    let id = create_item(dir.path(), "Lifecycle Test");
    let item = show_item_json(dir.path(), &id);
    assert_eq!(item["state"], "open");

    // Do
    bn_cmd(dir.path()).args(["do", &id]).assert().success();

    let item = show_item_json(dir.path(), &id);
    assert_eq!(item["state"], "doing");

    // Done
    bn_cmd(dir.path()).args(["done", &id]).assert().success();

    let item = show_item_json(dir.path(), &id);
    assert_eq!(item["state"], "done");
}

#[test]
fn do_transition_json_output() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "JSON Do Test");

    let output = bn_cmd(dir.path())
        .args(["do", &id, "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let r = &json["results"].as_array().expect("results")[0];
    assert_eq!(r["id"], id);
    assert_eq!(r["previous_state"], "open");
    assert_eq!(r["new_state"], "doing");
    assert!(r["event_hash"].as_str().is_some());
}

#[test]
fn done_transition_json_output() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "JSON Done Test");
    bn_cmd(dir.path()).args(["do", &id]).assert().success();

    let output = bn_cmd(dir.path())
        .args(["done", &id, "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let r = &json["results"].as_array().expect("results")[0];
    assert_eq!(r["id"], id);
    assert_eq!(r["previous_state"], "doing");
    assert_eq!(r["new_state"], "done");
}

#[test]
fn done_with_reason_flag() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Reason Test");
    bn_cmd(dir.path()).args(["do", &id]).assert().success();

    let output = bn_cmd(dir.path())
        .args(["done", &id, "--reason", "Merged to main", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let r = &json["results"].as_array().expect("results")[0];
    assert_eq!(r["new_state"], "done");
}

#[test]
fn skip_do_direct_open_to_done() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Direct Done Test");

    // open -> done directly (should work)
    bn_cmd(dir.path()).args(["done", &id]).assert().success();

    let item = show_item_json(dir.path(), &id);
    assert_eq!(item["state"], "done");
}

// ===========================================================================
// Test 3: Goal Hierarchy
// ===========================================================================

#[test]
fn create_goal_with_children() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let goal_id = create_item_kind(dir.path(), "My Goal", "goal");
    let child1_id = create_child(dir.path(), "Child Task 1", &goal_id);
    let child2_id = create_child(dir.path(), "Child Task 2", &goal_id);

    // Verify goal exists and is a goal
    let goal = show_item_json(dir.path(), &goal_id);
    assert_eq!(goal["kind"], "goal");
    assert_eq!(goal["state"], "open");

    // Verify children have parent set
    let child1 = show_item_json(dir.path(), &child1_id);
    assert_eq!(child1["parent_id"], goal_id);

    let child2 = show_item_json(dir.path(), &child2_id);
    assert_eq!(child2["parent_id"], goal_id);
}

#[test]
fn goal_auto_complete_when_all_children_done() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let goal_id = create_item_kind(dir.path(), "Auto-Close Goal", "goal");
    let child1_id = create_child(dir.path(), "Child 1", &goal_id);
    let child2_id = create_child(dir.path(), "Child 2", &goal_id);

    // Complete first child — goal should stay open
    bn_cmd(dir.path())
        .args(["done", &child1_id])
        .assert()
        .success();
    let goal = show_item_json(dir.path(), &goal_id);
    assert_eq!(
        goal["state"], "open",
        "goal should remain open with 1 child pending"
    );

    // Complete second child — goal should auto-complete
    bn_cmd(dir.path())
        .args(["done", &child2_id])
        .assert()
        .success();
    let goal = show_item_json(dir.path(), &goal_id);
    assert_eq!(
        goal["state"], "done",
        "goal should auto-complete when all children are done"
    );
}

#[test]
fn goal_auto_complete_single_child() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let goal_id = create_item_kind(dir.path(), "Single Child Goal", "goal");
    let child_id = create_child(dir.path(), "Only Child", &goal_id);

    bn_cmd(dir.path())
        .args(["done", &child_id])
        .assert()
        .success();

    let goal = show_item_json(dir.path(), &goal_id);
    assert_eq!(
        goal["state"], "done",
        "goal should auto-close on single child done"
    );
}

#[test]
fn move_item_under_parent() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let goal_id = create_item_kind(dir.path(), "New Parent Goal", "goal");
    let task_id = create_item(dir.path(), "Orphan Task");

    // Move task under goal
    bn_cmd(dir.path())
        .args(["move", &task_id, "--parent", &goal_id])
        .assert()
        .success();

    rebuild(dir.path());

    let task = show_item_json(dir.path(), &task_id);
    assert_eq!(task["parent_id"], goal_id);
}

// ===========================================================================
// Test 4: Tag Workflow
// ===========================================================================

#[test]
fn tag_and_show_labels() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Taggable Item");

    // Tag with a label
    bn_cmd(dir.path())
        .args(["tag", &id, "backend"])
        .assert()
        .success();

    // Rebuild projection so tag changes are visible to show/list
    rebuild(dir.path());

    let item = show_item_json(dir.path(), &id);
    let labels = item["labels"]
        .as_array()
        .expect("labels should be an array");
    assert!(
        labels.iter().any(|l| l == "backend"),
        "labels should contain 'backend': {labels:?}"
    );
}

#[test]
fn tag_multiple_labels() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Multi-Label Item");

    bn_cmd(dir.path())
        .args(["tag", &id, "backend", "urgent", "auth"])
        .assert()
        .success();

    rebuild(dir.path());

    let item = show_item_json(dir.path(), &id);
    let labels = item["labels"]
        .as_array()
        .expect("labels should be an array");
    for expected in &["backend", "urgent", "auth"] {
        assert!(
            labels.iter().any(|l| l == expected),
            "labels should contain '{expected}': {labels:?}"
        );
    }
}

#[test]
fn untag_removes_label() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Untag Test");

    // Add labels
    bn_cmd(dir.path())
        .args(["tag", &id, "backend", "frontend"])
        .assert()
        .success();

    // Rebuild so untag can read current labels
    rebuild(dir.path());

    // Remove one
    bn_cmd(dir.path())
        .args(["untag", &id, "backend"])
        .assert()
        .success();

    rebuild(dir.path());

    let item = show_item_json(dir.path(), &id);
    let labels = item["labels"]
        .as_array()
        .expect("labels should be an array");
    assert!(
        !labels.iter().any(|l| l == "backend"),
        "backend should be removed"
    );
    assert!(
        labels.iter().any(|l| l == "frontend"),
        "frontend should remain"
    );
}

#[test]
fn list_filter_by_label() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id1 = create_item(dir.path(), "Backend Item");
    let _id2 = create_item(dir.path(), "Frontend Item");

    bn_cmd(dir.path())
        .args(["tag", &id1, "backend"])
        .assert()
        .success();

    rebuild(dir.path());

    let items = list_items_filtered(dir.path(), &["--label", "backend"]);
    assert_eq!(items.len(), 1, "only 1 item has 'backend' label");
    assert_eq!(items[0]["id"], id1);
}

#[test]
fn create_with_initial_labels() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let output = bn_cmd(dir.path())
        .args([
            "create",
            "--title",
            "Pre-labeled",
            "-l",
            "bug",
            "-l",
            "critical",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = json["id"].as_str().unwrap();
    let labels = json["labels"].as_array().unwrap();
    assert!(labels.iter().any(|l| l == "bug"));
    assert!(labels.iter().any(|l| l == "critical"));

    // Verify via show
    let item = show_item_json(dir.path(), id);
    let show_labels = item["labels"].as_array().unwrap();
    assert!(show_labels.iter().any(|l| l == "bug"));
    assert!(show_labels.iter().any(|l| l == "critical"));
}

// ===========================================================================
// Test 5: JSON Contract Checks
// ===========================================================================

#[test]
fn create_json_contract() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let output = bn_cmd(dir.path())
        .args(["create", "--title", "Contract Test", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

    // Required fields
    assert!(json["id"].is_string(), "id must be a string");
    assert!(json["title"].is_string(), "title must be a string");
    assert!(json["kind"].is_string(), "kind must be a string");
    assert!(json["state"].is_string(), "state must be a string");
    assert!(json["agent"].is_string(), "agent must be a string");
    assert!(
        json["event_hash"].is_string(),
        "event_hash must be a string"
    );

    // Values
    assert_eq!(json["title"], "Contract Test");
    assert_eq!(json["kind"], "task");
    assert_eq!(json["state"], "open");
    assert!(
        json["event_hash"]
            .as_str()
            .unwrap_or("")
            .starts_with("blake3:"),
        "event_hash should start with blake3:"
    );
}

#[test]
fn list_json_contract() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    create_item(dir.path(), "Contract List Item");

    let items = list_items_json(dir.path());
    assert_eq!(items.len(), 1);

    let item = &items[0];
    assert!(item["id"].is_string());
    assert!(item["title"].is_string());
    assert!(item["kind"].is_string());
    assert!(item["state"].is_string());
    assert!(item["urgency"].is_string());
    assert!(item["updated_at_us"].is_number());
}

#[test]
fn show_json_contract() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Contract Show Item");
    let item = show_item_json(dir.path(), &id);

    // Required fields
    assert!(item["id"].is_string());
    assert!(item["title"].is_string());
    assert!(item["kind"].is_string());
    assert!(item["state"].is_string());
    assert!(item["urgency"].is_string());
    assert!(item["labels"].is_array());
    assert!(item["assignees"].is_array());
    assert!(item["depends_on"].is_array());
    assert!(item["dependents"].is_array());
    assert!(item["comments"].is_array());
    assert!(item["created_at_us"].is_number());
    assert!(item["updated_at_us"].is_number());
}

#[test]
fn init_json_output_is_not_json_by_default() {
    // init doesn't have --json flag; verify it produces human text
    let dir = TempDir::new().unwrap();
    bn_cmd(dir.path())
        .args(["init"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Initialized"));
}

// ===========================================================================
// Test 6: Error Paths
// ===========================================================================

#[test]
fn do_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["do", "bn-nonexist"])
        .assert()
        .failure();
}

#[test]
fn done_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["done", "bn-nonexist"])
        .assert()
        .failure();
}

#[test]
fn show_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["show", "bn-nonexist"])
        .assert()
        .failure();
}

#[test]
fn done_already_done_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Double Done");
    bn_cmd(dir.path()).args(["done", &id]).assert().success();

    // Second done should fail
    bn_cmd(dir.path()).args(["done", &id]).assert().failure();
}

#[test]
fn do_already_done_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Do After Done");
    bn_cmd(dir.path()).args(["done", &id]).assert().success();

    // do on a done item should fail
    bn_cmd(dir.path()).args(["do", &id]).assert().failure();
}

#[test]
fn create_invalid_kind_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["create", "--title", "Bad Kind", "--kind", "epic"])
        .assert()
        .failure();
}

#[test]
fn create_without_bones_dir_fails() {
    let dir = TempDir::new().unwrap();
    // No init — should fail
    bn_cmd(dir.path())
        .args(["create", "--title", "No Project"])
        .assert()
        .failure();
}

#[test]
fn tag_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["tag", "bn-nonexist", "label"])
        .assert()
        .failure();
}

#[test]
fn untag_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["untag", "bn-nonexist", "label"])
        .assert()
        .failure();
}

// ===========================================================================
// Test 7: List Filtering
// ===========================================================================

#[test]
fn list_filter_by_state() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id1 = create_item(dir.path(), "Open Item");
    let id2 = create_item(dir.path(), "Doing Item");
    bn_cmd(dir.path()).args(["do", &id2]).assert().success();

    // Default list shows open items
    let open_items = list_items_filtered(dir.path(), &["--state", "open"]);
    assert!(
        open_items.iter().all(|i| i["state"] == "open"),
        "all items should be in open state"
    );
    assert!(open_items.iter().any(|i| i["id"] == id1.as_str()));

    // Filter for doing
    let doing_items = list_items_filtered(dir.path(), &["--state", "doing"]);
    assert!(
        doing_items.iter().all(|i| i["state"] == "doing"),
        "all items should be in doing state"
    );
    assert!(doing_items.iter().any(|i| i["id"] == id2.as_str()));
}

#[test]
fn list_filter_by_kind() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let _task_id = create_item(dir.path(), "A Task");
    let _goal_id = create_item_kind(dir.path(), "A Goal", "goal");
    let _bug_id = create_item_kind(dir.path(), "A Bug", "bug");

    let goals = list_items_filtered(dir.path(), &["--kind", "goal"]);
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0]["kind"], "goal");
    assert_eq!(goals[0]["title"], "A Goal");
}

// ===========================================================================
// Test 8: Partial ID Resolution
// ===========================================================================

#[test]
fn show_with_partial_id() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Partial ID Test");
    // Strip the "bn-" prefix and use just the hash part
    let short_id = id.strip_prefix("bn-").unwrap_or(&id);

    // show with partial ID should work
    let item = show_item_json(dir.path(), short_id);
    assert_eq!(item["id"], id);
    assert_eq!(item["title"], "Partial ID Test");
}

#[test]
fn do_with_partial_id() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Partial Do Test");
    let short_id = id.strip_prefix("bn-").unwrap_or(&id);

    bn_cmd(dir.path()).args(["do", short_id]).assert().success();

    let item = show_item_json(dir.path(), &id);
    assert_eq!(item["state"], "doing");
}

// ===========================================================================
// Test 9: Create with All Options
// ===========================================================================

#[test]
fn create_with_all_options() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let output = bn_cmd(dir.path())
        .args([
            "create",
            "--title",
            "Full Options Item",
            "--kind",
            "bug",
            "--size",
            "m",
            "--urgency",
            "urgent",
            "-l",
            "backend",
            "--description",
            "A detailed bug report",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["title"], "Full Options Item");
    assert_eq!(json["kind"], "bug");
    assert_eq!(json["urgency"], "urgent");
    assert_eq!(json["size"], "m");
}

// ===========================================================================
// Test 10: Human-Readable Output
// ===========================================================================

#[test]
fn create_human_output_contains_id() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["create", "--title", "Human Output Test"])
        .assert()
        .success()
        .stdout(predicates::str::contains("bn-"))
        .stdout(predicates::str::contains("Human Output Test"));
}

#[test]
fn list_human_output_shows_items() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    create_item(dir.path(), "Listed Item");

    bn_cmd(dir.path())
        .args(["list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Listed Item"));
}

// ===========================================================================
// Test 11: Empty State
// ===========================================================================

#[test]
fn list_empty_project_returns_empty() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let items = list_items_json(dir.path());
    assert!(items.is_empty(), "fresh project should have no items");
}

#[test]
fn list_empty_project_human_output_succeeds() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path()).args(["list"]).assert().success();
}
