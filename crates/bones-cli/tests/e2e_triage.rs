//! E2E CLI workflow tests for triage commands (bn-if2):
//! `bn next`, `bn triage`, `bn plan`, `bn health`, `bn cycles`,
//! and feedback commands `bn did` / `bn skip`.
//!
//! Each test runs `bones-cli` (the `bn` binary) as a subprocess in an isolated
//! temp directory.  Tests cover both human-readable text and `--json` output.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test Harness
// ---------------------------------------------------------------------------

/// Build a Command targeting the `bn` binary, rooted in `dir`.
fn bn_cmd(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("bn").expect("bn binary must exist");
    cmd.current_dir(dir);
    // Provide a default agent so mutating commands don't fail.
    cmd.env("AGENT", "test-agent");
    // Suppress tracing output that goes to stderr.
    cmd.env("BONES_LOG", "error");
    cmd
}

/// Build a Command that forces human-readable output.
///
/// When running under `cargo test`, stdout is piped (not a TTY), which causes
/// the CLI to default to JSON mode.  Setting `BONES_OUTPUT=human` forces text
/// output so we can assert on human-readable content.
fn bn_human_cmd(dir: &Path) -> Command {
    let mut cmd = bn_cmd(dir);
    cmd.env("BONES_OUTPUT", "human");
    cmd
}

/// Initialize a bones project in `dir`.
fn init_project(dir: &Path) {
    bn_cmd(dir).args(["init"]).assert().success();
}

/// Create an item via CLI; return its full ID (e.g. "bn-abc123").
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

/// Add a blocking dependency: `blocker` blocks `blocked`.
///
/// Meaning: `blocked` cannot start until `blocker` is done.
fn add_dep_blocks(dir: &Path, blocker: &str, blocked: &str) {
    let output = bn_cmd(dir)
        .args(["dep", "add", blocker, "--blocks", blocked])
        .output()
        .expect("dep add should not crash");
    assert!(
        output.status.success(),
        "dep add {} --blocks {} failed: {}",
        blocker,
        blocked,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Rebuild the projection so all events are visible to query commands.
fn rebuild(dir: &Path) {
    bn_cmd(dir).args(["rebuild"]).assert().success();
}

/// Run `bn <subcommand> --json` and return the parsed JSON `Value`.
fn run_json(dir: &Path, subcmd: &[&str]) -> Value {
    let mut args = subcmd.to_vec();
    args.push("--json");
    let output = bn_cmd(dir)
        .args(&args)
        .output()
        .unwrap_or_else(|_| panic!("{} should not crash", subcmd.join(" ")));
    assert!(
        output.status.success(),
        "{} failed: {}",
        subcmd.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{} --json should produce valid JSON", subcmd.join(" ")))
}

/// Set up a representative project graph with 10+ items and a dependency chain.
///
/// Graph structure:
/// ```
///   a (Design API)     ← unblocked
///   b (Implement backend) ← blocked by a
///   c (Write tests)    ← blocked by b
///   d (Deploy)         ← blocked by b AND c
///   independent[0..6]  ← all unblocked
/// ```
///
/// Unblocked: a, independent[0..5] (7 items).
/// Blocked: b (by a), c (by b), d (by b, c).
fn setup_triage_graph(dir: &Path) -> TriageGraph {
    init_project(dir);

    let a = create_item(dir, "Design API");
    let b = create_item(dir, "Implement backend");
    let c = create_item(dir, "Write tests");
    let d = create_item(dir, "Deploy");

    // Dependency chain: a → b → c → d (and b → d).
    add_dep_blocks(dir, &a, &b);
    add_dep_blocks(dir, &b, &c);
    add_dep_blocks(dir, &b, &d);
    add_dep_blocks(dir, &c, &d);

    let mut independent = Vec::new();
    for i in 0..6 {
        let id = create_item(dir, &format!("Independent task {i}"));
        independent.push(id);
    }

    // Rebuild so dependency changes are visible to query commands.
    rebuild(dir);

    TriageGraph {
        a,
        b,
        c,
        d,
        independent,
    }
}

/// The IDs created by `setup_triage_graph`.
struct TriageGraph {
    /// Root unblocked item.
    a: String,
    /// Blocked by a.
    #[allow(dead_code)]
    b: String,
    /// Blocked by b.
    #[allow(dead_code)]
    c: String,
    /// Blocked by b and c (leaf).
    #[allow(dead_code)]
    d: String,
    /// Six items with no dependencies.
    #[allow(dead_code)]
    independent: Vec<String>,
}

// ===========================================================================
// bn next
// ===========================================================================

#[test]
fn next_returns_unblocked_item() {
    let dir = TempDir::new().unwrap();
    let graph = setup_triage_graph(dir.path());

    let next = run_json(dir.path(), &["next"]);

    // Must be a single-item object with required fields.
    assert!(next["id"].is_string(), "next --json must have 'id'");
    assert!(next["title"].is_string(), "next --json must have 'title'");
    assert!(next["score"].is_number(), "next --json must have 'score'");
    assert!(
        next["explanation"].is_string(),
        "next --json must have 'explanation'"
    );

    // The recommended item must NOT be one of the blocked items.
    let id = next["id"].as_str().unwrap();
    assert_ne!(
        id, graph.b,
        "bn next should not recommend a blocked item (b is blocked by a)"
    );
    assert_ne!(
        id, graph.c,
        "bn next should not recommend a blocked item (c is blocked by b)"
    );
    assert_ne!(
        id, graph.d,
        "bn next should not recommend a blocked item (d is blocked by b and c)"
    );
}

#[test]
fn next_human_output_is_non_empty() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    // BONES_OUTPUT=human required: in test context stdout is piped (non-TTY),
    // so the CLI defaults to JSON mode unless overridden.
    bn_human_cmd(dir.path())
        .args(["next"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty().not());
}

#[test]
fn next_empty_project_returns_message() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    rebuild(dir.path()); // projection DB must exist for triage commands

    // No items — should still succeed but signal "nothing ready".
    let output = bn_cmd(dir.path()).args(["next", "--json"]).output().unwrap();
    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    // EmptyNext response: { message: "..." }
    assert!(
        json["message"].is_string(),
        "empty project should return a 'message' field: {json}"
    );
}

#[test]
fn next_json_score_is_non_negative() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let next = run_json(dir.path(), &["next"]);
    let score = next["score"].as_f64().expect("score must be a number");
    assert!(score >= 0.0, "score must be non-negative, got {score}");
}

#[test]
fn next_json_explanation_is_non_empty() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let next = run_json(dir.path(), &["next"]);
    let explanation = next["explanation"].as_str().unwrap_or("");
    assert!(
        !explanation.is_empty(),
        "explanation should be non-empty text"
    );
}

