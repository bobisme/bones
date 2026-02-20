//! `bn agents` — list known agents with assignment counts and recent activity.

use crate::output::{CliError, OutputMode, render, render_error};
use anyhow::Context;
use bones_core::db::query::try_open_projection;
use bones_core::event::parser::{ParsedLine, parse_line};
use bones_core::shard::ShardManager;
use chrono::{DateTime, Utc};
use clap::Args;
use rusqlite::params;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;

#[derive(Args, Debug, Default)]
pub struct AgentsArgs {}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentInfo {
    pub agent: String,
    pub assigned: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active: Option<String>,
}

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

fn scan_event_activity(shard_mgr: &ShardManager) -> anyhow::Result<HashMap<String, i64>> {
    let mut last_by_agent: HashMap<String, i64> = HashMap::new();

    for (year, month) in shard_mgr
        .list_shards()
        .map_err(|e| anyhow::anyhow!("list shards: {e}"))?
    {
        let shard_path = shard_mgr.shard_path(year, month);
        let content = fs::read_to_string(&shard_path)
            .with_context(|| format!("read shard {}", shard_path.display()))?;

        for line in content.lines() {
            match parse_line(line) {
                Ok(ParsedLine::Event(event)) => {
                    let event = *event;
                    let entry = last_by_agent.entry(event.agent).or_insert(event.wall_ts_us);
                    if event.wall_ts_us > *entry {
                        *entry = event.wall_ts_us;
                    }
                }
                Ok(ParsedLine::Blank | ParsedLine::Comment(_)) => {}
                Err(_) => {}
            }
        }
    }

    Ok(last_by_agent)
}

fn load_assignment_counts(db_path: &Path) -> anyhow::Result<HashMap<String, usize>> {
    let Some(conn) = try_open_projection(db_path)? else {
        return Ok(HashMap::new());
    };

    let mut stmt = conn
        .prepare("SELECT agent, COUNT(*) FROM item_assignees GROUP BY agent")
        .context("prepare assignment count query")?;

    let mut rows = stmt
        .query(params![])
        .context("execute assignment count query")?;
    let mut out = HashMap::new();

    while let Some(row) = rows.next().context("iterate assignment count row")? {
        let agent: String = row.get(0).context("read assignment count agent")?;
        let count: i64 = row.get(1).context("read assignment count")?;
        let count = usize::try_from(count).unwrap_or(usize::MAX);
        out.insert(agent, count);
    }

    Ok(out)
}

fn to_rfc3339(us: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_micros(us).map(|ts| ts.to_rfc3339())
}

pub fn collect_agent_inventory(project_root: &Path) -> anyhow::Result<Vec<AgentInfo>> {
    let bones_dir = find_bones_dir(project_root)
        .ok_or_else(|| anyhow::anyhow!("Not a bones project: .bones directory not found"))?;

    let shard_mgr = ShardManager::new(&bones_dir);
    let event_activity = scan_event_activity(&shard_mgr)?;
    let assignment_counts = load_assignment_counts(&bones_dir.join("bones.db"))?;

    let mut all_agents = BTreeSet::new();
    all_agents.extend(event_activity.keys().cloned());
    all_agents.extend(assignment_counts.keys().cloned());

    let mut rows = Vec::with_capacity(all_agents.len());
    for agent in all_agents {
        rows.push(AgentInfo {
            assigned: assignment_counts.get(&agent).copied().unwrap_or(0),
            last_active: event_activity.get(&agent).copied().and_then(to_rfc3339),
            agent,
        });
    }

    Ok(rows)
}

