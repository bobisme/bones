#![forbid(unsafe_code)]

mod agent;
mod cmd;
mod git;
mod itc_state;
mod output;
mod tui;
mod validate;

use bones_core::timing;
use clap::{Args, CommandFactory, Parser, Subcommand};
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
    about = "bones: issue tracker for agents",
    long_about = None,
    after_help = "QUICK REFERENCE:\n    bn triage                # triage report (default)\n    bn triage dup <id>       # duplicate check for one item\n    bn triage plan           # parallel execution layers\n    bn bone log <id>         # item event timeline\n    bn bone assign <id> <a>  # assign item to agent\n    bn bone comment add <id> <text>\n    bn admin verify          # verify event/manifests\n    bn data export --output events.jsonl\n    bn dev sim run --seeds 100\n    bn ui                    # open interactive UI"
)]
struct Cli {
    /// Enable verbose logging.
    #[arg(short, long)]
    verbose: bool,

    /// Emit command timing report to stderr.
    #[arg(long, global = true)]
    timing: bool,

    /// Output format: pretty, text, or json.
    #[arg(long, global = true, value_enum)]
    format: Option<OutputMode>,

    /// Hidden alias for `--format json`.
    #[arg(long, global = true, hide = true)]
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
    /// `--format/--format json` > `FORMAT` env var > TTY-aware default.
    fn output_mode(&self) -> OutputMode {
        resolve_output_mode(self.format, self.json)
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
        after_help = "EXAMPLES:\n    # Initialize a project in the current directory\n    bn init\n\n    # Emit machine-readable output\n    bn init --format json"
    )]
    Init(cmd::init::InitArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Create a new work item",
        long_about = "Create a new work item and append an item.create event.",
        after_help = "EXAMPLES:\n    # Create a task\n    bn create --title \"Fix login timeout\"\n\n    # Create a goal\n    bn create --title \"Launch v2\" --kind goal\n\n    # Emit machine-readable output\n    bn create --title \"Fix login timeout\" --format json"
    )]
    Create(cmd::create::CreateArgs),

    #[command(
        next_help_heading = "Read",
        about = "List work items",
        long_about = "List work items with optional filters and sort order.",
        after_help = "EXAMPLES:\n    # List open items (default)\n    bn list\n\n    # Filter by state and label\n    bn list --state doing --label backend\n\n    # Emit machine-readable output\n    bn list --format json"
    )]
    List(cmd::list::ListArgs),

    #[command(
        next_help_heading = "Read",
        about = "Show one work item",
        long_about = "Show full details for a single work item by ID.",
        after_help = "EXAMPLES:\n    # Show an item\n    bn show bn-abc\n\n    # Use a short prefix when unique\n    bn show abc\n\n    # Emit machine-readable output\n    bn show bn-abc --format json"
    )]
    Show(cmd::show::ShowArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Read",
        about = "Show chronological event timeline for one item",
        long_about = "Read append-only event shards and show timeline entries for a single item ID.",
        after_help = "EXAMPLES:\n    # Show timeline for one item\n    bn bone log bn-abc\n\n    # Filter to recent events\n    bn bone log bn-abc --since 2026-02-01T00:00:00Z\n\n    # Machine-readable output\n    bn bone log bn-abc --format json"
    )]
    Log(cmd::log::LogArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Read",
        about = "Show recent global event history",
        long_about = "Read append-only event shards and show recent events across all items.",
        after_help = "EXAMPLES:\n    # Show recent global activity\n    bn bone history\n\n    # Filter by agent and limit\n    bn bone history --agent alice -n 20\n\n    # Machine-readable output\n    bn bone history --format json"
    )]
    History(cmd::log::HistoryArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Read",
        about = "Attribute a field's last write",
        long_about = "Find the most recent event that modified a field on an item.",
        after_help = "EXAMPLES:\n    # Show who last changed the title\n    bn bone blame bn-abc title\n\n    # Machine-readable output\n    bn bone blame bn-abc title --format json"
    )]
    Blame(cmd::log::BlameArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Read",
        about = "List known agents",
        long_about = "List all known agents with current assignment counts and last-activity timestamps.",
        after_help = "EXAMPLES:\n    # List known agents\n    bn bone agents\n\n    # Machine-readable output\n    bn bone agents --format json"
    )]
    Agents(cmd::agents::AgentsArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Read",
        about = "List items assigned to the current agent",
        long_about = "Shortcut for `bn list --assignee <resolved-agent>` using the standard agent identity resolution chain.",
        after_help = "EXAMPLES:\n    # List my open items\n    bn bone mine\n\n    # Include done items\n    bn bone mine --state done\n\n    # Machine-readable output\n    bn bone mine --format json"
    )]
    Mine(cmd::mine::MineArgs),

    #[command(
        next_help_heading = "Search",
        about = "Search items using full-text search",
        long_about = "Search work items using hybrid ranking (lexical BM25 + optional semantic + structural fusion).\n\n\
                      Supports FTS5 syntax: stemming ('run' matches 'running'), prefix ('auth*'), boolean (AND/OR/NOT).",
        after_help = "EXAMPLES:\n    # Search for items about authentication\n    bn search authentication\n\n    # Prefix search\n    bn search 'auth*'\n\n    # Limit results\n    bn search timeout -n 5\n\n    # Machine-readable output\n    bn search authentication --format json"
    )]
    Search(cmd::search::SearchArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Search",
        about = "Find potential duplicate work items",
        long_about = "Find work items that may be duplicates of the given item.\n\n\
                      Uses FTS5 lexical search with BM25 ranking. Similarity scores are \
                      normalized and classified using thresholds from .bones/config.toml.",
        after_help = "EXAMPLES:\n    # Find duplicates of an item\n    bn dup bn-abc\n\n    # Use a custom threshold\n    bn dup bn-abc --threshold 0.75\n\n    # Machine-readable output\n    bn dup bn-abc --format json"
    )]
    Dup(cmd::dup::DupArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Search",
        about = "Bulk duplicate detection across open items",
        long_about = "Scan all open items to find likely duplicate clusters.\n\n\
                      Uses FTS5 BM25 as a first-pass filter, then fusion scoring to\
                      confirm likely duplicate links.",
        after_help = "EXAMPLES:\n    # Scan with default threshold\n    bn dedup\n\n    # More permissive threshold\n    bn dedup --threshold 0.60\n\n    # Limit groups\n    bn dedup --limit 20\n\n    # Machine-readable output\n    bn dedup --format json"
    )]
    Dedup(cmd::dedup::DedupArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Search",
        about = "Find items most similar to a given item",
        long_about = "Find work items most similar to the given item using fusion scoring.\n\n\
                      Combines lexical (FTS5), semantic, and structural search layers via\n\
                      Reciprocal Rank Fusion (RRF) to rank candidates by similarity.\n\n\
                      Results exclude the source item and show per-layer score breakdown.",
        after_help = "EXAMPLES:\n    # Find items similar to bn-abc\n    bn similar bn-abc\n\n\
                      # Limit to top 5 results\n    bn similar bn-abc --limit 5\n\n\
                      # Machine-readable output\n    bn similar bn-abc --format json"
    )]
    Similar(cmd::similar::SimilarArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Mark item as doing",
        long_about = "Transition a work item to the doing state.",
        after_help = "EXAMPLES:\n    # Start work on an item\n    bn do bn-abc\n\n    # Emit machine-readable output\n    bn do bn-abc --format json"
    )]
    Do(cmd::do_cmd::DoArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Mark item as done",
        long_about = "Transition a work item to the done state.",
        after_help = "EXAMPLES:\n    # Complete an item\n    bn done bn-abc\n\n    # Emit machine-readable output\n    bn done bn-abc --format json"
    )]
    Done(cmd::done::DoneArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Feedback",
        about = "Record that you worked on this item (positive feedback)",
        long_about = "Record positive feedback: you acted on the triage recommendation and worked on this item.\n\nAppends a feedback entry to .bones/feedback.jsonl and updates the Thompson Sampling posterior for your agent profile.",
        after_help = "EXAMPLES:\n    # Record that you worked on bn-abc\n    bn bone did bn-abc\n\n    # With explicit agent identity\n    bn --agent alice did bn-abc\n\n    # Emit machine-readable output\n    bn bone did bn-abc --format json"
    )]
    Did(cmd::feedback::DidArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Feedback",
        about = "Record that you skipped this item (negative feedback)",
        long_about = "Record negative feedback: the triage recommendation was not followed and you skipped this item.\n\nAppends a feedback entry to .bones/feedback.jsonl and updates the Thompson Sampling posterior for your agent profile.",
        after_help = "EXAMPLES:\n    # Record that you skipped bn-abc\n    bn bone skip bn-abc\n\n    # With explicit agent identity\n    bn --agent alice skip bn-abc\n\n    # Emit machine-readable output\n    bn bone skip bn-abc --format json"
    )]
    Skip(cmd::feedback::SkipArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Lifecycle",
        about = "Archive done items",
        long_about = "Archive a done work item, or bulk-archive stale done items.",
        after_help = "EXAMPLES:\n    # Archive one done item\n    bn bone archive bn-abc\n\n    # Bulk-archive done items older than 30 days\n    bn bone archive --auto\n\n    # Use a custom staleness window\n    bn bone archive --auto --days 14\n\n    # Emit machine-readable output\n    bn bone archive bn-abc --format json"
    )]
    Archive(cmd::archive::ArchiveArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Update fields on a work item",
        long_about = "Update one or more fields on an existing work item. Each field change emits a separate item.update event.",
        after_help = "EXAMPLES:\n    # Update title\n    bn update bn-abc --title \"New title\"\n\n    # Update multiple fields\n    bn update bn-abc --title \"Fix\" --urgency urgent\n\n    # Emit machine-readable output\n    bn update bn-abc --title \"Fix\" --format json"
    )]
    Update(cmd::update::UpdateArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Lifecycle",
        about = "Close a work item (alias for done)",
        long_about = "Transition a work item to the done state. Equivalent to 'bn done'.",
        after_help = "EXAMPLES:\n    # Close an item\n    bn close bn-abc\n\n    # Close with reason\n    bn close bn-abc --reason \"Shipped in v2\"\n\n    # Emit machine-readable output\n    bn close bn-abc --format json"
    )]
    Close(cmd::close::CloseArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Lifecycle",
        about = "Soft-delete a work item",
        long_about = "Soft-delete a work item by appending an item.delete tombstone event.",
        after_help = "EXAMPLES:\n    # Delete an item (TTY asks for confirmation)\n    bn bone delete bn-abc\n\n    # Delete with reason and skip confirmation\n    bn bone delete bn-abc --reason \"Duplicate\" --force\n\n    # Emit machine-readable output\n    bn bone delete bn-abc --format json"
    )]
    Delete(cmd::delete::DeleteArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Lifecycle",
        about = "Reopen a closed or archived item",
        long_about = "Transition a done or archived work item back to the open state.",
        after_help = "EXAMPLES:\n    # Reopen an item\n    bn bone reopen bn-abc\n\n    # Emit machine-readable output\n    bn bone reopen bn-abc --format json"
    )]
    Reopen(cmd::reopen::ReopenArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Lifecycle",
        about = "Reverse the last N events on an item via compensating events",
        long_about = "Emit compensating events that reverse the effect of prior events.\n\n\
                      Does NOT delete or modify existing events — the append-only event log\n\
                      and Merkle-DAG integrity are preserved.\n\n\
                      Events that CANNOT be undone (grow-only):\n\
                      - item.comment (G-Set: comments are permanent)\n\
                      - item.compact, item.snapshot (compaction)\n\
                      - item.redact (intentionally permanent)",
        after_help = "EXAMPLES:\n    # Undo the last event on an item\n    bn bone undo bn-abc\n\n    # Undo the last 3 events\n    bn bone undo bn-abc --last 3\n\n    # Undo a specific event by hash\n    bn bone undo --event blake3:abcdef...\n\n    # Preview without emitting\n    bn bone undo bn-abc --dry-run\n\n    # Emit machine-readable output\n    bn bone undo bn-abc --format json"
    )]
    Undo(cmd::undo::UndoArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Metadata",
        about = "Add labels to an item",
        long_about = "Attach one or more labels to an existing work item.",
        after_help = "EXAMPLES:\n    # Add labels\n    bn bone tag bn-abc bug urgent\n\n    # Emit machine-readable output\n    bn bone tag bn-abc bug --format json"
    )]
    Tag(cmd::tag::TagArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Metadata",
        about = "Remove labels from an item",
        long_about = "Remove one or more labels from an existing work item.",
        after_help = "EXAMPLES:\n    # Remove a label\n    bn bone untag bn-abc urgent\n\n    # Emit machine-readable output\n    bn bone untag bn-abc urgent --format json"
    )]
    Untag(cmd::tag::UntagArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Metadata",
        about = "Add a comment to an item",
        long_about = "Append an immutable item.comment event to a work item.",
        after_help = "EXAMPLES:\n    # Add a comment\n    bn bone comment add bn-abc \"Investigating timeout path\"\n\n    # Emit machine-readable output\n    bn bone comment add bn-abc \"Investigating timeout path\" --format json"
    )]
    Comment(cmd::comment::CommentArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Read",
        about = "Show comment timeline for an item",
        long_about = "List comments for a work item in chronological order.",
        after_help = "EXAMPLES:\n    # Show comments\n    bn bone comments bn-abc\n\n    # Emit machine-readable output\n    bn bone comments bn-abc --format json"
    )]
    Comments(cmd::comment::CommentsArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Metadata",
        about = "List all labels with usage counts",
        long_about = "List global label inventory from the projection database.",
        after_help = "EXAMPLES:\n    # List labels\n    bn bone labels\n\n    # Group by namespace\n    bn bone labels --namespace\n\n    # Emit machine-readable output\n    bn bone labels --format json"
    )]
    Labels(cmd::labels::LabelsArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Metadata",
        about = "Canonical single-label operations",
        long_about = "Manage one label at a time. `bn bone label add/rm` are canonical aliases for `bn tag`/`bn untag`.",
        after_help = "EXAMPLES:\n    # Add one label\n    bn bone label add bn-abc area:backend\n\n    # Remove one label\n    bn bone label rm bn-abc area:backend"
    )]
    Label(cmd::labels::LabelArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Metadata",
        about = "Assign an item to an agent",
        long_about = "Assign an item to an agent by emitting an item.assign event.",
        after_help = "EXAMPLES:\n    # Assign item to alice\n    bn bone assign bn-abc alice\n\n    # Emit machine-readable output\n    bn bone assign bn-abc alice --format json"
    )]
    Assign(cmd::assign::AssignArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Metadata",
        about = "Unassign the current agent from an item",
        long_about = "Remove the current resolved agent from the item's assignee OR-Set.",
        after_help = "EXAMPLES:\n    # Unassign yourself from an item\n    bn --agent alice bone unassign bn-abc\n\n    # Emit machine-readable output\n    bn --agent alice bone unassign bn-abc --format json"
    )]
    Unassign(cmd::assign::UnassignArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Lifecycle",
        about = "Move item under a parent",
        long_about = "Change a work item's parent to reorganize hierarchy.",
        after_help = "EXAMPLES:\n    # Move under a goal\n    bn bone move bn-task --parent bn-goal\n\n    # Emit machine-readable output\n    bn bone move bn-task --parent bn-goal --format json"
    )]
    Move(cmd::move_cmd::MoveArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Dependencies",
        about = "Manage dependency links",
        long_about = "Add or remove dependency links between work items.\n\nUse 'bn triage dep add <from> --blocks <to>' to establish a blocking dependency.\nUse 'bn triage dep add <from> --relates <to>' for informational links.\nUse 'bn triage dep rm <from> <to>' to remove a link.",
        after_help = "EXAMPLES:\n    # Mark A as a blocker of B\n    bn triage dep add bn-abc --blocks bn-def\n\n    # Remove the dependency\n    bn triage dep rm bn-abc bn-def\n\n    # Emit machine-readable output\n    bn triage dep add bn-abc --blocks bn-def --format json"
    )]
    Dep(cmd::dep::DepArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Dependencies",
        about = "Visualize the dependency graph",
        long_about = "Show the dependency graph for an item or the whole project.\n\nWith an item ID: show upstream (blocked-by) and downstream (blocks) dependencies.\nWithout an ID: show project-level statistics and structural analysis.",
        after_help = "EXAMPLES:\n    # Show full graph for an item\n    bn triage graph bn-abc\n\n    # Only show what bn-abc blocks\n    bn triage graph bn-abc --down\n\n    # Project summary\n    bn triage graph\n\n    # Emit machine-readable output\n    bn triage graph bn-abc --format json"
    )]
    Graph(cmd::graph::GraphArgs),

    #[command(
        next_help_heading = "Triage",
        about = "Show the highest-priority unblocked item",
        long_about = "Compute composite priority scores and return the best unblocked candidate.\n\nUse '--agent N' to request N parallel assignments (multi-agent mode).",
        after_help = "EXAMPLES:\n    # Single best next item\n    bn next\n\n    # Multi-agent assignment (N slots)\n    bn next --agent 3\n\n    # Emit machine-readable output\n    bn next --format json"
    )]
    Next(cmd::next::NextArgs),

    #[command(
        next_help_heading = "Triage",
        about = "Triage workflows and reports",
        long_about = "Run triage report and triage-adjacent analysis commands.",
        after_help = "QUICK REFERENCE:\n    bn triage                # default triage report\n    bn triage report         # explicit report\n    bn triage dup <id>       # check one item for duplicates\n    bn triage dedup          # bulk duplicate scan\n    bn triage plan           # parallel execution layers\n    bn triage health         # dependency health metrics\n\nEXAMPLES:\n    # Human-readable triage report\n    bn triage\n\n    # Explicit report subcommand\n    bn triage report\n\n    # Duplicate analysis\n    bn triage dup bn-abc"
    )]
    Triage(TriageGroupArgs),

    #[command(
        next_help_heading = "Read",
        about = "Quick agent/human orientation",
        long_about = "Show agent identity, assigned items, and project-level counts.\n\nDesigned as a fast \"where am I?\" command after crash/restart.",
        after_help = "EXAMPLES:\n    # Human-readable status\n    bn status\n\n    # With explicit agent\n    bn --agent alice status\n\n    # Machine-readable output\n    bn status --format json"
    )]
    Status(cmd::status::StatusArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Read",
        about = "Show goal completion progress",
        long_about = "Show a focused goal-progress view with child tree and progress bars.\n\nDistinct from `bn show` — this is focused on completion status of a goal and its children.",
        after_help = "EXAMPLES:\n    # Show progress for a goal\n    bn triage progress bn-p1\n\n    # Machine-readable output\n    bn triage progress bn-p1 --format json"
    )]
    Progress(cmd::progress::ProgressArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Triage",
        about = "Compute parallel execution layers",
        long_about = "Compute topological dependency layers where each layer can be worked in parallel.",
        after_help = "EXAMPLES:\n    # Project-wide plan\n    bn triage plan\n\n    # Scope to one goal's children\n    bn triage plan bn-goal\n\n    # Emit machine-readable output\n    bn triage plan --format json"
    )]
    Plan(cmd::plan::PlanArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Triage",
        about = "Show project health metrics",
        long_about = "Summarize dependency graph health metrics: density, SCC count, critical path length, and blocker count.",
        after_help = "EXAMPLES:\n    # Human-readable dashboard\n    bn triage health\n\n    # Emit machine-readable output\n    bn triage health --format json"
    )]
    Health(cmd::health::HealthArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Triage",
        about = "List dependency cycles",
        long_about = "List strongly connected components that represent dependency cycles.",
        after_help = "EXAMPLES:\n    # Human-readable cycle groups\n    bn triage cycles\n\n    # Emit machine-readable output\n    bn triage cycles --format json"
    )]
    Cycles(cmd::cycles::CyclesArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Project Maintenance",
        about = "Generate shell completion scripts",
        long_about = "Generate shell completion scripts for supported shells.",
        after_help = "EXAMPLES:\n    # Generate bash completions\n    bn admin completions bash\n\n    # Generate zsh completions\n    bn admin completions zsh"
    )]
    Completions(cmd::completions::CompletionsArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Project Maintenance",
        about = "Manage optional git hooks"
    )]
    Hooks {
        #[command(subcommand)]
        command: HookCommand,
    },

    #[command(hide = true)]
    #[command(
        next_help_heading = "Project Maintenance",
        about = "Verify event and manifest integrity",
        long_about = "Verify shard manifests and event integrity checks for this project.",
        after_help = "EXAMPLES:\n    # Verify all shard files\n    bn admin verify\n\n    # Verify only staged files\n    bn admin verify --staged\n\n    # Emit machine-readable output\n    bn admin verify --format json"
    )]
    Verify {
        /// Validate only staged files.
        #[arg(long)]
        staged: bool,

        /// Regenerate missing manifests for sealed shards.
        #[arg(long)]
        regenerate_missing: bool,
    },

    #[command(hide = true)]
    #[command(
        name = "redact-verify",
        next_help_heading = "Security",
        about = "Verify redaction completeness",
        long_about = "Verify that all item.redact events have been fully applied.\n\n\
                      Checks projection rows, FTS5 index, and comment bodies for residual\n\
                      un-redacted content.",
        after_help = "EXAMPLES:\n    # Verify all redactions\n    bn admin redact-verify\n\n    # Verify one item\n    bn admin redact-verify bn-abc\n\n    # Machine-readable output\n    bn admin redact-verify --format json"
    )]
    RedactVerify(cmd::redact_verify::RedactVerifyArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Read",
        about = "Open interactive TUI list view",
        long_about = "Open an interactive terminal UI for browsing, filtering, and navigating work items.\n\n\
                      Key bindings:\n\
                      - j/k or arrows: navigate up/down\n\
                      - /: search (filter by text)\n\
                      - f: open filter popup (state, kind, urgency, label)\n\
                      - s: cycle sort order (updated → priority → created)\n\
                      - r: refresh from database\n\
                      - ESC: clear all filters\n\
                      - q or Ctrl+C: quit",
        after_help = "EXAMPLES:\n    # Open the interactive list\n    bn tui\n\n    # Must be run in a bones project directory\n    cd myproject && bn tui"
    )]
    Tui,

    #[command(hide = true)]
    #[command(
        next_help_heading = "Simulation",
        about = "Run deterministic simulation campaigns",
        long_about = "Deterministic simulation campaign runner for verifying CRDT convergence\n\
                      invariants across many seeds with fault injection.",
        after_help = "EXAMPLES:\n    # Run 100-seed campaign\n    bn dev sim run --seeds 100\n\n\
                      # Replay a failing seed\n    bn dev sim replay --seed 42\n\n\
                      # Custom parameters with JSON output\n    bn dev sim run --seeds 200 --agents 8 --faults 0.2 --format json"
    )]
    Sim(cmd::sim::SimArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Project Maintenance",
        about = "Compact event log for completed items",
        long_about = "Replace event sequences for old done/archived items with a single\n\
                      item.snapshot event (lattice-based compaction). Compaction is\n\
                      coordination-free: each replica can compact independently and converge.",
        after_help = "EXAMPLES:\n    # Compact items done for 30+ days (default)\n    bn admin compact\n\n    # Custom age threshold\n    bn admin compact --min-age-days 60\n\n    # Dry run — see what would be compacted\n    bn admin compact --dry-run\n\n    # Machine-readable output\n    bn admin compact --format json"
    )]
    Compact(cmd::compact::CompactArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Reporting",
        about = "Show project-level statistics and reporting dashboard",
        long_about = "Query the projection database for aggregate counts, velocity metrics, and aging stats.\n\n\
                      Requires a rebuilt projection (`bn admin rebuild`). Reports items by state, kind, urgency,\n\
                      and events by type and agent.",
        after_help = "EXAMPLES:\n    # Show human-readable stats\n    bn triage stats\n\n    # Machine-readable output\n    bn triage stats --format json"
    )]
    Stats(cmd::stats::StatsArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Project Maintenance",
        about = "Run repository diagnostics",
        long_about = "Summarize event-log health, integrity anomalies, and projection drift indicators.",
        after_help = "EXAMPLES:\n    # Human-readable diagnostics\n    bn admin diagnose\n\n    # Machine-readable diagnostics\n    bn admin diagnose --format json"
    )]
    Diagnose,

    #[command(hide = true)]
    #[command(
        next_help_heading = "Project Maintenance",
        about = "Inspect and update configuration",
        long_about = "Show resolved config values, inspect raw scope files, and update supported keys in project or user scope.",
        after_help = "EXAMPLES:\n    # Show resolved config\n    bn admin config show\n\n    # Show raw project config\n    bn admin config show --project\n\n    # Set project threshold\n    bn admin config set search.duplicate_threshold 0.85\n\n    # Set user output preference\n    bn admin config set --scope user user.output json"
    )]
    Config(cmd::config::ConfigArgs),

    #[command(
        next_help_heading = "Sync",
        about = "Synchronize local and remote state",
        long_about = "Run the git-oriented sync workflow for a bones project.\n\nThis command:\n1) ensures git config entries for bones files are present\n2) runs `git pull --rebase`\n3) runs `bn admin rebuild --incremental`\n4) runs `git push` (unless `--no-push`)\n\nThis is a repository workflow wrapper, not a direct CRDT transport protocol command.",
        after_help = "QUICK REFERENCE:\n    bn sync                 # config + pull + rebuild + push\n    bn sync --no-push       # stop before push\n    bn sync --config-only   # only update .gitattributes/.gitignore\n\nEXAMPLES:\n    # Full sync workflow\n    bn sync\n\n    # Local-only sync (no push)\n    bn sync --no-push\n\n    # Machine-readable output\n    bn sync --format json"
    )]
    Sync(cmd::sync::SyncArgs),

    #[command(
        next_help_heading = "Lifecycle",
        about = "Item-scoped operations",
        long_about = "Grouped item operations including history, metadata, assignment, comments, and lifecycle detail.",
        after_help = "QUICK REFERENCE:\n    bn bone log <id>                 # item event timeline\n    bn bone assign <id> <agent>      # assign\n    bn bone comment add <id> <text>  # add comment\n    bn bone tag <id> <label...>      # add labels\n    bn bone close <id>               # close item\n    bn bone reopen <id>              # reopen item\n\nEXAMPLES:\n    # Show item event timeline\n    bn bone log bn-abc\n\n    # Assign an item\n    bn bone assign bn-abc alice\n\n    # Add a comment\n    bn bone comment add bn-abc \"Investigating\""
    )]
    Bone {
        #[command(subcommand)]
        command: BoneCommand,
    },

    #[command(
        next_help_heading = "Project Maintenance",
        about = "Administrative and maintenance operations",
        long_about = "Grouped maintenance commands for verification, diagnostics, configuration, rebuild, and project housekeeping.",
        after_help = "QUICK REFERENCE:\n    bn admin verify                    # verify event/manifests\n    bn admin diagnose                  # health diagnostics\n    bn admin rebuild --incremental     # rebuild projection\n    bn admin config show               # inspect effective config\n    bn admin compact                   # compact completed-item history\n\nEXAMPLES:\n    # Verify integrity\n    bn admin verify\n\n    # Rebuild projection\n    bn admin rebuild --incremental\n\n    # Update config\n    bn admin config set user.output json"
    )]
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },

    #[command(
        next_help_heading = "Interoperability",
        about = "Data import/export and migrations",
        long_about = "Grouped data interchange commands including import, export, and legacy migration.",
        after_help = "QUICK REFERENCE:\n    bn data import ...                # ingest external tracker data\n    bn data export --output <file>    # export canonical JSONL\n    bn data migrate-from-beads ...    # one-time migration\n\nEXAMPLES:\n    # Import from GitHub\n    bn data import --github owner/repo\n\n    # Export canonical JSONL\n    bn data export --output events.jsonl"
    )]
    Data {
        #[command(subcommand)]
        command: DataCommand,
    },

    #[command(
        next_help_heading = "Developer",
        about = "Developer and simulation tooling",
        long_about = "Grouped developer-focused tools including simulation and merge utilities.",
        after_help = "QUICK REFERENCE:\n    bn dev sim run --seeds <n>        # simulation campaign\n    bn dev sim replay --seed <n>      # replay seed\n    bn dev merge-tool ...             # merge helper tool\n    bn dev merge-driver ...           # git merge-driver entrypoint\n\nEXAMPLES:\n    # Run simulation campaign\n    bn dev sim run --seeds 100\n\n    # Run merge driver helper\n    bn dev merge-driver BASE OURS THEIRS"
    )]
    Dev {
        #[command(subcommand)]
        command: DevCommand,
    },

    #[command(
        next_help_heading = "Read",
        about = "Open interactive UI",
        long_about = "Open the interactive terminal user interface for browsing and triaging work.",
        after_help = "EXAMPLES:\n    # Open the interactive UI\n    bn ui"
    )]
    Ui,

    #[command(hide = true)]
    #[command(
        next_help_heading = "Interoperability",
        about = "Import external tracker data",
        long_about = "Import tracker events from GitHub repos or generic JSONL event streams.",
        after_help = "EXAMPLES:\n    # Import from GitHub issues\n    bn data import --github owner/repo\n\n    # Import from a JSONL stream\n    bn data import --jsonl --input events.jsonl\n\n    # Emit machine-readable output\n    bn data import --github owner/repo --format json"
    )]
    Import(cmd::import::ImportArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Interoperability",
        about = "Export events in canonical JSONL format",
        long_about = "Export `.bones/events` shards to JSONL records preserving shard order for replay.",
        after_help = "EXAMPLES:\n    # Export to stdout\n    bn data export\n\n    # Export to file\n    bn data export --output events.jsonl"
    )]
    Export(cmd::export::ExportArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Sync",
        about = "Migrate from a beads project",
        long_about = "Migrate an existing beads project database into bones events.",
        after_help = "EXAMPLES:\n    # Migrate from a beads SQLite database\n    bn data migrate-from-beads --source beads.db\n\n    # Emit machine-readable output\n    bn data migrate-from-beads --source beads.db --format json"
    )]
    MigrateFromBeads(cmd::migrate::MigrateArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Project Maintenance",
        about = "Rewrite event shards to the current format version",
        long_about = "Read all .bones/events/*.events shards, apply version transforms, and rewrite them in the current format. Original files are preserved as .events.bak backups.",
        after_help = "EXAMPLES:\n    # Rewrite all shards to current format\n    bn migrate-format\n\n    # Overwrite existing .bak backups\n    bn migrate-format --force-backup"
    )]
    MigrateFormat(cmd::migrate_format::MigrateFormatArgs),

    #[command(hide = true)]
    #[command(
        next_help_heading = "Project Maintenance",
        about = "Rebuild the projection",
        long_about = "Rebuild the local projection database from append-only event shards.",
        after_help = "EXAMPLES:\n    # Full rebuild\n    bn admin rebuild\n\n    # Incremental rebuild\n    bn admin rebuild --incremental\n\n    # Emit machine-readable output\n    bn admin rebuild --format json"
    )]
    Rebuild {
        /// Rebuild incrementally from the last projection cursor.
        #[arg(long)]
        incremental: bool,
    },

    #[command(hide = true)]
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

    #[command(hide = true)]
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
enum BoneCommand {
    #[command(about = "Show chronological event timeline for one item")]
    Log(cmd::log::LogArgs),
    #[command(about = "Show recent global event history")]
    History(cmd::log::HistoryArgs),
    #[command(about = "Attribute a field's last write")]
    Blame(cmd::log::BlameArgs),
    #[command(about = "List known agents")]
    Agents(cmd::agents::AgentsArgs),
    #[command(about = "List items assigned to the current agent")]
    Mine(cmd::mine::MineArgs),
    #[command(about = "Record that you worked on this item")]
    Did(cmd::feedback::DidArgs),
    #[command(about = "Record that you skipped this item")]
    Skip(cmd::feedback::SkipArgs),
    #[command(about = "Archive done items")]
    Archive(cmd::archive::ArchiveArgs),
    #[command(about = "Close a work item")]
    Close(cmd::close::CloseArgs),
    #[command(about = "Soft-delete a work item")]
    Delete(cmd::delete::DeleteArgs),
    #[command(about = "Reopen a closed or archived item")]
    Reopen(cmd::reopen::ReopenArgs),
    #[command(about = "Reverse recent events with compensating events")]
    Undo(cmd::undo::UndoArgs),
    #[command(about = "Add labels to an item")]
    Tag(cmd::tag::TagArgs),
    #[command(about = "Remove labels from an item")]
    Untag(cmd::tag::UntagArgs),
    #[command(about = "Add a comment to an item")]
    Comment(cmd::comment::CommentArgs),
    #[command(about = "Show comment timeline for an item")]
    Comments(cmd::comment::CommentsArgs),
    #[command(about = "List all labels with usage counts")]
    Labels(cmd::labels::LabelsArgs),
    #[command(about = "Single-label operations")]
    Label(cmd::labels::LabelArgs),
    #[command(about = "Assign an item to an agent")]
    Assign(cmd::assign::AssignArgs),
    #[command(about = "Unassign the current agent from an item")]
    Unassign(cmd::assign::UnassignArgs),
    #[command(about = "Move an item under a parent")]
    Move(cmd::move_cmd::MoveArgs),
}

