use anyhow::{Context as _, Result};
use bones_core::event::{Event, ParsedLine, parse_line};
use bones_core::shard::ShardManager;
use clap::Args;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Output JSONL path (defaults to stdout).
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct JsonlExportRecord {
    timestamp: i64,
    agent: String,
    #[serde(rename = "type")]
    event_type: String,
    item_id: String,
    data: JsonValue,
}

pub fn run_export(args: &ExportArgs, project_root: &Path) -> Result<()> {
    let shard_manager = ShardManager::new(project_root.join(".bones"));
    let content = shard_manager
        .replay()
        .context("failed to read existing .bones event shards")?;

    let mut out: Box<dyn Write> = match args.output.as_ref() {
        Some(path) => {
            let file = File::create(path)
                .with_context(|| format!("failed to create output file {}", path.display()))?;
            Box::new(BufWriter::new(file))
        }
        None => Box::new(BufWriter::new(io::stdout())),
    };

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let parsed = parse_line(line).context("failed to parse TSJSON line for export")?;
        if let ParsedLine::Event(event) = parsed {
            let row = export_row(&event)?;
            writeln!(out, "{}", serde_json::to_string(&row)?)?;
        }
    }

    Ok(())
}

fn export_row(event: &Event) -> Result<JsonlExportRecord> {
    let data = event
        .data
        .to_json_value()
        .context("failed to serialize event payload to JSON")?;

    Ok(JsonlExportRecord {
        timestamp: event.wall_ts_us,
        agent: event.agent.clone(),
        event_type: event.event_type.as_str().to_string(),
        item_id: event.item_id.to_string(),
        data,
    })
}
