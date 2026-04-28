//! `bn create` — create a new bone.
//!
//! Generates a unique item ID, emits an `item.create` event to the active
//! shard, projects it into the `SQLite` database, and outputs the result.

use crate::agent;
use crate::cmd::dup::build_fts_query;
use crate::itc_state::assign_next_itc;
use crate::output::{CliError, OutputMode, render, render_error};
use crate::validate;
use anyhow::Context;
use clap::Args;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bones_core::config::load_project_config;
use bones_core::db;
use bones_core::db::project;
use bones_core::event::Event;
use bones_core::event::data::{CreateData, EventData, LinkData};
use bones_core::event::types::EventType;
use bones_core::event::writer;
use bones_core::model::item::Kind;
use bones_core::model::item::Size;
use bones_core::model::item::Urgency;
use bones_core::model::item_id::{ItemId, generate_item_id};
use bones_core::shard::ShardManager;
use bones_search::find_duplicates_with_model;
use bones_search::fusion::scoring::SearchConfig;
use bones_search::semantic::SemanticModel;
use bones_triage::graph::RawGraph;

#[derive(Args, Debug)]
pub struct CreateArgs {
    /// Title of the new bone.
    #[arg(short, long, required_unless_present = "from_file")]
    pub title: Option<String>,

    /// Load one or more bones from a YAML, JSON, or TOML file.
    #[arg(
        long,
        value_name = "path",
        conflicts_with_all = [
            "kind",
            "size",
            "urgency",
            "parent",
            "label",
            "tag",
            "labels",
            "tags",
            "description",
            "blocks"
        ]
    )]
    pub from_file: Option<PathBuf>,

    /// Bone kind: task, goal, or bug.
    #[arg(short, long, default_value = "task")]
    pub kind: String,

    /// T-shirt size estimate: xs, s, m, l, xl.
    #[arg(short, long)]
    pub size: Option<String>,

    /// Urgency override: urgent, default, or punt.
    #[arg(short, long)]
    pub urgency: Option<String>,

    /// Parent bone ID (makes this a child of a goal).
    #[arg(long)]
    pub parent: Option<String>,

    /// Labels to attach (can be repeated: -l foo -l bar).
    #[arg(short, long)]
    pub label: Vec<String>,

    /// Hidden alias: --tag (same as --label).
    #[arg(long, hide = true)]
    pub tag: Vec<String>,

    /// Hidden alias: --labels (comma-separated).
    #[arg(long, hide = true, value_delimiter = ',')]
    pub labels: Vec<String>,

    /// Hidden alias: --tags (comma-separated).
    #[arg(long, hide = true, value_delimiter = ',')]
    pub tags: Vec<String>,

    /// Description text (use '-' to read from stdin).
    #[arg(short, long)]
    pub description: Option<String>,

    /// Bones this new bone blocks (can be repeated).
    #[arg(long)]
    pub blocks: Vec<String>,

    /// Skip duplicate check entirely.
    #[arg(long)]
    pub force: bool,

    /// Allow writing high-confidence secret-like text.
    #[arg(long)]
    pub allow_secret: bool,
}

impl CreateArgs {
    /// Collect labels from all alias flags (--label, --tag, --labels, --tags).
    pub fn all_labels(&self) -> Vec<String> {
        let mut out = self.label.clone();
        out.extend(self.tag.iter().cloned());
        out.extend(self.labels.iter().cloned());
        out.extend(self.tags.iter().cloned());
        out
    }

    fn to_request(&self) -> anyhow::Result<CreateRequest> {
        let title = self
            .title
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--title is required unless --from-file is used"))?;

        Ok(CreateRequest {
            title,
            kind: self.kind.clone(),
            size: self.size.clone(),
            urgency: self.urgency.clone(),
            parent: self.parent.clone(),
            labels: self.all_labels(),
            description: self.description.clone(),
            blocks: self.blocks.clone(),
        })
    }
}

