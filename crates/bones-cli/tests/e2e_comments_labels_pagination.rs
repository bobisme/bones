//! E2E CLI tests covering:
//! - Label namespace commands (`bn label add/rm`, `bn labels`)
//! - Comment workflows (`bn comment add`, `bn comments`, visibility in `bn show`)
//! - Advanced `bn list` filtering: urgency, sort, limit/offset pagination
//! - Goal auto-close/reopen JSON contract (auto_completed_parent field)
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

/// Create an item with specific kind.
fn create_item_kind(dir: &Path, title: &str, kind: &str) -> String {
    let output = bn_cmd(dir)
        .args(["create", "--title", title, "--kind", kind, "--json"])
        .output()
        .expect("create should not crash");
    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    json["id"].as_str().expect("id field").to_string()
}

/// Create an item as a child of a parent.
fn create_child(dir: &Path, title: &str, parent_id: &str) -> String {
    let output = bn_cmd(dir)
        .args(["create", "--title", title, "--parent", parent_id, "--json"])
        .output()
        .expect("create child should not crash");
    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    json["id"].as_str().expect("id field").to_string()
}

/// Run `bn show <id> --json` and return parsed JSON.
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

/// Run `bn list --json` with extra args and return the parsed items array.
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

/// Run `bn list --json` with extra args and return the full response.
fn list_response_full(dir: &Path, args: &[&str]) -> Value {
    let mut full_args = vec!["list", "--json"];
    full_args.extend_from_slice(args);
    let output = bn_cmd(dir)
        .args(&full_args)
        .output()
        .expect("list should not crash");
    assert!(
        output.status.success(),
        "list response failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("valid JSON")
}

/// Rebuild the projection database.
fn rebuild(dir: &Path) {
    bn_cmd(dir).args(["rebuild"]).assert().success();
}

// ---------------------------------------------------------------------------
// bn label add / bn label rm tests
// ---------------------------------------------------------------------------

#[test]
fn label_add_single_label() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Label test item");

    let output = bn_cmd(dir.path())
        .args(["label", "add", &id, "area:backend", "--json"])
        .output()
        .expect("label add should not crash");

    assert!(
        output.status.success(),
        "label add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let results = json["results"].as_array().expect("results must be array");
    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert!(r["ok"].as_bool().unwrap_or(false), "result must be ok");
    assert_eq!(r["item_id"].as_str().unwrap(), id);
    let added = r["added"].as_array().expect("added must be array");
    assert!(added.iter().any(|v| v == "area:backend"));
    let labels = r["labels"].as_array().expect("labels must be array");
    assert!(labels.iter().any(|v| v == "area:backend"));
}

#[test]
fn label_add_then_show_reflects_label() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Show label test");

    bn_cmd(dir.path())
        .args(["label", "add", &id, "type:bug"])
        .assert()
        .success();

    rebuild(dir.path());

    let item = show_item_json(dir.path(), &id);
    let labels = item["labels"].as_array().expect("labels must be array");
    assert!(
        labels.iter().any(|l| l == "type:bug"),
        "label type:bug should appear in show output, got: {labels:?}"
    );
}

#[test]
fn label_rm_removes_label() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Remove label test");

    // Add label first
    bn_cmd(dir.path())
        .args(["label", "add", &id, "status:blocked"])
        .assert()
        .success();

    rebuild(dir.path());

    // Remove it
    let output = bn_cmd(dir.path())
        .args(["label", "rm", &id, "status:blocked", "--json"])
        .output()
        .expect("label rm should not crash");

    assert!(
        output.status.success(),
        "label rm failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let results = json["results"].as_array().expect("results must be array");
    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert!(r["ok"].as_bool().unwrap_or(false));
    let removed = r["removed"].as_array().expect("removed must be array");
    assert!(removed.iter().any(|v| v == "status:blocked"));
    let labels = r["labels"].as_array().expect("labels must be array");
    assert!(
        !labels.iter().any(|v| v == "status:blocked"),
        "removed label should not appear in labels: {labels:?}"
    );
}

