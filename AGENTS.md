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
  bones-verified/ Verus-verified merge kernels used by bones-core (`just verus`)
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

Tools teach their own commands: bones `bn tldr` · maw `maw tldr` (`maw --help`) · seal `seal --help` · rite `rite tldr` · edict `edict protocol --help`. Some tool docs are out of date: where a tool's quick start differs from the Rules below, the Rules win. Identity: `$AGENT`, set by the launcher; manual sessions use `<project>-dev`.

Layout: the repo root is the trunk (the `default` workspace); agent workspaces live in `.maw/workspaces/<name>/`.

### Rules

- **Track all work in a bone.** Create it before you start (`bn create`), move it `open` → `doing` → `done`, and post progress comments for crash recovery. See [update.md](.agents/edict/update.md).
- **Edit only in a maw workspace** named after the bone: `maw ws create <bone-id> --from main`, then work in `.maw/workspaces/<bone-id>/`. Never edit the trunk at the repo root directly, and never create git branches: maw workspaces replace them. See [start.md](.agents/edict/start.md).
- **Run each tool in its place.** Run commands in a workspace with `maw exec <ws> -- <cmd>`. Run `bn` directly at the repo root. Run seal through `maw exec <ws> -- seal`.
- **Never merge or destroy `default`.** It is the merge target.
- **Run the edict protocol at each transition:** `edict protocol resume|start|review|finish|merge|cleanup … --agent $AGENT`, then run the steps it prints, in order. If it exits 1, follow the matching workflow doc below.
- **Reviewed work merges only through `edict protocol merge <ws> --message "feat: …"`.** A bare `maw ws merge` skips the review log, the clean check and the `risk:critical` gate; use it only for work with no review. See [merge-check.md](.agents/edict/merge-check.md#the-review-log-and-the-clean-check).
- **A conflicted workspace is a normal state, not a failure:** `maw ws resolve <ws> --list`. See [merge-check.md](.agents/edict/merge-check.md#conflict-recovery).
- **Run `maw ws recover` before you conclude work is lost** or start a bone over. Destroyed workspaces keep snapshots. See [worker-loop.md](.agents/edict/worker-loop.md).
- **Answer `$RITE_MESSAGE_ID` with `--reply-to`.** Never reuse the anchor from an earlier turn. See [cross-channel.md](.agents/edict/cross-channel.md#threads).
- **Ask, then wait on the anchor:** capture the id you sent (`--format json`), then `rite wait --reply-to <id> -t 300`. On exit 1, post one `-L task-blocked` and move on; never re-send. Stuck on a companion tool? Ask its project channel this way. See [cross-channel.md](.agents/edict/cross-channel.md#ask-and-wait).
- **Rite messages are one labelled line that leads with the bone id**, e.g. `-L task-blocked "<bone-id>: blocked on <thing>, needs <what unblocks it>"`. No status blocks or recaps. See [cross-channel.md](.agents/edict/cross-channel.md#message-shape).
- **Run the project's check command before committing**, and fix failures first. Workers do not push; the lead merges and pushes. See [finish.md](.agents/edict/finish.md).
- **Confirm before destructive actions** (deleting data, force-pushing, discarding unmerged work): ask the human first.

### Release

- Make sure 'just check' passes
- Bump the version of all crates
- Regenerate the Cargo.lock
- Add notes to CHANGELOG.md
- If the README.md references the version, update it.
- Commit
- Tag and push: `maw release vX.Y.Z`
- use `gh release create vX.Y.Z --notes "..."`
- Install locally: `maw exec default -- just install`

### Design Guidelines

- [CLI tool design for humans, agents, and machines](.agents/edict/design/cli-conventions.md)

### Workflow Docs

- [worker-loop.md](.agents/edict/worker-loop.md): Full worker cycle: resume, triage, start, work, review, finish
- [triage.md](.agents/edict/triage.md): Find one actionable bone and groom along the way
- [start.md](.agents/edict/start.md): Claim a bone, create its workspace, announce
- [update.md](.agents/edict/update.md): Change a bone's state and announce it
- [review-request.md](.agents/edict/review-request.md): Request a review: commit first, review range, retarget before re-request
- [review-response.md](.agents/edict/review-response.md): Handle reviewer feedback; no code after the LGTM
- [security-review.md](.agents/edict/security-review.md): Launch one dedicated security review; who sends what
- [finish.md](.agents/edict/finish.md): Close the bone, merge or hand off, release claims; conflict recovery
- [merge-check.md](.agents/edict/merge-check.md): Merge a workspace: review log, clean check, merge gates, conflicts
- [cross-channel.md](.agents/edict/cross-channel.md): Rite threads, ask-and-wait, message shape, cross-project asks
- [report-issue.md](.agents/edict/report-issue.md): Superseded by cross-channel.md
- [planning.md](.agents/edict/planning.md): Turn a spec or PRD into actionable bones
- [scout.md](.agents/edict/scout.md): Explore unfamiliar code before planning
- [proposal.md](.agents/edict/proposal.md): Propose and validate a significant change before building it
- [groom.md](.agents/edict/groom.md): Groom ready bones to improve backlog quality
- [mission.md](.agents/edict/mission.md): Missions: split a parent bone across parallel workers
- [coordination.md](.agents/edict/coordination.md): Coordinate with sibling workers inside a mission
- [preflight.md](.agents/edict/preflight.md): Validate toolchain health before multi-agent work
<!-- edict:managed-end -->