#[derive(Args, Debug)]
struct TriageGroupArgs {
    #[command(subcommand)]
    command: Option<TriageCommand>,
}

#[derive(Subcommand, Debug)]
enum TriageCommand {
    #[command(about = "Show a full triage report")]
    Report(cmd::triage::TriageArgs),
    #[command(about = "Find potential duplicate work items")]
    Dup(cmd::dup::DupArgs),
    #[command(about = "Bulk duplicate detection across open items")]
    Dedup(cmd::dedup::DedupArgs),
    #[command(about = "Find items similar to a given item")]
    Similar(cmd::similar::SimilarArgs),
    #[command(about = "Manage dependency links")]
    Dep(cmd::dep::DepArgs),
    #[command(about = "Visualize the dependency graph")]
    Graph(cmd::graph::GraphArgs),
    #[command(about = "Show goal completion progress")]
    Progress(cmd::progress::ProgressArgs),
    #[command(about = "Compute parallel execution layers")]
    Plan(cmd::plan::PlanArgs),
    #[command(about = "Show project health metrics")]
    Health(cmd::health::HealthArgs),
    #[command(about = "List dependency cycles")]
    Cycles(cmd::cycles::CyclesArgs),
    #[command(about = "Show project-level statistics")]
    Stats(cmd::stats::StatsArgs),
}