#[test]
fn label_rm_then_show_does_not_reflect_label() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Remove verify test");

    bn_cmd(dir.path())
        .args(["label", "add", &id, "needs:review"])
        .assert()
        .success();

    rebuild(dir.path());

    bn_cmd(dir.path())
        .args(["label", "rm", &id, "needs:review"])
        .assert()
        .success();

    rebuild(dir.path());

    let item = show_item_json(dir.path(), &id);
    let labels = item["labels"].as_array().expect("labels must be array");
    assert!(
        !labels.iter().any(|l| l == "needs:review"),
        "removed label should not appear in show, got: {labels:?}"
    );
}

#[test]
fn label_add_is_alias_for_tag() {
    // Both `bn label add` and `bn tag` should produce the same state.
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id1 = create_item(dir.path(), "Via label add");
    let id2 = create_item(dir.path(), "Via tag");

    bn_cmd(dir.path())
        .args(["label", "add", &id1, "component:auth"])
        .assert()
        .success();

    bn_cmd(dir.path())
        .args(["tag", &id2, "component:auth"])
        .assert()
        .success();

    rebuild(dir.path());

    let item1 = show_item_json(dir.path(), &id1);
    let item2 = show_item_json(dir.path(), &id2);

    let labels1 = item1["labels"].as_array().expect("labels");
    let labels2 = item2["labels"].as_array().expect("labels");

    assert!(labels1.iter().any(|l| l == "component:auth"));
    assert!(labels2.iter().any(|l| l == "component:auth"));
}

#[test]
fn label_rm_is_alias_for_untag() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Via label rm");

    bn_cmd(dir.path())
        .args(["tag", &id, "component:cache"])
        .assert()
        .success();

    rebuild(dir.path());

    bn_cmd(dir.path())
        .args(["label", "rm", &id, "component:cache"])
        .assert()
        .success();

    rebuild(dir.path());

    let item = show_item_json(dir.path(), &id);
    let labels = item["labels"].as_array().expect("labels");
    assert!(
        !labels.iter().any(|l| l == "component:cache"),
        "label rm should remove the label: {labels:?}"
    );
}

#[test]
fn label_add_on_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["label", "add", "bn-doesnotexist", "area:backend"])
        .assert()
        .failure();
}

#[test]
fn label_rm_on_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["label", "rm", "bn-doesnotexist", "area:backend"])
        .assert()
        .failure();
}

// ---------------------------------------------------------------------------
// bn labels tests
// ---------------------------------------------------------------------------

#[test]
fn labels_list_shows_added_labels() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id1 = create_item(dir.path(), "Item A");
    let id2 = create_item(dir.path(), "Item B");

    // Rebuild between each tag so the projection DB is up-to-date before the
    // next tag reads the current label state for that item.
    bn_cmd(dir.path())
        .args(["tag", &id1, "area:backend"])
        .assert()
        .success();
    rebuild(dir.path());

    bn_cmd(dir.path())
        .args(["tag", &id2, "area:backend"])
        .assert()
        .success();
    rebuild(dir.path());

    bn_cmd(dir.path())
        .args(["tag", &id1, "type:bug"])
        .assert()
        .success();
    rebuild(dir.path());

    let output = bn_cmd(dir.path())
        .args(["labels", "--json"])
        .output()
        .expect("labels should not crash");

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert!(json["labels"].is_array(), "labels must be an array");

    let labels = json["labels"].as_array().unwrap();

    // Find area:backend (count=2) and type:bug (count=1)
    let backend = labels
        .iter()
        .find(|l| l["name"] == "area:backend")
        .expect("area:backend should be listed");
    assert_eq!(
        backend["count"].as_u64().unwrap(),
        2,
        "area:backend should have count=2"
    );

    let bug = labels
        .iter()
        .find(|l| l["name"] == "type:bug")
        .expect("type:bug should be listed");
    assert_eq!(
        bug["count"].as_u64().unwrap(),
        1,
        "type:bug should have count=1"
    );
}

#[test]
fn labels_list_empty_project_returns_empty_array() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    rebuild(dir.path()); // ensure projection exists

    let output = bn_cmd(dir.path())
        .args(["labels", "--json"])
        .output()
        .expect("labels should not crash");

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let labels = json["labels"].as_array().expect("labels must be array");
    assert!(labels.is_empty(), "fresh project should have no labels");
}

