#![forbid(unsafe_code)]

mod agent;
mod cmd;
mod git;
mod output;
mod validate;

use bones_core::timing;
use clap::{CommandFactory, Parser, Subcommand};
use output::OutputMode;
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

    /// Emit JSON output instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,

    /// Override agent identity (skips env resolution).
    #[arg(long, global = true)]
    agent: Option<String>,

    /// Suppress non-essential output.
    #[arg(short, long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    command: Commands,
}

impl Cli {
    /// Derive the output mode from flags.
    fn output_mode(&self) -> OutputMode {
        if self.json {
            OutputMode::Json
        } else {
            OutputMode::Human
        }
    }

    /// Get the agent flag as an Option<&str> for resolution.
    fn agent_flag(&self) -> Option<&str> {
        self.agent.as_deref()
    }
}

#[derive(Subcommand, Debug)]
enum Commands {
    #[command(
        next_help_heading = "Lifecycle",
        about = "Initialize a bones project",
        long_about = "Initialize a bones project in the current directory.",
        after_help = "EXAMPLES:\n    # Initialize a project in the current directory\n    bn init\n\n    # Emit machine-readable output\n    bn init --json"
    )]
    Init(cmd::init::InitArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Create a new work item",
        long_about = "Create a new work item and append an item.create event.",
        after_help = "EXAMPLES:\n    # Create a task\n    bn create --title \"Fix login timeout\"\n\n    # Create a goal\n    bn create --title \"Launch v2\" --kind goal\n\n    # Emit machine-readable output\n    bn create --title \"Fix login timeout\" --json"
    )]
    Create(cmd::create::CreateArgs),

    #[command(
        next_help_heading = "Read",
        about = "List work items",
        long_about = "List work items with optional filters and sort order.",
        after_help = "EXAMPLES:\n    # List open items (default)\n    bn list\n\n    # Filter by state and label\n    bn list --state doing --label backend\n\n    # Emit machine-readable output\n    bn list --json"
    )]
    List(cmd::list::ListArgs),

    #[command(
        next_help_heading = "Read",
        about = "Show one work item",
        long_about = "Show full details for a single work item by ID.",
        after_help = "EXAMPLES:\n    # Show an item\n    bn show bn-abc\n\n    # Use a short prefix when unique\n    bn show abc\n\n    # Emit machine-readable output\n    bn show bn-abc --json"
    )]
    Show(cmd::show::ShowArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Mark item as doing",
        long_about = "Transition a work item to the doing state.",
        after_help = "EXAMPLES:\n    # Start work on an item\n    bn do bn-abc\n\n    # Emit machine-readable output\n    bn do bn-abc --json"
    )]
    Do(cmd::do_cmd::DoArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Mark item as done",
        long_about = "Transition a work item to the done state.",
        after_help = "EXAMPLES:\n    # Complete an item\n    bn done bn-abc\n\n    # Emit machine-readable output\n    bn done bn-abc --json"
    )]
    Done(cmd::done::DoneArgs),

    #[command(
        next_help_heading = "Metadata",
        about = "Add labels to an item",
        long_about = "Attach one or more labels to an existing work item.",
        after_help = "EXAMPLES:\n    # Add labels\n    bn tag bn-abc bug urgent\n\n    # Emit machine-readable output\n    bn tag bn-abc bug --json"
    )]
    Tag(cmd::tag::TagArgs),

    #[command(
        next_help_heading = "Metadata",
        about = "Remove labels from an item",
        long_about = "Remove one or more labels from an existing work item.",
        after_help = "EXAMPLES:\n    # Remove a label\n    bn untag bn-abc urgent\n\n    # Emit machine-readable output\n    bn untag bn-abc urgent --json"
    )]
    Untag(cmd::tag::UntagArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Move item under a parent",
        long_about = "Change a work item's parent to reorganize hierarchy.",
        after_help = "EXAMPLES:\n    # Move under a goal\n    bn move bn-task --parent bn-goal\n\n    # Emit machine-readable output\n    bn move bn-task --parent bn-goal --json"
    )]
    Move(cmd::move_cmd::MoveArgs),

    #[command(
        next_help_heading = "Project Maintenance",
        about = "Generate shell completion scripts",
        long_about = "Generate shell completion scripts for supported shells.",
        after_help = "EXAMPLES:\n    # Generate bash completions\n    bn completions bash\n\n    # Generate zsh completions\n    bn completions zsh"
    )]
    Completions(cmd::completions::CompletionsArgs),

    #[command(next_help_heading = "Project Maintenance", about = "Manage optional git hooks")]
    Hooks {
        #[command(subcommand)]
        command: HookCommand,
    },

    #[command(
        next_help_heading = "Project Maintenance",
        about = "Verify event and manifest integrity",
        long_about = "Verify shard manifests and event integrity checks for this project.",
        after_help = "EXAMPLES:\n    # Verify all shard files\n    bn verify\n\n    # Verify only staged files\n    bn verify --staged\n\n    # Emit machine-readable output\n    bn verify --json"
    )]
    Verify {
        /// Validate only staged files.
        #[arg(long)]
        staged: bool,

        /// Regenerate missing manifests for sealed shards.
        #[arg(long)]
        regenerate_missing: bool,
    },

    #[command(
        next_help_heading = "Sync",
        about = "Synchronize local and remote state",
        long_about = "Synchronize with remote: git pull, rebuild projection, then git push.",
        after_help = "EXAMPLES:\n    # Sync from and to remote\n    bn sync\n\n    # Emit machine-readable output\n    bn sync --json"
    )]
    Sync(cmd::sync::SyncArgs),

    #[command(
        next_help_heading = "Interoperability",
        about = "Import external tracker data",
        long_about = "Import issues and metadata from supported external tracker formats.",
        after_help = "EXAMPLES:\n    # Import from a JSON export\n    bn import --from linear --file linear.json\n\n    # Emit machine-readable output\n    bn import --from linear --file linear.json --json"
    )]
    Import(cmd::import::ImportArgs),

    #[command(
        next_help_heading = "Sync",
        about = "Migrate from a beads project",
        long_about = "Migrate an existing beads project database into bones events.",
        after_help = "EXAMPLES:\n    # Migrate from a beads SQLite database\n    bn migrate-from-beads --source beads.db\n\n    # Emit machine-readable output\n    bn migrate-from-beads --source beads.db --json"
    )]
    MigrateFromBeads(cmd::migrate::MigrateArgs),

    #[command(
        next_help_heading = "Project Maintenance",
        about = "Rebuild the projection",
        long_about = "Rebuild the local projection database from append-only event shards.",
        after_help = "EXAMPLES:\n    # Full rebuild\n    bn rebuild\n\n    # Incremental rebuild\n    bn rebuild --incremental\n\n    # Emit machine-readable output\n    bn rebuild --json"
    )]
    Rebuild {
        /// Rebuild incrementally from the last projection cursor.
        #[arg(long)]
        incremental: bool,
    },

    #[command(
        next_help_heading = "Merge Integration",
        about = "Run merge tool for event files",
        long_about = "Run the merge tool used for append-only .events conflict resolution.",
        after_help = "EXAMPLES:\n    # Configure jj to use this merge tool\n    bn merge-tool --setup\n\n    # Run merge tool directly\n    bn merge-tool base.events left.events right.events out.events"
    )]
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

    #[command(
        next_help_heading = "Merge Integration",
        about = "Run git merge driver for .events",
        long_about = "Internal command invoked by git merge driver for .events shard files.",
        after_help = "EXAMPLES:\n    # Invoked by git; not typically run manually\n    bn merge-driver %O %A %B"
    )]
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
    #[command(
        about = "Install optional git hooks",
        after_help = "EXAMPLES:\n    # Install post-merge and pre-commit hooks\n    bn hooks install"
    )]
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
    let output = cli.output_mode();

    let command_result = match cli.command {
        Commands::Init(args) => {
            timing::timed("cmd.init", || cmd::init::run_init(&args, &project_root))
        }
        Commands::Create(ref args) => timing::timed("cmd.create", || {
            cmd::create::run_create(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::List(ref args) => {
            timing::timed("cmd.list", || cmd::list::run_list(args, output, &project_root))
        }
        Commands::Show(ref args) => {
            timing::timed("cmd.show", || cmd::show::run_show(args, output, &project_root))
        }
        Commands::Do(ref args) => timing::timed("cmd.do", || {
            cmd::do_cmd::run_do(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Done(ref args) => timing::timed("cmd.done", || {
            cmd::done::run_done(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Tag(ref args) => timing::timed("cmd.tag", || {
            cmd::tag::run_tag(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Untag(ref args) => timing::timed("cmd.untag", || {
            cmd::tag::run_untag(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Move(ref args) => timing::timed("cmd.move", || {
            cmd::move_cmd::run_move(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Sync(args) => {
            timing::timed("cmd.sync", || cmd::sync::run_sync(&args, &project_root))
        }
        Commands::Import(args) => timing::timed("cmd.import", || {
            cmd::import::run_import(&args, &project_root)
        }),
        Commands::MigrateFromBeads(args) => timing::timed("cmd.migrate_from_beads", || {
            cmd::migrate::run_migrate(&args, &project_root)
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
            cmd::rebuild::run_rebuild(&project_root, incremental)
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

        Commands::Completions(args) => timing::timed("cmd.completions", || {
            let mut command = Cli::command();
            cmd::completions::run_completions(args.shell, &mut command)
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

    #[test]
    fn json_flag_sets_output_mode() {
        let cli = Cli::parse_from(["bn", "--json", "list"]);
        assert!(cli.json);
        assert!(cli.output_mode().is_json());
    }

    #[test]
    fn json_flag_after_subcommand() {
        let cli = Cli::parse_from(["bn", "list", "--json"]);
        assert!(cli.json);
        assert!(cli.output_mode().is_json());
    }

    #[test]
    fn default_output_is_human() {
        let cli = Cli::parse_from(["bn", "list"]);
        assert!(!cli.json);
        assert!(!cli.output_mode().is_json());
    }

    #[test]
    fn agent_flag_parsed() {
        let cli = Cli::parse_from(["bn", "--agent", "test-agent", "list"]);
        assert_eq!(cli.agent.as_deref(), Some("test-agent"));
        assert_eq!(cli.agent_flag(), Some("test-agent"));
    }

    #[test]
    fn agent_flag_none_by_default() {
        let cli = Cli::parse_from(["bn", "list"]);
        assert!(cli.agent.is_none());
        assert!(cli.agent_flag().is_none());
    }

    #[test]
    fn quiet_flag_parsed() {
        let cli = Cli::parse_from(["bn", "-q", "list"]);
        assert!(cli.quiet);
    }

    #[test]
    fn create_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "create", "--title", "My task"]);
        assert!(matches!(cli.command, Commands::Create(_)));
    }

    #[test]
    fn list_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "list"]);
        assert!(matches!(cli.command, Commands::List(_)));
    }

    #[test]
    fn show_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "show", "item-123"]);
        assert!(matches!(cli.command, Commands::Show(_)));
    }

    #[test]
    fn do_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "do", "item-123"]);
        assert!(matches!(cli.command, Commands::Do(_)));
    }

    #[test]
    fn done_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "done", "item-123"]);
        assert!(matches!(cli.command, Commands::Done(_)));
    }

    #[test]
    fn tag_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "tag", "item-123", "bug", "urgent"]);
        assert!(matches!(cli.command, Commands::Tag(_)));
    }

    #[test]
    fn untag_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "untag", "item-123", "stale"]);
        assert!(matches!(cli.command, Commands::Untag(_)));
    }

    #[test]
    fn move_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "move", "item-123", "--parent", "goal-1"]);
        assert!(matches!(cli.command, Commands::Move(_)));
    }

    #[test]
    fn completions_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "completions", "bash"]);
        assert!(matches!(
            cli.command,
            Commands::Completions(cmd::completions::CompletionsArgs {
                shell: clap_complete::Shell::Bash,
            })
        ));
    }

    #[test]
    fn all_subcommands_listed() {
        // Verify all planned lifecycle subcommands exist by parsing each
        let subcommands = [
            vec!["bn", "init"],
            vec!["bn", "create", "--title", "x"],
            vec!["bn", "list"],
            vec!["bn", "show", "x"],
            vec!["bn", "do", "x"],
            vec!["bn", "done", "x"],
            vec!["bn", "tag", "x", "l"],
            vec!["bn", "untag", "x", "l"],
            vec!["bn", "move", "x", "--parent", "p"],
            vec!["bn", "completions", "bash"],
        ];
        for args in &subcommands {
            let result = Cli::try_parse_from(args.iter());
            assert!(
                result.is_ok(),
                "Failed to parse: {:?} — error: {:?}",
                args,
                result.err()
            );
        }
    }

    #[test]
    fn read_only_commands_work_without_agent() {
        // list and show are read-only — they should parse without --agent
        let cli = Cli::parse_from(["bn", "list"]);
        assert!(cli.agent_flag().is_none());

        let cli = Cli::parse_from(["bn", "show", "item-1"]);
        assert!(cli.agent_flag().is_none());
    }

    #[test]
    fn mutating_commands_accept_agent_flag() {
        let cli = Cli::parse_from(["bn", "--agent", "me", "create", "--title", "t"]);
        assert_eq!(cli.agent_flag(), Some("me"));

        let cli = Cli::parse_from(["bn", "--agent", "me", "do", "x"]);
        assert_eq!(cli.agent_flag(), Some("me"));

        let cli = Cli::parse_from(["bn", "--agent", "me", "done", "x"]);
        assert_eq!(cli.agent_flag(), Some("me"));

        let cli = Cli::parse_from(["bn", "--agent", "me", "tag", "x", "l"]);
        assert_eq!(cli.agent_flag(), Some("me"));

        let cli = Cli::parse_from(["bn", "--agent", "me", "move", "x", "--parent", "p"]);
        assert_eq!(cli.agent_flag(), Some("me"));
    }
}
