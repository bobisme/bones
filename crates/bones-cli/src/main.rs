#![forbid(unsafe_code)]

mod cmd;
mod git;

use bones_core::timing;
use clap::{Parser, Subcommand};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "bones: CRDT-native issue tracker",
    long_about = None
)]
struct Cli {
    /// Enable verbose logging.
    #[arg(short, long)]
    verbose: bool,

    /// Emit command timing report to stderr.
    #[arg(long, global = true)]
    timing: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Initialize a new bones project in the current directory.
    Init(cmd::init::InitArgs),

    /// Install optional git hooks for projection refresh and staged event validation.
    Hooks {
        #[command(subcommand)]
        command: HookCommand,
    },

    /// Verify shard manifests and event integrity.
    Verify {
        /// Validate only staged files.
        #[arg(long)]
        staged: bool,

        /// Regenerate missing manifests for sealed shards.
        #[arg(long)]
        regenerate_missing: bool,
    },

    /// Synchronize with remote: git pull → rebuild projection → git push.
    ///
    /// Also ensures `.gitattributes` and `.gitignore` are correctly configured
    /// for the bones-events merge driver and derived-file ignores.
    Sync(cmd::sync::SyncArgs),

    /// Import data from external trackers.
    Import(cmd::import::ImportArgs),

    /// Rebuild the projection (currently a placeholder command for hook integration).
    Rebuild {
        /// Rebuild incrementally from the last projection cursor.
        #[arg(long)]
        incremental: bool,
    },

    /// Merge tool for jj conflict resolution on append-only event files
    MergeTool {
        /// Configure jj to use bones as a merge tool
        #[arg(long)]
        setup: bool,

        /// Base file (original)
        #[arg(value_name = "BASE")]
        base: Option<PathBuf>,

        /// Left file (our version)
        #[arg(value_name = "LEFT")]
        left: Option<PathBuf>,

        /// Right file (their version)
        #[arg(value_name = "RIGHT")]
        right: Option<PathBuf>,

        /// Output file (merged result)
        #[arg(value_name = "OUTPUT")]
        output: Option<PathBuf>,
    },

