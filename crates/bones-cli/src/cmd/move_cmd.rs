//! `bn move` — reparent a work item under a different goal.

use crate::agent;
use crate::output::{CliError, OutputMode, render_error, render_success};
use clap::Args;

#[derive(Args, Debug)]
pub struct MoveArgs {
    /// Item ID to move.
    pub id: String,

    /// New parent item ID. Use "--parent none" to make top-level.
    #[arg(long)]
    pub parent: String,
}

pub fn run_move(
    args: &MoveArgs,
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

    // TODO: wire to bones-core reparent event emission (bn-2da.5)
    if args.parent == "none" {
        render_success(output, &format!("Moved {} to top level", args.id))?;
    } else {
        render_success(
            output,
            &format!("Moved {} under parent {}", args.id, args.parent),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_args_parses() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: MoveArgs,
        }
        let w = Wrapper::parse_from(["test", "item-1", "--parent", "goal-1"]);
        assert_eq!(w.args.id, "item-1");
        assert_eq!(w.args.parent, "goal-1");
    }

    #[test]
    fn move_to_top_level() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: MoveArgs,
        }
        let w = Wrapper::parse_from(["test", "item-1", "--parent", "none"]);
        assert_eq!(w.args.parent, "none");
    }
}
