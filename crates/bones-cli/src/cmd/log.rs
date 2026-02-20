//! `bn log`, `bn history`, and `bn blame` — audit trail inspection commands.

use crate::output::{CliError, OutputMode, render, render_error};
use anyhow::Context;
use bones_core::event::Event;
use bones_core::event::data::EventData;
use bones_core::event::parser::{ParsedLine, PartialParsedLine, parse_line, parse_line_partial};
use bones_core::shard::ShardManager;
use chrono::{DateTime, Utc};
use clap::Args;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Args, Debug, Clone)]
pub struct LogArgs {
    /// Item ID to inspect.
    pub id: String,

    /// Include events at/after this RFC3339 timestamp.
    #[arg(long)]
    pub since: Option<String>,

    /// Maximum number of rows to show.
    #[arg(short = 'n', long)]
    pub limit: Option<usize>,
}

#[derive(Args, Debug, Clone)]
pub struct HistoryArgs {
    /// Include events at/after this RFC3339 timestamp.
    #[arg(long)]
    pub since: Option<String>,

    /// Maximum number of rows to show.
    #[arg(short = 'n', long, default_value = "50")]
    pub limit: usize,

    /// Filter to one agent.
    #[arg(long)]
    pub agent: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct BlameArgs {
    /// Item ID to inspect.
    pub id: String,

    /// Field name to attribute (e.g. title, description, state).
    pub field: String,
}

#[derive(Debug, Clone, Serialize)]
struct EventRow {
    pub timestamp: String,
    pub timestamp_us: i64,
    pub item_id: String,
    pub agent: String,
    pub event_type: String,
    pub summary: String,
    #[serde(skip_serializing)]
    sort_event_hash: String,
}

#[derive(Debug, Clone, Serialize)]
struct BlameRow {
    pub item_id: String,
    pub field: String,
    pub agent: String,
    pub timestamp: String,
    pub timestamp_us: i64,
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_value: Option<Value>,
    pub new_value: Value,
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

fn parse_since_micros(input: &str) -> anyhow::Result<i64> {
    let ts = DateTime::parse_from_rfc3339(input)
        .with_context(|| format!("invalid --since timestamp: {input}"))?
        .with_timezone(&Utc);
    let secs = ts.timestamp();
    let micros = i64::from(ts.timestamp_subsec_micros());
    secs.checked_mul(1_000_000)
        .and_then(|v| v.checked_add(micros))
        .ok_or_else(|| anyhow::anyhow!("timestamp overflow for --since: {input}"))
}

fn micros_to_rfc3339(us: i64) -> String {
    DateTime::<Utc>::from_timestamp_micros(us)
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_else(|| us.to_string())
}

fn collect_events<F>(project_root: &Path, mut keep: F) -> anyhow::Result<Vec<Event>>
where
    F: FnMut(&bones_core::event::parser::PartialEvent<'_>) -> bool,
{
    let bones_dir = find_bones_dir(project_root)
        .ok_or_else(|| anyhow::anyhow!("Not a bones project: .bones directory not found"))?;
    let shard_mgr = ShardManager::new(&bones_dir);

    let mut events = Vec::new();
    for (year, month) in shard_mgr
        .list_shards()
        .map_err(|e| anyhow::anyhow!("list shards: {e}"))?
    {
        let shard_path = shard_mgr.shard_path(year, month);
        let content = shard_mgr
            .read_shard(year, month)
            .map_err(|e| anyhow::anyhow!("read shard {}: {e}", shard_path.display()))?;

        for (line_no, line) in content.lines().enumerate() {
            let partial = parse_line_partial(line).with_context(|| {
                format!(
                    "parse header {}:{}",
                    shard_path.display(),
                    line_no.saturating_add(1)
                )
            })?;

            let PartialParsedLine::Event(partial_event) = partial else {
                continue;
            };

            if !keep(&partial_event) {
                continue;
            }

            let parsed = parse_line(line).with_context(|| {
                format!(
                    "parse event {}:{}",
                    shard_path.display(),
                    line_no.saturating_add(1)
                )
            })?;

            if let ParsedLine::Event(event) = parsed {
                events.push(*event);
            }
        }
    }

    Ok(events)
}

fn event_summary(event: &Event) -> String {
    match &event.data {
        EventData::Create(data) => format!("create \"{}\"", data.title),
        EventData::Update(data) => format!("set {}={}", data.field, json_inline(&data.value)),
        EventData::Move(data) => match &data.reason {
            Some(reason) => format!("state={} ({reason})", data.state),
            None => format!("state={}", data.state),
        },
        EventData::Assign(data) => format!("{} {}", data.action, data.agent),
        EventData::Comment(data) => format!("comment {}", truncate(&data.body, 64)),
        EventData::Link(data) => format!("link {} -> {}", data.link_type, data.target),
        EventData::Unlink(data) => format!("unlink {}", data.target),
        EventData::Delete(_) => "delete".to_string(),
        EventData::Compact(data) => format!("compact {}", truncate(&data.summary, 64)),
        EventData::Snapshot(_) => "snapshot".to_string(),
        EventData::Redact(data) => format!("redact {} ({})", data.target_hash, data.reason),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

fn json_inline(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "<invalid-json>".to_string())
}

fn to_event_row(event: &Event) -> EventRow {
    EventRow {
        timestamp: micros_to_rfc3339(event.wall_ts_us),
        timestamp_us: event.wall_ts_us,
        item_id: event.item_id.to_string(),
        agent: event.agent.clone(),
        event_type: event.event_type.as_str().to_string(),
        summary: event_summary(event),
        sort_event_hash: event.event_hash.clone(),
    }
}

fn sort_rows_asc(rows: &mut [EventRow]) {
    rows.sort_by(|a, b| {
        a.timestamp_us
            .cmp(&b.timestamp_us)
            .then_with(|| a.agent.cmp(&b.agent))
            .then_with(|| a.sort_event_hash.cmp(&b.sort_event_hash))
    });
}

fn collect_log_rows(project_root: &Path, args: &LogArgs) -> anyhow::Result<Vec<EventRow>> {
    let since_us = args.since.as_deref().map(parse_since_micros).transpose()?;

    let mut rows: Vec<EventRow> = collect_events(project_root, |partial| {
        partial.item_id_raw == args.id
            && since_us
                .map(|cutoff| partial.wall_ts_us >= cutoff)
                .unwrap_or(true)
    })?
    .iter()
    .map(to_event_row)
    .collect();

    sort_rows_asc(&mut rows);

    if let Some(limit) = args.limit {
        rows.truncate(limit);
    }

    Ok(rows)
}

fn collect_history_rows(project_root: &Path, args: &HistoryArgs) -> anyhow::Result<Vec<EventRow>> {
    let since_us = args.since.as_deref().map(parse_since_micros).transpose()?;

    let mut rows: Vec<EventRow> = collect_events(project_root, |partial| {
        since_us
            .map(|cutoff| partial.wall_ts_us >= cutoff)
            .unwrap_or(true)
            && args
                .agent
                .as_deref()
                .map(|agent| partial.agent == agent)
                .unwrap_or(true)
    })?
    .iter()
    .map(to_event_row)
    .collect();

    sort_rows_asc(&mut rows);
    rows.reverse();
    rows.truncate(args.limit);
    Ok(rows)
}

fn event_field_changes(event: &Event) -> Vec<(String, Value)> {
    match &event.data {
        EventData::Update(data) => vec![(data.field.clone(), data.value.clone())],
        _ => event
            .data
            .to_json_value()
            .ok()
            .and_then(|v| match v {
                Value::Object(obj) => Some(
                    obj.into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .unwrap_or_default(),
    }
}

fn collect_blame(project_root: &Path, args: &BlameArgs) -> anyhow::Result<BlameRow> {
    let mut events = collect_events(project_root, |partial| partial.item_id_raw == args.id)?;
    events.sort_by(|a, b| {
        a.wall_ts_us
            .cmp(&b.wall_ts_us)
            .then_with(|| a.agent.cmp(&b.agent))
            .then_with(|| a.event_hash.cmp(&b.event_hash))
    });

    let mut state: BTreeMap<String, Value> = BTreeMap::new();
    let mut last: Option<BlameRow> = None;

    for event in events {
        for (name, new_value) in event_field_changes(&event) {
            let old_value = state.insert(name.clone(), new_value.clone());
            if name == args.field {
                last = Some(BlameRow {
                    item_id: event.item_id.to_string(),
                    field: name,
                    agent: event.agent.clone(),
                    timestamp: micros_to_rfc3339(event.wall_ts_us),
                    timestamp_us: event.wall_ts_us,
                    event_type: event.event_type.as_str().to_string(),
                    old_value,
                    new_value,
                });
            }
        }
    }

    last.ok_or_else(|| {
        anyhow::anyhow!(
            "No write found for field '{}' on item '{}'.",
            args.field,
            args.id
        )
    })
}

pub fn run_log(args: &LogArgs, output: OutputMode, project_root: &Path) -> anyhow::Result<()> {
    let rows = match collect_log_rows(project_root, args) {
        Ok(rows) => rows,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(
                    e.to_string(),
                    "Verify .bones/events shards and try `bn verify` if corruption is suspected",
                    "log_query_failed",
                ),
            )?;
            return Err(e);
        }
    };

    render(output, &rows, |items, w| {
        if items.is_empty() {
            return writeln!(w, "(no matching events)");
        }
        for item in items {
            writeln!(
                w,
                "{}  {:<24} {:<14} {}",
                item.timestamp, item.agent, item.event_type, item.summary
            )?;
        }
        Ok(())
    })
}

pub fn run_history(
    args: &HistoryArgs,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let rows = match collect_history_rows(project_root, args) {
        Ok(rows) => rows,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(
                    e.to_string(),
                    "Verify .bones/events shards and try `bn verify` if corruption is suspected",
                    "history_query_failed",
                ),
            )?;
            return Err(e);
        }
    };

    render(output, &rows, |items, w| {
        if items.is_empty() {
            return writeln!(w, "(no events)");
        }
        for item in items {
            writeln!(
                w,
                "{}  {:<12} {:<24} {:<14} {}",
                item.timestamp, item.item_id, item.agent, item.event_type, item.summary
            )?;
        }
        Ok(())
    })
}

pub fn run_blame(args: &BlameArgs, output: OutputMode, project_root: &Path) -> anyhow::Result<()> {
    let row = match collect_blame(project_root, args) {
        Ok(row) => row,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(
                    e.to_string(),
                    "Ensure the item/field exists and events were imported correctly",
                    "blame_query_failed",
                ),
            )?;
            return Err(e);
        }
    };