#[test]
fn labels_namespace_groups_by_prefix() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Namespaced item");

    bn_cmd(dir.path())
        .args(["tag", &id, "area:backend", "area:frontend", "type:bug"])
        .assert()
        .success();

    rebuild(dir.path());

    let output = bn_cmd(dir.path())
        .args(["labels", "--namespace", "--json"])
        .output()
        .expect("labels --namespace should not crash");

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

    // Should have both labels and namespaces fields
    assert!(json["labels"].is_array(), "labels field must be present");
    assert!(
        json["namespaces"].is_array(),
        "namespaces field must be present with --namespace flag"
    );

    let namespaces = json["namespaces"].as_array().unwrap();

    // Find the "area" namespace
    let area_ns = namespaces
        .iter()
        .find(|ns| ns["namespace"] == "area")
        .expect("area namespace should be present");

    assert!(
        area_ns["total"].as_u64().unwrap_or(0) >= 1,
        "area namespace should have at least 1 distinct label"
    );
    assert!(
        area_ns["labels"].is_array(),
        "area namespace should have labels array"
    );

    // Find the "type" namespace
    let type_ns = namespaces
        .iter()
        .find(|ns| ns["namespace"] == "type")
        .expect("type namespace should be present");
    assert!(type_ns["labels"].is_array());
}

#[test]
fn labels_json_schema_is_stable() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Schema check");
    bn_cmd(dir.path())
        .args(["tag", &id, "priority:high"])
        .assert()
        .success();
    rebuild(dir.path());

    let output = bn_cmd(dir.path())
        .args(["labels", "--json"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let labels = json["labels"].as_array().expect("labels array");

    // Each label entry must have name (string) and count (number).
    for label in labels {
        assert!(label["name"].is_string(), "label must have string 'name'");
        assert!(label["count"].is_number(), "label must have number 'count'");
    }
}

#[test]
fn labels_count_decrements_when_removed() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id1 = create_item(dir.path(), "Item X");
    let id2 = create_item(dir.path(), "Item Y");

    bn_cmd(dir.path())
        .args(["tag", &id1, "priority:high"])
        .assert()
        .success();
    bn_cmd(dir.path())
        .args(["tag", &id2, "priority:high"])
        .assert()
        .success();

    rebuild(dir.path());

    // Verify count is 2
    let output = bn_cmd(dir.path())
        .args(["labels", "--json"])
        .output()
        .unwrap();
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let labels = json["labels"].as_array().unwrap();
    let label = labels
        .iter()
        .find(|l| l["name"] == "priority:high")
        .expect("priority:high should exist");
    assert_eq!(label["count"].as_u64().unwrap(), 2);

    // Remove from one item
    bn_cmd(dir.path())
        .args(["untag", &id1, "priority:high"])
        .assert()
        .success();

    rebuild(dir.path());

    // Verify count is now 1
    let output = bn_cmd(dir.path())
        .args(["labels", "--json"])
        .output()
        .unwrap();
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let labels = json["labels"].as_array().unwrap();
    let label = labels
        .iter()
        .find(|l| l["name"] == "priority:high")
        .expect("priority:high should still exist with count=1");
    assert_eq!(
        label["count"].as_u64().unwrap(),
        1,
        "count should decrement after removing label from one item"
    );
}

// ---------------------------------------------------------------------------
// bn comment add / bn comments tests
// ---------------------------------------------------------------------------

#[test]
fn comment_add_json_contract() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Commentable");

    let output = bn_cmd(dir.path())
        .args(["comment", "add", &id, "First comment", "--json"])
        .output()
        .expect("comment add should not crash");

    assert!(
        output.status.success(),
        "comment add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert!(
        json["ok"].as_bool().unwrap_or(false),
        "comment add result must have ok=true"
    );
    assert_eq!(json["item_id"].as_str().unwrap(), id);
    assert!(json["agent"].is_string(), "agent must be string");
    assert_eq!(json["body"].as_str().unwrap(), "First comment");
    assert!(json["ts"].is_number(), "ts must be a numeric timestamp");
    assert!(
        json["event_hash"]
            .as_str()
            .unwrap_or("")
            .starts_with("blake3:"),
        "event_hash should start with blake3:"
    );
}

#[test]
fn comment_add_multiple_comments() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Multi-comment");

    bn_cmd(dir.path())
        .args(["comment", "add", &id, "First comment"])
        .assert()
        .success();

    bn_cmd(dir.path())
        .args(["comment", "add", &id, "Second comment"])
        .assert()
        .success();

    bn_cmd(dir.path())
        .args(["comment", "add", &id, "Third comment"])
        .assert()
        .success();

    let output = bn_cmd(dir.path())
        .args(["comments", &id, "--json"])
        .output()
        .expect("comments should not crash");

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let comments = json.as_array().expect("comments output must be array");
    assert_eq!(comments.len(), 3, "should have 3 comments");
}

