//! `bn bone punt|escalate|normalize` — quick urgency shortcuts.

use crate::cmd::update::{UpdateArgs, run_update};
use crate::output::OutputMode;
use clap::Args;
use std::path::Path;

/// Shared arguments for urgency shortcut commands.
#[derive(Args, Debug, Clone)]
pub struct UrgencyQuickArgs {
    /// Bone ID to update urgency for (supports partial IDs).
    pub id: String,

    /// Additional bone IDs to update with the same urgency.
    #[arg(value_name = "ID")]
    pub ids: Vec<String>,
}

fn to_update_args(args: &UrgencyQuickArgs, urgency: &str) -> UpdateArgs {
    UpdateArgs {
        id: args.id.clone(),
        ids: args.ids.clone(),
        title: None,
        description: None,
        size: None,
        urgency: Some(urgency.to_string()),
        kind: None,
    }
}

/// Set urgency to `punt`.
pub fn run_punt(
    args: &UrgencyQuickArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let update_args = to_update_args(args, "punt");
    run_update(&update_args, agent_flag, output, project_root)
}

/// Set urgency to `urgent`.
pub fn run_escalate(
    args: &UrgencyQuickArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let update_args = to_update_args(args, "urgent");
    run_update(&update_args, agent_flag, output, project_root)
}

/// Reset urgency to `default`.
pub fn run_normalize(
    args: &UrgencyQuickArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    project_root: &Path,
) -> anyhow::Result<()> {
    let update_args = to_update_args(args, "default");
    run_update(&update_args, agent_flag, output, project_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: UrgencyQuickArgs,
    }

    #[test]
    fn urgency_quick_args_parse_multiple_ids() {
        let parsed = Wrapper::parse_from(["test", "bn-abc", "bn-def", "bn-ghi"]);
        assert_eq!(parsed.args.id, "bn-abc");
        assert_eq!(parsed.args.ids, vec!["bn-def", "bn-ghi"]);
    }

    #[test]
    fn update_args_only_sets_urgency_field() {
        let args = UrgencyQuickArgs {
            id: "bn-abc".to_string(),
            ids: vec!["bn-def".to_string()],
        };
        let update = to_update_args(&args, "urgent");
        assert_eq!(update.id, "bn-abc");
        assert_eq!(update.ids, vec!["bn-def"]);
        assert_eq!(update.urgency.as_deref(), Some("urgent"));
        assert!(update.title.is_none());
        assert!(update.description.is_none());
        assert!(update.size.is_none());
        assert!(update.kind.is_none());
    }
}
