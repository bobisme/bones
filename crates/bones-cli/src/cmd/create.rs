//! `bn create` — create a new work item.

use crate::agent;
use crate::output::{CliError, OutputMode, render_error, render_success};
use clap::Args;

#[derive(Args, Debug)]
pub struct CreateArgs {
    /// Title of the new item.
    #[arg(short, long)]
    pub title: String,

    /// Item kind: task, goal, or bug.
    #[arg(short, long, default_value = "task")]
    pub kind: String,

    /// Parent item ID (makes this a child of a goal).
    #[arg(long)]
    pub parent: Option<String>,

    /// Labels to attach (comma-separated or repeated).
    #[arg(short, long)]
    pub label: Vec<String>,

    /// Description text.
    #[arg(short, long)]
    pub description: Option<String>,
}

pub fn run_create(
    args: &CreateArgs,
    agent_flag: Option<&str>,
    output: OutputMode,
    _project_root: &std::path::Path,
) -> anyhow::Result<()> {
    let _agent = match agent::require_agent(agent_flag) {
        Ok(a) => a,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(&e.message, "Set --agent, BONES_AGENT, or AGENT", e.code),
            )?;
            anyhow::bail!("{}", e.message);
        }
    };

    // TODO: wire to bones-core event emission once projection layer is ready (bn-2da.2)
    render_success(output, &format!("Created item: {}", args.title))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_args_defaults() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: CreateArgs,
        }
        let w = Wrapper::parse_from(["test", "--title", "Hello"]);
        assert_eq!(w.args.title, "Hello");
        assert_eq!(w.args.kind, "task");
        assert!(w.args.parent.is_none());
        assert!(w.args.label.is_empty());
    }
}