#[derive(Debug, Clone)]
struct CreateRequest {
    title: String,
    kind: String,
    size: Option<String>,
    urgency: Option<String>,
    parent: Option<String>,
    labels: Vec<String>,
    description: Option<String>,
    blocks: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CreateFileEntry {
    title: String,
    #[serde(default = "default_kind")]
    kind: String,
    size: Option<String>,
    urgency: Option<String>,
    parent: Option<String>,
    #[serde(default)]
    label: Vec<String>,
    #[serde(default)]
    tag: Vec<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    description: Option<String>,
    #[serde(default)]
    blocks: Vec<String>,
}

impl CreateFileEntry {
    fn all_labels(&self) -> Vec<String> {
        let mut out = self.label.clone();
        out.extend(self.tag.iter().cloned());
        out.extend(self.labels.iter().cloned());
        out.extend(self.tags.iter().cloned());
        out
    }

    fn into_request(self) -> CreateRequest {
        let labels = self.all_labels();
        CreateRequest {
            title: self.title,
            kind: self.kind,
            size: self.size,
            urgency: self.urgency,
            parent: self.parent,
            labels,
            description: self.description,
            blocks: self.blocks,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct CreateFileCollection {
    bones: Vec<CreateFileEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum CreateFileDocument {
    One(CreateFileEntry),
    Many(Vec<CreateFileEntry>),
    Collection(CreateFileCollection),
}

impl CreateFileDocument {
    fn into_requests(self) -> Vec<CreateRequest> {
        match self {
            Self::One(entry) => vec![entry.into_request()],
            Self::Many(entries) => entries
                .into_iter()
                .map(CreateFileEntry::into_request)
                .collect(),
            Self::Collection(collection) => collection
                .bones
                .into_iter()
                .map(CreateFileEntry::into_request)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum CreateFileFormat {
    Json,
    Toml,
    Yaml,
}

fn default_kind() -> String {
    "task".to_string()
}

/// JSON output for a created bone.
#[derive(Debug, Clone, Serialize)]
struct CreateOutput {
    schema_version: u32,
    id: String,
    title: String,
    kind: String,
    state: String,
    previous_state: Option<String>,
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

#[derive(Debug, Serialize)]
struct CreateBatchOutput {
    schema_version: u32,
    created: Vec<CreateOutput>,
}

/// A duplicate candidate match in JSON output.
#[derive(Debug, Clone, Serialize)]
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

fn detect_create_file_format(path: &Path, raw: &str) -> anyhow::Result<CreateFileFormat> {
    if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
        match ext.to_ascii_lowercase().as_str() {
            "json" => return Ok(CreateFileFormat::Json),
            "toml" => return Ok(CreateFileFormat::Toml),
            "yaml" | "yml" => return Ok(CreateFileFormat::Yaml),
            _ => {}
        }
    }

    if serde_json::from_str::<CreateFileDocument>(raw).is_ok() {
        return Ok(CreateFileFormat::Json);
    }
    if toml::from_str::<CreateFileDocument>(raw).is_ok() {
        return Ok(CreateFileFormat::Toml);
    }

    let mut yaml_docs = serde_yaml::Deserializer::from_str(raw);
    if yaml_docs
        .next()
        .is_some_and(|doc| CreateFileDocument::deserialize(doc).is_ok())
    {
        return Ok(CreateFileFormat::Yaml);
    }

    anyhow::bail!(
        "could not detect file format for '{}': expected YAML, JSON, or TOML",
        path.display()
    );
}

fn parse_create_file(path: &Path) -> anyhow::Result<Vec<CreateRequest>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read create input file {}", path.display()))?;
    let format = detect_create_file_format(path, &raw)?;

    let requests = match format {
        CreateFileFormat::Json => serde_json::from_str::<CreateFileDocument>(&raw)
            .context("parse JSON create input")?
            .into_requests(),
        CreateFileFormat::Toml => toml::from_str::<CreateFileDocument>(&raw)
            .context("parse TOML create input")?
            .into_requests(),
        CreateFileFormat::Yaml => {
            let mut requests = Vec::new();
            for doc in serde_yaml::Deserializer::from_str(&raw) {
                requests.extend(
                    CreateFileDocument::deserialize(doc)
                        .context("parse YAML create input")?
                        .into_requests(),
                );
            }
            requests
        }
    };

    if requests.is_empty() {
        anyhow::bail!(
            "create input file '{}' did not contain any bones",
            path.display()
        );
    }

    Ok(requests)
}

fn render_create_result(output: OutputMode, result: &CreateOutput) -> anyhow::Result<()> {
    render(output, result, |r, w| {
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
    })
}

fn render_create_batch(output: OutputMode, results: &[CreateOutput]) -> anyhow::Result<()> {
    let batch = CreateBatchOutput {
        schema_version: 1,
        created: results.to_vec(),
    };
    render(output, &batch, |r, w| {
        for (idx, item) in r.created.iter().enumerate() {
            if idx > 0 {
                writeln!(w)?;
            }
            writeln!(w, "Created item")?;
            writeln!(w, "{:-<72}", "")?;
            writeln!(w, "ID:      {}", item.id)?;
            writeln!(w, "Title:   {}", item.title)?;
            writeln!(w, "Kind:    {}", item.kind)?;
            writeln!(w, "Urgency: {}", item.urgency)?;
            if let Some(ref parent) = item.parent {
                writeln!(w, "Parent:  {parent}")?;
            }
            if !item.labels.is_empty() {
                writeln!(w, "Labels:  {}", item.labels.join(", "))?;
            }
            if let Some(ref size) = item.size {
                writeln!(w, "Size:    {size}")?;
            }
        }
        Ok(())
    })
}

fn render_and_bail<T>(output: OutputMode, err: CliError) -> anyhow::Result<T> {
    render_error(output, &err)?;
    anyhow::bail!(err.message)
}

#[tracing::instrument(skip_all, name = "cmd.create")]
pub fn run_create(
    args: &CreateArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    if let Some(path) = &args.from_file {
        let requests = parse_create_file(path)?;
        let mut created = Vec::with_capacity(requests.len());
        for request in &requests {
            created.push(run_create_single(
                request,
                args.force,
                args.allow_secret,
                agent_flag,
                output,
                project_root,
            )?);
        }
        return render_create_batch(output, &created);
    }

    let request = args.to_request()?;
    let result = run_create_single(
        &request,
        args.force,
        args.allow_secret,
        agent_flag,
        output,
        project_root,
    )?;
    render_create_result(output, &result)
}

fn run_create_single(
    request: &CreateRequest,
    force: bool,
    allow_secret: bool,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<CreateOutput> {
    // 1. Require agent identity for mutating command
    let agent = match agent::require_agent(agent_flag) {
        Ok(a) => a,
        Err(e) => {
            return render_and_bail(
                output,
                CliError::with_details(&e.message, "Set --agent, BONES_AGENT, or AGENT", e.code),
            );
        }
    };

    // 2. Validate simple input fields early
    if let Err(e) = validate::validate_agent(&agent) {
        return render_and_bail(output, e.to_cli_error());
    }
    if let Err(e) = validate::validate_title(&request.title) {
        return render_and_bail(output, e.to_cli_error());
    }
    let all_labels = request.labels.clone();
    for label in &all_labels {
        if let Err(e) = validate::validate_label(label) {
            return render_and_bail(output, e.to_cli_error());
        }
    }

    // 3. Parse/validate kind
    let kind: Kind = match validate::validate_kind(&request.kind) {
        Ok(k) => k,
        Err(e) => {
            return render_and_bail(output, e.to_cli_error());
        }
    };

    // 4. Parse/validate size (optional)
    let size: Option<Size> = match &request.size {
        Some(s) => Some(match validate::validate_size(s) {
            Ok(size) => size,
            Err(e) => {
                return render_and_bail(output, e.to_cli_error());
            }
        }),
        None => None,
    };

    // 4. Parse urgency (optional, defaults to "default")
    let urgency: Urgency = match &request.urgency {
        Some(u) => u.parse().map_err(|_| {
            let msg = format!("invalid urgency '{u}': expected one of urgent, default, punt");
            render_error(
                output,
                &CliError::with_details(
                    &msg,
                    "Use --urgency urgent, --urgency punt, etc.",
                    "invalid_urgency",
                ),
            )
            .ok();
            anyhow::anyhow!("{msg}")
        })?,
        None => Urgency::Default,
    };

    // 5. Read description
    let description = read_description(&request.description)?;

    if allow_secret {
        if let Some(kind) = validate::detect_secret_kind(&request.title) {
            tracing::warn!(
                secret_kind = kind,
                "allowing secret-like title due to --allow-secret"
            );
        }
        if let Some(desc) = &description
            && let Some(kind) = validate::detect_secret_kind(desc)
        {
            tracing::warn!(
                secret_kind = kind,
                "allowing secret-like description due to --allow-secret"
            );
        }
    } else {
        if let Err(e) = validate::validate_no_secrets("title", &request.title) {
            return render_and_bail(output, e.to_cli_error());
        }
        if let Some(desc) = &description
            && let Err(e) = validate::validate_no_secrets("description", desc)
        {
            return render_and_bail(output, e.to_cli_error());
        }
    }

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
        anyhow::anyhow!("{msg}")
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
    if let Some(ref parent_id) = request.parent {
        if let Err(e) = validate::validate_item_id(parent_id) {
            anyhow::bail!("{}", e.reason);
        }
        if db_path.exists()
            && let Some(conn) = db::query::try_open_projection(&db_path)?
            && !db::query::item_exists(&conn, parent_id)?
        {
            anyhow::bail!("parent item '{parent_id}' not found");
        }
    }

    // 10. Validate --blocks targets exist. Preserve input order while avoiding
    // duplicate link events for repeated flags.
    let mut block_targets: Vec<String> = Vec::new();
    for block_target in &request.blocks {
        if let Err(e) = validate::validate_item_id(block_target) {
            anyhow::bail!("{}", e.reason);
        }
        if db_path.exists()
            && let Some(conn) = db::query::try_open_projection(&db_path)?
            && !db::query::item_exists(&conn, block_target)?
        {
            anyhow::bail!("blocks target '{block_target}' not found");
        }
        if !block_targets.iter().any(|target| target == block_target) {
            block_targets.push(block_target.clone());
        }
    }

    // 11. Check for duplicate items (unless --force is set)
    let mut duplicate_matches: Vec<DuplicateMatch> = Vec::new();
    if !force
        && db_path.exists()
        && let Some(conn) = db::query::try_open_projection(&db_path)?
    {
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
                tracing::warn!("unable to load dependency graph for duplicate detection: {err}");
                petgraph::graph::DiGraph::new()
            });

        let duplicate_query = build_fts_query(&request.title, description.as_deref());
        if duplicate_query.is_empty() {
            tracing::debug!(
                "duplicate check skipped: no usable lexical tokens from title/description"
            );
        } else {
            // Run duplicate detection
            match find_duplicates_with_model(
                &duplicate_query,
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
                        if output == OutputMode::Pretty {
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
    let item_id = generate_item_id(&request.title, item_count, |candidate| {
        if !db_path.exists() {
            return false;
        }
        match db::query::try_open_projection(&db_path) {
            Ok(Some(conn)) => db::query::item_exists(&conn, candidate).unwrap_or(false),
            _ => false,
        }
    });

    // 13-16. Build event and append to shard (atomic timestamp+hash+write)
    let create_data = CreateData {
        title: request.title.clone(),
        kind,
        size,
        urgency,
        labels: all_labels.clone(),
        parent: request.parent.clone(),
        causation: None,
        description: description.clone(),
        extra: BTreeMap::new(),
    };

    let mut event = Event {
        wall_ts_us: 0, // Placeholder, set under lock
        agent: agent.clone(),
        itc: String::new(),
        parents: vec![],
        event_type: EventType::Create,
        item_id: item_id.clone(),
        data: EventData::Create(create_data),
        event_hash: String::new(),
    };
    let mut emitted_events = Vec::with_capacity(1 + block_targets.len());

    {
        use bones_core::lock::ShardLock;
        let lock_path = shard_mgr.lock_path();
        let _lock = ShardLock::acquire(&lock_path, Duration::from_secs(5))
            .map_err(|e| anyhow::anyhow!("failed to acquire lock: {e}"))?;

        // Rotate if needed
        let (year, month) = shard_mgr
            .rotate_if_needed()
            .map_err(|e| anyhow::anyhow!("failed to rotate shards: {e}"))?;

        // Get monotonic timestamp
        event.wall_ts_us = shard_mgr
            .next_timestamp()
            .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?;

        // Assign ITC (local file op, safe under lock)
        assign_next_itc(project_root, &mut event)?;

        // Compute hash and serialize
        let line = writer::write_event(&mut event)
            .map_err(|e| anyhow::anyhow!("failed to serialize event: {e}"))?;

        // Append raw (we hold the lock)
        shard_mgr
            .append_raw(year, month, &line)
            .map_err(|e| anyhow::anyhow!("failed to write event: {e}"))?;

        emitted_events.push(event.clone());

        for block_target in &block_targets {
            let mut link_event = Event {
                wall_ts_us: shard_mgr
                    .next_timestamp()
                    .map_err(|e| anyhow::anyhow!("failed to get timestamp: {e}"))?,
                agent: agent.clone(),
                itc: String::new(),
                parents: vec![],
                event_type: EventType::Link,
                item_id: ItemId::parse(block_target)
                    .map_err(|e| anyhow::anyhow!("invalid blocks target '{block_target}': {e}"))?,
                data: EventData::Link(LinkData {
                    target: item_id.as_str().to_string(),
                    link_type: "blocks".to_string(),
                    extra: BTreeMap::new(),
                }),
                event_hash: String::new(),
            };

            assign_next_itc(project_root, &mut link_event)?;

            let line = writer::write_event(&mut link_event)
                .map_err(|e| anyhow::anyhow!("failed to serialize link event: {e}"))?;

            shard_mgr
                .append_raw(year, month, &line)
                .map_err(|e| anyhow::anyhow!("failed to write link event: {e}"))?;

            emitted_events.push(link_event);
        }
    }

    // 17. Project into SQLite (best-effort — projection can be rebuilt)
    if let Ok(conn) = db::open_projection(&db_path) {
        let _ = project::ensure_tracking_table(&conn);
        let projector = project::Projector::new(&conn);
        for emitted_event in &emitted_events {
            if let Err(e) = projector.project_event(emitted_event) {
                tracing::warn!("projection failed (will be fixed on next rebuild): {e}");
            }
        }
    }

    // 18. Output
    let result = CreateOutput {
        schema_version: 1,
        id: item_id.as_str().to_string(),
        title: request.title.clone(),
        kind: kind.to_string(),
        state: "open".to_string(),
        previous_state: None,
        urgency: urgency.to_string(),
        size: size.map(|s| s.to_string()),
        parent: request.parent.clone(),
        labels: all_labels,
        description,
        agent,
        event_hash: event.event_hash.clone(),
        duplicates: duplicate_matches,
    };

    Ok(result)
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
        assert_eq!(w.args.title.as_deref(), Some("Hello"));
        assert!(w.args.from_file.is_none());
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
        assert_eq!(w.args.title.as_deref(), Some("My Bug"));
        assert_eq!(w.args.kind, "bug");
        assert_eq!(w.args.size.as_deref(), Some("m"));
        assert_eq!(w.args.urgency.as_deref(), Some("urgent"));
        assert_eq!(w.args.parent.as_deref(), Some("bn-a7x"));
        assert_eq!(w.args.label, vec!["backend", "auth"]);
        assert_eq!(w.args.description.as_deref(), Some("Fix the auth timeout"));
        assert_eq!(w.args.blocks, vec!["bn-b8y"]);
    }

    #[test]
    fn create_args_from_file() {
        let w = TestCli::parse_from(["test", "--from-file", "bones.yaml", "--force"]);
        assert!(w.args.title.is_none());
        assert_eq!(w.args.from_file.as_deref(), Some(Path::new("bones.yaml")));
        assert!(w.args.force);
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

    #[test]
    fn parse_create_file_detects_json_object() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bones.json");
        std::fs::write(
            &path,
            r#"{"title":"JSON bone","description":"from json","labels":["one"]}"#,
        )
        .unwrap();

        let requests = parse_create_file(&path).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].title, "JSON bone");
        assert_eq!(requests[0].labels, vec!["one"]);
    }

    #[test]
    fn parse_create_file_detects_toml_collection() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bones.toml");
        std::fs::write(
            &path,
            r#"[[bones]]
title = "First"
description = "one"

[[bones]]
title = "Second"
labels = ["two", "three"]
"#,
        )
        .unwrap();

        let requests = parse_create_file(&path).unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].title, "Second");
        assert_eq!(requests[1].labels, vec!["two", "three"]);
    }

    #[test]
    fn create_from_file_supports_multiple_yaml_documents() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let input_path = root.join("bones.yaml");
        std::fs::write(
            &input_path,
            r#"title: First from file
description: |
  Here's the multi-line description.
  It can have `backticks`.
---
title: Second from file
labels:
  - one
  - two
"#,
        )
        .unwrap();

        let args = CreateArgs {
            title: None,
            from_file: Some(input_path),
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: false,
            allow_secret: false,
        };

        let result = run_create(&args, Some("agent"), OutputMode::Json, root);
        assert!(
            result.is_ok(),
            "create from file failed: {:?}",
            result.err()
        );

        let replay = shard_mgr.replay().unwrap();
        let lines: Vec<&str> = replay
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(replay.contains("First from file"));
        assert!(replay.contains("Second from file"));
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
            title: Some("Test item".to_string()),
            from_file: None,
            kind: "task".to_string(),
            size: Some("m".to_string()),
            urgency: None,
            parent: None,
            label: vec!["test".to_string()],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: Some("A test description".to_string()),
            blocks: vec![],
            force: false,
            allow_secret: false,
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
            title: Some("JSON test".to_string()),
            from_file: None,
            kind: "bug".to_string(),
            size: None,
            urgency: Some("urgent".to_string()),
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: false,
            allow_secret: false,
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
            title: Some("Test".to_string()),
            from_file: None,
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: false,
            allow_secret: false,
        };

        let result = run_create(&args, Some("agent"), OutputMode::Pretty, dir.path());
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
            title: Some("Test".to_string()),
            from_file: None,
            kind: "epic".to_string(), // invalid
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: false,
            allow_secret: false,
        };

        let result = run_create(&args, Some("agent"), OutputMode::Pretty, root);
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
            title: Some("Test".to_string()),
            from_file: None,
            kind: "task".to_string(),
            size: Some("mega".to_string()), // invalid
            urgency: None,
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: false,
            allow_secret: false,
        };