pub fn run_agents(
    _args: &AgentsArgs,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let rows = match collect_agent_inventory(project_root) {
        Ok(rows) => rows,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(
                    e.to_string(),
                    "Run 'bn init' to create a project, then 'bn admin rebuild' if needed",
                    "agents_query_failed",
                ),
            )?;
            return Err(e);
        }
    };

    render(output, &rows, |items, w| {
        if items.is_empty() {
            return writeln!(w, "(no known agents)");
        }

        writeln!(w, "{:<28} {:>8}  LAST ACTIVE", "AGENT", "ASSIGNED")?;
        writeln!(w, "{}", "-".repeat(72))?;
        for item in items {
            writeln!(
                w,
                "{:<28} {:>8}  {}",
                item.agent,
                item.assigned,
                item.last_active.as_deref().unwrap_or("-")
            )?;
        }
        Ok(())
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db;
    use bones_core::db::project;
    use bones_core::event::Event;
    use bones_core::event::data::{AssignAction, AssignData, CreateData, EventData, MoveData};
    use bones_core::event::types::EventType;
    use bones_core::event::writer::write_event;
    use bones_core::model::item::{Kind, State, Urgency};
    use bones_core::model::item_id::ItemId;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn setup_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let bones_dir = root.join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init");

        let db_path = bones_dir.join("bones.db");
        let conn = db::open_projection(&db_path).expect("open projection");
        let _ = project::ensure_tracking_table(&conn);
        let projector = project::Projector::new(&conn);

        let append = |mut event: Event| {
            let line = write_event(&mut event).expect("serialize event");
            shard_mgr
                .append(&line, false, Duration::from_secs(5))
                .expect("append event");
            projector.project_event(&event).expect("project event");
        };

        let ts1 = shard_mgr.next_timestamp().expect("ts1");
        append(Event {
            wall_ts_us: ts1,
            agent: "alice".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-a1"),
            data: EventData::Create(CreateData {
                title: "A".to_string(),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        });

        let ts2 = shard_mgr.next_timestamp().expect("ts2");
        append(Event {
            wall_ts_us: ts2,
            agent: "bob".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-a2"),
            data: EventData::Create(CreateData {
                title: "B".to_string(),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        });

        let ts3 = shard_mgr.next_timestamp().expect("ts3");
        append(Event {
            wall_ts_us: ts3,
            agent: "alice".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Assign,
            item_id: ItemId::new_unchecked("bn-a1"),
            data: EventData::Assign(AssignData {
                agent: "alice".to_string(),
                action: AssignAction::Assign,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        });

        let ts4 = shard_mgr.next_timestamp().expect("ts4");
        append(Event {
            wall_ts_us: ts4,
            agent: "bob".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Assign,
            item_id: ItemId::new_unchecked("bn-a2"),
            data: EventData::Assign(AssignData {
                agent: "bob".to_string(),
                action: AssignAction::Assign,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        });

        let ts5 = shard_mgr.next_timestamp().expect("ts5");
        append(Event {
            wall_ts_us: ts5,
            agent: "alice".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Move,
            item_id: ItemId::new_unchecked("bn-a1"),
            data: EventData::Move(MoveData {
                state: State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        });

        conn.execute(
            "INSERT INTO item_assignees (item_id, agent, created_at_us) VALUES (?1, ?2, ?3)",
            rusqlite::params!["bn-a1", "carol", ts5],
        )
        .expect("insert synthetic assignee");

        dir
    }

    #[test]
    fn collect_agent_inventory_merges_event_and_assignment_sources() {
        let dir = setup_project();
        let rows = collect_agent_inventory(dir.path()).expect("collect inventory");

        let by_name: HashMap<&str, &AgentInfo> =
            rows.iter().map(|r| (r.agent.as_str(), r)).collect();

        let alice = by_name.get("alice").expect("alice present");
        assert_eq!(alice.assigned, 1);
        assert!(alice.last_active.is_some());

        let bob = by_name.get("bob").expect("bob present");
        assert_eq!(bob.assigned, 1);
        assert!(bob.last_active.is_some());

        let carol = by_name
            .get("carol")
            .expect("carol present from assignment table");
        assert_eq!(carol.assigned, 1);
        assert!(carol.last_active.is_none());
    }

    #[test]
    fn to_rfc3339_formats_valid_microsecond_timestamp() {
        let iso = to_rfc3339(1_700_000_000_000_000).expect("valid timestamp");
        assert!(iso.contains('T'));
        assert!(iso.ends_with('Z') || iso.contains('+'));
    }
}
