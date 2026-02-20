//! `bn create` — create a new work item.
//!
//! Generates a unique item ID, emits an `item.create` event to the active
//! shard, projects it into the SQLite database, and outputs the result.

use crate::agent;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;
use clap::Args;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use bones_core::config::load_project_config;
use bones_core::db;
use bones_core::db::project;
use bones_core::event::Event;
use bones_core::event::data::{CreateData, EventData};
use bones_core::event::types::EventType;
use bones_core::event::writer;
use bones_core::model::item::Kind;
use bones_core::model::item::Size;
use bones_core::model::item::Urgency;
use bones_core::model::item_id::generate_item_id;
use bones_core::shard::ShardManager;
use bones_search::find_duplicates_with_model;
use bones_search::fusion::scoring::SearchConfig;
use bones_search::semantic::SemanticModel;
use bones_triage::graph::RawGraph;

#[derive(Args, Debug)]
pub struct CreateArgs {
    /// Title of the new item.
    #[arg(short, long)]
    pub title: String,

    /// Item kind: task, goal, or bug.
    #[arg(short, long, default_value = "task")]
    pub kind: String,

    /// T-shirt size estimate: xxs, xs, s, m, l, xl, xxl.
    #[arg(short, long)]
    pub size: Option<String>,

    /// Urgency override: urgent, default, or punt.
    #[arg(short, long)]
    pub urgency: Option<String>,

    /// Parent item ID (makes this a child of a goal).
    #[arg(long)]
    pub parent: Option<String>,

    /// Labels to attach (can be repeated: -l foo -l bar).
    #[arg(short, long)]
    pub label: Vec<String>,

    /// Description text (use '-' to read from stdin).
    #[arg(short, long)]
    pub description: Option<String>,

    /// Items this new item blocks (can be repeated).
    #[arg(long)]
    pub blocks: Vec<String>,

    /// Skip duplicate check entirely.
    #[arg(long)]
    pub force: bool,
}

/// JSON output for a created item.
#[derive(Debug, Serialize)]
struct CreateOutput {
    id: String,
    title: String,
    kind: String,
    state: String,
    urgency: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    agent: String,
    event_hash: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    duplicates: Vec<DuplicateMatch>,
}

/// A duplicate candidate match in JSON output.
#[derive(Debug, Serialize)]
struct DuplicateMatch {
    item_id: String,
    score: f32,
    classification: String,
}

/// Find the `.bones` directory by walking up from `start`.
fn find_bones_dir(start: &Path) -> Option<std::path::PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let candidate = current.join(".bones");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Read description from stdin when `-` is passed.
fn read_description(desc: &Option<String>) -> anyhow::Result<Option<String>> {
    match desc.as_deref() {
        Some("-") => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            let trimmed = buf.trim().to_string();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed))
            }
        }
        Some(s) => Ok(Some(s.to_string())),
        None => Ok(None),
    }
}

