# bones

Project type: cli, tui
Tools: `beads`, `maw`, `crit`, `botbus`, `botty`

## Project Overview

bones is a CRDT-native issue tracker designed for distributed human and agent collaboration.
The repository is organized as a Cargo workspace with focused crates.

## Crate Layout

```
crates/
  bones-core/    Core data structures, locking, errors, event/CRDT foundations
  bones-triage/  Prioritization and scoring logic
  bones-search/  Search/index abstractions
  bones-cli/     `bn` command-line entry point
  bones-sim/     Deterministic simulation harness
```

## Architecture Diagram (high level)

```
             +---------------------+
             |      bones-cli      |
             |  command parsing    |
             +----------+----------+
                        |
                        v
     +------------------+------------------+
     |             bones-core              |
     | event model, ids, CRDT + projection |
     +---------+--------------------+------+
               |                    |
               v                    v
       +-------+------+      +------+-------+
       | bones-triage |      | bones-search |
       | scoring/rank |      | retrieval    |
       +--------------+      +--------------+
               ^
               |
       +-------+------+
       |  bones-sim   |
       | replay/tests |
       +--------------+
```

## Build & Test

```bash
# Build all crates
cargo build

# Run all tests
cargo test

# Run one crate
cargo test -p bones-core

# Run CLI
cargo run -p bones-cli -- --help
```

## Contributor Onboarding

See `docs/contributor-guide.md` for:
- adding new event types
- adding new CLI commands
- adding new triage/search metrics
- local development setup and conventions

## Conventions

- Prefer deterministic behavior; seed randomness in tests/simulations.
- Keep user-facing terms consistent (`agent`, `item`, `event`).
- Preserve machine readability for CLI output (`--json` support where applicable).
- Treat `.bones/events/*.events` as append-only logs; derived state belongs in projections.

<!-- botbox:managed-start -->
## Botbox Workflow

### How to Make Changes

1. **Create a bone** to track your work: `maw exec default -- bn create --title "..." --description "..."`
2. **Create a workspace** for your changes: `maw ws create --random` — this gives you `ws/<name>/`
3. **Edit files in your workspace** (`ws/<name>/`), never in `ws/default/`
4. **Merge when done**: `maw ws merge <name> --destroy`
5. **Close the bone**: `maw exec default -- bn done <id>`

Do not create git branches manually — `maw ws create` handles branching for you. See [worker-loop.md](.agents/botbox/worker-loop.md) for the full triage → start → work → finish cycle.

**All tools have `--help`** with usage examples. When unsure, run `<tool> --help` or `<tool> <command> --help`.

### Directory Structure (maw v2)

This project uses a **bare repo** layout. Source files live in workspaces under `ws/`, not at the project root.

```
project-root/          ← bare repo (no source files here)
├── ws/
│   ├── default/       ← main working copy (AGENTS.md, .bones/, src/, etc.)
│   ├── frost-castle/  ← agent workspace (isolated Git worktree)
│   └── amber-reef/    ← another agent workspace
├── .manifold/         ← maw metadata/artifacts
├── .git/              ← git data (core.bare=true)
├── AGENTS.md          ← stub redirecting to ws/default/AGENTS.md
└── CLAUDE.md          ← symlink → AGENTS.md
```