        let result = run_create(&args, Some("agent"), OutputMode::Pretty, root);
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
            title: Some("Test".to_string()),
            from_file: None,
            kind: "task".to_string(),
            size: None,
            urgency: Some("hot".to_string()), // invalid
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: false,
            allow_secret: false,
        };

        let result = run_create(&args, Some("agent"), OutputMode::Pretty, root);
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
                title: Some(title.to_string()),
                from_file: None,
                kind: "task".to_string(),
                size: None,
                urgency: None,
                parent: None,
                label: vec![],
                tag: vec![],
                labels: vec![],
                tags: vec![],
                description: None,
                blocks: vec![],
                force: false,
                allow_secret: false,
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
    fn create_with_blocks_emits_dependency_link() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let target_args = CreateArgs {
            title: Some("Blocked target".to_string()),
            from_file: None,
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: true,
            allow_secret: false,
        };
        run_create(&target_args, Some("agent"), OutputMode::Json, root).unwrap();

        let db_path = bones_dir.join("bones.db");
        let conn = db::open_projection(&db_path).unwrap();
        let target_id = db::query::list_items(
            &conn,
            &db::query::ItemFilter {
                include_deleted: true,
                ..Default::default()
            },
        )
        .unwrap()
        .into_iter()
        .find(|item| item.title == "Blocked target")
        .unwrap()
        .item_id;
        drop(conn);

