//! `bn show` — display a single work item.

use crate::output::OutputMode;
use clap::Args;

#[derive(Args, Debug)]
pub struct ShowArgs {
    /// Item ID to display.
    pub id: String,
}

pub fn run_show(
    _args: &ShowArgs,
    output: OutputMode,
    _project_root: &std::path::Path,
) -> anyhow::Result<()> {
    // TODO: wire to SQLite projection queries (bn-2da.3)
    crate::output::render_success(output, "Show not yet connected to projection")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_args_parses_id() {
        use clap::Parser;

        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: ShowArgs,
        }
        let w = Wrapper::parse_from(["test", "item-123"]);
        assert_eq!(w.args.id, "item-123");
    }
}