#[test]
fn next_agent_slots_returns_assignments() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let output = bn_cmd(dir.path())
        .args(["next", "--agent", "3", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "next --agent 3 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    // Should return NextAssignments: { assignments: [...] }
    let assignments = json["assignments"].as_array().expect("should have 'assignments' array");
    assert!(
        !assignments.is_empty(),
        "assignments should be non-empty for a project with items"
    );

    // Each assignment must have required fields.
    for assignment in assignments {
        assert!(assignment["agent_slot"].is_number(), "agent_slot must be a number");
        assert!(assignment["id"].is_string(), "assignment must have 'id'");
        assert!(assignment["title"].is_string(), "assignment must have 'title'");
        assert!(assignment["score"].is_number(), "assignment must have 'score'");
        assert!(assignment["explanation"].is_string(), "assignment must have 'explanation'");
    }
}

#[test]
fn next_deterministic_for_stable_graph() {
    // Run `bn next` twice on the same project; the top recommendation should be identical.
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let first = run_json(dir.path(), &["next"]);
    let second = run_json(dir.path(), &["next"]);

    assert_eq!(
        first["id"], second["id"],
        "bn next should be deterministic for a stable graph"
    );
    assert_eq!(
        first["score"], second["score"],
        "bn next score should be deterministic"
    );
}

// ===========================================================================
// bn triage
// ===========================================================================

#[test]
fn triage_produces_json_array() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let output = bn_cmd(dir.path())
        .args(["triage", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "triage failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert!(json.is_array(), "triage --json should return an array");

    let rows = json.as_array().unwrap();
    assert!(!rows.is_empty(), "triage should return at least one row");
}

#[test]
fn triage_json_row_schema() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let json = run_json(dir.path(), &["triage"]);
    let rows = json.as_array().expect("triage --json must be an array");

    for row in rows {
        assert!(row["id"].is_string(), "triage row must have 'id'");
        assert!(row["title"].is_string(), "triage row must have 'title'");
        assert!(row["score"].is_number(), "triage row must have 'score'");
        assert!(row["section"].is_string(), "triage row must have 'section'");
    }
}

#[test]
fn triage_json_sections_are_known_values() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let json = run_json(dir.path(), &["triage"]);
    let rows = json.as_array().expect("triage --json must be an array");

    let valid_sections = ["top_pick", "blocker", "quick_win", "cycle"];
    for row in rows {
        let section = row["section"].as_str().unwrap_or("");
        assert!(
            valid_sections.contains(&section),
            "unexpected triage section '{section}'"
        );
    }
}