#[derive(Subcommand, Debug)]
enum AdminCommand {
    #[command(about = "Generate shell completion scripts")]
    Completions(cmd::completions::CompletionsArgs),
    #[command(about = "Manage optional git hooks")]
    Hooks {
        #[command(subcommand)]
        command: HookCommand,
    },
    #[command(about = "Verify event and manifest integrity")]
    Verify {
        #[arg(long)]
        staged: bool,
        #[arg(long)]
        regenerate_missing: bool,
    },
    #[command(name = "redact-verify", about = "Verify redaction completeness")]
    RedactVerify(cmd::redact_verify::RedactVerifyArgs),
    #[command(about = "Compact event log for completed items")]
    Compact(cmd::compact::CompactArgs),
    #[command(about = "Run repository diagnostics")]
    Diagnose,
    #[command(about = "Inspect and update configuration")]
    Config(cmd::config::ConfigArgs),
    #[command(about = "Rewrite event shards to current format version")]
    MigrateFormat(cmd::migrate_format::MigrateFormatArgs),
    #[command(about = "Rebuild the projection")]
    Rebuild {
        #[arg(long)]
        incremental: bool,
    },
}

#[derive(Subcommand, Debug)]
enum DataCommand {
    #[command(about = "Import external tracker data")]
    Import(cmd::import::ImportArgs),
    #[command(about = "Export events in canonical JSONL format")]
    Export(cmd::export::ExportArgs),
    #[command(name = "migrate-from-beads", about = "Migrate from a beads project")]
    MigrateFromBeads(cmd::migrate::MigrateArgs),
}

