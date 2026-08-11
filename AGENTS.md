# bones

Project type: cli, tui
Tools: `bones`, `maw`, `seal`, `rite`, `vessel`

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

<!-- edict:managed-start -->
## Edict Workflow

### How to Make Changes

1. **Create a bone** to track your work: `bn create --title "..." --description "..."`
2. **Create a workspace** for your changes: `maw ws create <bone-id> --from main --description "<bone-title>"` — use the bone ID as workspace name; this gives you `.maw/workspaces/<bone-id>/`
3. **Edit files in your workspace** (`.maw/workspaces/<name>/`), never in the trunk at the repo root
4. **Merge when done**: `maw ws merge <name> --into default --destroy --message "feat: <bone-title>"` (use conventional commit prefix: `feat:`, `fix:`, `chore:`, etc.; swap `default` for a change id when merging back into a tracked change)
5. **Close the bone**: `bn done <id>`

Do not create git branches manually — `maw ws create` handles branching for you. See [worker-loop.md](.agents/edict/worker-loop.md) for the full triage → start → work → finish cycle.

**All tools have `--help`** with usage examples. When unsure, run `<tool> --help` or `<tool> <command> --help`.

### Conflicts Are Data, Not Errors

`maw ws sync` rebases committed-ahead workspaces onto the latest epoch by default. On conflict it does not abort — it commits labeled conflict markers and leaves the workspace `lifecycle:conflicted` (visible in `maw ws list`). Treat a conflicted workspace as a normal state, not a failure.

- `maw ws resolve <ws> --list` shows conflicts; `--keep epoch|<ws>|both|union` (or `--keep PATH=NAME`) resolves them.
- `maw ws merge` auto-syncs stale sources and accepts `--resolve cf-id=<ws>` / `--resolve-all=<ws>` to resolve inline.
- `maw ws conflicts <ws>` inspects conflict details.
- The one hard gate: merge refuses a source whose HEAD still has unresolved conflict markers (bypass with `--force` only for legitimate marker-like content).

### Directory Structure

This project uses the **root** layout. The project root is the trunk working copy — source files, `.bones/`, config, and `AGENTS.md` live there. Extra agent workspaces live under `.maw/workspaces/`.

```
project-root/              ← trunk working copy (AGENTS.md, .bones/, src/, etc.)
├── src/, AGENTS.md, …     ← your project files, edited here directly
├── .maw/
│   ├── workspaces/
│   │   ├── bn-1abc/       ← agent workspace (named after bone ID)
│   │   └── bn-2def/       ← another agent workspace
│   └── manifold/          ← maw metadata/artifacts
└── .git/                  ← git data
```