#[test]
fn triage_human_output_contains_sections() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    bn_human_cmd(dir.path())
        .args(["triage"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Top Picks"))
        .stdout(predicate::str::contains("Blockers"))
        .stdout(predicate::str::contains("Quick Wins"))
        .stdout(predicate::str::contains("Cycles"));
}

#[test]
fn triage_human_output_does_not_contain_errors() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    bn_human_cmd(dir.path())
        .args(["triage"])
        .assert()
        .success()
        .stderr(predicate::str::contains("error").not());
}

#[test]
fn triage_empty_project_succeeds() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    rebuild(dir.path()); // projection DB must exist for triage commands

    bn_cmd(dir.path()).args(["triage", "--json"]).assert().success();
}

#[test]
fn triage_deterministic_for_stable_graph() {
    // Run `bn triage --json` twice; the IDs and sections should match.
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let first = run_json(dir.path(), &["triage"]);
    let second = run_json(dir.path(), &["triage"]);

    let first_ids: Vec<&str> = first
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap_or(""))
        .collect();
    let second_ids: Vec<&str> = second
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap_or(""))
        .collect();

    assert_eq!(
        first_ids, second_ids,
        "bn triage should be deterministic for a stable graph"
    );
}

// ===========================================================================
// bn plan
// ===========================================================================

#[test]
fn plan_shows_execution_layers() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let plan = run_json(dir.path(), &["plan"]);

    assert!(
        plan["layers"].is_array(),
        "plan --json must have 'layers' array"
    );
    let layers = plan["layers"].as_array().unwrap();
    assert!(
        layers.len() > 1,
        "dependency graph should produce multiple plan layers, got {}",
        layers.len()
    );
}

#[test]
fn plan_layer_one_contains_only_unblocked_items() {
    let dir = TempDir::new().unwrap();
    let graph = setup_triage_graph(dir.path());

    let plan = run_json(dir.path(), &["plan"]);
    let layers = plan["layers"].as_array().unwrap();
    assert!(!layers.is_empty(), "plan must have at least one layer");

    let layer_one: Vec<&str> = layers[0]
        .as_array()
        .expect("layer must be an array")
        .iter()
        .map(|v| v.as_str().unwrap_or(""))
        .collect();

    // Layer 1 must include the root unblocked item 'a'.
    assert!(
        layer_one.contains(&graph.a.as_str()),
        "layer 1 must contain the root item 'a' ({}), got: {layer_one:?}",
        graph.a
    );

    // Items in layer 1 must NOT include the blocked items b, c, or d.
    assert!(
        !layer_one.contains(&graph.b.as_str()),
        "layer 1 must not contain blocked item 'b' ({})",
        graph.b
    );
    assert!(
        !layer_one.contains(&graph.c.as_str()),
        "layer 1 must not contain blocked item 'c' ({})",
        graph.c
    );
    assert!(
        !layer_one.contains(&graph.d.as_str()),
        "layer 1 must not contain blocked item 'd' ({})",
        graph.d
    );
}

#[test]
fn plan_all_items_appear_exactly_once() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let plan = run_json(dir.path(), &["plan"]);
    let layers = plan["layers"].as_array().unwrap();

    let mut all_ids: Vec<&str> = layers
        .iter()
        .flat_map(|layer| {
            layer
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap_or(""))
        })
        .collect();

    let total = all_ids.len();
    all_ids.sort_unstable();
    all_ids.dedup();
    assert_eq!(
        all_ids.len(),
        total,
        "each item should appear exactly once in the plan layers"
    );
    // 10 items total: a, b, c, d + 6 independent.
    assert_eq!(total, 10, "plan should cover all 10 items");
}