pub fn run_create(
    args: &CreateArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    // 1. Require agent identity for mutating command
    let agent = match agent::require_agent(agent_flag) {
        Ok(a) => a,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(&e.message, "Set --agent, BONES_AGENT, or AGENT", e.code),
            )?;
            anyhow::bail!("{}", e.message);
        }
    };

    // 2. Validate simple input fields early
    if let Err(e) = validate::validate_agent(&agent) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }
    if let Err(e) = validate::validate_title(&args.title) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!("{}", e.reason);
    }
    for label in &args.label {
        if let Err(e) = validate::validate_label(label) {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
        }
    }

    // 3. Parse/validate kind
    let kind: Kind = match validate::validate_kind(&args.kind) {
        Ok(k) => k,
        Err(e) => {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
        }
    };

    // 4. Parse/validate size (optional)
    let size: Option<Size> = match &args.size {
        Some(s) => Some(match validate::validate_size(s) {
            Ok(size) => size,
            Err(e) => {
                render_error(output, &e.to_cli_error())?;
                anyhow::bail!("{}", e.reason);
            }
        }),
        None => None,
    };

    // 4. Parse urgency (optional, defaults to "default")
    let urgency: Urgency = match &args.urgency {
        Some(u) => u.parse().map_err(|_| {
            let msg = format!(
                "invalid urgency '{}': expected one of urgent, default, punt",
                u
            );
            render_error(
                output,
                &CliError::with_details(
                    &msg,
                    "Use --urgency urgent, --urgency punt, etc.",
                    "invalid_urgency",
                ),
            )
            .ok();
            anyhow::anyhow!("{}", msg)
        })?,
        None => Urgency::Default,
    };

    // 5. Read description
    let description = read_description(&args.description)?;

    // 6. Find .bones directory
    let bones_dir = find_bones_dir(project_root).ok_or_else(|| {
        let msg = "Not a bones project: .bones directory not found";
        render_error(
            output,
            &CliError::with_details(
                msg,
                "Run 'bn init' to create a new bones project",
                "not_a_project",
            ),
        )
        .ok();
        anyhow::anyhow!("{}", msg)
    })?;

    // 7. Set up shard manager
    let shard_mgr = ShardManager::new(&bones_dir);
    shard_mgr
        .ensure_dirs()
        .map_err(|e| anyhow::anyhow!("shard setup failed: {e}"))?;

    // 8. Count existing items to drive adaptive ID length
    let db_path = bones_dir.join("bones.db");
    let item_count = if db_path.exists() {
        match db::query::try_open_projection(&db_path)? {
            Some(conn) => {
                let filter = db::query::ItemFilter {
                    include_deleted: true,
                    ..Default::default()
                };
                db::query::count_items(&conn, &filter).unwrap_or(0) as usize
            }
            None => 0,
        }
    } else {
        0
    };

    // 9. Validate parent exists (if specified)
    if let Some(ref parent_id) = args.parent {
        if let Err(e) = validate::validate_item_id(parent_id) {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
        }
        if db_path.exists() {
            if let Some(conn) = db::query::try_open_projection(&db_path)? {
                if !db::query::item_exists(&conn, parent_id)? {
                    let msg = format!("parent item '{}' not found", parent_id);
                    render_error(
                        output,
                        &CliError::with_details(
                            &msg,
                            "Check the item ID and ensure it exists",
                            "parent_not_found",
                        ),
                    )?;
                    anyhow::bail!("{}", msg);
                }
            }
        }
    }

    // 10. Validate --blocks targets exist
    for block_target in &args.blocks {
        if let Err(e) = validate::validate_item_id(block_target) {
            render_error(output, &e.to_cli_error())?;
            anyhow::bail!("{}", e.reason);
        }
        if db_path.exists() {
            if let Some(conn) = db::query::try_open_projection(&db_path)? {
                if !db::query::item_exists(&conn, block_target)? {
                    let msg = format!("blocks target '{}' not found", block_target);
                    render_error(
                        output,
                        &CliError::with_details(
                            &msg,
                            "Check the item ID and ensure it exists",
                            "blocks_target_not_found",
                        ),
                    )?;
                    anyhow::bail!("{}", msg);
                }
            }
        }
    }

    // 11. Check for duplicate items (unless --force is set)
    let mut duplicate_matches: Vec<DuplicateMatch> = Vec::new();
    if !args.force && db_path.exists() {
        if let Some(conn) = db::query::try_open_projection(&db_path)? {
            // Load project config to get search configuration
            let project_config = load_project_config(project_root).unwrap_or_default();

            // Build search config from project config
            let search_config = SearchConfig {
                rrf_k: 60,
                likely_duplicate_threshold: project_config.search.duplicate_threshold as f32,
                possibly_related_threshold: 0.70,
                maybe_related_threshold: 0.50,
            };
            let semantic_model = if project_config.search.semantic {
                match SemanticModel::load() {
                    Ok(model) => Some(model),
                    Err(err) => {
                        tracing::warn!(
                            "semantic model unavailable during duplicate check; using lexical+structural only: {err}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            let dependency_graph = RawGraph::from_sqlite(&conn)
                .map(|raw| raw.graph)
                .unwrap_or_else(|err| {
                    tracing::warn!(
                        "unable to load dependency graph for duplicate detection: {err}"
                    );
                    petgraph::graph::DiGraph::new()
                });

            // Run duplicate detection
            match find_duplicates_with_model(
                &args.title,
                &conn,
                &dependency_graph,
                &search_config,
                semantic_model.as_ref(),
                10,
            ) {
                Ok(candidates) => {
                    if !candidates.is_empty() {
                        // Convert to DuplicateMatch for output
                        for candidate in &candidates {
                            duplicate_matches.push(DuplicateMatch {
                                item_id: candidate.item_id.clone(),
                                score: candidate.composite_score,
                                classification: format!("{:?}", candidate.risk),
                            });
                        }

                        // In interactive mode, warn user
                        if output == OutputMode::Human {
                            eprintln!(
                                "⚠ Warning: {} potential duplicate(s) found",
                                candidates.len()
                            );
                            for (i, cand) in candidates.iter().enumerate().take(3) {
                                eprintln!(
                                    "  {}. {} (score: {:.2}, {})",
                                    i + 1,
                                    cand.item_id,
                                    cand.composite_score,
                                    format!("{:?}", cand.risk)
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    // Log error but don't block creation
                    tracing::warn!("duplicate check failed: {}", e);
                }
            }
        }
    }

    // 12. Generate item ID
    let item_id = generate_item_id(&args.title, item_count, |candidate| {
        if !db_path.exists() {
            return false;
        }
        match db::query::try_open_projection(&db_path) {
            Ok(Some(conn)) => db::query::item_exists(&conn, candidate).unwrap_or(false),
            _ => false,
        }
    });

    // 13. Get monotonic timestamp
    let ts = shard_mgr
        .next_timestamp()
        .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

    // 14. Build create event
    let create_data = CreateData {
        title: args.title.clone(),
        kind,
        size,
        urgency,
        labels: args.label.clone(),
        parent: args.parent.clone(),
        causation: None,
        description: description.clone(),
        extra: BTreeMap::new(),
    };

    let mut event = Event {
        wall_ts_us: ts,
        agent: agent.clone(),
        itc: "itc:AQ".to_string(), // Initial ITC stamp
        parents: vec![],
        event_type: EventType::Create,
        item_id: item_id.clone(),
        data: EventData::Create(create_data),
        event_hash: String::new(), // Will be computed by write_event
    };

    // 15. Compute hash and serialize
    let line = writer::write_event(&mut event)
        .map_err(|e| anyhow::anyhow!("failed to serialize event: {e}"))?;

    // 16. Append to shard
    shard_mgr
        .append(&line, false, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("failed to write event: {e}"))?;

    // 17. Project into SQLite (best-effort — projection can be rebuilt)
    if let Ok(conn) = db::open_projection(&db_path) {
        let _ = project::ensure_tracking_table(&conn);
        let projector = project::Projector::new(&conn);
        if let Err(e) = projector.project_event(&event) {
            tracing::warn!("projection failed (will be fixed on next rebuild): {e}");
        }
    }

    // 18. Output
    let result = CreateOutput {
        id: item_id.as_str().to_string(),
        title: args.title.clone(),
        kind: kind.to_string(),
        state: "open".to_string(),
        urgency: urgency.to_string(),
        size: size.map(|s| s.to_string()),
        parent: args.parent.clone(),
        labels: args.label.clone(),
        description,
        agent,
        event_hash: event.event_hash.clone(),
        duplicates: duplicate_matches,
    };

    render(output, &result, |r, w| {
        writeln!(w, "Created item")?;
        writeln!(w, "{:-<72}", "")?;
        writeln!(w, "ID:      {}", r.id)?;
        writeln!(w, "Title:   {}", r.title)?;
        writeln!(w, "Kind:    {}", r.kind)?;
        writeln!(w, "Urgency: {}", r.urgency)?;
        if let Some(ref parent) = r.parent {
            writeln!(w, "Parent:  {parent}")?;
        }
        if !r.labels.is_empty() {
            writeln!(w, "Labels:  {}", r.labels.join(", "))?;
        }
        if let Some(ref size) = r.size {
            writeln!(w, "Size:    {size}")?;
        }
        Ok(())
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use tempfile::TempDir;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: CreateArgs,
    }

    #[test]
    fn create_args_defaults() {
        let w = TestCli::parse_from(["test", "--title", "Hello"]);
        assert_eq!(w.args.title, "Hello");
        assert_eq!(w.args.kind, "task");
        assert!(w.args.parent.is_none());
        assert!(w.args.label.is_empty());
        assert!(w.args.description.is_none());
        assert!(w.args.size.is_none());
        assert!(w.args.urgency.is_none());
        assert!(w.args.blocks.is_empty());
    }

    #[test]
    fn create_args_all_flags() {
        let w = TestCli::parse_from([
            "test",
            "--title",
            "My Bug",
            "--kind",
            "bug",
            "--size",
            "m",
            "--urgency",
            "urgent",
            "--parent",
            "bn-a7x",
            "-l",
            "backend",
            "-l",
            "auth",
            "--description",
            "Fix the auth timeout",
            "--blocks",
            "bn-b8y",
        ]);
        assert_eq!(w.args.title, "My Bug");
        assert_eq!(w.args.kind, "bug");
        assert_eq!(w.args.size.as_deref(), Some("m"));
        assert_eq!(w.args.urgency.as_deref(), Some("urgent"));
        assert_eq!(w.args.parent.as_deref(), Some("bn-a7x"));
        assert_eq!(w.args.label, vec!["backend", "auth"]);
        assert_eq!(w.args.description.as_deref(), Some("Fix the auth timeout"));
        assert_eq!(w.args.blocks, vec!["bn-b8y"]);
    }

    #[test]
    fn find_bones_dir_found() {
        let dir = TempDir::new().unwrap();
        let bones = dir.path().join(".bones");
        std::fs::create_dir(&bones).unwrap();
        let result = find_bones_dir(dir.path());
        assert!(result.is_some());
        assert_eq!(result.unwrap(), bones);
    }

    #[test]
    fn find_bones_dir_in_parent() {
        let dir = TempDir::new().unwrap();
        let bones = dir.path().join(".bones");
        std::fs::create_dir(&bones).unwrap();
        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        let result = find_bones_dir(&subdir);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), bones);
    }

    #[test]
    fn find_bones_dir_not_found() {
        let dir = TempDir::new().unwrap();
        let result = find_bones_dir(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn read_description_regular() {
        let desc = Some("hello world".to_string());
        let result = read_description(&desc).unwrap();
        assert_eq!(result, Some("hello world".to_string()));
    }

    #[test]
    fn read_description_none() {
        let result = read_description(&None).unwrap();
        assert!(result.is_none());
    }

    /// Integration test: full create flow in a temp directory.
    #[test]
    fn create_item_end_to_end() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Initialize a bones project
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();

        // Write initial shard header
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let args = CreateArgs {
            title: "Test item".to_string(),
            kind: "task".to_string(),
            size: Some("m".to_string()),
            urgency: None,
            parent: None,
            label: vec!["test".to_string()],
            description: Some("A test description".to_string()),
            blocks: vec![],
            force: false,
        };

        let result = run_create(&args, Some("test-agent"), OutputMode::Json, root);
        assert!(result.is_ok(), "create failed: {:?}", result.err());

        // Verify event was written to shard
        let replay = shard_mgr.replay().unwrap();
        let lines: Vec<&str> = replay
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 1, "expected 1 event line, got {}", lines.len());

        let fields: Vec<&str> = lines[0].split('\t').collect();
        assert_eq!(fields.len(), 8, "expected 8 TSJSON fields");
        assert_eq!(fields[1], "test-agent");
        assert_eq!(fields[4], "item.create");
        assert!(fields[5].starts_with("bn-"), "expected bn- prefix ID");
        assert!(fields[7].starts_with("blake3:"), "expected blake3 hash");
    }

    #[test]
    fn create_item_json_output() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let args = CreateArgs {
            title: "JSON test".to_string(),
            kind: "bug".to_string(),
            size: None,
            urgency: Some("urgent".to_string()),
            parent: None,
            label: vec![],
            description: None,
            blocks: vec![],
            force: false,
        };

        // Just verify it doesn't error
        let result = run_create(&args, Some("agent"), OutputMode::Json, root);
        assert!(result.is_ok());
    }

    /// Verify that the agent resolution error struct is well-formed.
    /// Full "no agent available" testing lives in agent.rs tests
    /// (we can't safely clear env vars with forbid(unsafe_code)).
    #[test]
    fn create_agent_error_has_correct_code() {
        let err = agent::AgentResolutionError {
            message: "test".to_string(),
            code: "missing_agent",
        };
        assert_eq!(err.code, "missing_agent");
        assert_eq!(format!("{err}"), "test");
    }

    #[test]
    fn create_fails_without_bones_dir() {
        let dir = TempDir::new().unwrap();
        let args = CreateArgs {
            title: "Test".to_string(),
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            description: None,
            blocks: vec![],
            force: false,
        };

        let result = run_create(&args, Some("agent"), OutputMode::Human, dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn create_rejects_invalid_kind() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let args = CreateArgs {
            title: "Test".to_string(),
            kind: "epic".to_string(), // invalid
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            description: None,
            blocks: vec![],
            force: false,
        };

        let result = run_create(&args, Some("agent"), OutputMode::Human, root);
        assert!(result.is_err());
    }

    #[test]
    fn create_rejects_invalid_size() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let args = CreateArgs {
            title: "Test".to_string(),
            kind: "task".to_string(),
            size: Some("mega".to_string()), // invalid
            urgency: None,
            parent: None,
            label: vec![],
            description: None,
            blocks: vec![],
            force: false,
        };

        let result = run_create(&args, Some("agent"), OutputMode::Human, root);
        assert!(result.is_err());
    }

    #[test]
    fn create_rejects_invalid_urgency() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let args = CreateArgs {
            title: "Test".to_string(),
            kind: "task".to_string(),
            size: None,
            urgency: Some("hot".to_string()), // invalid
            parent: None,
            label: vec![],
            description: None,
            blocks: vec![],
            force: false,
        };

        let result = run_create(&args, Some("agent"), OutputMode::Human, root);
        assert!(result.is_err());
    }

    #[test]
    fn create_generates_unique_ids() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        // Create two items with different titles
        for title in ["First item", "Second item"] {
            let args = CreateArgs {
                title: title.to_string(),
                kind: "task".to_string(),
                size: None,
                urgency: None,
                parent: None,
                label: vec![],
                description: None,
                blocks: vec![],
                force: false,
            };
            let result = run_create(&args, Some("agent"), OutputMode::Json, root);
            assert!(
                result.is_ok(),
                "create '{}' failed: {:?}",
                title,
                result.err()
            );
        }

        // Verify two distinct events
        let replay = shard_mgr.replay().unwrap();
        let lines: Vec<&str> = replay
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 2);

        let id1: Vec<&str> = lines[0].split('\t').collect();
        let id2: Vec<&str> = lines[1].split('\t').collect();
        assert_ne!(id1[5], id2[5], "IDs should be unique");
    }

    #[test]
    fn create_with_description() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let args = CreateArgs {
            title: "Item with desc".to_string(),
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            description: Some("Detailed description here".to_string()),
            blocks: vec![],
            force: false,
        };

        let result = run_create(&args, Some("agent"), OutputMode::Json, root);
        assert!(result.is_ok());

        // Verify description is in event payload
        let replay = shard_mgr.replay().unwrap();
        let line = replay
            .lines()
            .find(|l| !l.starts_with('#') && !l.is_empty())
            .unwrap();
        let fields: Vec<&str> = line.split('\t').collect();
        let data_json = fields[6];
        assert!(
            data_json.contains("Detailed description here"),
            "description not in event data"
        );
    }

    #[test]
    fn create_with_labels() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let args = CreateArgs {
            title: "Labeled item".to_string(),
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec!["backend".to_string(), "auth".to_string()],
            description: None,
            blocks: vec![],
            force: false,
        };

        let result = run_create(&args, Some("agent"), OutputMode::Json, root);
        assert!(result.is_ok());

        let replay = shard_mgr.replay().unwrap();
        let line = replay
            .lines()
            .find(|l| !l.starts_with('#') && !l.is_empty())
            .unwrap();
        let fields: Vec<&str> = line.split('\t').collect();
        let data_json = fields[6];
        assert!(data_json.contains("backend"));
        assert!(data_json.contains("auth"));
    }

    #[test]
    fn create_force_flag_parsing() {
        let w = TestCli::parse_from(["test", "--title", "Hello", "--force"]);
        assert_eq!(w.args.title, "Hello");
        assert!(w.args.force);
    }

    #[test]
    fn create_force_flag_default_false() {
        let w = TestCli::parse_from(["test", "--title", "Hello"]);
        assert!(!w.args.force);
    }

    #[test]
    fn create_with_duplicate_detection() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        // Create first item
        let args1 = CreateArgs {
            title: "Fix authentication timeout bug".to_string(),
            kind: "bug".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec!["backend".to_string()],
            description: None,
            blocks: vec![],
            force: false,
        };

        let result1 = run_create(&args1, Some("agent"), OutputMode::Json, root);
        assert!(result1.is_ok(), "first create failed: {:?}", result1.err());

        // Create second item with similar title (should detect first as duplicate)
        let args2 = CreateArgs {
            title: "Fix auth timeout issue".to_string(),
            kind: "bug".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            description: None,
            blocks: vec![],
            force: false,
        };

        let result2 = run_create(&args2, Some("agent"), OutputMode::Json, root);
        assert!(result2.is_ok(), "second create failed: {:?}", result2.err());

        // Verify both events were created
        let replay = shard_mgr.replay().unwrap();
        let lines: Vec<&str> = replay
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 2, "expected 2 events, got {}", lines.len());
    }

    #[test]
    fn create_force_skips_duplicate_check() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        // Create first item
        let args1 = CreateArgs {
            title: "Test item".to_string(),
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            description: None,
            blocks: vec![],
            force: false,
        };

        let result1 = run_create(&args1, Some("agent"), OutputMode::Json, root);
        assert!(result1.is_ok());

        // Create second item with --force (should not run duplicate check)
        let args2 = CreateArgs {
            title: "Test item".to_string(),
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            description: None,
            blocks: vec![],
            force: true, // Force skip duplicate check
        };

        let result2 = run_create(&args2, Some("agent"), OutputMode::Json, root);
        assert!(result2.is_ok(), "--force should allow duplicate creation");

        // Verify both events were created
        let replay = shard_mgr.replay().unwrap();
        let lines: Vec<&str> = replay
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 2);
    }
}