#[test]
fn comments_list_json_schema() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Schema test item");

    bn_cmd(dir.path())
        .args(["comment", "add", &id, "Schema comment"])
        .assert()
        .success();

    let output = bn_cmd(dir.path())
        .args(["comments", &id, "--json"])
        .output()
        .expect("comments should not crash");

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let comments = json.as_array().expect("comments must be array");
    assert_eq!(comments.len(), 1);

    let comment = &comments[0];
    // Required fields from comments --json
    assert!(
        comment["hash"].is_string(),
        "comment must have 'hash' field"
    );
    assert!(
        comment["agent"].is_string(),
        "comment must have 'agent' field"
    );
    assert!(
        comment["body"].is_string(),
        "comment must have 'body' field"
    );
    assert!(
        comment["ts"].is_number(),
        "comment must have 'ts' timestamp field"
    );
    assert_eq!(comment["body"].as_str().unwrap(), "Schema comment");
    assert_eq!(comment["agent"].as_str().unwrap(), "test-agent");
}

#[test]
fn comments_visible_in_show_output() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Show comments test");

    bn_cmd(dir.path())
        .args(["comment", "add", &id, "Visible comment"])
        .assert()
        .success();

    let item = show_item_json(dir.path(), &id);
    let comments = item["comments"]
        .as_array()
        .expect("show must have comments array");
    assert_eq!(comments.len(), 1, "show should include 1 comment");

    let comment = &comments[0];
    assert_eq!(comment["body"].as_str().unwrap(), "Visible comment");
    assert!(
        comment["author"].is_string(),
        "show comment must have 'author' field"
    );
    assert!(
        comment["created_at_us"].is_number(),
        "show comment must have 'created_at_us' field"
    );
}

#[test]
fn show_includes_all_comments() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "Ordered comments test");

    bn_cmd(dir.path())
        .args(["comment", "add", &id, "Alpha"])
        .assert()
        .success();

    bn_cmd(dir.path())
        .args(["comment", "add", &id, "Beta"])
        .assert()
        .success();

    bn_cmd(dir.path())
        .args(["comment", "add", &id, "Gamma"])
        .assert()
        .success();

    let item = show_item_json(dir.path(), &id);
    let comments = item["comments"].as_array().expect("comments must be array");
    assert_eq!(comments.len(), 3, "show should include all 3 comments");

    // All three comment bodies must be present (order is implementation-defined)
    let bodies: Vec<&str> = comments
        .iter()
        .map(|c| c["body"].as_str().unwrap_or(""))
        .collect();
    assert!(
        bodies.contains(&"Alpha"),
        "Alpha should be in comments: {bodies:?}"
    );
    assert!(
        bodies.contains(&"Beta"),
        "Beta should be in comments: {bodies:?}"
    );
    assert!(
        bodies.contains(&"Gamma"),
        "Gamma should be in comments: {bodies:?}"
    );

    // All comments must have a valid timestamp
    for comment in comments {
        assert!(
            comment["created_at_us"].is_number(),
            "each comment must have a numeric created_at_us timestamp"
        );
        assert!(
            comment["created_at_us"].as_u64().unwrap_or(0) > 0,
            "created_at_us must be non-zero"
        );
    }
}

#[test]
fn comment_on_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["comment", "add", "bn-doesnotexist", "Test"])
        .assert()
        .failure();
}

#[test]
fn comments_on_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["comments", "bn-doesnotexist"])
        .assert()
        .failure();
}

#[test]
fn comments_empty_list_on_new_item() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    let id = create_item(dir.path(), "No comments yet");

    let item = show_item_json(dir.path(), &id);
    let comments = item["comments"].as_array().expect("comments must be array");
    assert!(
        comments.is_empty(),
        "new item should have no comments: {comments:?}"
    );
}

// ---------------------------------------------------------------------------
// Advanced bn list filtering tests
// ---------------------------------------------------------------------------