#[test]
fn plan_human_output_shows_layers() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    bn_human_cmd(dir.path())
        .args(["plan"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Layer 1"))
        .stdout(predicate::str::contains("Layer 2"));
}

#[test]
fn plan_empty_project_succeeds() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    rebuild(dir.path()); // projection DB must exist for plan command

    let plan = run_json(dir.path(), &["plan"]);
    assert!(plan["layers"].is_array());
    assert!(
        plan["layers"].as_array().unwrap().is_empty(),
        "empty project should have no layers"
    );
}

#[test]
fn plan_human_output_empty_project_succeeds() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    rebuild(dir.path()); // projection DB must exist for plan command

    bn_human_cmd(dir.path())
        .args(["plan"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no open items"));
}

#[test]
fn plan_deterministic_for_stable_graph() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let first = run_json(dir.path(), &["plan"]);
    let second = run_json(dir.path(), &["plan"]);

    assert_eq!(
        first, second,
        "bn plan should be deterministic for a stable graph"
    );
}

// ===========================================================================
// bn health
// ===========================================================================

#[test]
fn health_shows_metrics_json() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let health = run_json(dir.path(), &["health"]);

    assert!(
        health["density"].is_number(),
        "health must have 'density' field"
    );
    assert!(
        health["scc_count"].is_number(),
        "health must have 'scc_count' field"
    );
    assert!(
        health["critical_path_length"].is_number(),
        "health must have 'critical_path_length' field"
    );
    assert!(
        health["blocker_count"].is_number(),
        "health must have 'blocker_count' field"
    );
}

#[test]
fn health_density_is_between_zero_and_one() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let health = run_json(dir.path(), &["health"]);
    let density = health["density"].as_f64().expect("density must be a number");
    assert!(
        (0.0..=1.0).contains(&density),
        "density must be in [0, 1], got {density}"
    );
}

#[test]
fn health_critical_path_length_is_positive_for_dependency_graph() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let health = run_json(dir.path(), &["health"]);
    let critical_path = health["critical_path_length"]
        .as_u64()
        .expect("critical_path_length must be a non-negative integer");
    assert!(
        critical_path >= 1,
        "dependency chain should have critical path length >= 1, got {critical_path}"
    );
}

#[test]
fn health_human_output_shows_dashboard() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    bn_human_cmd(dir.path())
        .args(["health"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Project health dashboard"))
        .stdout(predicate::str::contains("density"))
        .stdout(predicate::str::contains("critical_path_length"));
}

#[test]
fn health_empty_project_succeeds() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    rebuild(dir.path()); // projection DB must exist for health command

    let health = run_json(dir.path(), &["health"]);
    assert!(health["density"].is_number());
    // Empty project: density is 0.0 (no edges).
    let density = health["density"].as_f64().unwrap();
    assert_eq!(density, 0.0, "empty project should have density 0.0");
}

#[test]
fn health_scc_count_equals_node_count_for_dag() {
    // In a DAG every node is its own trivial SCC, so scc_count == node_count.
    // Our setup_triage_graph creates 10 items (a, b, c, d, independent[0..5]).
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let health = run_json(dir.path(), &["health"]);
    let scc_count = health["scc_count"].as_u64().expect("scc_count must be a number");
    assert_eq!(
        scc_count, 10,
        "DAG with 10 nodes should have scc_count == 10 (each node is its own SCC)"
    );
}

// ===========================================================================
// bn cycles
// ===========================================================================

#[test]
fn cycles_reports_clean_for_dag() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let result = run_json(dir.path(), &["cycles"]);

    assert!(
        result["cycles"].is_array(),
        "cycles --json must have 'cycles' array"
    );
    let cycles = result["cycles"].as_array().unwrap();
    assert!(
        cycles.is_empty(),
        "DAG should have no cycles, got {cycles:?}"
    );
}

