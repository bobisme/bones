//! `bn labels` and `bn label` namespace-aware label workflows.

use std::collections::BTreeMap;
use std::path::Path;

use clap::{Args, Subcommand};
use serde::Serialize;

use crate::cmd::tag::{TagArgs, UntagArgs, run_tag, run_untag};
use crate::output::{CliError, OutputMode, render, render_error};
use bones_core::db::query::{LabelCount, list_labels, try_open_projection};

#[derive(Args, Debug, Default)]
pub struct LabelsArgs {
    /// Group labels by namespace prefix (text before `:`).
    #[arg(long)]
    pub namespace: bool,

    /// Maximum number of labels to list.
    #[arg(long, short)]
    pub limit: Option<u32>,

    /// Offset for pagination.
    #[arg(long)]
    pub offset: Option<u32>,
}

#[derive(Args, Debug)]
pub struct LabelArgs {
    #[command(subcommand)]
    pub command: LabelCommand,
}

#[derive(Subcommand, Debug)]
pub enum LabelCommand {
    #[command(about = "Add one label to an item")]
    Add(LabelAddArgs),

    #[command(about = "Remove one label from an item")]
    Rm(LabelRmArgs),
}

#[derive(Args, Debug)]
pub struct LabelAddArgs {
    /// Item ID to label.
    pub id: String,

    /// Label to add.
    pub label: String,
}

#[derive(Args, Debug)]
pub struct LabelRmArgs {
    /// Item ID to unlabel.
    pub id: String,

    /// Label to remove.
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
struct LabelRow {
    name: String,
    count: usize,
}

#[derive(Debug, Clone, Serialize)]
struct NamespaceGroup {
    namespace: String,
    total: usize,
    labels: Vec<LabelRow>,
}

#[derive(Debug, Clone, Serialize)]
struct LabelsOutput {
    labels: Vec<LabelRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespaces: Option<Vec<NamespaceGroup>>,
}

fn open_db(project_root: &Path) -> anyhow::Result<rusqlite::Connection> {
    let db_path = project_root.join(".bones").join("bones.db");
    match try_open_projection(&db_path)? {
        Some(conn) => Ok(conn),
        None => anyhow::bail!(
            "projection database not found or corrupt at {}.\n  Run `bn rebuild` to initialize it.",
            db_path.display()
        ),
    }
}

fn namespace_of(label: &str) -> String {
    label
        .split_once(':')
        .map_or_else(|| "(none)".to_string(), |(ns, _)| ns.to_string())
}

fn to_rows(rows: Vec<LabelCount>) -> Vec<LabelRow> {
    rows.into_iter()
        .map(|row| LabelRow {
            name: row.name,
            count: row.count,
        })
        .collect()
}

fn group_by_namespace(labels: &[LabelRow]) -> Vec<NamespaceGroup> {
    let mut grouped: BTreeMap<String, Vec<LabelRow>> = BTreeMap::new();

    for row in labels {
        grouped
            .entry(namespace_of(&row.name))
            .or_default()
            .push(row.clone());
    }

    grouped
        .into_iter()
        .map(|(namespace, labels)| NamespaceGroup {
            total: labels.iter().map(|l| l.count).sum(),
            namespace,
            labels,
        })
        .collect()
}

pub fn run_labels(
    args: &LabelsArgs,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let conn = match open_db(project_root) {
        Ok(conn) => conn,
        Err(e) => {
            render_error(output, &CliError::new(e.to_string()))?;
            return Err(e);
        }
    };

    let rows = list_labels(&conn, args.limit, args.offset)?;
    let labels = to_rows(rows);
    let namespaces = args.namespace.then(|| group_by_namespace(&labels));
    let payload = LabelsOutput { labels, namespaces };

    render(output, &payload, |value, w| {
        if value.labels.is_empty() {
            return writeln!(w, "(no labels)");
        }

        if let Some(groups) = &value.namespaces {
            for group in groups {
                writeln!(w, "{} ({})", group.namespace, group.total)?;
                for row in &group.labels {
                    writeln!(w, "  {:<32} {:>6}", row.name, row.count)?;
                }
                writeln!(w)?;
            }
            return Ok(());
        }

        writeln!(w, "{:<32} {:>6}", "LABEL", "COUNT")?;
        writeln!(w, "{}", "-".repeat(40))?;
        for row in &value.labels {
            writeln!(w, "{:<32} {:>6}", row.name, row.count)?;
        }
        Ok(())
    })?;

    Ok(())
}

pub fn run_label(
    args: &LabelArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    match &args.command {
        LabelCommand::Add(add) => {
            let delegate = TagArgs {
                id: add.id.clone(),
                labels: vec![add.label.clone()],
                additional_ids: vec![],
            };
            run_tag(&delegate, agent_flag, output, project_root)
        }
        LabelCommand::Rm(rm) => {
            let delegate = UntagArgs {
                id: rm.id.clone(),
                labels: vec![rm.label.clone()],
                additional_ids: vec![],
            };
            run_untag(&delegate, agent_flag, output, project_root)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn labels_args_parses_namespace() {
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: LabelsArgs,
        }

        let w = Wrapper::parse_from(["test", "--namespace"]);
        assert!(w.args.namespace);
    }

    #[test]
    fn label_add_args_parse() {
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: LabelArgs,
        }

        let w = Wrapper::parse_from(["test", "add", "bn-123", "area:backend"]);
        match w.args.command {
            LabelCommand::Add(add) => {
                assert_eq!(add.id, "bn-123");
                assert_eq!(add.label, "area:backend");
            }
            _ => panic!("expected add"),
        }
    }
}
