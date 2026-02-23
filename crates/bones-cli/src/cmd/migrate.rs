use anyhow::{Context, Result};
use blake3::Hasher;
use bones_core::db;
use bones_core::event::writer::write_event;
use bones_core::event::{
    AssignAction, AssignData, CommentData, CreateData, Event, EventData, EventType, LinkData,
    MoveData,
};
use bones_core::model::item::{Kind, State, Urgency};
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;
use clap::Args;
use rusqlite::Connection;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::cmd::bones_gitattributes::{
    ensure_bones_gitattributes, remove_legacy_root_gitattributes_entry,
};
use crate::cmd::bones_gitignore::ensure_bones_gitignore;
use crate::itc_state::assign_next_itc;
use crate::output::{OutputMode, pretty_kv, pretty_section};

#[derive(Args, Debug)]
pub struct MigrateArgs {
    /// Path to beads SQLite database.
    #[arg(long, value_name = "PATH", conflicts_with = "beads_jsonl")]
    pub beads_db: Option<PathBuf>,

    /// Path to beads JSONL export.
    #[arg(long, value_name = "PATH", conflicts_with = "beads_db")]
    pub beads_jsonl: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct SourceIssue {
    source_id: String,
    title: String,
    description: Option<String>,
    extra_fields: Vec<(String, String)>,
    issue_type: String,
    status: String,
    priority: String,
    labels: Vec<String>,
    assignee: Option<String>,
    actor: Option<String>,
    created_us: i64,
    updated_us: i64,
    closed_us: Option<i64>,
    parent_source_id: Option<String>,
}

#[derive(Debug, Clone)]
struct SourceComment {
    issue_source_id: String,
    author: String,
    body: String,
    created_us: i64,
}

#[derive(Debug, Clone)]
struct SourceDependency {
    source_id: String,
    target_id: String,
    link_type: String,
}

#[derive(Debug, Default)]
struct SourceData {
    issues: Vec<SourceIssue>,
    comments: Vec<SourceComment>,
    dependencies: Vec<SourceDependency>,
}

#[derive(Debug, Serialize)]
struct MigrateReport {
    source: String,
    issues_seen: usize,
    issues_imported: usize,
    comments_imported: usize,
    dependencies_imported: usize,
    projection_events: usize,
}

#[derive(Debug, Deserialize)]
struct JsonlIssue {
    id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, alias = "type")]
    issue_type: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<JsonValue>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default, alias = "created_by")]
    actor: Option<String>,
    #[serde(default)]
    created_at: Option<JsonValue>,
    #[serde(default)]
    updated_at: Option<JsonValue>,
    #[serde(default)]
    closed_at: Option<JsonValue>,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    comments: Vec<JsonlComment>,
    #[serde(default)]
    dependencies: Vec<JsonlDependency>,
    #[serde(flatten)]
    extra_fields: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Deserialize)]
struct JsonlComment {
    #[serde(default, alias = "agent")]
    author: Option<String>,
    #[serde(alias = "text")]
    body: String,
    #[serde(default)]
    created_at: Option<JsonValue>,
}

#[derive(Debug, Deserialize)]
struct JsonlDependency {
    #[serde(alias = "depends_on_id", alias = "target_id")]
    target: String,
    #[serde(default, alias = "type", alias = "link_type")]
    kind: Option<String>,
}