**Key rules:**
- The project root is the trunk — bones, config, and project files live here, and you edit them directly
- **Never merge or destroy the `default` workspace.** `default` names the trunk (the repo root); other workspaces merge INTO it, not the other way around.
- Agent workspaces (`.maw/workspaces/<name>/`) are isolated Git worktrees managed by maw
- Use `maw exec <ws> -- <command>` to run commands in a non-default workspace context
- Run `bn ...` directly at the repo root for bones commands (no `maw exec` prefix needed — they always target the trunk)
- Use `maw exec <ws> -- seal ...` for review commands (always in the review's workspace)

### Bones Quick Reference

| Operation | Command |
|-----------|---------|
| Triage (scores) | `bn triage` |
| Next bone | `bn next` |
| Next N bones | `bn next N` (e.g., `bn next 4` for dispatch) |
| Show bone | `bn show <id>` |
| Create | `bn create --title "..." --description "..."` |
| Start work | `bn do <id>` |
| Add comment | `bn bone comment add <id> "message"` |
| Close | `bn done <id>` |
| Add dependency | `bn triage dep add <blocker> --blocks <blocked>` |
| Search | `bn search <query>` |

Identity resolved from `$AGENT` env. No flags needed in agent loops.

### Workspace Quick Reference

| Operation | Command |
|-----------|---------|
| Create workspace | `maw ws create <bone-id> --from main --description "<title>"` |
| List workspaces | `maw ws list` |
| Check merge readiness | `maw ws merge <name> --into default --check` |
| Merge to main | `maw ws merge <name> --into default --destroy --message "feat: <bone-title>"` |
| Destroy (no merge) | `maw ws destroy <name>` |
| Run command in workspace | `maw exec <name> -- <command>` |
| Diff workspace vs epoch | `maw ws diff <name>` |
| Check workspace overlap | `maw ws overlap <name1> <name2>` |
| View workspace history | `maw ws history <name>` |
| Sync stale workspace | `maw ws sync <name>` |
| Inspect merge conflicts | `maw ws conflicts <name>` |
| Undo local workspace changes | `maw ws undo <name>` |
| List recovery snapshots | `maw ws recover` |
| Recover destroyed workspace | `maw ws recover <name> --to <new-name>` |
| Search recovery snapshots | `maw ws recover --search <pattern>` |
| Show file from snapshot | `maw ws recover <name> --show <path>` |

**Inspecting a workspace:**
```bash
maw exec <name> -- git status             # what changed (unstaged)
maw exec <name> -- git log --oneline -5   # recent commits
maw ws diff <name>                        # diff vs epoch (maw-native)
```

**Lead agent merge workflow** — after a worker finishes a bone:
1. `maw ws list` — look for `active (+N to merge)` entries
2. `maw ws merge <name> --into default --check` — verify no conflicts
3. `maw ws merge <name> --into default --destroy --message "feat: <bone-title>"` — merge and clean up (use conventional commit prefix)

**Workspace safety:**
- Never merge or destroy `default`.
- Always `maw ws merge <name> --into default --check` before `--destroy`.
- Commit workspace changes with `maw exec <name> -- git add -A && maw exec <name> -- git commit -m "..."`.
- **No work is ever lost in maw.** Recovery snapshots are created automatically on every destroy. If a workspace was destroyed and you suspect code is missing, ALWAYS run `maw ws recover` before concluding work was lost. Never reopen a bone or start over without checking recovery first.

### Protocol Quick Reference

Use these commands at protocol transitions to check state and get exact guidance. Each command outputs instructions for the next steps.

| Step | Command | Who | Purpose |
|------|---------|-----|---------|
| Resume | `edict protocol resume --agent $AGENT` | Worker | Detect in-progress work from previous session |
| Start | `edict protocol start <bone-id> --agent $AGENT` | Worker | Verify bone is ready, get start commands |
| Review | `edict protocol review <bone-id> --agent $AGENT` | Worker | Verify work is complete, get review commands |
| Finish | `edict protocol finish <bone-id> --agent $AGENT` | Worker | Verify review approved, get close/cleanup commands |
| Merge | `edict protocol merge <workspace> --agent $AGENT` | Lead | Check preconditions, detect conflicts, get merge steps |
| Cleanup | `edict protocol cleanup --agent $AGENT` | Worker | Check for held resources to release |

All commands support JSON output with `--format json` for parsing. If a command is unavailable or fails (exit code 1), fall back to manual steps documented in [start](.agents/edict/start.md), [review-request](.agents/edict/review-request.md), and [finish](.agents/edict/finish.md).

### Bones Conventions

- Create a bone before starting work. Update state: `open` → `doing` → `done`.
- Post progress comments during work for crash recovery.
- **Run checks before committing**: `just check` (or your project's build/test command). Fix any failures before proceeding.
- After finishing a bone, follow [finish.md](.agents/edict/finish.md). **Workers: do NOT push** — the lead handles merges and pushes.

### Release Instructions

- Make sure 'just check' passes
- Bump the version of all crates
- Regenerate the Cargo.lock
- Add notes to CHANGELOG.md
- If the README.md references the version, update it.
- Commit
- Tag and push: `maw release vX.Y.Z`
- use `gh release create vX.Y.Z --notes "..."`
- Install locally: `maw exec default -- just install`

### Identity

Your agent name is set by the hook or script that launched you. Use `$AGENT` in commands.
For manual sessions, use `<project>-dev` (e.g., `myapp-dev`).

### Claims

When working on a bone, stake claims to prevent conflicts:

```bash
rite claims stake --agent $AGENT "bone://<project>/<id>" -m "<id>"
rite claims stake --agent $AGENT "workspace://<project>/<ws>" -m "<id>"
rite claims release --agent $AGENT --all  # when done
```

### Reviews

Use `@<project>-<role>` mentions to request reviews. The @mention triggers the auto-spawn
hook for the reviewer. Capture the request id and block on the verdict:

```bash
maw exec $WS -- seal reviews request <review-id> --reviewers $PROJECT-security --agent $AGENT
req=$(rite send --agent $AGENT $PROJECT "Review requested: <review-id> @$PROJECT-security" -L review-request --format json | jq -r .id)
bn bone comment add <bone-id> "Review anchor: $req for <review-id>"
rite wait --agent $AGENT --reply-to "$req" -t 300 --format json
```

- Exit 0: confirm the verdict with `maw exec $WS -- seal review <review-id>`, then finish
  or fix in the same turn.
- Exit 1: do NOT request the review again. Post one `-L task-blocked` message naming the
  anchor and stop. The next turn reads review state from seal, not from a new request.
- Exit 2: the anchor is wrong. Re-read it from history. Do NOT request the review again.

**Reviewers**: post the verdict as a reply to the request that woke you
(`--reply-to "$RITE_MESSAGE_ID"`, `-L review-done`). A top-level verdict leaves the author
blocked until timeout.

### Bus Communication

Agents communicate via rite channels. You don't need to be expert on everything — ask the right project.

| Operation | Command |
|-----------|---------|
| Send message | `rite send --agent $AGENT <channel> "message" [-L label]` |
| Reply to a message | `rite send --agent $AGENT <channel> "message" --reply-to <msg-id>` |
| Capture the id you sent | `rite send ... --format json \| jq -r .id` |
| Check inbox | `rite inbox --agent $AGENT --channels <ch> [--mark-read]` |
| Wait for an answer | `rite wait --agent $AGENT --reply-to <msg-id> -t 300 --format json` |
| Wait for any mention | `rite wait --mentions --from <agent> -t 120` |
| Read a thread | `rite history --thread <msg-id>` |
| Browse history | `rite history <channel> -n 20` |
| Search messages | `rite search "query" -c <channel>` |

**Project experts**: Each `<project>-dev` is the expert on their project. When stuck on a companion tool (rite, maw, seal, vessel, bn), post a question to its project channel instead of guessing.

#### Threads

Every message you send answers something or starts something. Anchor the answers.

- `--reply-to <id>` anchors a message under a parent. No `--reply-to` means top-level.
- When a hook spawned you, the message that woke you is `$RITE_MESSAGE_ID`. Answer it:
  `rite send --agent $AGENT "$RITE_CHANNEL" "on it" --reply-to "$RITE_MESSAGE_ID"`.
  A lease batch instead sets `$RITE_BATCH_MESSAGE_IDS`, in chronological order. The
  anchor is the LAST id in that list, not the first.
- An anchor that is not in the store yet gives a warning, not an error. The reply links
  up when the parent syncs in.
- `rite history --thread <id>` reads the whole thread from any message in it, and finds
  the channel itself. A thread reported `complete:false` is a fragment — say so, do not
  present it as the whole conversation.
- Your prompt carries the anchor for the current turn. Use that one. Never reuse the
  anchor from an earlier turn.

#### Ask and Wait

Never post a question and hope. Anchor it, then block on the answer:

```bash
id=$(rite send --agent $AGENT <channel> "<question> @<target>" -L feedback --format json | jq -r .id)
rite wait --agent $AGENT --reply-to "$id" -t 300 --format json
```

| Exit | Meaning | What to do |
|------|---------|------------|
| 0 | Answered | Read `.message.body` from the JSON and act on it |
| 1 | Nobody answered in time | Escalate: post one `-L task-blocked` naming the anchor, record it on the bone, move on. **Never re-send the request.** |
| 2 | Not a ULID, or this store never saw it | Fix the id (`rite history <channel> --from $AGENT -n 1 --format json` returns `last_id`). Do not re-send. Add `--allow-missing-parent` only when the parent is still syncing in from another machine. |

`--reply-to` narrows the wait, it never widens it. `--from`, `-c` and `-L` only subtract
candidate answers. With no `-c` every channel counts, so a reply in a DM satisfies the
wait. A reply that arrived before the wait started still counts. Your own reply never
satisfies your own wait.

### Cross-Project Communication

**Don't suffer in silence.** If a tool confuses you or behaves unexpectedly, post to its project channel.

1. Find the project: `rite history projects -n 50` (the #projects channel has project registry entries)
2. Ask and wait — capture the id, then block on the answer:
   ```bash
   id=$(rite send --agent $AGENT <project> "<question> @<project>-dev" -L feedback --format json | jq -r .id)
   rite wait --agent $AGENT --reply-to "$id" -t 300 --format json
   ```
3. For bugs, create bones in their repo first
4. **On exit 1 (no answer), create a local tracking bone** and move on. Record the anchor
   so the next check reads the thread instead of asking again:
   ```bash
   bn create --title "[tracking] <summary>" --tag tracking --kind task \
     --description "Asked #<project>: <question>. Anchor: <id>. Check: rite history --thread <id>"
   ```

See [cross-channel.md](.agents/edict/cross-channel.md) for the full workflow.

### Communication

Use ASD-STE100 Simplified Technical English for prose. Strict compliance is not the goal. Aim for terse, unambiguous language.

Do not apply STE to code, identifiers, commands, marketing copy, essays, or voice-driven writing.

#### Language

- Limit sentences to 20 words.
- Replace semicolons and contractions.
- Use active voice when the actor is known.
- Use plain verbs. Avoid nominalization, phrasal verbs, and "-ing" main verbs.
- Use one consistent name for each thing.

#### rite messages

Keep a channel message to one or two lines. Lead with the subject of the label. The label and the bone ID already carry the context, so do not add status blocks, numbered steps, or closing actions.

- `[task-claim] Working on <bone-id>: <title>`
- `[review-request] Review requested: <review-id> for <bone-id> @<reviewer>`
- `[task-blocked] Blocked on <thing>: <what unblocks it>`

Anchor an answer with `--reply-to` instead of quoting the message you answer. The anchor
carries the context.

#### Replies to a human

1. Start with a concrete action. Put commands, paths, or snippets first.
2. Number multistep tasks. Give each step one bounded action.
3. Limit lists to five items. Split longer lists by priority.
4. State the current step, what is complete, what remains, and what it waits on.
5. End with the next action, or state what you wait on.

Finish the current issue before you present another. State errors as evidence, cause, and fix.

Do not use preambles, recaps, pleasantries, tangents, emotional error language, or empty hedges.

Never state a time estimate you cannot support. You do not know how long a build, a test run, or another agent takes. Name what you wait on instead.

#### Exceptions

- Explain fully when the user asks for an explanation or a walkthrough.
- Confirm before destructive actions.
- After three failed fixes, state the uncertain assumption and ask one diagnostic question.
- Ask one short question when real ambiguity makes a guess risky.

Before you send, remove announcements, repeated summaries, sidebars, and empty closing questions.

The first line must give the action. The last line must give the result or the next action.

### Session Search (optional)

Use `cass search "error or problem"` to find how similar issues were solved in past sessions.


### Design Guidelines


- [CLI tool design for humans, agents, and machines](.agents/edict/design/cli-conventions.md)



### Workflow Docs


- [Find work from inbox and bones](.agents/edict/triage.md)

- [Claim bone, create workspace, announce](.agents/edict/start.md)

- [Change bone state (open/doing/done)](.agents/edict/update.md)

- [Close bone, merge workspace, release claims](.agents/edict/finish.md)

- [Full triage-work-finish lifecycle](.agents/edict/worker-loop.md)

- [Turn specs/PRDs into actionable bones](.agents/edict/planning.md)

- [Explore unfamiliar code before planning](.agents/edict/scout.md)

- [Create and validate proposals before implementation](.agents/edict/proposal.md)

- [Request a review](.agents/edict/review-request.md)

- [Handle reviewer feedback (fix/address/defer)](.agents/edict/review-response.md)

- [Reviewer agent loop](.agents/edict/review-loop.md)

- [Merge a worker workspace (protocol merge + conflict recovery)](.agents/edict/merge-check.md)

- [Validate toolchain health](.agents/edict/preflight.md)

- [Ask questions, report bugs, and track responses across projects](.agents/edict/cross-channel.md)

- [Report bugs/features to other projects](.agents/edict/report-issue.md)

- [groom](.agents/edict/groom.md)

<!-- edict:managed-end -->
