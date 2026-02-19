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
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct MigrateArgs {
    /// Path to beads SQLite database.
    #[arg(long, value_name = "PATH", conflicts_with = "beads_jsonl")]
    pub beads_db: Option<PathBuf>,

    /// Path to beads JSONL export.
    #[arg(long, value_name = "PATH", conflicts_with = "beads_db")]
    pub beads_jsonl: Option<PathBuf>,

    /// Output summary in JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone)]
struct SourceIssue {
    source_id: String,
    title: String,
    description: Option<String>,
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
    #[serde(default)]
    issue_type: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
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
}

#[derive(Debug, Deserialize)]
struct JsonlComment {
    #[serde(default)]
    author: Option<String>,
    body: String,
    #[serde(default)]
    created_at: Option<JsonValue>,
}

#[derive(Debug, Deserialize)]
struct JsonlDependency {
    target: String,
    #[serde(default)]
    kind: Option<String>,
}

pub fn run_migrate(args: &MigrateArgs, project_root: &Path) -> Result<()> {
    let source = match (&args.beads_db, &args.beads_jsonl) {
        (Some(path), None) => load_from_sqlite(path)
            .with_context(|| format!("failed to read beads sqlite: {}", path.display()))?,
        (None, Some(path)) => load_from_jsonl(path)
            .with_context(|| format!("failed to read beads JSONL: {}", path.display()))?,
        _ => anyhow::bail!("provide exactly one source: --beads-db <path> OR --beads-jsonl <path>"),
    };

    let bones_dir = find_bones_dir(project_root)
        .ok_or_else(|| anyhow::anyhow!("Not a bones project: .bones directory not found"))?;

    let shard_manager = ShardManager::new(&bones_dir);
    let active_shard = shard_manager
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

    for issue in &source.issues {
        let item_id = id_map
            .get(&issue.source_id)
            .cloned()
            .with_context(|| format!("missing mapped item id for {}", issue.source_id))?;

        let (kind, mut labels, state, urgency) = map_issue_fields(issue);

        if let Some(parent) = issue.parent_source_id.as_ref() {
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
            parent: issue
                .parent_source_id
                .as_ref()
                .and_then(|p| id_map.get(p))
                .map(ToString::to_string),
            causation: None,
            description: issue.description.clone(),
            extra: BTreeMap::new(),
        };

        let mut create_event = Event {
            wall_ts_us: issue.created_us,
            agent: issue
                .actor
                .clone()
                .unwrap_or_else(|| "beads/importer".to_string()),
            itc: format!("itc:beads:{}:create", issue.source_id),
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
        append_event(&shard_manager, active_shard, &mut create_event)?;
        previous_hash.insert(item_id.to_string(), create_event.event_hash.clone());
        report.projection_events += 1;

        if let Some(assignee) = issue.assignee.as_deref().filter(|a| !a.trim().is_empty()) {
            let mut assign_event = Event {
                wall_ts_us: issue.created_us.saturating_add(1),
                agent: issue
                    .actor
                    .clone()
                    .unwrap_or_else(|| "beads/importer".to_string()),
                itc: format!("itc:beads:{}:assign", issue.source_id),
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
            append_event(&shard_manager, active_shard, &mut assign_event)?;
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
                itc: format!("itc:beads:{}:move", issue.source_id),
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
            append_event(&shard_manager, active_shard, &mut move_event)?;
            previous_hash.insert(item_id.to_string(), move_event.event_hash.clone());
            report.projection_events += 1;
        }

        if let Some(comments) = comments_by_issue.get(issue.source_id.as_str()) {
            for comment in comments {
                let mut comment_event = Event {
                    wall_ts_us: comment.created_us,
                    agent: comment.author.clone(),
                    itc: format!("itc:beads:{}:comment", issue.source_id),
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
                append_event(&shard_manager, active_shard, &mut comment_event)?;
                previous_hash.insert(item_id.to_string(), comment_event.event_hash.clone());
                report.comments_imported += 1;
                report.projection_events += 1;
            }
        }

        report.issues_imported += 1;
    }

    for dep in &source.dependencies {
        let Some(source_item_id) = id_map.get(&dep.source_id).cloned() else {
            continue;
        };
        let Some(target_item_id) = id_map.get(&dep.target_id).cloned() else {
            continue;
        };

        let mut link_event = Event {
            wall_ts_us: 0,
            agent: "beads/importer".to_string(),
            itc: format!("itc:beads:{}:link", dep.source_id),
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

        append_event(&shard_manager, active_shard, &mut link_event)?;
        previous_hash.insert(source_item_id.to_string(), link_event.event_hash.clone());
        report.dependencies_imported += 1;
        report.projection_events += 1;
    }

    let events_dir = bones_dir.join("events");
    let db_path = bones_dir.join("bones.db");
    db::rebuild::rebuild(&events_dir, &db_path)
        .context("failed to rebuild projection after migration")?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("bn migrate-from-beads");
        println!("  source:                {}", report.source);
        println!("  issues seen:           {}", report.issues_seen);
        println!("  issues imported:       {}", report.issues_imported);
        println!("  comments imported:     {}", report.comments_imported);
        println!("  dependencies imported: {}", report.dependencies_imported);
        println!("  events written:        {}", report.projection_events);
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
    shard_manager: &ShardManager,
    active_shard: (i32, u32),
    event: &mut Event,
) -> Result<()> {
    let line = write_event(event).context("failed to serialize migrated event")?;
    shard_manager
        .append_raw(active_shard.0, active_shard.1, &line)
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

    let urgency = match issue.priority.to_ascii_uppercase().as_str() {
        "P0" | "URGENT" => Urgency::Urgent,
        _ => Urgency::Default,
    };

    (kind, labels, state, urgency)
}

fn normalize_link_type(kind: &str) -> String {
    match kind.to_ascii_lowercase().as_str() {
        "blocks" | "block" => "blocks".to_string(),
        "relates" | "related" | "related_to" => "relates".to_string(),
        _ => "blocks".to_string(),
    }
}

fn load_from_sqlite(path: &Path) -> Result<SourceData> {
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open sqlite database {}", path.display()))?;

    let mut issues = Vec::new();
    {
        let sql = "
            SELECT
                id,
                title,
                COALESCE(description, ''),
                COALESCE(type, 'task'),
                COALESCE(status, 'open'),
                COALESCE(priority, 'P2'),
                NULLIF(TRIM(COALESCE(assignee, '')), ''),
                COALESCE(labels, ''),
                COALESCE(actor, ''),
                CAST(COALESCE(created_at, 0) AS TEXT),
                CAST(COALESCE(updated_at, COALESCE(created_at, 0)) AS TEXT),
                CAST(closed_at AS TEXT),
                parent_id
            FROM issues
            ORDER BY id ASC
        ";

        let mut stmt = conn.prepare(sql).context(
            "expected beads table 'issues' with columns id,title,description,type,status,priority,assignee,labels,actor,created_at,updated_at,closed_at,parent_id",
        )?;

        let rows = stmt.query_map([], |row| {
            let labels_raw: String = row.get(7)?;
            let actor_raw: String = row.get(8)?;
            Ok(SourceIssue {
                source_id: row.get::<_, String>(0)?,
                title: row.get::<_, String>(1)?,
                description: {
                    let v: String = row.get(2)?;
                    if v.trim().is_empty() { None } else { Some(v) }
                },
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
        let sql = "
            SELECT issue_id, COALESCE(author, ''), COALESCE(body, ''), COALESCE(created_at, 0)
            FROM comments
            ORDER BY issue_id ASC
        ";

        if let Ok(mut stmt) = conn.prepare(sql) {
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
        let sql = "SELECT source_id, target_id, COALESCE(kind, 'blocks') FROM dependencies";

        if let Ok(mut stmt) = conn.prepare(sql) {
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

        let issue = SourceIssue {
            source_id: parsed.id.clone(),
            title: parsed.title,
            description: parsed.description,
            issue_type: parsed.issue_type.unwrap_or_else(|| "task".to_string()),
            status: parsed.status.unwrap_or_else(|| "open".to_string()),
            priority: parsed.priority.unwrap_or_else(|| "P2".to_string()),
            labels: parsed.labels,
            assignee: parsed.assignee,
            actor: parsed.actor,
            created_us: to_micros_opt(parsed.created_at.as_ref()).unwrap_or(0),
            updated_us: to_micros_opt(parsed.updated_at.as_ref()).unwrap_or(0),
            closed_us: to_micros_opt(parsed.closed_at.as_ref()),
            parent_source_id: parsed.parent_id,
        };

        for c in parsed.comments {
            comments.push(SourceComment {
                issue_source_id: issue.source_id.clone(),
                author: c.author.unwrap_or_else(|| "beads/importer".to_string()),
                body: c.body,
                created_us: to_micros_opt(c.created_at.as_ref()).unwrap_or(issue.created_us),
            });
        }

        for d in parsed.dependencies {
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

    #[test]
    fn map_status_and_priority() {
        let issue = SourceIssue {
            source_id: "1".to_string(),
            title: "x".to_string(),
            description: None,
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
}