    /// Git merge driver for *.events shard files.
    ///
    /// Invoked automatically by git when merging `.events` files.
    /// Registered via `.gitattributes` and `.git/config`:
    ///
    ///   .gitattributes:  *.events merge=bones-events
    ///   .git/config:     [merge "bones-events"]
    ///                        driver = bn merge-driver %O %A %B
    ///
    /// Reads base (%O), ours (%A), and theirs (%B) versions of a shard file,
    /// merges ours and theirs using CRDT union semantics (dedup by hash,
    /// sort by timestamp/agent/hash), and writes the merged result to the
    /// ours path (%A). Exits 0 on success.
    MergeDriver {
        /// Base file — the common ancestor version (git %O placeholder).
        #[arg(value_name = "BASE")]
        base: PathBuf,

        /// Ours file — the local branch version; also the output path (git %A placeholder).
        #[arg(value_name = "OURS")]
        ours: PathBuf,

        /// Theirs file — the remote branch version (git %B placeholder).
        #[arg(value_name = "THEIRS")]
        theirs: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum HookCommand {
    /// Install optional git hooks (`post-merge`, `pre-commit`).
    Install,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("BONES_LOG").unwrap_or_else(|_| {
        EnvFilter::new(if env::var("DEBUG").is_ok() {
            "bones=debug,info"
        } else {
            "bones=info,warn"
        })
    });

    let format = env::var("BONES_LOG_FORMAT").unwrap_or_else(|_| "compact".to_string());

    let registry = tracing_subscriber::registry().with(filter);

    match format.as_str() {
        "json" => {
            registry.with(fmt::layer().json().with_ansi(false)).init();
        }
        _ => {
            registry.with(fmt::layer().compact()).init();
        }
    }
}

/// Count the number of lines in a file
fn count_lines(path: &PathBuf) -> anyhow::Result<usize> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    Ok(reader.lines().count())
}

/// Merge two append-only event files using union merge strategy:
/// 1. Copy left file (base + left's appends) to output
/// 2. Append right file's appends (lines N+1..end) to output
fn merge_files(
    base: &PathBuf,
    left: &PathBuf,
    right: &PathBuf,
    output: &PathBuf,
) -> anyhow::Result<()> {
    // Count lines in base file
    let base_lines = count_lines(base)?;
    info!(
        "Base file has {} lines. Merging left and right appends.",
        base_lines
    );

    // Copy left file to output
    fs::copy(left, output)?;
    info!("Copied left file to output");

    // Append right file's appends to output
    let right_file = fs::File::open(right)?;
    let reader = BufReader::new(right_file);
    let mut output_file = fs::OpenOptions::new().append(true).open(output)?;

    for (line_no, line) in reader.lines().enumerate() {
        if line_no >= base_lines {
            let line_content = line?;
            writeln!(output_file, "{}", line_content)?;
            info!("Appended line {} from right file", line_no + 1);
        }
    }

    info!("Successfully merged files into output");
    Ok(())
}

/// Setup jj configuration to use bones as a merge tool
fn setup_merge_tool() -> anyhow::Result<()> {
    info!("Setting up jj configuration for bones merge tool");

    let commands = vec![
        vec![
            "jj",
            "config",
            "set",
            "--user",
            "merge-tools.bones.program",
            "bn",
        ],
        vec![
            "jj",
            "config",
            "set",
            "--user",
            "merge-tools.bones.merge-args",
            r#"["merge-tool", "$base", "$left", "$right", "$output"]"#,
        ],
    ];

    for cmd in commands {
        info!("Running: {}", cmd.join(" "));
        let status = std::process::Command::new(cmd[0])
            .args(&cmd[1..])
            .status()?;

        if !status.success() {
            return Err(anyhow::anyhow!(
                "Failed to configure jj: {}",
                status.code().unwrap_or(-1)
            ));
        }
    }

    println!("✓ bones merge tool configured in jj");
    println!("You can now use: jj resolve --tool bones");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let timing_enabled = cli.timing || timing::timing_enabled_from_env();
    timing::set_timing_enabled(timing_enabled);
    timing::clear_timings();

    if cli.verbose {
        info!("Verbose mode enabled");
    }

    let project_root = std::env::current_dir()?;

    let command_result = match cli.command {
        Commands::Init(args) => {
            timing::timed("cmd.init", || cmd::init::run_init(&args, &project_root))
        }
        Commands::Sync(args) => {
            timing::timed("cmd.sync", || cmd::sync::run_sync(&args, &project_root))
        }
        Commands::Import(args) => timing::timed("cmd.import", || {
            cmd::import::run_import(&args, &project_root)
        }),
        Commands::Hooks {
            command: HookCommand::Install,
        } => timing::timed("cmd.hooks.install", || {
            git::hooks::install_hooks(&project_root)
        }),
        Commands::Verify {
            staged,
            regenerate_missing,
        } => timing::timed("cmd.verify", || {
            if staged {
                git::hooks::verify_staged_events()
            } else {
                cmd::verify::run_verify(&project_root, regenerate_missing)
            }
        }),
        Commands::Rebuild { incremental } => timing::timed("cmd.rebuild", || {
            if incremental {
                println!("[bn] incremental rebuild requested (currently a CLI placeholder).\n");
            } else {
                println!("[bn] rebuild requested (currently a CLI placeholder).\n");
            }

            Ok(())
        }),
        Commands::MergeTool {
            setup,
            base,
            left,
            right,
            output,
        } => timing::timed("cmd.merge-tool", || {
            if setup {
                return setup_merge_tool();
            }

            let base = base.ok_or_else(|| anyhow::anyhow!("Missing base file argument"))?;
            let left = left.ok_or_else(|| anyhow::anyhow!("Missing left file argument"))?;
            let right = right.ok_or_else(|| anyhow::anyhow!("Missing right file argument"))?;
            let output = output.ok_or_else(|| anyhow::anyhow!("Missing output file argument"))?;

            merge_files(&base, &left, &right, &output)
        }),

        Commands::MergeDriver { base, ours, theirs } => timing::timed("cmd.merge-driver", || {
            git::merge_driver::merge_driver_main(&base, &ours, &theirs)
        }),
    };

    if timing_enabled {
        let report = timing::collect_report();
        if report.is_empty() {
            eprintln!("timing report: no samples recorded");
        } else {
            eprintln!("timing report:");
            eprintln!("{}", report.display_table());
            eprintln!("timing report (json):");
            eprintln!("{}", serde_json::to_string_pretty(&report.to_json())?);
        }
    }

    command_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_flag_parses_before_subcommand() {
        let cli = Cli::parse_from(["bn", "--timing", "rebuild"]);
        assert!(cli.timing);
        assert!(matches!(
            cli.command,
            Commands::Rebuild { incremental: false }
        ));
    }

    #[test]
    fn timing_flag_parses_after_subcommand() {
        let cli = Cli::parse_from(["bn", "rebuild", "--timing", "--incremental"]);
        assert!(cli.timing);
        assert!(matches!(
            cli.command,
            Commands::Rebuild { incremental: true }
        ));
    }
}
