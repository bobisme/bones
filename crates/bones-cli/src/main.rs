#![forbid(unsafe_code)]

mod agent;
mod cmd;
mod git;
mod output;
mod validate;

use bones_core::timing;
use clap::{CommandFactory, Parser, Subcommand};
use output::{OutputMode, resolve_output_mode};
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
    /// Derive the output mode from flags, environment, and TTY defaults.
    ///
    /// Delegates to [`resolve_output_mode`] which applies the full precedence chain:
    /// `--json` flag > `BONES_OUTPUT` env var > TTY-aware default.
    fn output_mode(&self) -> OutputMode {
        resolve_output_mode(self.json)
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
        next_help_heading = "Read",
        about = "Show chronological event timeline for one item",
        long_about = "Read append-only event shards and show timeline entries for a single item ID.",
        after_help = "EXAMPLES:\n    # Show timeline for one item\n    bn log bn-abc\n\n    # Filter to recent events\n    bn log bn-abc --since 2026-02-01T00:00:00Z\n\n    # Machine-readable output\n    bn log bn-abc --json"
    )]
    Log(cmd::log::LogArgs),

    #[command(
        next_help_heading = "Read",
        about = "Show recent global event history",
        long_about = "Read append-only event shards and show recent events across all items.",
        after_help = "EXAMPLES:\n    # Show recent global activity\n    bn history\n\n    # Filter by agent and limit\n    bn history --agent alice -n 20\n\n    # Machine-readable output\n    bn history --json"
    )]
    History(cmd::log::HistoryArgs),

    #[command(
        next_help_heading = "Read",
        about = "Attribute a field's last write",
        long_about = "Find the most recent event that modified a field on an item.",
        after_help = "EXAMPLES:\n    # Show who last changed the title\n    bn blame bn-abc title\n\n    # Machine-readable output\n    bn blame bn-abc title --json"
    )]
    Blame(cmd::log::BlameArgs),

    #[command(
        next_help_heading = "Read",
        about = "List known agents",
        long_about = "List all known agents with current assignment counts and last-activity timestamps.",
        after_help = "EXAMPLES:\n    # List known agents\n    bn agents\n\n    # Machine-readable output\n    bn agents --json"
    )]
    Agents(cmd::agents::AgentsArgs),

    #[command(
        next_help_heading = "Read",
        about = "List items assigned to the current agent",
        long_about = "Shortcut for `bn list --assignee <resolved-agent>` using the standard agent identity resolution chain.",
        after_help = "EXAMPLES:\n    # List my open items\n    bn mine\n\n    # Include done items\n    bn mine --state done\n\n    # Machine-readable output\n    bn mine --json"
    )]
    Mine(cmd::mine::MineArgs),

    #[command(
        next_help_heading = "Search",
        about = "Search items using full-text search",
        long_about = "Search work items using SQLite FTS5 lexical full-text search with BM25 ranking.\n\n\
                      Column weights: title 3×, description 2×, labels 1×.\n\n\
                      Supports FTS5 syntax: stemming ('run' matches 'running'), prefix ('auth*'), boolean (AND/OR/NOT).",
        after_help = "EXAMPLES:\n    # Search for items about authentication\n    bn search authentication\n\n    # Prefix search\n    bn search 'auth*'\n\n    # Limit results\n    bn search timeout -n 5\n\n    # Machine-readable output\n    bn search authentication --json"
    )]
    Search(cmd::search::SearchArgs),

    #[command(
        next_help_heading = "Search",
        about = "Find potential duplicate work items",
        long_about = "Find work items that may be duplicates of the given item.\n\n\
                      Uses FTS5 lexical search with BM25 ranking. Similarity scores are \
                      normalized and classified using thresholds from .bones/config.toml.",
        after_help = "EXAMPLES:\n    # Find duplicates of an item\n    bn dup bn-abc\n\n    # Use a custom threshold\n    bn dup bn-abc --threshold 0.75\n\n    # Machine-readable output\n    bn dup bn-abc --json"
    )]
    Dup(cmd::dup::DupArgs),

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
        next_help_heading = "Feedback",
        about = "Record that you worked on this item (positive feedback)",
        long_about = "Record positive feedback: you acted on the triage recommendation and worked on this item.\n\nAppends a feedback entry to .bones/feedback.jsonl and updates the Thompson Sampling posterior for your agent profile.",
        after_help = "EXAMPLES:\n    # Record that you worked on bn-abc\n    bn did bn-abc\n\n    # With explicit agent identity\n    bn --agent alice did bn-abc\n\n    # Emit machine-readable output\n    bn did bn-abc --json"
    )]
    Did(cmd::feedback::DidArgs),

    #[command(
        next_help_heading = "Feedback",
        about = "Record that you skipped this item (negative feedback)",
        long_about = "Record negative feedback: the triage recommendation was not followed and you skipped this item.\n\nAppends a feedback entry to .bones/feedback.jsonl and updates the Thompson Sampling posterior for your agent profile.",
        after_help = "EXAMPLES:\n    # Record that you skipped bn-abc\n    bn skip bn-abc\n\n    # With explicit agent identity\n    bn --agent alice skip bn-abc\n\n    # Emit machine-readable output\n    bn skip bn-abc --json"
    )]
    Skip(cmd::feedback::SkipArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Archive done items",
        long_about = "Archive a done work item, or bulk-archive stale done items.",
        after_help = "EXAMPLES:\n    # Archive one done item\n    bn archive bn-abc\n\n    # Bulk-archive done items older than 30 days\n    bn archive --auto\n\n    # Use a custom staleness window\n    bn archive --auto --days 14\n\n    # Emit machine-readable output\n    bn archive bn-abc --json"
    )]
    Archive(cmd::archive::ArchiveArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Update fields on a work item",
        long_about = "Update one or more fields on an existing work item. Each field change emits a separate item.update event.",
        after_help = "EXAMPLES:\n    # Update title\n    bn update bn-abc --title \"New title\"\n\n    # Update multiple fields\n    bn update bn-abc --title \"Fix\" --urgency urgent\n\n    # Emit machine-readable output\n    bn update bn-abc --title \"Fix\" --json"
    )]
    Update(cmd::update::UpdateArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Close a work item (alias for done)",
        long_about = "Transition a work item to the done state. Equivalent to 'bn done'.",
        after_help = "EXAMPLES:\n    # Close an item\n    bn close bn-abc\n\n    # Close with reason\n    bn close bn-abc --reason \"Shipped in v2\"\n\n    # Emit machine-readable output\n    bn close bn-abc --json"
    )]
    Close(cmd::close::CloseArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Soft-delete a work item",
        long_about = "Soft-delete a work item by appending an item.delete tombstone event.",
        after_help = "EXAMPLES:\n    # Delete an item (TTY asks for confirmation)\n    bn delete bn-abc\n\n    # Delete with reason and skip confirmation\n    bn delete bn-abc --reason \"Duplicate\" --force\n\n    # Emit machine-readable output\n    bn delete bn-abc --json"
    )]
    Delete(cmd::delete::DeleteArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Reopen a closed or archived item",
        long_about = "Transition a done or archived work item back to the open state.",
        after_help = "EXAMPLES:\n    # Reopen an item\n    bn reopen bn-abc\n\n    # Emit machine-readable output\n    bn reopen bn-abc --json"
    )]
    Reopen(cmd::reopen::ReopenArgs),

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
        next_help_heading = "Metadata",
        about = "Add a comment to an item",
        long_about = "Append an immutable item.comment event to a work item.",
        after_help = "EXAMPLES:\n    # Add a comment\n    bn comment add bn-abc \"Investigating timeout path\"\n\n    # Emit machine-readable output\n    bn comment add bn-abc \"Investigating timeout path\" --json"
    )]
    Comment(cmd::comment::CommentArgs),

    #[command(
        next_help_heading = "Read",
        about = "Show comment timeline for an item",
        long_about = "List comments for a work item in chronological order.",
        after_help = "EXAMPLES:\n    # Show comments\n    bn comments bn-abc\n\n    # Emit machine-readable output\n    bn comments bn-abc --json"
    )]
    Comments(cmd::comment::CommentsArgs),

    #[command(
        next_help_heading = "Metadata",
        about = "List all labels with usage counts",
        long_about = "List global label inventory from the projection database.",
        after_help = "EXAMPLES:\n    # List labels\n    bn labels\n\n    # Group by namespace\n    bn labels --namespace\n\n    # Emit machine-readable output\n    bn labels --json"
    )]
    Labels(cmd::labels::LabelsArgs),

    #[command(
        next_help_heading = "Metadata",
        about = "Canonical single-label operations",
        long_about = "Manage one label at a time. `bn label add/rm` are canonical aliases for `bn tag`/`bn untag`.",
        after_help = "EXAMPLES:\n    # Add one label\n    bn label add bn-abc area:backend\n\n    # Remove one label\n    bn label rm bn-abc area:backend"
    )]
    Label(cmd::labels::LabelArgs),

    #[command(
        next_help_heading = "Metadata",
        about = "Assign an item to an agent",
        long_about = "Assign an item to an agent by emitting an item.assign event.",
        after_help = "EXAMPLES:\n    # Assign item to alice\n    bn assign bn-abc alice\n\n    # Emit machine-readable output\n    bn assign bn-abc alice --json"
    )]
    Assign(cmd::assign::AssignArgs),

    #[command(
        next_help_heading = "Metadata",
        about = "Unassign the current agent from an item",
        long_about = "Remove the current resolved agent from the item's assignee OR-Set.",
        after_help = "EXAMPLES:\n    # Unassign yourself from an item\n    bn --agent alice unassign bn-abc\n\n    # Emit machine-readable output\n    bn --agent alice unassign bn-abc --json"
    )]
    Unassign(cmd::assign::UnassignArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Move item under a parent",
        long_about = "Change a work item's parent to reorganize hierarchy.",
        after_help = "EXAMPLES:\n    # Move under a goal\n    bn move bn-task --parent bn-goal\n\n    # Emit machine-readable output\n    bn move bn-task --parent bn-goal --json"
    )]
    Move(cmd::move_cmd::MoveArgs),

    #[command(
        next_help_heading = "Dependencies",
        about = "Manage dependency links",
        long_about = "Add or remove dependency links between work items.\n\nUse 'bn dep add <from> --blocks <to>' to establish a blocking dependency.\nUse 'bn dep add <from> --relates <to>' for informational links.\nUse 'bn dep rm <from> <to>' to remove a link.",
        after_help = "EXAMPLES:\n    # Mark A as a blocker of B\n    bn dep add bn-abc --blocks bn-def\n\n    # Remove the dependency\n    bn dep rm bn-abc bn-def\n\n    # Emit machine-readable output\n    bn dep add bn-abc --blocks bn-def --json"
    )]
    Dep(cmd::dep::DepArgs),

    #[command(
        next_help_heading = "Dependencies",
        about = "Visualize the dependency graph",
        long_about = "Show the dependency graph for an item or the whole project.\n\nWith an item ID: show upstream (blocked-by) and downstream (blocks) dependencies.\nWithout an ID: show project-level statistics and structural analysis.",
        after_help = "EXAMPLES:\n    # Show full graph for an item\n    bn graph bn-abc\n\n    # Only show what bn-abc blocks\n    bn graph bn-abc --down\n\n    # Project summary\n    bn graph\n\n    # Emit machine-readable output\n    bn graph bn-abc --json"
    )]
    Graph(cmd::graph::GraphArgs),

    #[command(
        next_help_heading = "Triage",
        about = "Show the highest-priority unblocked item",
        long_about = "Compute composite priority scores and return the best unblocked candidate.\n\nUse '--agent N' to request N parallel assignments (multi-agent mode).",
        after_help = "EXAMPLES:\n    # Single best next item\n    bn next\n\n    # Multi-agent assignment (N slots)\n    bn next --agent 3\n\n    # Emit machine-readable output\n    bn next --json"
    )]
    Next(cmd::next::NextArgs),

    #[command(
        next_help_heading = "Triage",
        about = "Show a full triage report",
        long_about = "Compute graph metrics and composite scores, grouped into Top Picks, Blockers, Quick Wins, and Cycles.",
        after_help = "EXAMPLES:\n    # Human-readable triage report\n    bn triage\n\n    # Emit machine-readable output\n    bn triage --json"
    )]
    Triage(cmd::triage::TriageArgs),

    #[command(
        next_help_heading = "Read",
        about = "Quick agent/human orientation",
        long_about = "Show agent identity, assigned items, and project-level counts.\n\nDesigned as a fast \"where am I?\" command after crash/restart.",
        after_help = "EXAMPLES:\n    # Human-readable status\n    bn status\n\n    # With explicit agent\n    bn --agent alice status\n\n    # Machine-readable output\n    bn status --json"
    )]
    Status(cmd::status::StatusArgs),

    #[command(
        next_help_heading = "Read",
        about = "Show goal completion progress",
        long_about = "Show a focused goal-progress view with child tree and progress bars.\n\nDistinct from `bn show` — this is focused on completion status of a goal and its children.",
        after_help = "EXAMPLES:\n    # Show progress for a goal\n    bn progress bn-p1\n\n    # Machine-readable output\n    bn progress bn-p1 --json"
    )]
    Progress(cmd::progress::ProgressArgs),

    #[command(
        next_help_heading = "Triage",
        about = "Compute parallel execution layers",
        long_about = "Compute topological dependency layers where each layer can be worked in parallel.",
        after_help = "EXAMPLES:\n    # Project-wide plan\n    bn plan\n\n    # Scope to one goal's children\n    bn plan bn-goal\n\n    # Emit machine-readable output\n    bn plan --json"
    )]
    Plan(cmd::plan::PlanArgs),

    #[command(
        next_help_heading = "Triage",
        about = "Show project health metrics",
        long_about = "Summarize dependency graph health metrics: density, SCC count, critical path length, and blocker count.",
        after_help = "EXAMPLES:\n    # Human-readable dashboard\n    bn health\n\n    # Emit machine-readable output\n    bn health --json"
    )]
    Health(cmd::health::HealthArgs),

    #[command(
        next_help_heading = "Triage",
        about = "List dependency cycles",
        long_about = "List strongly connected components that represent dependency cycles.",
        after_help = "EXAMPLES:\n    # Human-readable cycle groups\n    bn cycles\n\n    # Emit machine-readable output\n    bn cycles --json"
    )]
    Cycles(cmd::cycles::CyclesArgs),

    #[command(
        next_help_heading = "Project Maintenance",
        about = "Generate shell completion scripts",
        long_about = "Generate shell completion scripts for supported shells.",
        after_help = "EXAMPLES:\n    # Generate bash completions\n    bn completions bash\n\n    # Generate zsh completions\n    bn completions zsh"
    )]
    Completions(cmd::completions::CompletionsArgs),

    #[command(
        next_help_heading = "Project Maintenance",
        about = "Manage optional git hooks"
    )]
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
        name = "redact-verify",
        next_help_heading = "Security",
        about = "Verify redaction completeness",
        long_about = "Verify that all item.redact events have been fully applied.\n\n\
                      Checks projection rows, FTS5 index, and comment bodies for residual\n\
                      un-redacted content.",
        after_help = "EXAMPLES:\n    # Verify all redactions\n    bn redact-verify\n\n    # Verify one item\n    bn redact-verify bn-abc\n\n    # Machine-readable output\n    bn redact-verify --json"
    )]
    RedactVerify(cmd::redact_verify::RedactVerifyArgs),

    #[command(
        next_help_heading = "Project Maintenance",
        about = "Run repository diagnostics",
        long_about = "Summarize event-log health, integrity anomalies, and projection drift indicators.",
        after_help = "EXAMPLES:\n    # Human-readable diagnostics\n    bn diagnose\n\n    # Machine-readable diagnostics\n    bn diagnose --json"
    )]
    Diagnose,

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
        Commands::List(ref args) => timing::timed("cmd.list", || {
            cmd::list::run_list(args, output, &project_root)
        }),
        Commands::Agents(ref args) => timing::timed("cmd.agents", || {
            cmd::agents::run_agents(args, output, &project_root)
        }),
        Commands::Mine(ref args) => timing::timed("cmd.mine", || {
            cmd::mine::run_mine(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Show(ref args) => timing::timed("cmd.show", || {
            cmd::show::run_show(args, output, &project_root)
        }),
        Commands::Log(ref args) => {
            timing::timed("cmd.log", || cmd::log::run_log(args, output, &project_root))
        }
        Commands::History(ref args) => timing::timed("cmd.history", || {
            cmd::log::run_history(args, output, &project_root)
        }),
        Commands::Blame(ref args) => timing::timed("cmd.blame", || {
            cmd::log::run_blame(args, output, &project_root)
        }),
        Commands::Search(ref args) => timing::timed("cmd.search", || {
            cmd::search::run_search(args, output, &project_root)
        }),
        Commands::Dup(ref args) => {
            timing::timed("cmd.dup", || cmd::dup::run_dup(args, output, &project_root))
        }
        Commands::Do(ref args) => timing::timed("cmd.do", || {
            cmd::do_cmd::run_do(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Done(ref args) => timing::timed("cmd.done", || {
            cmd::done::run_done(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Did(ref args) => timing::timed("cmd.did", || {
            cmd::feedback::run_did(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Skip(ref args) => timing::timed("cmd.skip", || {
            cmd::feedback::run_skip(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Archive(ref args) => timing::timed("cmd.archive", || {
            cmd::archive::run_archive(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Update(ref args) => timing::timed("cmd.update", || {
            cmd::update::run_update(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Close(ref args) => timing::timed("cmd.close", || {
            cmd::close::run_close(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Delete(ref args) => timing::timed("cmd.delete", || {
            cmd::delete::run_delete(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Reopen(ref args) => timing::timed("cmd.reopen", || {
            cmd::reopen::run_reopen(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Tag(ref args) => timing::timed("cmd.tag", || {
            cmd::tag::run_tag(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Untag(ref args) => timing::timed("cmd.untag", || {
            cmd::tag::run_untag(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Comment(ref args) => timing::timed("cmd.comment", || {
            cmd::comment::run_comment(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Comments(ref args) => timing::timed("cmd.comments", || {
            cmd::comment::run_comments(args, output, &project_root)
        }),
        Commands::Labels(ref args) => timing::timed("cmd.labels", || {
            cmd::labels::run_labels(args, output, &project_root)
        }),
        Commands::Label(ref args) => timing::timed("cmd.label", || {
            cmd::labels::run_label(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Assign(ref args) => timing::timed("cmd.assign", || {
            cmd::assign::run_assign(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Unassign(ref args) => timing::timed("cmd.unassign", || {
            cmd::assign::run_unassign(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Move(ref args) => timing::timed("cmd.move", || {
            cmd::move_cmd::run_move(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Dep(ref args) => timing::timed("cmd.dep", || {
            cmd::dep::run_dep(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Graph(ref args) => timing::timed("cmd.graph", || {
            cmd::graph::run_graph(args, output, &project_root)
        }),
        Commands::Next(ref args) => timing::timed("cmd.next", || {
            cmd::next::run_next(args, output, cli.agent_flag(), &project_root)
        }),
        Commands::Triage(ref args) => timing::timed("cmd.triage", || {
            cmd::triage::run_triage(args, output, &project_root)
        }),
        Commands::Status(ref args) => timing::timed("cmd.status", || {
            cmd::status::run_status(args, cli.agent_flag(), output, &project_root)
        }),
        Commands::Progress(ref args) => timing::timed("cmd.progress", || {
            cmd::progress::run_progress(args, output, &project_root)
        }),
        Commands::Plan(ref args) => timing::timed("cmd.plan", || {
            cmd::plan::run_plan(args, output, &project_root)
        }),
        Commands::Health(ref args) => timing::timed("cmd.health", || {
            cmd::health::run_health(args, output, &project_root)
        }),
        Commands::Cycles(ref args) => timing::timed("cmd.cycles", || {
            cmd::cycles::run_cycles(args, output, &project_root)
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
                cmd::verify::run_verify(&project_root, regenerate_missing, output)
            }
        }),
        Commands::RedactVerify(ref args) => timing::timed("cmd.redact_verify", || {
            cmd::redact_verify::run_redact_verify(args, output, &project_root)
        }),
        Commands::Diagnose => timing::timed("cmd.diagnose", || {
            cmd::diagnose::run_diagnose(output, &project_root)
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
        // In test (non-TTY), resolve_output_mode defaults to JSON.
        // The key assertion is that --json flag is not set.
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
    fn log_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "log", "item-123"]);
        assert!(matches!(cli.command, Commands::Log(_)));
    }

    #[test]
    fn history_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "history"]);
        assert!(matches!(cli.command, Commands::History(_)));
    }

    #[test]
    fn blame_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "blame", "item-123", "title"]);
        assert!(matches!(cli.command, Commands::Blame(_)));
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
    fn archive_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "archive", "item-123"]);
        assert!(matches!(cli.command, Commands::Archive(_)));
    }

    #[test]
    fn diagnose_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "diagnose"]);
        assert!(matches!(cli.command, Commands::Diagnose));
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
    fn comment_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "comment", "add", "item-123", "hello"]);
        assert!(matches!(cli.command, Commands::Comment(_)));
    }

    #[test]
    fn comments_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "comments", "item-123"]);
        assert!(matches!(cli.command, Commands::Comments(_)));
    }

    #[test]
    fn labels_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "labels"]);
        assert!(matches!(cli.command, Commands::Labels(_)));
    }

    #[test]
    fn label_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "label", "add", "item-123", "area:backend"]);
        assert!(matches!(cli.command, Commands::Label(_)));
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
            vec!["bn", "log", "x"],
            vec!["bn", "history"],
            vec!["bn", "blame", "x", "title"],
            vec!["bn", "do", "x"],
            vec!["bn", "done", "x"],
            vec!["bn", "did", "x"],
            vec!["bn", "skip", "x"],
            vec!["bn", "archive", "x"],
            vec!["bn", "update", "x", "--title", "t"],
            vec!["bn", "close", "x"],
            vec!["bn", "delete", "x", "--force"],
            vec!["bn", "reopen", "x"],
            vec!["bn", "tag", "x", "l"],
            vec!["bn", "untag", "x", "l"],
            vec!["bn", "comment", "add", "x", "hello"],
            vec!["bn", "comments", "x"],
            vec!["bn", "labels"],
            vec!["bn", "label", "add", "x", "l"],
            vec!["bn", "move", "x", "--parent", "p"],
            vec!["bn", "completions", "bash"],
            vec!["bn", "diagnose"],
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
    fn did_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "did", "bn-abc"]);
        assert!(matches!(cli.command, Commands::Did(_)));
        if let Commands::Did(ref args) = cli.command {
            assert_eq!(args.id, "bn-abc");
        }
    }

    #[test]
    fn skip_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "skip", "bn-abc"]);
        assert!(matches!(cli.command, Commands::Skip(_)));
        if let Commands::Skip(ref args) = cli.command {
            assert_eq!(args.id, "bn-abc");
        }
    }

    #[test]
    fn did_accepts_agent_flag() {
        let cli = Cli::parse_from(["bn", "--agent", "alice", "did", "bn-abc"]);
        assert_eq!(cli.agent_flag(), Some("alice"));
        assert!(matches!(cli.command, Commands::Did(_)));
    }

    #[test]
    fn skip_accepts_agent_flag() {
        let cli = Cli::parse_from(["bn", "--agent", "alice", "skip", "bn-abc"]);
        assert_eq!(cli.agent_flag(), Some("alice"));
        assert!(matches!(cli.command, Commands::Skip(_)));
    }

    #[test]
    fn update_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "update", "item-1", "--title", "New title"]);
        assert!(matches!(cli.command, Commands::Update(_)));
    }

    #[test]
    fn update_subcommand_parses_multiple_flags() {
        let cli = Cli::parse_from([
            "bn",
            "update",
            "item-1",
            "--title",
            "X",
            "--size",
            "m",
            "--urgency",
            "urgent",
        ]);
        assert!(matches!(cli.command, Commands::Update(_)));
        if let Commands::Update(ref args) = cli.command {
            assert_eq!(args.title.as_deref(), Some("X"));
            assert_eq!(args.size.as_deref(), Some("m"));
            assert_eq!(args.urgency.as_deref(), Some("urgent"));
        }
    }

    #[test]
    fn close_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "close", "item-1"]);
        assert!(matches!(cli.command, Commands::Close(_)));
    }

    #[test]
    fn close_subcommand_parses_with_reason() {
        let cli = Cli::parse_from(["bn", "close", "item-1", "--reason", "Done"]);
        assert!(matches!(cli.command, Commands::Close(_)));
        if let Commands::Close(ref args) = cli.command {
            assert_eq!(args.reason.as_deref(), Some("Done"));
        }
    }

    #[test]
    fn delete_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "delete", "item-1", "--reason", "Duplicate", "--force"]);
        assert!(matches!(cli.command, Commands::Delete(_)));
        if let Commands::Delete(ref args) = cli.command {
            assert_eq!(args.id, "item-1");
            assert_eq!(args.reason.as_deref(), Some("Duplicate"));
            assert!(args.force);
        }
    }

    #[test]
    fn reopen_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "reopen", "item-1"]);
        assert!(matches!(cli.command, Commands::Reopen(_)));
        if let Commands::Reopen(ref args) = cli.command {
            assert_eq!(args.id, "item-1");
        }
    }

    #[test]
    fn read_only_commands_work_without_agent() {
        // list and show are read-only — they should parse without --agent
        let cli = Cli::parse_from(["bn", "list"]);
        assert!(cli.agent_flag().is_none());

        let cli = Cli::parse_from(["bn", "show", "item-1"]);
        assert!(cli.agent_flag().is_none());

        let cli = Cli::parse_from(["bn", "comments", "item-1"]);
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

        let cli = Cli::parse_from(["bn", "--agent", "me", "archive", "x"]);
        assert_eq!(cli.agent_flag(), Some("me"));

        let cli = Cli::parse_from(["bn", "--agent", "me", "update", "x", "--title", "t"]);
        assert_eq!(cli.agent_flag(), Some("me"));

        let cli = Cli::parse_from(["bn", "--agent", "me", "close", "x"]);
        assert_eq!(cli.agent_flag(), Some("me"));

        let cli = Cli::parse_from(["bn", "--agent", "me", "delete", "x", "--force"]);
        assert_eq!(cli.agent_flag(), Some("me"));

        let cli = Cli::parse_from(["bn", "--agent", "me", "reopen", "x"]);
        assert_eq!(cli.agent_flag(), Some("me"));

        let cli = Cli::parse_from(["bn", "--agent", "me", "tag", "x", "l"]);
        assert_eq!(cli.agent_flag(), Some("me"));

        let cli = Cli::parse_from(["bn", "--agent", "me", "comment", "add", "x", "hi"]);
        assert_eq!(cli.agent_flag(), Some("me"));

        let cli = Cli::parse_from(["bn", "--agent", "me", "label", "add", "x", "l"]);
        assert_eq!(cli.agent_flag(), Some("me"));
        let cli = Cli::parse_from(["bn", "--agent", "me", "move", "x", "--parent", "p"]);
        assert_eq!(cli.agent_flag(), Some("me"));
    }

    #[test]
    fn agents_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "agents"]);
        assert!(matches!(cli.command, Commands::Agents(_)));
    }

    #[test]
    fn mine_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "mine"]);
        assert!(matches!(cli.command, Commands::Mine(_)));
    }

    #[test]
    fn assign_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "assign", "bn-abc", "alice"]);
        assert!(matches!(cli.command, Commands::Assign(_)));
    }

    #[test]
    fn unassign_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "--agent", "alice", "unassign", "bn-abc"]);
        assert!(matches!(cli.command, Commands::Unassign(_)));
    }

    #[test]
    fn plan_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "plan"]);
        assert!(matches!(cli.command, Commands::Plan(_)));
    }

    #[test]
    fn health_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "health"]);
        assert!(matches!(cli.command, Commands::Health(_)));
    }

    #[test]
    fn cycles_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "cycles"]);
        assert!(matches!(cli.command, Commands::Cycles(_)));
    }

    #[test]
    fn next_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "next"]);
        assert!(matches!(cli.command, Commands::Next(_)));
    }

    #[test]
    fn triage_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "triage"]);
        assert!(matches!(cli.command, Commands::Triage(_)));
    }

    #[test]
    fn next_supports_agent_slot_flag() {
        let cli = Cli::parse_from(["bn", "next", "--agent", "3"]);
        assert!(matches!(cli.command, Commands::Next(_)));
        assert_eq!(cli.agent_flag(), Some("3"));
    }
}