pub fn run_migrate(args: &MigrateArgs, output: OutputMode, project_root: &Path) -> Result<()> {
    let source = match (&args.beads_db, &args.beads_jsonl) {
        (Some(path), None) => load_from_sqlite(path)
            .with_context(|| format!("failed to read beads sqlite: {}", path.display()))?,
        (None, Some(path)) => load_from_jsonl(path)
            .with_context(|| format!("failed to read beads JSONL: {}", path.display()))?,
        _ => anyhow::bail!("provide exactly one source: --beads-db <path> OR --beads-jsonl <path>"),
    };

    let bones_dir = find_bones_dir(project_root)
        .ok_or_else(|| anyhow::anyhow!("Not a bones project: .bones directory not found"))?;

    ensure_bones_gitignore(&bones_dir)
        .context("failed to ensure .bones/.gitignore for derived files")?;
    ensure_bones_gitattributes(&bones_dir)
        .context("failed to ensure .bones/.gitattributes for merge attributes")?;
    remove_legacy_root_gitattributes_entry(project_root)
        .context("failed to migrate legacy root .gitattributes entry")?;

    let shard_manager = ShardManager::new(&bones_dir);
    shard_manager
        .init()
        .context("failed to initialize .bones shard state")?;

    let mut id_map: HashMap<String, ItemId> = HashMap::new();
    for issue in &source.issues {
        id_map.insert(issue.source_id.clone(), map_item_id(&issue.source_id)?);
    }

    let mut previous_hash: HashMap<String, String> = HashMap::new();
    let mut comments_by_issue: HashMap<&str, Vec<&SourceComment>> = HashMap::new();
    for comment in &source.comments {
        comments_by_issue
            .entry(comment.issue_source_id.as_str())
            .or_default()
            .push(comment);
    }

    let mut report = MigrateReport {
        source: if args.beads_db.is_some() {
            "sqlite".to_string()
        } else {
            "jsonl".to_string()
        },
        issues_seen: source.issues.len(),
        issues_imported: 0,
        comments_imported: 0,
        dependencies_imported: 0,
        projection_events: 0,
    };

    let mut inferred_parent_by_source: HashMap<String, String> = HashMap::new();
    for dep in &source.dependencies {
        if is_parent_child_link(&dep.link_type) {
            inferred_parent_by_source
                .entry(dep.source_id.clone())
                .or_insert_with(|| dep.target_id.clone());
        }
    }

    let mut effective_parent_by_source: HashMap<String, String> = HashMap::new();
    for issue in &source.issues {
        if let Some(parent) = issue
            .parent_source_id
            .clone()
            .or_else(|| inferred_parent_by_source.get(&issue.source_id).cloned())
        {
            effective_parent_by_source.insert(issue.source_id.clone(), parent);
        }
    }

    let mut remaining: HashSet<String> =
        source.issues.iter().map(|i| i.source_id.clone()).collect();
    let mut ordered_issues: Vec<&SourceIssue> = Vec::with_capacity(source.issues.len());
    while !remaining.is_empty() {
        let mut progressed = false;
        for issue in &source.issues {
            if !remaining.contains(&issue.source_id) {
                continue;
            }

            let parent_ready = effective_parent_by_source
                .get(&issue.source_id)
                .map(|parent| !remaining.contains(parent))
                .unwrap_or(true);

            if parent_ready {
                remaining.remove(&issue.source_id);
                ordered_issues.push(issue);
                progressed = true;
            }
        }

        if !progressed {
            for issue in &source.issues {
                if remaining.remove(&issue.source_id) {
                    ordered_issues.push(issue);
                }
            }
        }
    }

    for issue in ordered_issues {
        let item_id = id_map
            .get(&issue.source_id)
            .cloned()
            .with_context(|| format!("missing mapped item id for {}", issue.source_id))?;

        let (kind, mut labels, state, urgency) = map_issue_fields(issue);

        let inferred_parent = inferred_parent_by_source.get(&issue.source_id).cloned();
        let effective_parent_source_id = effective_parent_by_source.get(&issue.source_id).cloned();

        if let (Some(explicit), Some(inferred)) =
            (issue.parent_source_id.as_ref(), inferred_parent.as_ref())
            && explicit != inferred
        {
            labels.push("migration:parent-conflict".to_string());
        }

        if let Some(parent) = effective_parent_source_id.as_ref() {
            if !id_map.contains_key(parent) {
                labels.push("migration:missing-parent".to_string());
            }
        }

        let create = CreateData {
            title: issue.title.clone(),
            kind,
            size: None,
            urgency,
            labels,
            parent: effective_parent_source_id
                .as_ref()
                .and_then(|p| id_map.get(p))
                .map(ToString::to_string),
            causation: None,
            description: merged_description(issue),
            extra: BTreeMap::new(),
        };

        let mut create_event = Event {
            wall_ts_us: issue.created_us,
            agent: issue
                .actor
                .clone()
                .unwrap_or_else(|| "beads/importer".to_string()),
            itc: String::new(),
            parents: previous_hash
                .get(item_id.as_str())
                .cloned()
                .into_iter()
                .collect(),
            event_type: EventType::Create,
            item_id: item_id.clone(),
            data: EventData::Create(create),
            event_hash: String::new(),
        };
        append_event(project_root, &shard_manager, &mut create_event)?;
        previous_hash.insert(item_id.to_string(), create_event.event_hash.clone());
        report.projection_events += 1;

        if let Some(assignee) = issue.assignee.as_deref().filter(|a| !a.trim().is_empty()) {
            let mut assign_event = Event {
                wall_ts_us: issue.created_us.saturating_add(1),
                agent: issue
                    .actor
                    .clone()
                    .unwrap_or_else(|| "beads/importer".to_string()),
                itc: String::new(),
                parents: previous_hash
                    .get(item_id.as_str())
                    .cloned()
                    .into_iter()
                    .collect(),
                event_type: EventType::Assign,
                item_id: item_id.clone(),
                data: EventData::Assign(AssignData {
                    agent: assignee.to_string(),
                    action: AssignAction::Assign,
                    extra: BTreeMap::new(),
                }),
                event_hash: String::new(),
            };
            append_event(project_root, &shard_manager, &mut assign_event)?;
            previous_hash.insert(item_id.to_string(), assign_event.event_hash.clone());
            report.projection_events += 1;
        }

        if state != State::Open {
            let mut move_event = Event {
                wall_ts_us: issue
                    .closed_us
                    .unwrap_or(issue.updated_us)
                    .max(issue.created_us),
                agent: issue
                    .actor
                    .clone()
                    .unwrap_or_else(|| "beads/importer".to_string()),
                itc: String::new(),
                parents: previous_hash
                    .get(item_id.as_str())
                    .cloned()
                    .into_iter()
                    .collect(),
                event_type: EventType::Move,
                item_id: item_id.clone(),
                data: EventData::Move(MoveData {
                    state,
                    reason: Some("Imported from beads".to_string()),
                    extra: BTreeMap::new(),
                }),
                event_hash: String::new(),
            };
            append_event(project_root, &shard_manager, &mut move_event)?;
            previous_hash.insert(item_id.to_string(), move_event.event_hash.clone());
            report.projection_events += 1;
        }

        if let Some(comments) = comments_by_issue.get(issue.source_id.as_str()) {
            for comment in comments {
                let mut comment_event = Event {
                    wall_ts_us: comment.created_us,
                    agent: comment.author.clone(),
                    itc: String::new(),
                    parents: previous_hash
                        .get(item_id.as_str())
                        .cloned()
                        .into_iter()
                        .collect(),
                    event_type: EventType::Comment,
                    item_id: item_id.clone(),
                    data: EventData::Comment(CommentData {
                        body: comment.body.clone(),
                        extra: BTreeMap::new(),
                    }),
                    event_hash: String::new(),
                };
                append_event(project_root, &shard_manager, &mut comment_event)?;
                previous_hash.insert(item_id.to_string(), comment_event.event_hash.clone());
                report.comments_imported += 1;
                report.projection_events += 1;
            }
        }

        report.issues_imported += 1;
    }

    for dep in &source.dependencies {
        if is_parent_child_link(&dep.link_type) {
            continue;
        }

        let Some(source_item_id) = id_map.get(&dep.source_id).cloned() else {
            continue;
        };
        let Some(target_item_id) = id_map.get(&dep.target_id).cloned() else {
            continue;
        };

        let mut link_event = Event {
            wall_ts_us: 0,
            agent: "beads/importer".to_string(),
            itc: String::new(),
            parents: previous_hash
                .get(source_item_id.as_str())
                .cloned()
                .into_iter()
                .collect(),
            event_type: EventType::Link,
            item_id: source_item_id.clone(),
            data: EventData::Link(LinkData {
                target: target_item_id.to_string(),
                link_type: normalize_link_type(&dep.link_type),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };

        let source_ts = source
            .issues
            .iter()
            .find(|x| x.source_id == dep.source_id)
            .map(|x| x.updated_us)
            .unwrap_or(0);
        link_event.wall_ts_us = source_ts.saturating_add(1);

        append_event(project_root, &shard_manager, &mut link_event)?;
        previous_hash.insert(source_item_id.to_string(), link_event.event_hash.clone());
        report.dependencies_imported += 1;
        report.projection_events += 1;
    }

    let events_dir = bones_dir.join("events");
    let db_path = bones_dir.join("bones.db");
    db::rebuild::rebuild(&events_dir, &db_path)
        .context("failed to rebuild projection after migration")?;

    match output {
        OutputMode::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        OutputMode::Text => {
            println!(
                "migrate source={} issues_seen={} issues_imported={} comments_imported={} dependencies_imported={} events_written={}",
                report.source,
                report.issues_seen,
                report.issues_imported,
                report.comments_imported,
                report.dependencies_imported,
                report.projection_events
            );
        }
        OutputMode::Pretty => {
            let stdout = std::io::stdout();
            let mut w = stdout.lock();
            pretty_section(&mut w, "Migration Report")?;
            pretty_kv(&mut w, "Source", &report.source)?;
            pretty_kv(&mut w, "Issues seen", report.issues_seen.to_string())?;
            pretty_kv(
                &mut w,
                "Issues imported",
                report.issues_imported.to_string(),
            )?;
            pretty_kv(
                &mut w,
                "Comments imported",
                report.comments_imported.to_string(),
            )?;
            pretty_kv(
                &mut w,
                "Dependencies",
                report.dependencies_imported.to_string(),
            )?;
            pretty_kv(
                &mut w,
                "Events written",
                report.projection_events.to_string(),
            )?;
        }
    }

    Ok(())
}

fn find_bones_dir(start: &Path) -> Option<PathBuf> {
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

fn append_event(
    project_root: &Path,
    shard_manager: &ShardManager,
    event: &mut Event,
) -> Result<()> {
    use bones_core::lock::ShardLock;
    use std::time::Duration;

    let lock_path = shard_manager.lock_path();
    let _lock = ShardLock::acquire(&lock_path, Duration::from_secs(5))
        .context("failed to acquire shard lock")?;

    let (year, month) = shard_manager
        .rotate_if_needed()
        .context("failed to rotate shards")?;

    assign_next_itc(project_root, event)?;
    let line = write_event(event).context("failed to serialize migrated event")?;
    shard_manager
        .append_raw(year, month, &line)
        .context("failed to append migrated event")
}

fn map_item_id(source_id: &str) -> Result<ItemId> {
    if let Ok(existing) = ItemId::parse(source_id) {
        return Ok(existing);
    }

    let mut hasher = Hasher::new();
    hasher.update(b"beads:");
    hasher.update(source_id.as_bytes());
    let hex = hasher.finalize().to_hex().to_string();
    let mapped = format!("bn-b{}", &hex[..8]);
    ItemId::parse(&mapped)
        .with_context(|| format!("failed to generate item id from source id '{source_id}'"))
}

fn map_issue_fields(issue: &SourceIssue) -> (Kind, Vec<String>, State, Urgency) {
    let mut labels = issue.labels.clone();

    let kind = match issue.issue_type.to_ascii_lowercase().as_str() {
        "bug" => Kind::Bug,
        "epic" | "goal" => Kind::Goal,
        _ => Kind::Task,
    };

    let status = issue.status.to_ascii_lowercase();
    let state = match status.as_str() {
        "in_progress" | "doing" => State::Doing,
        "closed" | "verified" | "wontfix" | "done" => State::Done,
        "deferred" => {
            labels.push("punt".to_string());
            State::Open
        }
        _ => State::Open,
    };

    let priority = issue.priority.trim().to_ascii_uppercase();
    let urgency = if priority == "P0" {
        Urgency::Urgent
    } else if let Ok(value) = priority.parse::<i64>() {
        if value <= 0 {
            Urgency::Urgent
        } else {
            Urgency::Default
        }
    } else if let Some(value) = priority
        .strip_prefix('P')
        .and_then(|raw| raw.parse::<i64>().ok())
    {
        if value == 0 {
            Urgency::Urgent
        } else {
            Urgency::Default
        }
    } else {
        Urgency::Default
    };

    (kind, labels, state, urgency)
}

fn is_parent_child_link(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "parent-child" | "parent_child" | "parentchild"
    )
}

fn normalize_link_type(kind: &str) -> String {
    match kind.to_ascii_lowercase().as_str() {
        "blocks" | "block" => "blocks".to_string(),
        "relates" | "related" | "related_to" => "relates".to_string(),
        _ => "blocks".to_string(),
    }
}

fn merged_description(issue: &SourceIssue) -> Option<String> {
    let mut out = String::new();
    if let Some(description) = issue.description.as_ref().filter(|v| !v.trim().is_empty()) {
        out.push_str(description.trim_end());
    }

    if !issue.extra_fields.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Imported beads fields:\n");
        for (idx, (name, value)) in issue.extra_fields.iter().enumerate() {
            if idx > 0 {
                out.push('\n');
            }
            out.push_str(name);
            out.push_str(":\n");
            out.push_str(value.trim_end());
            out.push('\n');
        }
    }

    if out.trim().is_empty() {
        None
    } else {
        Some(out.trim_end().to_string())
    }
}

fn json_value_to_text(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::Null => None,
        JsonValue::String(v) => {
            if v.trim().is_empty() {
                None
            } else {
                Some(v.clone())
            }
        }
        JsonValue::Number(v) => Some(v.to_string()),
        JsonValue::Bool(v) => Some(v.to_string()),
        JsonValue::Array(v) => {
            if v.is_empty() {
                None
            } else {
                serde_json::to_string(v).ok()
            }
        }
        JsonValue::Object(v) => {
            if v.is_empty() {
                None
            } else {
                serde_json::to_string(v).ok()
            }
        }
    }
}