#[derive(Subcommand, Debug)]
enum DevCommand {
    #[command(about = "Run deterministic simulation campaigns")]
    Sim(cmd::sim::SimArgs),
    #[command(about = "Run merge tool for event files")]
    MergeTool {
        #[arg(long)]
        setup: bool,
        #[arg(value_name = "BASE")]
        base: Option<PathBuf>,
        #[arg(value_name = "LEFT")]
        left: Option<PathBuf>,
        #[arg(value_name = "RIGHT")]
        right: Option<PathBuf>,
        #[arg(value_name = "OUTPUT")]
        output: Option<PathBuf>,
    },
    #[command(about = "Run git merge driver for .events")]
    MergeDriver {
        #[arg(value_name = "BASE")]
        base: PathBuf,
        #[arg(value_name = "OURS")]
        ours: PathBuf,
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
        Commands::Init(args) => timing::timed("cmd.init", || {
            cmd::init::run_init(&args, output, &project_root)
        }),
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
        Commands::Dedup(ref args) => timing::timed("cmd.dedup", || {
            cmd::dedup::run_dedup(args, output, &project_root)
        }),
        Commands::Similar(ref args) => timing::timed("cmd.similar", || {
            cmd::similar::run_similar(args, output, &project_root)
        }),
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
        Commands::Undo(ref args) => timing::timed("cmd.undo", || {
            cmd::undo::run_undo(args, cli.agent_flag(), output, &project_root)
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
        Commands::Triage(ref args) => timing::timed("cmd.triage", || match &args.command {
            None => {
                let defaults = cmd::triage::TriageArgs::default();
                cmd::triage::run_triage(&defaults, output, &project_root)
            }
            Some(TriageCommand::Report(report_args)) => {
                cmd::triage::run_triage(report_args, output, &project_root)
            }
            Some(TriageCommand::Dup(dup_args)) => {
                cmd::dup::run_dup(dup_args, output, &project_root)
            }
            Some(TriageCommand::Dedup(dedup_args)) => {
                cmd::dedup::run_dedup(dedup_args, output, &project_root)
            }
            Some(TriageCommand::Similar(similar_args)) => {
                cmd::similar::run_similar(similar_args, output, &project_root)
            }
            Some(TriageCommand::Dep(dep_args)) => {
                cmd::dep::run_dep(dep_args, cli.agent_flag(), output, &project_root)
            }
            Some(TriageCommand::Graph(graph_args)) => {
                cmd::graph::run_graph(graph_args, output, &project_root)
            }
            Some(TriageCommand::Progress(progress_args)) => {
                cmd::progress::run_progress(progress_args, output, &project_root)
            }
            Some(TriageCommand::Plan(plan_args)) => {
                cmd::plan::run_plan(plan_args, output, &project_root)
            }
            Some(TriageCommand::Health(health_args)) => {
                cmd::health::run_health(health_args, output, &project_root)
            }
            Some(TriageCommand::Cycles(cycles_args)) => {
                cmd::cycles::run_cycles(cycles_args, output, &project_root)
            }
            Some(TriageCommand::Stats(stats_args)) => {
                cmd::stats::run_stats(stats_args, output, &project_root)
            }
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
        Commands::Sync(args) => timing::timed("cmd.sync", || {
            cmd::sync::run_sync(&args, output, &project_root)
        }),

        Commands::Bone { ref command } => timing::timed("cmd.bone", || match command {
            BoneCommand::Log(args) => cmd::log::run_log(args, output, &project_root),
            BoneCommand::History(args) => cmd::log::run_history(args, output, &project_root),
            BoneCommand::Blame(args) => cmd::log::run_blame(args, output, &project_root),
            BoneCommand::Agents(args) => cmd::agents::run_agents(args, output, &project_root),
            BoneCommand::Mine(args) => {
                cmd::mine::run_mine(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Did(args) => {
                cmd::feedback::run_did(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Skip(args) => {
                cmd::feedback::run_skip(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Archive(args) => {
                cmd::archive::run_archive(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Close(args) => {
                cmd::close::run_close(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Delete(args) => {
                cmd::delete::run_delete(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Reopen(args) => {
                cmd::reopen::run_reopen(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Undo(args) => {
                cmd::undo::run_undo(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Tag(args) => {
                cmd::tag::run_tag(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Untag(args) => {
                cmd::tag::run_untag(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Comment(args) => {
                cmd::comment::run_comment(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Comments(args) => cmd::comment::run_comments(args, output, &project_root),
            BoneCommand::Labels(args) => cmd::labels::run_labels(args, output, &project_root),
            BoneCommand::Label(args) => {
                cmd::labels::run_label(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Assign(args) => {
                cmd::assign::run_assign(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Unassign(args) => {
                cmd::assign::run_unassign(args, cli.agent_flag(), output, &project_root)
            }
            BoneCommand::Move(args) => {
                cmd::move_cmd::run_move(args, cli.agent_flag(), output, &project_root)
            }
        }),

        Commands::Admin { ref command } => timing::timed("cmd.admin", || match command {
            AdminCommand::Completions(args) => {
                let mut command = Cli::command();
                cmd::completions::run_completions(args.shell, &mut command)
            }
            AdminCommand::Hooks {
                command: HookCommand::Install,
            } => git::hooks::install_hooks(&project_root),
            AdminCommand::Verify {
                staged,
                regenerate_missing,
            } => {
                if *staged {
                    git::hooks::verify_staged_events()
                } else {
                    cmd::verify::run_verify(&project_root, *regenerate_missing, output)
                }
            }
            AdminCommand::RedactVerify(args) => {
                cmd::redact_verify::run_redact_verify(args, output, &project_root)
            }
            AdminCommand::Compact(args) => cmd::compact::run_compact(args, output, &project_root),
            AdminCommand::Diagnose => cmd::diagnose::run_diagnose(output, &project_root),
            AdminCommand::Config(args) => cmd::config::run_config(args, &project_root, output),
            AdminCommand::MigrateFormat(args) => {
                cmd::migrate_format::run_migrate_format(args, output, &project_root)
            }
            AdminCommand::Rebuild { incremental } => {
                cmd::rebuild::run_rebuild(&project_root, *incremental, output)
            }
        }),

        Commands::Data { ref command } => timing::timed("cmd.data", || match command {
            DataCommand::Import(args) => cmd::import::run_import(args, output, &project_root),
            DataCommand::Export(args) => cmd::export::run_export(args, &project_root),
            DataCommand::MigrateFromBeads(args) => {
                cmd::migrate::run_migrate(args, output, &project_root)
            }
        }),

        Commands::Dev { ref command } => timing::timed("cmd.dev", || match command {
            DevCommand::Sim(args) => cmd::sim::run_sim(args, output, &project_root),
            DevCommand::MergeTool {
                setup,
                base,
                left,
                right,
                output,
            } => {
                if *setup {
                    return setup_merge_tool();
                }

                let base = base
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Missing base file argument"))?;
                let left = left
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Missing left file argument"))?;
                let right = right
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Missing right file argument"))?;
                let output = output
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Missing output file argument"))?;

                merge_files(base, left, right, output)
            }
            DevCommand::MergeDriver { base, ours, theirs } => {
                git::merge_driver::merge_driver_main(base, ours, theirs)
            }
        }),
        Commands::Import(args) => timing::timed("cmd.import", || {
            cmd::import::run_import(&args, output, &project_root)
        }),
        Commands::Export(args) => timing::timed("cmd.export", || {
            cmd::export::run_export(&args, &project_root)
        }),
        Commands::MigrateFromBeads(args) => timing::timed("cmd.migrate_from_beads", || {
            cmd::migrate::run_migrate(&args, output, &project_root)
        }),
        Commands::MigrateFormat(args) => timing::timed("cmd.migrate_format", || {
            cmd::migrate_format::run_migrate_format(&args, output, &project_root)
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
        Commands::Compact(ref args) => timing::timed("cmd.compact", || {
            cmd::compact::run_compact(args, output, &project_root)
        }),
        Commands::Stats(ref args) => timing::timed("cmd.stats", || {
            cmd::stats::run_stats(args, output, &project_root)
        }),
        Commands::Diagnose => timing::timed("cmd.diagnose", || {
            cmd::diagnose::run_diagnose(output, &project_root)
        }),
        Commands::Config(ref args) => timing::timed("cmd.config", || {
            cmd::config::run_config(args, &project_root, output)
        }),
        Commands::Rebuild { incremental } => timing::timed("cmd.rebuild", || {
            cmd::rebuild::run_rebuild(&project_root, incremental, output)
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

        Commands::Sim(ref args) => {
            timing::timed("cmd.sim", || cmd::sim::run_sim(args, output, &project_root))
        }

        Commands::Ui => timing::timed("cmd.ui", || tui::run_tui(&project_root)),
        Commands::Tui => timing::timed("cmd.ui", || tui::run_tui(&project_root)),
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
    fn format_flag_sets_output_mode() {
        let cli = Cli::parse_from(["bn", "--format", "text", "list"]);
        assert_eq!(cli.format, Some(OutputMode::Text));
        assert!(cli.output_mode().is_text());
    }

    #[test]
    fn format_json_sets_output_mode() {
        let cli = Cli::parse_from(["bn", "--format", "json", "list"]);
        assert_eq!(cli.format, Some(OutputMode::Json));
        assert!(!cli.json);
        assert!(cli.output_mode().is_json());
    }

    #[test]
    fn json_flag_after_subcommand() {
        let cli = Cli::parse_from(["bn", "list", "--json"]);
        assert!(cli.json);
        assert!(cli.output_mode().is_json());
    }

    #[test]
    fn default_output_uses_auto_detection() {
        let cli = Cli::parse_from(["bn", "list"]);
        assert!(!cli.json);
        // In test (non-TTY), resolve_output_mode defaults to Text.
        assert!(cli.output_mode().is_text());
    }

    #[test]
    fn format_flag_supported_on_admin_and_data_commands() {
        let cli = Cli::parse_from(["bn", "init", "--format", "json"]);
        assert!(cli.output_mode().is_json());

        let cli = Cli::parse_from(["bn", "admin", "rebuild", "--format", "json"]);
        assert!(cli.output_mode().is_json());

        let cli = Cli::parse_from([
            "bn",
            "data",
            "import",
            "--github",
            "owner/repo",
            "--format",
            "json",
        ]);
        assert!(cli.output_mode().is_json());

        let cli = Cli::parse_from([
            "bn",
            "data",
            "migrate-from-beads",
            "--beads-jsonl",
            "beads.jsonl",
            "--format",
            "json",
        ]);
        assert!(cli.output_mode().is_json());

        let cli = Cli::parse_from(["bn", "admin", "config", "show", "--format", "json"]);
        assert!(cli.output_mode().is_json());
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
    fn dup_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "dup", "bn-123"]);
        assert!(matches!(cli.command, Commands::Dup(_)));
    }

    #[test]
    fn dedup_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "dedup", "--threshold", "0.7"]);
        assert!(matches!(cli.command, Commands::Dedup(_)));
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
    fn config_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "config", "show"]);
        assert!(matches!(cli.command, Commands::Config(_)));
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
            vec!["bn", "dup", "x"],
            vec!["bn", "dedup"],
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
            vec!["bn", "export"],
            vec!["bn", "import", "--jsonl"],
            vec!["bn", "completions", "bash"],
            vec!["bn", "diagnose"],
            vec!["bn", "config", "show"],
            vec!["bn", "undo", "bn-abc"],
            vec!["bn", "bone", "log", "x"],
            vec!["bn", "triage", "report"],
            vec!["bn", "admin", "verify"],
            vec!["bn", "data", "export"],
            vec!["bn", "dev", "sim", "run", "--seeds", "1"],
            vec!["bn", "ui"],
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
    fn undo_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "undo", "bn-abc"]);
        assert!(matches!(cli.command, Commands::Undo(_)));
    }

    #[test]
    fn undo_subcommand_parses_dry_run() {
        let cli = Cli::parse_from(["bn", "undo", "bn-abc", "--dry-run"]);
        assert!(matches!(cli.command, Commands::Undo(_)));
        if let Commands::Undo(ref args) = cli.command {
            assert!(args.dry_run);
        }
    }

    #[test]
    fn undo_subcommand_parses_last_n() {
        let cli = Cli::parse_from(["bn", "undo", "bn-abc", "--last", "5"]);
        assert!(matches!(cli.command, Commands::Undo(_)));
        if let Commands::Undo(ref args) = cli.command {
            assert_eq!(args.last_n, 5);
        }
    }

    #[test]
    fn undo_subcommand_parses_event_hash() {
        let cli = Cli::parse_from(["bn", "undo", "--event", "blake3:abc123"]);
        assert!(matches!(cli.command, Commands::Undo(_)));
        if let Commands::Undo(ref args) = cli.command {
            assert_eq!(args.event_hash.as_deref(), Some("blake3:abc123"));
            assert!(args.id.is_none());
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
    fn triage_group_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "triage", "dup", "bn-abc"]);
        assert!(matches!(cli.command, Commands::Triage(_)));
    }

    #[test]
    fn bone_group_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "bone", "log", "bn-abc"]);
        assert!(matches!(cli.command, Commands::Bone { .. }));
    }

    #[test]
    fn admin_group_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "admin", "verify"]);
        assert!(matches!(cli.command, Commands::Admin { .. }));
    }

    #[test]
    fn data_group_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "data", "export"]);
        assert!(matches!(cli.command, Commands::Data { .. }));
    }

    #[test]
    fn dev_group_subcommand_parses() {
        let cli = Cli::parse_from(["bn", "dev", "sim", "run", "--seeds", "1"]);
        assert!(matches!(cli.command, Commands::Dev { .. }));
    }

    #[test]
    fn ui_command_parses() {
        let cli = Cli::parse_from(["bn", "ui"]);
        assert!(matches!(cli.command, Commands::Ui));
    }

    #[test]
    fn next_supports_agent_slot_flag() {
        let cli = Cli::parse_from(["bn", "next", "--agent", "3"]);
        assert!(matches!(cli.command, Commands::Next(_)));
        assert_eq!(cli.agent_flag(), Some("3"));
    }
}
