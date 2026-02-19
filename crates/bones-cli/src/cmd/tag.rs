//! `bn tag` and `bn untag` — add/remove labels from work items.

use crate::agent;
use crate::output::{CliError, OutputMode, render_error, render_success};
use clap::Args;

#[derive(Args, Debug)]
pub struct TagArgs {
    /// Item ID to tag.
    pub id: String,

    /// Labels to add.
    #[arg(required = true)]
    pub labels: Vec<String>,
}

#[derive(Args, Debug)]
pub struct UntagArgs {
    /// Item ID to untag.
    pub id: String,

    /// Labels to remove.
    #[arg(required = true)]
    pub labels: Vec<String>,
}

pub fn run_tag(
    args: &TagArgs,
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

    // TODO: wire to bones-core event emission (bn-2da.5)
    render_success(
        output,
        &format!("Tagged {} with: {}", args.id, args.labels.join(", ")),
    )?;
    Ok(())
}

pub fn run_untag(
    args: &UntagArgs,
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

    // TODO: wire to bones-core event emission (bn-2da.5)
    render_success(
        output,
        &format!("Removed tags from {}: {}", args.id, args.labels.join(", ")),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_args_parses() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: TagArgs,
        }
        let w = Wrapper::parse_from(["test", "item-1", "bug", "urgent"]);
        assert_eq!(w.args.id, "item-1");
        assert_eq!(w.args.labels, vec!["bug", "urgent"]);
    }

    #[test]
    fn untag_args_parses() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: UntagArgs,
        }
        let w = Wrapper::parse_from(["test", "item-1", "stale"]);
        assert_eq!(w.args.id, "item-1");
        assert_eq!(w.args.labels, vec!["stale"]);
    }
}