fn normalize_json_extra_fields(extra: BTreeMap<String, JsonValue>) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    for (name, value) in extra {
        if let Some(text) = json_value_to_text(&value) {
            fields.push((name, text));
        }
    }
    fields
}

fn quote_sql_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn is_mapped_issue_column(name: &str) -> bool {
    matches!(
        name,
        "id" | "title"
            | "description"
            | "type"
            | "issue_type"
            | "status"
            | "priority"
            | "assignee"
            | "labels"
            | "actor"
            | "created_by"
            | "created_at"
            | "updated_at"
            | "closed_at"
            | "parent_id"
    )
}

fn table_columns(conn: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .with_context(|| format!("failed to inspect sqlite table '{table}'"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut cols = HashSet::new();
    for row in rows {
        cols.insert(row?);
    }
    Ok(cols)
}

fn pick_first_column<'a>(columns: &'a HashSet<String>, candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .find(|candidate| columns.contains(*candidate))
}

fn load_from_sqlite(path: &Path) -> Result<SourceData> {
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open sqlite database {}", path.display()))?;

    let issue_columns = table_columns(&conn, "issues")?;
    if issue_columns.is_empty() {
        anyhow::bail!("expected beads table 'issues'");
    }

    let issue_type_expr = if issue_columns.contains("type") {
        "COALESCE(type, 'task')"
    } else if issue_columns.contains("issue_type") {
        "COALESCE(issue_type, 'task')"
    } else {
        "'task'"
    };
    let status_expr = if issue_columns.contains("status") {
        "COALESCE(status, 'open')"
    } else {
        "'open'"
    };
    let priority_expr = if issue_columns.contains("priority") {
        "CAST(COALESCE(priority, 'P2') AS TEXT)"
    } else {
        "'P2'"
    };
    let assignee_expr = if issue_columns.contains("assignee") {
        "NULLIF(TRIM(COALESCE(assignee, '')), '')"
    } else {
        "NULL"
    };
    let labels_expr = if issue_columns.contains("labels") {
        "COALESCE(labels, '')"
    } else {
        "''"
    };
    let actor_expr = if issue_columns.contains("actor") {
        "COALESCE(actor, '')"
    } else if issue_columns.contains("created_by") {
        "COALESCE(created_by, '')"
    } else {
        "''"
    };
    let created_expr = if issue_columns.contains("created_at") {
        "created_at"
    } else {
        "0"
    };
    let updated_expr = if issue_columns.contains("updated_at") {
        "updated_at"
    } else {
        created_expr
    };
    let closed_expr = if issue_columns.contains("closed_at") {
        "closed_at"
    } else {
        "NULL"
    };
    let parent_expr = if issue_columns.contains("parent_id") {
        "parent_id"
    } else {
        "NULL"
    };
    let mut extra_columns: Vec<String> = issue_columns
        .iter()
        .filter(|name| !is_mapped_issue_column(name))
        .cloned()
        .collect();
    extra_columns.sort_unstable();
    let extra_select = if extra_columns.is_empty() {
        String::new()
    } else {
        extra_columns
            .iter()
            .map(|name| format!(", CAST(COALESCE({}, '') AS TEXT)", quote_sql_ident(name)))
            .collect::<String>()
    };

    let mut issues = Vec::new();
    {
        let sql = format!(
            "
            SELECT
                id,
                title,
                COALESCE(description, ''),
                {issue_type_expr},
                {status_expr},
                {priority_expr},
                {assignee_expr},
                {labels_expr},
                {actor_expr},
                CAST(COALESCE({created_expr}, 0) AS TEXT),
                CAST(COALESCE({updated_expr}, COALESCE({created_expr}, 0)) AS TEXT),
                CAST({closed_expr} AS TEXT),
                {parent_expr}
                {extra_select}
            FROM issues
            ORDER BY id ASC
        "
        );

        let mut stmt = conn
            .prepare(&sql)
            .context("failed to query beads 'issues' table")?;

        let rows = stmt.query_map([], |row| {
            let labels_raw: String = row.get(7)?;
            let actor_raw: String = row.get(8)?;
            let mut extra_fields = Vec::new();
            for (offset, name) in extra_columns.iter().enumerate() {
                let value: String = row.get(13 + offset)?;
                if value.trim().is_empty() {
                    continue;
                }
                extra_fields.push((name.clone(), value));
            }
            Ok(SourceIssue {
                source_id: row.get::<_, String>(0)?,
                title: row.get::<_, String>(1)?,
                description: {
                    let v: String = row.get(2)?;
                    if v.trim().is_empty() { None } else { Some(v) }
                },
                extra_fields,
                issue_type: row.get::<_, String>(3)?,
                status: row.get::<_, String>(4)?,
                priority: row.get::<_, String>(5)?,
                assignee: row.get::<_, Option<String>>(6)?,
                labels: split_labels(&labels_raw),
                actor: if actor_raw.trim().is_empty() {
                    None
                } else {
                    Some(actor_raw)
                },
                created_us: parse_any_timestamp(&row.get::<_, String>(9)?).unwrap_or(0),
                updated_us: parse_any_timestamp(&row.get::<_, String>(10)?).unwrap_or(0),
                closed_us: row
                    .get::<_, Option<String>>(11)
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(parse_any_timestamp),
                parent_source_id: row.get::<_, Option<String>>(12).ok().flatten(),
            })
        })?;

        for row in rows {
            issues.push(row?);
        }
    }

    let mut comments = Vec::new();
    {
        let comment_columns = table_columns(&conn, "comments")?;
        if !comment_columns.is_empty()
            && let (Some(issue_col), Some(body_col)) = (
                pick_first_column(&comment_columns, &["issue_id", "item_id", "source_id"]),
                pick_first_column(&comment_columns, &["body", "text", "comment"]),
            )
        {
            let author_expr = if let Some(author_col) =
                pick_first_column(&comment_columns, &["author", "agent", "created_by"])
            {
                format!("COALESCE({author_col}, '')")
            } else {
                "''".to_string()
            };
            let created_expr = if let Some(created_col) = pick_first_column(
                &comment_columns,
                &["created_at", "created_us", "wall_ts_us"],
            ) {
                format!("COALESCE({created_col}, 0)")
            } else {
                "0".to_string()
            };

            let sql = format!(
                "
                SELECT {issue_col}, {author_expr}, COALESCE({body_col}, ''), {created_expr}
                FROM comments
                ORDER BY {issue_col} ASC
            "
            );

            let mut stmt = conn
                .prepare(&sql)
                .context("failed to query beads 'comments' table")?;
            let rows = stmt.query_map([], |row| {
                let author_raw: String = row.get(1)?;
                let created_value: String = row.get(3)?;
                Ok(SourceComment {
                    issue_source_id: row.get::<_, String>(0)?,
                    author: if author_raw.trim().is_empty() {
                        "beads/importer".to_string()
                    } else {
                        author_raw
                    },
                    body: row.get::<_, String>(2)?,
                    created_us: parse_any_timestamp(&created_value).unwrap_or(0),
                })
            })?;

            for row in rows {
                comments.push(row?);
            }
        }
    }

    let mut dependencies = Vec::new();
    {
        let dep_columns = table_columns(&conn, "dependencies")?;
        if !dep_columns.is_empty()
            && let (Some(source_col), Some(target_col)) = (
                pick_first_column(&dep_columns, &["source_id", "issue_id", "item_id"]),
                pick_first_column(&dep_columns, &["target_id", "depends_on_id", "depends_on"]),
            )
        {
            let kind_expr = if let Some(kind_col) =
                pick_first_column(&dep_columns, &["kind", "type", "link_type"])
            {
                format!("COALESCE({kind_col}, 'blocks')")
            } else {
                "'blocks'".to_string()
            };

            let sql = format!("SELECT {source_col}, {target_col}, {kind_expr} FROM dependencies");

            let mut stmt = conn
                .prepare(&sql)
                .context("failed to query beads 'dependencies' table")?;
            let rows = stmt.query_map([], |row| {
                Ok(SourceDependency {
                    source_id: row.get::<_, String>(0)?,
                    target_id: row.get::<_, String>(1)?,
                    link_type: row.get::<_, String>(2)?,
                })
            })?;

            for row in rows {
                dependencies.push(row?);
            }
        }
    }

    Ok(SourceData {
        issues,
        comments,
        dependencies,
    })
}

fn load_from_jsonl(path: &Path) -> Result<SourceData> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut issues = Vec::new();
    let mut comments = Vec::new();
    let mut dependencies = Vec::new();

    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let parsed: JsonlIssue = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSON on line {}", idx + 1))?;

        let JsonlIssue {
            id,
            title,
            description,
            issue_type,
            status,
            priority,
            labels,
            assignee,
            actor,
            created_at,
            updated_at,
            closed_at,
            parent_id,
            comments: issue_comments,
            dependencies: issue_dependencies,
            extra_fields,
        } = parsed;

        let issue = SourceIssue {
            source_id: id,
            title,
            description,
            extra_fields: normalize_json_extra_fields(extra_fields),
            issue_type: issue_type.unwrap_or_else(|| "task".to_string()),
            status: status.unwrap_or_else(|| "open".to_string()),
            priority: normalize_priority_value(priority.as_ref()),
            labels,
            assignee,
            actor,
            created_us: to_micros_opt(created_at.as_ref()).unwrap_or(0),
            updated_us: to_micros_opt(updated_at.as_ref()).unwrap_or(0),
            closed_us: to_micros_opt(closed_at.as_ref()),
            parent_source_id: parent_id,
        };

        for c in issue_comments {
            comments.push(SourceComment {
                issue_source_id: issue.source_id.clone(),
                author: c.author.unwrap_or_else(|| "beads/importer".to_string()),
                body: c.body,
                created_us: to_micros_opt(c.created_at.as_ref()).unwrap_or(issue.created_us),
            });
        }

        for d in issue_dependencies {
            dependencies.push(SourceDependency {
                source_id: issue.source_id.clone(),
                target_id: d.target,
                link_type: d.kind.unwrap_or_else(|| "blocks".to_string()),
            });
        }

        issues.push(issue);
    }

    Ok(SourceData {
        issues,
        comments,
        dependencies,
    })
}

