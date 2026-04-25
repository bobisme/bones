//! E2E checks for the chief-facing JSON provider contract.

use assert_cmd::Command;
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

fn init_project(dir: &Path) {
    bn_cmd(dir).args(["init"]).assert().success();
}

fn create_item(dir: &Path, title: &str, kind: Option<&str>) -> String {
    let mut args = vec!["create", "--title", title, "--json"];
    if let Some(kind) = kind {
        args.extend(["--kind", kind]);
    }

    let output = bn_cmd(dir).args(args).output().expect("create runs");
    assert!(
        output.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("create json");
    assert_eq!(json["schema_version"], 1);
    json["id"].as_str().expect("id").to_string()
}

fn json_command(dir: &Path, args: &[&str]) -> Value {
    let output = bn_cmd(dir).args(args).output().expect("command runs");
    assert!(
        output.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("valid json")
}

#[test]
fn context_json_contains_chief_snapshot_fields() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let goal = create_item(dir.path(), "Chief provider transition", Some("goal"));
    let blocker = create_item(dir.path(), "Decide provider schema", None);
    let blocked = create_item(dir.path(), "Wire chief context", None);

    bn_cmd(dir.path())
        .args(["dep", "add", &blocker, "--blocks", &blocked])
        .assert()
        .success();
    bn_cmd(dir.path())
        .args(["admin", "rebuild"])
        .assert()
        .success();

    let json = json_command(dir.path(), &["context", "--json"]);
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["provider"], "bones");
    assert_eq!(json["command"], "bn context --format json");
    assert!(json["generated_at"].as_str().is_some());
    assert!(json["summary"]["open_count"].as_u64().unwrap() >= 3);
    assert_eq!(json["summary"]["blocked_count"], 1);

    let recommended = &json["recommended_next"];
    assert!(recommended["id"].as_str().is_some());
    assert!(recommended["why"].as_array().is_some());

    let blocked_items = json["blocked"].as_array().expect("blocked array");
    let blocked_row = blocked_items
        .iter()
        .find(|item| item["id"] == blocked)
        .expect("blocked item present");
    assert_eq!(blocked_row["blocked_by"], serde_json::json!([blocker]));

    let goals = json["active_goals"].as_array().expect("goals array");
    assert!(goals.iter().any(|item| item["id"] == goal));
    assert_eq!(json["provenance"]["provider"], "bones");
    assert_eq!(json["provenance"]["command"], "bn context --format json");
    assert!(
        json["provenance"]["projection_last_event_hash"]
            .as_str()
            .is_some()
    );
}

#[test]
fn list_status_filter_accepts_repeated_and_virtual_blocked() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let blocker = create_item(dir.path(), "Open blocker", None);
    let blocked = create_item(dir.path(), "Blocked task", None);
    let doing = create_item(dir.path(), "Doing task", None);

    bn_cmd(dir.path()).args(["do", &doing]).assert().success();
    bn_cmd(dir.path())
        .args(["dep", "add", &blocker, "--blocks", &blocked])
        .assert()
        .success();
    bn_cmd(dir.path())
        .args(["admin", "rebuild"])
        .assert()
        .success();

    let repeated = json_command(
        dir.path(),
        &[
            "list",
            "--json",
            "--status",
            "open",
            "--status",
            "doing,blocked",
        ],
    );
    let ids: Vec<&str> = repeated["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    assert!(ids.contains(&blocker.as_str()));
    assert!(ids.contains(&blocked.as_str()));
    assert!(ids.contains(&doing.as_str()));

    let blocked_only = json_command(dir.path(), &["list", "--json", "--status", "blocked"]);
    let blocked_ids: Vec<&str> = blocked_only["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    assert_eq!(blocked_ids, vec![blocked.as_str()]);
}

#[test]
fn mutation_json_includes_schema_and_current_state() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let item = create_item(dir.path(), "Mutation contract", None);
    let doing = json_command(dir.path(), &["do", &item, "--json"]);
    assert_eq!(doing["schema_version"], 1);
    let doing_result = &doing["results"][0];
    assert_eq!(doing_result["id"], item);
    assert_eq!(doing_result["previous_state"], "open");
    assert_eq!(doing_result["state"], "doing");
    assert_eq!(doing_result["new_state"], "doing");

    let comment = json_command(
        dir.path(),
        &["bone", "comment", "add", &item, "contract note", "--json"],
    );
    assert_eq!(comment["schema_version"], 1);
    assert_eq!(comment["id"], item);
    assert_eq!(comment["item_id"], item);

    let goal = create_item(dir.path(), "Mutation goal", Some("goal"));
    let moved = json_command(
        dir.path(),
        &["bone", "move", &item, "--parent", &goal, "--json"],
    );
    assert_eq!(moved["schema_version"], 1);
    assert_eq!(moved["id"], item);
    assert_eq!(moved["item_id"], item);
    assert!(moved["previous_parent_id"].is_null());
    assert_eq!(moved["parent_id"], goal);

    let done = json_command(dir.path(), &["done", &item, "--json"]);
    assert_eq!(done["schema_version"], 1);
    let done_result = &done["results"][0];
    assert_eq!(done_result["previous_state"], "doing");
    assert_eq!(done_result["state"], "done");
    assert_eq!(done_result["new_state"], "done");
}