#[test]
fn list_filter_by_urgency() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let _id1 = create_item(dir.path(), "Default urgency item");
    let output = bn_cmd(dir.path())
        .args([
            "create",
            "--title",
            "Urgent item",
            "--urgency",
            "urgent",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let id_urgent = json["id"].as_str().unwrap().to_string();

    // Filter for urgent only
    let urgent_items = list_items_filtered(dir.path(), &["--urgency", "urgent"]);
    assert!(
        urgent_items.iter().all(|i| i["urgency"] == "urgent"),
        "all returned items should have urgency=urgent"
    );
    assert!(
        urgent_items.iter().any(|i| i["id"] == id_urgent.as_str()),
        "urgent item should be in filtered results"
    );
}

#[test]
fn list_pagination_limit_and_offset() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    // Create 5 items
    for i in 0..5 {
        create_item(dir.path(), &format!("Item {i}"));
    }

    // Get first 2 items
    let page1 = list_response_full(dir.path(), &["--limit", "2", "--offset", "0"]);
    assert_eq!(
        page1["limit"].as_u64().unwrap(),
        2,
        "limit field should match requested limit"
    );
    assert_eq!(
        page1["offset"].as_u64().unwrap(),
        0,
        "offset field should match requested offset"
    );
    assert_eq!(
        page1["total"].as_u64().unwrap(),
        5,
        "total field should count all items"
    );
    assert!(
        page1["has_more"].as_bool().unwrap_or(false),
        "has_more should be true when more items remain"
    );
    let page1_items = page1["items"].as_array().unwrap();
    assert_eq!(page1_items.len(), 2, "should return exactly 2 items");

    // Get next 2 items
    let page2 = list_response_full(dir.path(), &["--limit", "2", "--offset", "2"]);
    assert_eq!(page2["offset"].as_u64().unwrap(), 2);
    assert!(page2["has_more"].as_bool().unwrap_or(false));
    let page2_items = page2["items"].as_array().unwrap();
    assert_eq!(page2_items.len(), 2);

    // Page 3 should have 1 item and no more
    let page3 = list_response_full(dir.path(), &["--limit", "2", "--offset", "4"]);
    assert!(
        !page3["has_more"].as_bool().unwrap_or(true),
        "has_more should be false at last page"
    );
    let page3_items = page3["items"].as_array().unwrap();
    assert_eq!(page3_items.len(), 1, "last page should have 1 item");

    // All IDs should be distinct across pages
    let mut all_ids: Vec<&str> = page1_items
        .iter()
        .chain(page2_items.iter())
        .chain(page3_items.iter())
        .filter_map(|i| i["id"].as_str())
        .collect();
    let total_count = all_ids.len();
    all_ids.sort_unstable();
    all_ids.dedup();
    assert_eq!(
        all_ids.len(),
        total_count,
        "all page items should have distinct IDs"
    );
}

#[test]
fn list_pagination_total_is_consistent() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    for i in 0..7 {
        create_item(dir.path(), &format!("Consistent {i}"));
    }

    // Different pages should report the same total
    let page1 = list_response_full(dir.path(), &["--limit", "3", "--offset", "0"]);
    let page2 = list_response_full(dir.path(), &["--limit", "3", "--offset", "3"]);

    assert_eq!(
        page1["total"], page2["total"],
        "total should be consistent across pages"
    );
    assert_eq!(page1["total"].as_u64().unwrap(), 7, "total should be 7");
}

#[test]
fn list_has_more_false_when_on_last_page() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    create_item(dir.path(), "Only item");

    let response = list_response_full(dir.path(), &["--limit", "10", "--offset", "0"]);
    assert!(
        !response["has_more"].as_bool().unwrap_or(true),
        "has_more must be false when limit >= total"
    );
}

#[test]
fn list_sort_by_created() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    // Create items in sequence
    let id1 = create_item(dir.path(), "First created");
    let id2 = create_item(dir.path(), "Second created");
    let id3 = create_item(dir.path(), "Third created");

    // Sort by created descending (most recent first)
    let items = list_items_filtered(dir.path(), &["--sort", "created"]);
    assert_eq!(items.len(), 3);

    // Items should be in some consistent order
    let ids: Vec<&str> = items.iter().filter_map(|i| i["id"].as_str()).collect();
    assert!(ids.contains(&id1.as_str()), "id1 should appear");
    assert!(ids.contains(&id2.as_str()), "id2 should appear");
    assert!(ids.contains(&id3.as_str()), "id3 should appear");
}