fn split_labels(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn normalize_priority_value(value: Option<&JsonValue>) -> String {
    match value {
        Some(JsonValue::String(s)) => s.clone(),
        Some(JsonValue::Number(n)) => {
            if let Some(v) = n.as_i64() {
                format!("P{v}")
            } else {
                "P2".to_string()
            }
        }
        _ => "P2".to_string(),
    }
}

fn to_micros_opt(value: Option<&JsonValue>) -> Option<i64> {
    value.and_then(to_micros_value)
}

fn parse_any_timestamp(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    if let Ok(num) = raw.parse::<i64>() {
        if num > 1_000_000_000_000 {
            return Some(num);
        }
        return Some(num.saturating_mul(1_000_000));
    }

    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp_micros())
}

fn to_micros_value(value: &JsonValue) -> Option<i64> {
    match value {
        JsonValue::Number(n) => {
            let raw = n.as_i64()?;
            if raw > 1_000_000_000_000 {
                Some(raw)
            } else {
                Some(raw.saturating_mul(1_000_000))
            }
        }
        JsonValue::String(s) => parse_any_timestamp(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputMode;
    use tempfile::TempDir;

    #[test]
    fn map_status_and_priority() {
        let issue = SourceIssue {
            source_id: "1".to_string(),
            title: "x".to_string(),
            description: None,
            extra_fields: vec![],
            issue_type: "feature".to_string(),
            status: "deferred".to_string(),
            priority: "P0".to_string(),
            labels: vec![],
            assignee: None,
            actor: None,
            created_us: 0,
            updated_us: 0,
            closed_us: None,
            parent_source_id: None,
        };

        let (kind, labels, state, urgency) = map_issue_fields(&issue);
        assert_eq!(kind, Kind::Task);
        assert_eq!(state, State::Open);
        assert_eq!(urgency, Urgency::Urgent);
        assert!(labels.iter().any(|l| l == "punt"));
    }

    #[test]
    fn map_priority_marks_only_p0_as_urgent() {
        let mut issue = SourceIssue {
            source_id: "1".to_string(),
            title: "x".to_string(),
            description: None,
            extra_fields: vec![],
            issue_type: "task".to_string(),
            status: "open".to_string(),
            priority: "P0".to_string(),
            labels: vec![],
            assignee: None,
            actor: None,
            created_us: 0,
            updated_us: 0,
            closed_us: None,
            parent_source_id: None,
        };

        let (_, _, _, urgency_p0) = map_issue_fields(&issue);
        assert_eq!(urgency_p0, Urgency::Urgent);

        issue.priority = "P1".to_string();
        let (_, _, _, urgency_p1) = map_issue_fields(&issue);
        assert_eq!(urgency_p1, Urgency::Default);

        issue.priority = "0".to_string();
        let (_, _, _, urgency_zero) = map_issue_fields(&issue);
        assert_eq!(urgency_zero, Urgency::Urgent);

        issue.priority = "1".to_string();
        let (_, _, _, urgency_one) = map_issue_fields(&issue);
        assert_eq!(urgency_one, Urgency::Default);
    }

    #[test]
    fn item_id_mapping_is_stable() {
        let a = map_item_id("42").expect("id");
        let b = map_item_id("42").expect("id");
        assert_eq!(a, b);
    }

    #[test]
    fn parse_seconds_to_micros() {
        let v = JsonValue::Number(serde_json::Number::from(1_700_000_000_i64));
        assert_eq!(to_micros_value(&v), Some(1_700_000_000_000_000));
    }

    #[test]
    fn merged_description_appends_extra_fields() {
        let issue = SourceIssue {
            source_id: "x".to_string(),
            title: "x".to_string(),
            description: Some("Primary body".to_string()),
            extra_fields: vec![
                (
                    "acceptance_criteria".to_string(),
                    "- first\n- second".to_string(),
                ),
                ("notes".to_string(), "Remember to migrate".to_string()),
            ],
            issue_type: "task".to_string(),
            status: "open".to_string(),
            priority: "P2".to_string(),
            labels: vec![],
            assignee: None,
            actor: None,
            created_us: 0,
            updated_us: 0,
            closed_us: None,
            parent_source_id: None,
        };

        let merged = merged_description(&issue).expect("description");
        assert!(merged.contains("Primary body"));
        assert!(merged.contains("Imported beads fields:"));
        assert!(merged.contains("acceptance_criteria:\n- first\n- second"));
        assert!(merged.contains("notes:\nRemember to migrate"));
    }

    #[test]
    fn parent_child_dependency_is_detected() {
        assert!(is_parent_child_link("parent-child"));
        assert!(is_parent_child_link("PARENT_CHILD"));
        assert!(is_parent_child_link("ParentChild"));
        assert!(!is_parent_child_link("blocks"));
    }

    #[test]
    fn migrate_moves_legacy_root_gitattributes_entry_into_bones_dir() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        std::fs::create_dir_all(root.join(".bones")).expect("create .bones");

        std::fs::write(
            root.join(".gitattributes"),
            ".bones/events merge=union\n*.png binary\n",
        )
        .expect("seed root .gitattributes");

        let source = root.join("beads.jsonl");
        std::fs::write(
            &source,
            r#"{"id":"42","title":"Imported from beads"}"#.to_string() + "\n",
        )
        .expect("seed jsonl source");

        let args = MigrateArgs {
            beads_db: None,
            beads_jsonl: Some(source),
        };

        run_migrate(&args, OutputMode::Text, root).expect("migration should succeed");

        let bones_attrs = std::fs::read_to_string(root.join(".bones/.gitattributes"))
            .expect("read .bones/.gitattributes");
        assert!(bones_attrs.contains("events merge=union"));

        let root_attrs =
            std::fs::read_to_string(root.join(".gitattributes")).expect("read root .gitattributes");
        assert!(!root_attrs.contains(".bones/events merge=union"));
        assert!(root_attrs.contains("*.png binary"));
    }

    #[test]
    fn migrate_uses_parent_child_dependency_as_hierarchy() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        std::fs::create_dir_all(root.join(".bones")).expect("create .bones");

        let source = root.join("beads.jsonl");
        std::fs::write(
            &source,
            concat!(
                "{\"id\":\"task-1\",\"title\":\"Child\",\"dependencies\":[{\"target_id\":\"epic-1\",\"type\":\"parent-child\"}]}\n",
                "{\"id\":\"epic-1\",\"title\":\"Parent\",\"type\":\"epic\"}\n"
            ),
        )
        .expect("seed jsonl source");

        let args = MigrateArgs {
            beads_db: None,
            beads_jsonl: Some(source),
        };

        run_migrate(&args, OutputMode::Text, root).expect("migration should succeed");

        let parent_id = map_item_id("epic-1").expect("mapped parent id").to_string();
        let child_id = map_item_id("task-1").expect("mapped child id").to_string();

        let conn = Connection::open(root.join(".bones/bones.db")).expect("open projection");
        let db_parent: Option<String> = conn
            .query_row(
                "SELECT parent_id FROM items WHERE item_id = ?1",
                [child_id.as_str()],
                |row| row.get(0),
            )
            .expect("query child parent");
        assert_eq!(db_parent.as_deref(), Some(parent_id.as_str()));

        let dep_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM item_dependencies WHERE item_id = ?1",
                [child_id.as_str()],
                |row| row.get(0),
            )
            .expect("query child dependencies");
        assert_eq!(
            dep_count, 0,
            "parent-child should not be imported as blocks"
        );
    }
}
