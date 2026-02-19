#![forbid(unsafe_code)]
use clap::Parser;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Parser, Debug)]
enum Commands {
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
            registry
                .with(fmt::layer().json().with_ansi(false))
                .init();
        }
        _ => {
            registry
                .with(fmt::layer().compact())
                .init();
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
        // Line numbers are 0-indexed, but base_lines is 1-indexed
        // We want to append lines from index base_lines onwards
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

    // Try to run jj config set commands
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
    // Initialize logging
    init_tracing();

    let args = Args::parse();

    if args.verbose {
        info!("Verbose mode enabled");
    }

    info!("bones-cli initialized");

    // Handle subcommands
    if let Some(Commands::MergeTool {
        setup,
        base,
        left,
        right,
        output,
    }) = args.command
    {
        if setup {
            return setup_merge_tool();
        }

        // Validate that all file arguments are provided
        let base = base.ok_or_else(|| anyhow::anyhow!("Missing base file argument"))?;
        let left = left.ok_or_else(|| anyhow::anyhow!("Missing left file argument"))?;
        let right = right.ok_or_else(|| anyhow::anyhow!("Missing right file argument"))?;
        let output = output.ok_or_else(|| anyhow::anyhow!("Missing output file argument"))?;

        return merge_files(&base, &left, &right, &output);
    }

    // Initialize core systems (demonstration)
    bones_core::init();
    bones_triage::init();
    bones_search::init();
    bones_sim::init();

    Ok(())
}
