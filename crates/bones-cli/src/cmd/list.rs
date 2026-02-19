//! `bn list` — list work items with filtering.

use crate::output::OutputMode;
use clap::Args;

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Filter by status: open, doing, done, archived.
    #[arg(short, long)]
    pub status: Option<String>,

    /// Filter by kind: task, goal, bug.
    #[arg(short, long)]
    pub kind: Option<String>,

    /// Filter by label.
    #[arg(short, long)]
    pub label: Vec<String>,

    /// Filter by parent item ID.
    #[arg(long)]
    pub parent: Option<String>,

    /// Maximum items to show.
    #[arg(short = 'n', long, default_value = "50")]
    pub limit: usize,
}

pub fn run_list(
    _args: &ListArgs,
    output: OutputMode,
    _project_root: &std::path::Path,
) -> anyhow::Result<()> {
    // TODO: wire to SQLite projection queries (bn-2da.3)
    crate::output::render_success(output, "No items found (projection not yet connected)")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_args_defaults() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: ListArgs,
        }
        let w = Wrapper::parse_from(["test"]);
        assert!(w.args.status.is_none());
        assert!(w.args.kind.is_none());
        assert!(w.args.label.is_empty());
        assert_eq!(w.args.limit, 50);
    }
}
