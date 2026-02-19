use anyhow::Result;
use clap::Args;
use clap_complete::{Shell, generate};

/// Arguments for `bn completions`.
#[derive(Args, Debug)]
pub struct CompletionsArgs {
    /// Target shell for completion script generation.
    #[arg(value_enum)]
    pub shell: Shell,
}

/// Generate shell completion script to stdout.
///
/// # Errors
///
/// Returns an error if writing to stdout fails.
pub fn run_completions(shell: Shell, command: &mut clap::Command) -> Result<()> {
    let mut out = std::io::stdout();
    generate(shell, command, "bn", &mut out);
    Ok(())
}
