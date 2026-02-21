//! `bn mine` — list items assigned to the current agent.
//!
//! This is a shortcut for `bn list --assignee <resolved-agent>` while still
//! supporting the standard `bn list` filters.

use crate::agent;
use crate::cmd::list::{ListArgs, run_list};
use crate::output::{CliError, OutputMode, render_error};
use crate::validate;
use clap::Args;
use std::path::Path;

#[derive(Args, Debug, Clone)]
pub struct MineArgs {
    #[command(flatten)]
    pub list: ListArgs,
}

fn build_mine_filter(args: &MineArgs, resolved_agent: String) -> ListArgs {
    let mut list = args.list.clone();
    list.assignee = Some(resolved_agent);
    list
}

pub fn run_mine(
    args: &MineArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let agent = match agent::require_agent(agent_flag) {
        Ok(a) => a,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(
                    &e.message,
                    "Set --agent, BONES_AGENT, AGENT, or USER (interactive only)",
                    e.code,
                ),
            )?;
            anyhow::bail!(e.message);
        }
    };

    if let Err(e) = validate::validate_agent(&agent) {
        render_error(output, &e.to_cli_error())?;
        anyhow::bail!(e.reason);
    }

    let list_args = build_mine_filter(args, agent);
    run_list(&list_args, output, project_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db::migrations;
    use clap::Parser;
    use rusqlite::Connection;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: MineArgs,
    }

    #[test]
    fn mine_args_parse_with_list_filters() {
        let parsed = Wrapper::parse_from([
            "test", "--state", "doing", "--kind", "task", "--label", "backend", "--sort",
            "priority",
        ]);

        assert_eq!(parsed.args.list.state.as_deref(), Some("doing"));
        assert_eq!(parsed.args.list.kind.as_deref(), Some("task"));
        assert_eq!(parsed.args.list.label, vec!["backend"]);
        assert_eq!(parsed.args.list.sort, "priority");
    }

    #[test]
    fn build_mine_filter_overrides_assignee() {
        let args = MineArgs {
            list: ListArgs {
                state: Some("open".to_string()),
                all: false,
                kind: None,
                label: vec![],
                urgency: None,
                assignee: Some("someone-else".to_string()),
                parent: None,
                since: None,
                until: None,
                limit: 50,
                offset: 0,
                sort: "updated".to_string(),
            },
        };

        let filtered = build_mine_filter(&args, "alice".to_string());
        assert_eq!(filtered.assignee.as_deref(), Some("alice"));
        assert_eq!(filtered.state.as_deref(), Some("open"));
    }

    #[test]
    fn run_mine_smoke() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bones_dir = dir.path().join(".bones");
        std::fs::create_dir_all(&bones_dir).expect("create .bones");
        let db_path = bones_dir.join("bones.db");

        let mut conn = Connection::open(&db_path).expect("open db");
        migrations::migrate(&mut conn).expect("migrate");

        conn.execute(
            "INSERT INTO items (item_id, title, kind, state, urgency, is_deleted, search_labels, created_at_us, updated_at_us) \
             VALUES ('bn-001', 'Mine', 'task', 'open', 'default', 0, '', 1000, 1000)",
            [],
        )
        .expect("insert item");
        conn.execute(
            "INSERT INTO item_assignees (item_id, agent, created_at_us) VALUES ('bn-001', 'alice', 1000)",
            [],
        )
        .expect("insert assignee");

        let args = MineArgs {
            list: ListArgs {
                state: None,
                all: false,
                kind: None,
                label: vec![],
                urgency: None,
                assignee: None,
                parent: None,
                since: None,
                until: None,
                limit: 50,
                offset: 0,
                sort: "updated".to_string(),
            },
        };

        run_mine(&args, Some("alice"), OutputMode::Json, dir.path()).expect("run mine");
    }
}
