//! E2E tests for reporting and interoperability commands:
//! `bn stats`, `bn export`, `bn import`.
//!
//! Covers: stats JSON schema, export JSONL format, import round-trip,
//! and graceful handling of malformed import input.

use assert_cmd::Command;
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test harness helpers
// ---------------------------------------------------------------------------

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
    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON from create");
    json["id"].as_str().expect("id must exist").to_string()
}

fn done_item(dir: &Path, id: &str) {
    bn_cmd(dir).args(["done", id]).assert().success();
}

fn rebuild(dir: &Path) {
    bn_cmd(dir).args(["admin", "rebuild"]).assert().success();
}

// ---------------------------------------------------------------------------
// bn stats tests
// ---------------------------------------------------------------------------

#[test]
fn stats_json_output_has_expected_top_level_fields() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let id1 = create_item(dir.path(), "Open item alpha");
    let id2 = create_item(dir.path(), "Open item beta");
    let id3 = create_item(dir.path(), "Done item gamma");
    drop(id1);
    drop(id2);
    done_item(dir.path(), &id3);

    let output = bn_cmd(dir.path())
        .args(["stats", "--json"])
        .output()
        .expect("stats should not crash");

    assert!(
        output.status.success(),
        "bn stats --json failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stats: Value =
        serde_json::from_slice(&output.stdout).expect("stats --json must produce valid JSON");

    // Top-level fields must be present
    assert!(stats["by_state"].is_object(), "by_state must be an object");
    assert!(stats["by_kind"].is_object(), "by_kind must be an object");
    assert!(
        stats["by_urgency"].is_object(),
        "by_urgency must be an object"
    );
    assert!(
        stats["events_by_type"].is_object(),
        "events_by_type must be an object"
    );
    assert!(
        stats["events_by_agent"].is_object(),
        "events_by_agent must be an object"
    );
    assert!(
        stats["shard_bytes"].is_number(),
        "shard_bytes must be a number"
    );
    assert!(stats["velocity"].is_object(), "velocity must be an object");
    assert!(stats["aging"].is_object(), "aging must be an object");
}

#[test]
fn stats_reflects_item_counts_by_state() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    create_item(dir.path(), "Open item A");
    create_item(dir.path(), "Open item B");
    let done_id = create_item(dir.path(), "Done item");
    done_item(dir.path(), &done_id);

    let output = bn_cmd(dir.path())
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let stats: Value = serde_json::from_slice(&output.stdout).unwrap();

    let open_count = stats["by_state"]["open"].as_u64().unwrap_or(0);
    let done_count = stats["by_state"]["done"].as_u64().unwrap_or(0);

    assert_eq!(open_count, 2, "expected 2 open items");
    assert_eq!(done_count, 1, "expected 1 done item");
}

#[test]
fn stats_velocity_fields_are_non_negative() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    create_item(dir.path(), "Velocity test item");

    let output = bn_cmd(dir.path())
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let stats: Value = serde_json::from_slice(&output.stdout).unwrap();
    let velocity = &stats["velocity"];

    assert!(
        velocity["opened_7d"].as_u64().is_some(),
        "opened_7d must be a non-negative integer"
    );
    assert!(
        velocity["closed_7d"].as_u64().is_some(),
        "closed_7d must be a non-negative integer"
    );
    assert!(
        velocity["opened_30d"].as_u64().is_some(),
        "opened_30d must be a non-negative integer"
    );
    assert!(
        velocity["closed_30d"].as_u64().is_some(),
        "closed_30d must be a non-negative integer"
    );
}

#[test]
fn stats_aging_fields_present() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    create_item(dir.path(), "Aging test item");

    let output = bn_cmd(dir.path())
        .args(["stats", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let stats: Value = serde_json::from_slice(&output.stdout).unwrap();
    let aging = &stats["aging"];

    assert!(
        aging["avg_open_age_days"].is_number(),
        "avg_open_age_days must be a number"
    );
    assert!(
        aging["stale_count_30d"].as_u64().is_some(),
        "stale_count_30d must be a non-negative integer"
    );
}

#[test]
fn stats_fails_without_projection() {
    let dir = TempDir::new().unwrap();
    // Do NOT init: no .bones directory, no projection
    let output = bn_cmd(dir.path())
        .args(["stats", "--json"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "stats should fail when no project is initialized"
    );
}

#[test]
fn stats_human_output_contains_known_sections() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    create_item(dir.path(), "Human stats test");

    // Force pretty output mode (tests run without a TTY, so default is text)
    let output = bn_cmd(dir.path())
        .env("FORMAT", "pretty")
        .args(["stats"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Items by state"),
        "human output should contain 'Items by state'"
    );
    assert!(
        stdout.contains("Items by kind"),
        "human output should contain 'Items by kind'"
    );
}

// ---------------------------------------------------------------------------
// bn export tests
// ---------------------------------------------------------------------------

#[test]
fn export_produces_valid_jsonl_with_expected_fields() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    create_item(dir.path(), "Export item one");
    create_item(dir.path(), "Export item two");

    let export_path = dir.path().join("export.jsonl");

    bn_cmd(dir.path())
        .args(["export", "--output", export_path.to_str().unwrap()])
        .assert()
        .success();

    let content = std::fs::read_to_string(&export_path).expect("export file must exist");
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    assert_eq!(lines.len(), 2, "expected 2 exported events (one per item)");

    for line in &lines {
        let record: Value =
            serde_json::from_str(line).expect("each export line must be valid JSON");

        // Required fields per schema
        assert!(
            record["timestamp"].is_number(),
            "timestamp must be a number"
        );
        assert!(record["agent"].is_string(), "agent must be a string");
        assert!(record["type"].is_string(), "type must be a string");
        assert!(record["item_id"].is_string(), "item_id must be a string");
        assert!(record["data"].is_object(), "data must be an object");

        // Event type for creates
        assert_eq!(
            record["type"].as_str().unwrap(),
            "item.create",
            "type must be item.create for newly created items"
        );
    }
}

#[test]
fn export_to_stdout_exits_successfully() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    create_item(dir.path(), "Stdout export test");

    // Export without --output (goes to stdout)
    let output = bn_cmd(dir.path()).args(["export"]).output().unwrap();

    assert!(
        output.status.success(),
        "export to stdout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "stdout export should produce output"
    );

    // Verify stdout is valid JSONL
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        serde_json::from_str::<Value>(line).expect("each stdout export line must be valid JSON");
    }
}