#[test]
fn list_sort_by_updated() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id1 = create_item(dir.path(), "Item Alpha");
    let id2 = create_item(dir.path(), "Item Beta");

    // Update id1 to make it more recently updated
    bn_cmd(dir.path())
        .args(["update", &id1, "--title", "Item Alpha Updated"])
        .assert()
        .success();

    let items = list_items_filtered(dir.path(), &["--sort", "updated"]);
    assert!(items.len() >= 2);

    // id1 should appear first (most recently updated)
    let first_id = items[0]["id"].as_str().unwrap_or("");
    assert_eq!(
        first_id,
        id1.as_str(),
        "most recently updated item should appear first"
    );

    // id2 should appear after id1
    let second_id = items[1]["id"].as_str().unwrap_or("");
    assert_eq!(second_id, id2.as_str());
}

#[test]
fn list_filter_combined_state_and_label() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id_open_labeled = create_item(dir.path(), "Open labeled");
    let id_open_unlabeled = create_item(dir.path(), "Open unlabeled");
    let id_doing_labeled = create_item(dir.path(), "Doing labeled");

    // Note: --label filter uses validate_label which requires [a-zA-Z0-9-_] only (no ':').
    // Use a plain label name without namespace separator.
    bn_cmd(dir.path())
        .args(["tag", &id_open_labeled, "target-release"])
        .assert()
        .success();
    bn_cmd(dir.path())
        .args(["tag", &id_doing_labeled, "target-release"])
        .assert()
        .success();
    bn_cmd(dir.path())
        .args(["do", &id_doing_labeled])
        .assert()
        .success();

    rebuild(dir.path());

    // Filter: open AND label=target-release
    let filtered = list_items_filtered(
        dir.path(),
        &["--state", "open", "--label", "target-release"],
    );

    // Only id_open_labeled should match
    assert_eq!(
        filtered.len(),
        1,
        "only 1 item should match open + target-release"
    );
    assert_eq!(
        filtered[0]["id"].as_str().unwrap(),
        id_open_labeled.as_str()
    );

    // Unused variable suppression
    let _ = id_open_unlabeled;
}

#[test]
fn list_json_response_has_required_fields() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    create_item(dir.path(), "Contract check");

    let response = list_response_full(dir.path(), &[]);
    assert!(response["items"].is_array(), "response must have 'items'");
    assert!(response["total"].is_number(), "response must have 'total'");
    assert!(response["limit"].is_number(), "response must have 'limit'");
    assert!(
        response["offset"].is_number(),
        "response must have 'offset'"
    );
    assert!(
        response["has_more"].is_boolean(),
        "response must have 'has_more'"
    );
}

// ---------------------------------------------------------------------------
// Goal auto-close / reopen policy workflow tests
// ---------------------------------------------------------------------------

#[test]
fn done_response_contains_auto_completed_parent() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let goal_id = create_item_kind(dir.path(), "Auto-close goal", "goal");
    let child_id = create_child(dir.path(), "Only child", &goal_id);

    let output = bn_cmd(dir.path())
        .args(["done", &child_id, "--json"])
        .output()
        .expect("done should not crash");

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let r = &json["results"].as_array().expect("results array")[0];

    assert_eq!(r["id"].as_str().unwrap(), child_id.as_str());
    assert_eq!(r["new_state"].as_str().unwrap(), "done");
    assert_eq!(
        r["auto_completed_parent"].as_str().unwrap_or(""),
        goal_id.as_str(),
        "auto_completed_parent should reference the goal ID"
    );
}