**Key rules:**
- `ws/default/` is the main workspace — bones, config, and project files live here
- **Never merge or destroy the default workspace.** It is where other branches merge INTO, not something you merge.
- Agent workspaces (`ws/<name>/`) are isolated Git worktrees managed by maw
- Use `maw exec <ws> -- <command>` to run commands in a workspace context
- Use `maw exec default -- bn ...` for bones commands (always in default workspace)
- Use `maw exec <ws> -- crit ...` for review commands (always in the review's workspace)
- Never run `bn` or `crit` directly — always go through `maw exec`
- Do not run `jj`; this workflow is Git + maw.

### Bones Quick Reference

| Operation | Command |
|-----------|---------|
| Triage (scores) | `maw exec default -- bn triage` |
| Next bone | `maw exec default -- bn next` |
| Next N bones | `maw exec default -- bn next N` (e.g., `bn next 4` for dispatch) |
| Show bone | `maw exec default -- bn show <id>` |
| Create | `maw exec default -- bn create --title "..." --description "..."` |
| Start work | `maw exec default -- bn do <id>` |
| Add comment | `maw exec default -- bn bone comment add <id> "message"` |
| Close | `maw exec default -- bn done <id>` |
| Add dependency | `maw exec default -- bn triage dep add <blocker> --blocks <blocked>` |
| Search | `maw exec default -- bn search <query>` |

Identity resolved from `$AGENT` env. No flags needed in agent loops.

### Workspace Quick Reference

| Operation | Command |
|-----------|---------|
| Create workspace | `maw ws create <name>` |
| List workspaces | `maw ws list` |
| Check merge readiness | `maw ws merge <name> --check` |
| Merge to main | `maw ws merge <name> --destroy` |
| Destroy (no merge) | `maw ws destroy <name>` |
| Run command in workspace | `maw exec <name> -- <command>` |
| Diff workspace vs epoch | `maw ws diff <name>` |
| Check workspace overlap | `maw ws overlap <name1> <name2>` |
| View workspace history | `maw ws history <name>` |
| Sync stale workspace | `maw ws sync <name>` |
| Inspect merge conflicts | `maw ws conflicts <name>` |
| Undo local workspace changes | `maw ws undo <name>` |

**Inspecting a workspace (use git, not jj):**
```bash
maw exec <name> -- git status             # what changed (unstaged)
maw exec <name> -- git log --oneline -5   # recent commits
maw ws diff <name>                        # diff vs epoch (maw-native)
```

**Lead agent merge workflow** — after a worker finishes a bone:
1. `maw ws list` — look for `active (+N to merge)` entries
2. `maw ws merge <name> --check` — verify no conflicts
3. `maw ws merge <name> --destroy` — merge and clean up

**Workspace safety:**
- Never merge or destroy `default`.
- Always `maw ws merge <name> --check` before `--destroy`.
- Commit workspace changes with `maw exec <name> -- git add -A && maw exec <name> -- git commit -m "..."`.

### Protocol Quick Reference

Use these commands at protocol transitions to check state and get exact guidance. Each command outputs instructions for the next steps.

| Step | Command | Who | Purpose |
|------|---------|-----|---------|
| Resume | `botbox protocol resume --agent $AGENT` | Worker | Detect in-progress work from previous session |
| Start | `botbox protocol start <bone-id> --agent $AGENT` | Worker | Verify bone is ready, get start commands |
| Review | `botbox protocol review <bone-id> --agent $AGENT` | Worker | Verify work is complete, get review commands |
| Finish | `botbox protocol finish <bone-id> --agent $AGENT` | Worker | Verify review approved, get close/cleanup commands |
| Merge | `botbox protocol merge <workspace> --agent $AGENT` | Lead | Check preconditions, detect conflicts, get merge steps |
| Cleanup | `botbox protocol cleanup --agent $AGENT` | Worker | Check for held resources to release |

All commands support JSON output with `--format json` for parsing. If a command is unavailable or fails (exit code 1), fall back to manual steps documented in [start](.agents/botbox/start.md), [review-request](.agents/botbox/review-request.md), and [finish](.agents/botbox/finish.md).

### Bones Conventions

- Create a bone before starting work. Update state: `open` → `doing` → `done`.
- Post progress comments during work for crash recovery.
- **Run checks before requesting review**: `just check` (or your project's build/test command). Fix any failures before proceeding.
- After finishing a bone, follow [finish.md](.agents/botbox/finish.md). **Workers: do NOT push** — the lead handles merges and pushes.
- **Install locally** after releasing: `maw exec default -- just install`
- **Update README install tag** on every release: bump the `--tag vX.Y.Z` in `README.md` to match the new version.

### Identity

Your agent name is set by the hook or script that launched you. Use `$AGENT` in commands.
For manual sessions, use `<project>-dev` (e.g., `myapp-dev`).

### Claims

When working on a bone, stake claims to prevent conflicts:

```bash
bus claims stake --agent $AGENT "bone://<project>/<id>" -m "<id>"
bus claims stake --agent $AGENT "workspace://<project>/<ws>" -m "<id>"
bus claims release --agent $AGENT --all  # when done
```

### Reviews

Use `@<project>-<role>` mentions to request reviews:

```bash
maw exec $WS -- crit reviews request <review-id> --reviewers $PROJECT-security --agent $AGENT
bus send --agent $AGENT $PROJECT "Review requested: <review-id> @$PROJECT-security" -L review-request
```

The @mention triggers the auto-spawn hook for the reviewer.

### Bus Communication

Agents communicate via bus channels. You don't need to be expert on everything — ask the right project.

| Operation | Command |
|-----------|---------|
| Send message | `bus send --agent $AGENT <channel> "message" [-L label]` |
| Check inbox | `bus inbox --agent $AGENT --channels <ch> [--mark-read]` |
| Wait for reply | `bus wait -c <channel> --mention -t 120` |
| Browse history | `bus history <channel> -n 20` |
| Search messages | `bus search "query" -c <channel>` |

**Conversations**: After sending a question, use `bus wait -c <channel> --mention -t <seconds>` to block until the other agent replies. This enables back-and-forth conversations across channels.

**Project experts**: Each `<project>-dev` is the expert on their project. When stuck on a companion tool (bus, maw, crit, botty, bn), post a question to its project channel instead of guessing.

### Cross-Project Communication

**Don't suffer in silence.** If a tool confuses you or behaves unexpectedly, post to its project channel.

1. Find the project: `bus history projects -n 50` (the #projects channel has project registry entries)
2. Post question or feedback: `bus send --agent $AGENT <project> "..." -L feedback`
3. For bugs, create bones in their repo first
4. **Always create a local tracking bone** so you check back later:
   ```bash
   maw exec default -- bn create --title "[tracking] <summary>" --tag tracking --kind task
   ```

See [cross-channel.md](.agents/botbox/cross-channel.md) for the full workflow.

### Session Search (optional)

Use `cass search "error or problem"` to find how similar issues were solved in past sessions.


### Design Guidelines


- [CLI tool design for humans, agents, and machines](.agents/botbox/design/cli-conventions.md)



### Workflow Docs


- [Find work from inbox and bones](.agents/botbox/triage.md)

- [Claim bone, create workspace, announce](.agents/botbox/start.md)

- [Change bone state (open/doing/done)](.agents/botbox/update.md)

- [Close bone, merge workspace, release claims](.agents/botbox/finish.md)

- [Full triage-work-finish lifecycle](.agents/botbox/worker-loop.md)

- [Turn specs/PRDs into actionable bones](.agents/botbox/planning.md)

- [Explore unfamiliar code before planning](.agents/botbox/scout.md)

- [Create and validate proposals before implementation](.agents/botbox/proposal.md)

- [Request a review](.agents/botbox/review-request.md)

- [Handle reviewer feedback (fix/address/defer)](.agents/botbox/review-response.md)

- [Reviewer agent loop](.agents/botbox/review-loop.md)

- [Merge a worker workspace (protocol merge + conflict recovery)](.agents/botbox/merge-check.md)

- [Validate toolchain health](.agents/botbox/preflight.md)

- [Ask questions, report bugs, and track responses across projects](.agents/botbox/cross-channel.md)

- [Report bugs/features to other projects](.agents/botbox/report-issue.md)

- [groom](.agents/botbox/groom.md)

<!-- botbox:managed-end -->