        let blocker_args = CreateArgs {
            title: Some("New blocker".to_string()),
            from_file: None,
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![target_id.clone()],
            force: true,
            allow_secret: false,
        };
        run_create(&blocker_args, Some("agent"), OutputMode::Json, root).unwrap();

        let conn = db::open_projection(&db_path).unwrap();
        let blocker_id = db::query::list_items(
            &conn,
            &db::query::ItemFilter {
                include_deleted: true,
                ..Default::default()
            },
        )
        .unwrap()
        .into_iter()
        .find(|item| item.title == "New blocker")
        .unwrap()
        .item_id;

        let deps = db::query::get_dependencies(&conn, &target_id).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].item_id, target_id);
        assert_eq!(deps[0].depends_on_item_id, blocker_id);
        assert_eq!(deps[0].link_type, "blocks");

        let replay = shard_mgr.replay().unwrap();
        let lines: Vec<&str> = replay
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 3);
        let link_fields: Vec<&str> = lines[2].split('\t').collect();
        assert_eq!(link_fields[4], "item.link");
        assert_eq!(link_fields[5], target_id);
        assert!(link_fields[6].contains(&blocker_id));
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
            title: Some("Item with desc".to_string()),
            from_file: None,
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: Some("Detailed description here".to_string()),
            blocks: vec![],
            force: false,
            allow_secret: false,
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
            title: Some("Labeled item".to_string()),
            from_file: None,
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec!["backend".to_string(), "auth".to_string()],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: false,
            allow_secret: false,
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
        assert_eq!(w.args.title.as_deref(), Some("Hello"));
        assert!(w.args.force);
    }

    #[test]
    fn create_force_flag_default_false() {
        let w = TestCli::parse_from(["test", "--title", "Hello"]);
        assert!(!w.args.force);
    }

    #[test]
    fn create_blocks_secret_without_override() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let args = CreateArgs {
            title: Some("ghp_abcdefghijklmnopqrstuvwxyz012345".to_string()),
            from_file: None,
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: false,
            allow_secret: false,
        };

        let result = run_create(&args, Some("agent"), OutputMode::Json, root);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("secret pattern"));
    }

    #[test]
    fn create_allows_secret_with_override() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let bones_dir = root.join(".bones");
        std::fs::create_dir_all(bones_dir.join("events")).unwrap();
        std::fs::create_dir_all(bones_dir.join("cache")).unwrap();
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.init().unwrap();

        let args = CreateArgs {
            title: Some("ghp_abcdefghijklmnopqrstuvwxyz012345".to_string()),
            from_file: None,
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: false,
            allow_secret: true,
        };

        let result = run_create(&args, Some("agent"), OutputMode::Json, root);
        assert!(result.is_ok());
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
            title: Some("Fix authentication timeout bug".to_string()),
            from_file: None,
            kind: "bug".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec!["backend".to_string()],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: false,
            allow_secret: false,
        };

        let result1 = run_create(&args1, Some("agent"), OutputMode::Json, root);
        assert!(result1.is_ok(), "first create failed: {:?}", result1.err());

        // Create second item with similar title (should detect first as duplicate)
        let args2 = CreateArgs {
            title: Some("Fix auth timeout issue".to_string()),
            from_file: None,
            kind: "bug".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: false,
            allow_secret: false,
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
            title: Some("Test item".to_string()),
            from_file: None,
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: false,
            allow_secret: false,
        };

        let result1 = run_create(&args1, Some("agent"), OutputMode::Json, root);
        assert!(result1.is_ok());

        // Create second item with --force (should not run duplicate check)
        let args2 = CreateArgs {
            title: Some("Test item".to_string()),
            from_file: None,
            kind: "task".to_string(),
            size: None,
            urgency: None,
            parent: None,
            label: vec![],
            tag: vec![],
            labels: vec![],
            tags: vec![],
            description: None,
            blocks: vec![],
            force: true, // Force skip duplicate check
            allow_secret: false,
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

    #[test]
    fn tag_alias_works_like_label() {
        let w = TestCli::parse_from(["test", "--title", "Hello", "--tag", "foo"]);
        assert_eq!(w.args.all_labels(), vec!["foo"]);
    }

    #[test]
    fn tags_alias_splits_commas() {
        let w = TestCli::parse_from(["test", "--title", "Hello", "--tags", "a,b,c"]);
        assert_eq!(w.args.all_labels(), vec!["a", "b", "c"]);
    }

    #[test]
    fn labels_alias_splits_commas() {
        let w = TestCli::parse_from(["test", "--title", "Hello", "--labels", "a,b,c"]);
        assert_eq!(w.args.all_labels(), vec!["a", "b", "c"]);
    }

    #[test]
    fn mixed_label_aliases_merge() {
        let w = TestCli::parse_from([
            "test", "--title", "Hello", "-l", "x", "--tag", "y", "--tags", "a,b",
        ]);
        assert_eq!(w.args.all_labels(), vec!["x", "y", "a", "b"]);
    }
}