#[test]
fn cycles_human_output_clean_dag() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    bn_human_cmd(dir.path())
        .args(["cycles"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No dependency cycles found."));
}

#[test]
fn cycles_empty_project_returns_empty_array() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());
    rebuild(dir.path()); // projection DB must exist for cycles command

    let result = run_json(dir.path(), &["cycles"]);
    let cycles = result["cycles"].as_array().expect("'cycles' must be an array");
    assert!(cycles.is_empty(), "empty project has no cycles");
}

#[test]
fn dep_add_prevents_cycle_creation() {
    // The CLI validates `dep add` and rejects edges that would form a cycle.
    // This ensures the `cycles` command always reports a clean graph in
    // normal usage.
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let x = create_item(dir.path(), "X");
    let y = create_item(dir.path(), "Y");

    // x blocks y (succeeds).
    add_dep_blocks(dir.path(), &x, &y);

    // y blocks x would close a cycle — the CLI must reject this.
    let output = bn_cmd(dir.path())
        .args(["dep", "add", &y, "--blocks", &x])
        .output()
        .expect("dep add should not crash");
    assert!(
        !output.status.success(),
        "dep add forming a cycle should be rejected by the CLI"
    );
    // Error message should mention the cycle.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cycle"),
        "error message should mention 'cycle': {stderr}"
    );

    // Verify no cycle is present.
    rebuild(dir.path());
    let result = run_json(dir.path(), &["cycles"]);
    let cycles = result["cycles"].as_array().unwrap();
    assert!(
        cycles.is_empty(),
        "cycles should still be empty after rejected dep add"
    );
}

#[test]
fn cycles_human_output_no_cycles_for_chain() {
    // A linear chain (x → y → z) has no cycles.
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    let x = create_item(dir.path(), "Alpha");
    let y = create_item(dir.path(), "Beta");
    let z = create_item(dir.path(), "Gamma");

    add_dep_blocks(dir.path(), &x, &y);
    add_dep_blocks(dir.path(), &y, &z);
    rebuild(dir.path());

    bn_human_cmd(dir.path())
        .args(["cycles"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No dependency cycles found."));
}

// ===========================================================================
// bn did / bn skip (feedback commands)
// ===========================================================================

#[test]
fn did_records_feedback_and_returns_json() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    // Use one of the unblocked items.
    let next = run_json(dir.path(), &["next"]);
    let id = next["id"].as_str().expect("next must return an id");

    let output = bn_cmd(dir.path())
        .args(["did", id, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "bn did failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(json["id"].as_str().unwrap(), id);
    assert_eq!(
        json["action"].as_str().unwrap(),
        "did",
        "'action' field should be 'did'"
    );
    assert!(
        json["agent"].is_string(),
        "'agent' field must be a string"
    );
    assert!(
        json["ts"].is_number(),
        "'ts' field must be a numeric timestamp"
    );
}

#[test]
fn skip_records_feedback_and_returns_json() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let next = run_json(dir.path(), &["next"]);
    let id = next["id"].as_str().expect("next must return an id");

    let output = bn_cmd(dir.path())
        .args(["skip", id, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "bn skip failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(json["id"].as_str().unwrap(), id);
    assert_eq!(
        json["action"].as_str().unwrap(),
        "skip",
        "'action' field should be 'skip'"
    );
    assert!(json["agent"].is_string());
    assert!(json["ts"].is_number());
}

#[test]
fn did_human_output_is_non_empty() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let next = run_json(dir.path(), &["next"]);
    let id = next["id"].as_str().expect("next must return an id");

    bn_human_cmd(dir.path())
        .args(["did", id])
        .assert()
        .success()
        .stdout(predicate::str::is_empty().not());
}

#[test]
fn skip_human_output_is_non_empty() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let next = run_json(dir.path(), &["next"]);
    let id = next["id"].as_str().expect("next must return an id");

    bn_human_cmd(dir.path())
        .args(["skip", id])
        .assert()
        .success()
        .stdout(predicate::str::is_empty().not());
}

#[test]
fn did_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["did", "bn-nonexist"])
        .assert()
        .failure();
}

#[test]
fn skip_nonexistent_item_fails() {
    let dir = TempDir::new().unwrap();
    init_project(dir.path());

    bn_cmd(dir.path())
        .args(["skip", "bn-nonexist"])
        .assert()
        .failure();
}

