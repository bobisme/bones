//! `bn do` — transition an item to "doing" state.

use crate::agent;
use crate::output::{CliError, OutputMode, render_error, render_success};
use clap::Args;

#[derive(Args, Debug)]
pub struct DoArgs {
    /// Item ID to transition.
    pub id: String,
}

pub fn run_do(
    args: &DoArgs,
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

    // TODO: wire to bones-core state transition + event emission (bn-2da.4)
    render_success(output, &format!("Transitioned {} to doing", args.id))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn do_args_parses_id() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: DoArgs,
        }
        let w = Wrapper::parse_from(["test", "item-456"]);
        assert_eq!(w.args.id, "item-456");
    }
}