    render(output, &row, |item, w| {
        writeln!(w, "item:      {}", item.item_id)?;
        writeln!(w, "field:     {}", item.field)?;
        writeln!(w, "agent:     {}", item.agent)?;
        writeln!(w, "timestamp: {}", item.timestamp)?;
        writeln!(w, "event:     {}", item.event_type)?;
        if let Some(old) = &item.old_value {
            writeln!(w, "old:       {}", json_inline(old))?;
        } else {
            writeln!(w, "old:       <unset>")?;
        }
        writeln!(w, "new:       {}", json_inline(&item.new_value))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::event::Event;
    use bones_core::event::data::{CreateData, EventData, MoveData, UpdateData};
    use bones_core::event::types::EventType;
    use bones_core::event::writer::write_event;
    use bones_core::model::item::{Kind, State, Urgency};
    use bones_core::model::item_id::ItemId;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn setup_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let bones_dir = dir.path().join(".bones");
        let shard_mgr = ShardManager::new(&bones_dir);
        shard_mgr.ensure_dirs().expect("ensure dirs");
        shard_mgr.init().expect("init");

        let append = |mut event: Event| {
            let line = write_event(&mut event).expect("serialize event");
            shard_mgr
                .append(&line, false, Duration::from_secs(1))
                .expect("append event");
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
                description: Some("first".to_string()),
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
            event_type: EventType::Move,
            item_id: ItemId::new_unchecked("bn-a1"),
            data: EventData::Move(MoveData {
                state: State::Doing,
                reason: None,
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        });

        let ts3 = shard_mgr.next_timestamp().expect("ts3");
        append(Event {
            wall_ts_us: ts3,
            agent: "carol".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-b2"),
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

        let ts4 = shard_mgr.next_timestamp().expect("ts4");
        append(Event {
            wall_ts_us: ts4,
            agent: "dana".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Update,
            item_id: ItemId::new_unchecked("bn-a1"),
            data: EventData::Update(UpdateData {
                field: "title".to_string(),
                value: Value::String("A2".to_string()),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        });

        dir
    }

    #[test]
    fn log_filters_by_item_and_orders_ascending() {
        let dir = setup_project();
        let rows = collect_log_rows(
            dir.path(),
            &LogArgs {
                id: "bn-a1".to_string(),
                since: None,
                limit: None,
            },
        )
        .expect("log rows");

        assert_eq!(rows.len(), 3);
        assert!(
            rows.windows(2)
                .all(|w| w[0].timestamp_us <= w[1].timestamp_us)
        );
        assert!(rows.iter().all(|r| r.item_id == "bn-a1"));
    }

    #[test]
    fn history_filters_agent_and_applies_limit() {
        let dir = setup_project();
        let rows = collect_history_rows(
            dir.path(),
            &HistoryArgs {
                since: None,
                limit: 1,
                agent: Some("dana".to_string()),
            },
        )
        .expect("history rows");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent, "dana");
    }

    #[test]
    fn blame_returns_last_writer_for_field() {
        let dir = setup_project();
        let row = collect_blame(
            dir.path(),
            &BlameArgs {
                id: "bn-a1".to_string(),
                field: "title".to_string(),
            },
        )
        .expect("blame");

        assert_eq!(row.agent, "dana");
        assert_eq!(row.old_value, Some(Value::String("A".to_string())));
        assert_eq!(row.new_value, Value::String("A2".to_string()));
    }
}