#[test]
fn goal_auto_close_requires_all_children_done() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let goal_id = create_item_kind(dir.path(), "Staged goal", "goal");
    let child1_id = create_child(dir.path(), "Child 1", &goal_id);
    let child2_id = create_child(dir.path(), "Child 2", &goal_id);
    let child3_id = create_child(dir.path(), "Child 3", &goal_id);

    // Done child 1 — goal still open
    bn_cmd(dir.path())
        .args(["done", &child1_id])
        .assert()
        .success();
    let goal = show_item_json(dir.path(), &goal_id);
    assert_eq!(
        goal["state"].as_str().unwrap(),
        "open",
        "goal should remain open after first child done"
    );

    // Done child 2 — goal still open
    bn_cmd(dir.path())
        .args(["done", &child2_id])
        .assert()
        .success();
    let goal = show_item_json(dir.path(), &goal_id);
    assert_eq!(
        goal["state"].as_str().unwrap(),
        "open",
        "goal should remain open after second child done"
    );

    // Done child 3 — goal should auto-close
    let output = bn_cmd(dir.path())
        .args(["done", &child3_id, "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let r = &json["results"].as_array().unwrap()[0];
    assert_eq!(
        r["auto_completed_parent"].as_str().unwrap_or(""),
        goal_id.as_str(),
        "last child done should trigger auto_completed_parent"
    );

    let goal = show_item_json(dir.path(), &goal_id);
    assert_eq!(
        goal["state"].as_str().unwrap(),
        "done",
        "goal should auto-close when all children are done"
    );
}

#[test]
fn done_on_standalone_item_has_no_auto_completed_parent() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id = create_item(dir.path(), "Standalone task");

    let output = bn_cmd(dir.path())
        .args(["done", &id, "--json"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let r = &json["results"].as_array().unwrap()[0];

    assert_eq!(r["new_state"].as_str().unwrap(), "done");
    // auto_completed_parent should be absent or null for standalone items
    let acp = &r["auto_completed_parent"];
    assert!(
        acp.is_null() || acp.is_string() == false || acp.as_str().unwrap_or("").is_empty(),
        "standalone item done should not set auto_completed_parent, got: {acp}"
    );
}

#[test]
fn goal_state_after_reopen_child_remains_done() {
    // Reopening a child does NOT auto-reopen the goal. The goal stays done.
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let goal_id = create_item_kind(dir.path(), "Stable goal", "goal");
    let child_id = create_child(dir.path(), "Sole child", &goal_id);

    // Close child → goal auto-closes
    bn_cmd(dir.path())
        .args(["done", &child_id])
        .assert()
        .success();

    let goal = show_item_json(dir.path(), &goal_id);
    assert_eq!(goal["state"].as_str().unwrap(), "done");

    // Reopen child
    bn_cmd(dir.path())
        .args(["reopen", &child_id])
        .assert()
        .success();

    // Goal should stay done (no auto-reopen behavior)
    let goal = show_item_json(dir.path(), &goal_id);
    assert_eq!(
        goal["state"].as_str().unwrap(),
        "done",
        "reopening a child should not auto-reopen the parent goal"
    );
}

#[test]
fn goal_without_children_does_not_auto_close() {
    // A goal without children should not auto-close just because it exists.
    // It should stay open until explicitly closed.
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let goal_id = create_item_kind(dir.path(), "Childless goal", "goal");

    let goal = show_item_json(dir.path(), &goal_id);
    assert_eq!(
        goal["state"].as_str().unwrap(),
        "open",
        "goal without children should stay open"
    );
}

#[test]
fn close_child_with_done_then_reopen_then_close_again() {
    // Full cycle: done → reopen → done again shows auto_completed_parent each time.
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let goal_id = create_item_kind(dir.path(), "Cycle goal", "goal");
    let child_id = create_child(dir.path(), "Cycle child", &goal_id);

    // First done
    let output = bn_cmd(dir.path())
        .args(["done", &child_id, "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    let r = &json["results"].as_array().unwrap()[0];
    assert_eq!(
        r["auto_completed_parent"].as_str().unwrap_or(""),
        goal_id.as_str()
    );

    // Manually reopen goal (child was reopened above, goal is still done)
    bn_cmd(dir.path())
        .args(["reopen", &goal_id])
        .assert()
        .success();

    // Reopen child
    bn_cmd(dir.path())
        .args(["reopen", &child_id])
        .assert()
        .success();

    // Second done — should auto-close goal again
    let output2 = bn_cmd(dir.path())
        .args(["done", &child_id, "--json"])
        .output()
        .unwrap();
    assert!(output2.status.success());
    let json2: Value = serde_json::from_slice(&output2.stdout).unwrap();
    let r2 = &json2["results"].as_array().unwrap()[0];
    assert_eq!(r2["new_state"].as_str().unwrap(), "done");
    assert_eq!(
        r2["auto_completed_parent"].as_str().unwrap_or(""),
        goal_id.as_str(),
        "second done should also trigger auto_completed_parent"
    );
}