#[test]
fn did_then_skip_both_succeed() {
    // Feedback can be recorded multiple times (each writes a journal entry).
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let next = run_json(dir.path(), &["next"]);
    let id = next["id"].as_str().expect("next must return an id");

    bn_cmd(dir.path()).args(["did", id]).assert().success();
    bn_cmd(dir.path()).args(["skip", id]).assert().success();
}

// ===========================================================================
// Feedback alters subsequent recommendations
// ===========================================================================

#[test]
fn skip_feedback_does_not_crash_next_invocation() {
    // After skipping the top item, `bn next` should still succeed.
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let first_next = run_json(dir.path(), &["next"]);
    let id = first_next["id"].as_str().unwrap();

    bn_cmd(dir.path()).args(["skip", id]).assert().success();

    // Should still succeed after feedback.
    bn_cmd(dir.path()).args(["next"]).assert().success();
}

// ===========================================================================
// JSON output contract checks (automation consumers)
// ===========================================================================

#[test]
fn triage_json_ids_start_with_bn_prefix() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let triage = run_json(dir.path(), &["triage"]);
    let rows = triage.as_array().unwrap();

    for row in rows {
        let id = row["id"].as_str().unwrap_or("");
        assert!(
            id.starts_with("bn-"),
            "triage row id '{id}' should start with 'bn-'"
        );
    }
}

#[test]
fn plan_layers_contain_valid_item_ids() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let plan = run_json(dir.path(), &["plan"]);
    let layers = plan["layers"].as_array().unwrap();

    for layer in layers {
        for id in layer.as_array().unwrap() {
            let id_str = id.as_str().expect("layer item must be a string");
            assert!(
                id_str.starts_with("bn-"),
                "plan layer item '{id_str}' should start with 'bn-'"
            );
        }
    }
}

#[test]
fn cycles_json_schema_is_valid() {
    // Verify the `cycles --json` schema: an object with a `cycles` array field.
    // Even with no cycles, the schema must be correct.
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path()); // DAG — no cycles

    let result = run_json(dir.path(), &["cycles"]);
    assert!(
        result.is_object(),
        "cycles --json must be a JSON object, got: {result}"
    );
    assert!(
        result["cycles"].is_array(),
        "cycles --json must have a 'cycles' array"
    );
    // For a DAG, cycles are empty.
    let cycles = result["cycles"].as_array().unwrap();
    assert!(cycles.is_empty(), "DAG should have no cycles");
}

#[test]
fn next_json_id_matches_known_schema() {
    let dir = TempDir::new().unwrap();
    setup_triage_graph(dir.path());

    let next = run_json(dir.path(), &["next"]);
    let id = next["id"].as_str().expect("id must be a string");
    assert!(
        id.starts_with("bn-"),
        "next item id '{id}' should start with 'bn-'"
    );
}

// ===========================================================================
// Failure scenario: actionable diagnostics
// ===========================================================================

#[test]
fn next_without_init_fails_with_message() {
    let dir = TempDir::new().unwrap();
    // No init — projection DB doesn't exist.
    bn_cmd(dir.path())
        .args(["next"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("projection")
                .or(predicate::str::contains("not found"))
                .or(predicate::str::contains("init")),
        );
}

#[test]
fn triage_without_init_fails_with_message() {
    let dir = TempDir::new().unwrap();
    bn_cmd(dir.path())
        .args(["triage"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("projection")
                .or(predicate::str::contains("not found"))
                .or(predicate::str::contains("init")),
        );
}

#[test]
fn plan_without_init_fails_with_message() {
    let dir = TempDir::new().unwrap();
    bn_cmd(dir.path())
        .args(["plan"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("projection")
                .or(predicate::str::contains("not found"))
                .or(predicate::str::contains("init")),
        );
}

#[test]
fn health_without_init_fails_with_message() {
    let dir = TempDir::new().unwrap();
    bn_cmd(dir.path())
        .args(["health"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("projection")
                .or(predicate::str::contains("not found"))
                .or(predicate::str::contains("init")),
        );
}

#[test]
fn cycles_without_init_fails_with_message() {
    let dir = TempDir::new().unwrap();
    bn_cmd(dir.path())
        .args(["cycles"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("projection")
                .or(predicate::str::contains("not found"))
                .or(predicate::str::contains("init")),
        );
}