#[test]
fn export_preserves_ordering_from_shards() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let ids: Vec<String> = (0..5)
        .map(|i| create_item(dir.path(), &format!("Ordered item {i}")))
        .collect();

    let export_path = dir.path().join("ordered.jsonl");
    bn_cmd(dir.path())
        .args(["export", "--output", export_path.to_str().unwrap()])
        .assert()
        .success();

    let content = std::fs::read_to_string(&export_path).unwrap();
    let exported_ids: Vec<String> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["item_id"].as_str().unwrap().to_string()
        })
        .collect();

    assert_eq!(
        exported_ids, ids,
        "exported item_ids must preserve shard order"
    );
}

// ---------------------------------------------------------------------------
// bn import round-trip tests
// ---------------------------------------------------------------------------

#[test]
fn export_import_roundtrip_preserves_item_count() {
    let source_dir = TempDir::new().unwrap();
    init_project(source_dir.path());

    let item_count = 5_usize;
    for i in 0..item_count {
        create_item(source_dir.path(), &format!("Roundtrip item {i}"));
    }

    // Export to JSONL
    let export_path = source_dir.path().join("export.jsonl");
    bn_cmd(source_dir.path())
        .args(["export", "--output", export_path.to_str().unwrap()])
        .assert()
        .success();

    // Verify export has the right line count
    let content = std::fs::read_to_string(&export_path).unwrap();
    let line_count = content.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(
        line_count, item_count,
        "export should have {item_count} lines"
    );

    // Import into a fresh project
    let dest_dir = TempDir::new().unwrap();
    init_project(dest_dir.path());

    bn_cmd(dest_dir.path())
        .args([
            "import",
            "--jsonl",
            "--input",
            export_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    // Rebuild projection so list can see imported items
    rebuild(dest_dir.path());

    // Verify item count matches
    let list_output = bn_cmd(dest_dir.path())
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert!(list_output.status.success());

    let list_json: Value = serde_json::from_slice(&list_output.stdout).unwrap();
    let total = list_json["total"].as_u64().unwrap_or(0) as usize;

    assert_eq!(
        total, item_count,
        "imported project should have {item_count} items, got {total}"
    );
}

#[test]
fn export_import_roundtrip_preserves_item_ids() {
    let source_dir = TempDir::new().unwrap();
    init_project(source_dir.path());

    let original_ids: Vec<String> = (0..3)
        .map(|i| create_item(source_dir.path(), &format!("ID roundtrip {i}")))
        .collect();

    let export_path = source_dir.path().join("ids_export.jsonl");
    bn_cmd(source_dir.path())
        .args(["export", "--output", export_path.to_str().unwrap()])
        .assert()
        .success();

    let dest_dir = TempDir::new().unwrap();
    init_project(dest_dir.path());

    bn_cmd(dest_dir.path())
        .args([
            "import",
            "--jsonl",
            "--input",
            export_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    rebuild(dest_dir.path());

    // Verify each original item ID is present in the imported project
    for id in &original_ids {
        let show_output = bn_cmd(dest_dir.path())
            .args(["show", id, "--json"])
            .output()
            .unwrap();
        assert!(
            show_output.status.success(),
            "item {id} should be present after import"
        );
    }
}

#[test]
fn export_import_roundtrip_preserves_item_titles() {
    let source_dir = TempDir::new().unwrap();
    init_project(source_dir.path());

    let titles = vec!["Alpha task", "Beta goal", "Gamma bug"];
    for title in &titles {
        create_item(source_dir.path(), title);
    }

    let export_path = source_dir.path().join("titles_export.jsonl");
    bn_cmd(source_dir.path())
        .args(["export", "--output", export_path.to_str().unwrap()])
        .assert()
        .success();

    let dest_dir = TempDir::new().unwrap();
    init_project(dest_dir.path());

    bn_cmd(dest_dir.path())
        .args([
            "import",
            "--jsonl",
            "--input",
            export_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    rebuild(dest_dir.path());

    let list_output = bn_cmd(dest_dir.path())
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert!(list_output.status.success());

    let list_json: Value = serde_json::from_slice(&list_output.stdout).unwrap();
    let items = list_json["items"].as_array().expect("items must be array");

    let imported_titles: Vec<&str> = items
        .iter()
        .filter_map(|item| item["title"].as_str())
        .collect();

    for expected_title in &titles {
        assert!(
            imported_titles.contains(expected_title),
            "title '{expected_title}' should be present after import"
        );
    }
}

#[test]
fn import_json_output_reports_import_summary() {
    let source_dir = TempDir::new().unwrap();
    init_project(source_dir.path());

    for i in 0..3 {
        create_item(source_dir.path(), &format!("Summary item {i}"));
    }

    let export_path = source_dir.path().join("summary_export.jsonl");
    bn_cmd(source_dir.path())
        .args(["export", "--output", export_path.to_str().unwrap()])
        .assert()
        .success();

    let dest_dir = TempDir::new().unwrap();
    init_project(dest_dir.path());

    let import_output = bn_cmd(dest_dir.path())
        .args([
            "import",
            "--jsonl",
            "--input",
            export_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    assert!(import_output.status.success());
    let report: Value = serde_json::from_slice(&import_output.stdout)
        .expect("import --json must produce valid JSON");

    assert_eq!(
        report["mode"].as_str().unwrap_or(""),
        "jsonl",
        "mode must be 'jsonl'"
    );
    assert_eq!(
        report["imported"].as_u64().unwrap_or(0),
        3,
        "should report 3 imported events"
    );
    assert_eq!(
        report["skipped_invalid"].as_u64().unwrap_or(1),
        0,
        "should report 0 skipped lines"
    );
}

// ---------------------------------------------------------------------------
// Error-path validation for malformed import sources
// ---------------------------------------------------------------------------

#[test]
fn import_jsonl_skips_malformed_lines_and_reports_to_stderr() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let bad_file = dir.path().join("bad.jsonl");
    std::fs::write(&bad_file, "not valid json\n{incomplete json\n").unwrap();

    let result = bn_cmd(dir.path())
        .args(["import", "--jsonl", "--input", bad_file.to_str().unwrap()])
        .output()
        .unwrap();

    // Malformed lines are skipped gracefully — command still exits 0
    assert!(
        result.status.success(),
        "import of malformed JSONL should succeed (skipping bad lines)"
    );

    // Stderr should contain diagnostics about the skipped lines
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("skip line") || stderr.contains("invalid JSON"),
        "stderr should report which lines were skipped; got: {stderr}"
    );
}

#[test]
fn import_jsonl_reports_skipped_count_in_summary() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    // Mix valid and invalid lines
    let valid_line = r#"{"timestamp":1771563284510882,"agent":"test-agent","type":"item.create","item_id":"bn-abc","data":{"kind":"task","title":"Valid item"}}"#;
    let bad_line = "not json at all";
    let mixed_file = dir.path().join("mixed.jsonl");
    std::fs::write(&mixed_file, format!("{valid_line}\n{bad_line}\n")).unwrap();

    let result = bn_cmd(dir.path())
        .args([
            "import",
            "--jsonl",
            "--input",
            mixed_file.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();

    assert!(result.status.success());

    let report: Value = serde_json::from_slice(&result.stdout).expect("must produce valid JSON");
    assert_eq!(
        report["total_lines"].as_u64().unwrap_or(0),
        2,
        "total_lines should be 2"
    );
    assert_eq!(
        report["skipped_invalid"].as_u64().unwrap_or(0),
        1,
        "skipped_invalid should be 1"
    );
    assert_eq!(
        report["imported"].as_u64().unwrap_or(0),
        1,
        "imported should be 1"
    );
}

#[test]
fn import_without_mode_flag_fails_with_useful_error() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    // Provide a file path but no --jsonl or --github flag
    let result = bn_cmd(dir.path()).args(["import"]).output().unwrap();

    assert!(!result.status.success(), "import with no flags should fail");

    let stderr = String::from_utf8_lossy(&result.stderr);
    // Should tell user what flags are available
    assert!(
        stderr.contains("--github") || stderr.contains("--jsonl") || stderr.contains("missing"),
        "error should mention available flags; got: {stderr}"
    );
}

#[test]
fn import_nonexistent_file_fails_gracefully() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let result = bn_cmd(dir.path())
        .args([
            "import",
            "--jsonl",
            "--input",
            "/nonexistent/path/to/file.jsonl",
        ])
        .output()
        .unwrap();

    assert!(
        !result.status.success(),
        "import of nonexistent file should fail"
    );

    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        !stderr.trim().is_empty(),
        "error should produce actionable stderr message"
    );
}
