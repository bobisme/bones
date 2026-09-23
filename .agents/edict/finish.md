# Finish

**Mandatory teardown** after completing work on a bone. Never skip this, even on failure paths.

All steps below are required — they clean up resources, prevent workspace leaks, and ensure the bone ledger stays consistent. Run `bn` commands directly at the repo root and `seal` commands via `maw exec $WS --`.

## Arguments

- `$AGENT` = agent identity (required)
- `<bone-id>` = bone to close out (required)

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, stop and instruct the user. Run `rite whoami --agent $AGENT` first to confirm; if it returns a name, use it.
2. Verify you posted at least one progress comment (`bn show <bone-id>`). If not, add one now: `bn bone comment add <bone-id> "Progress: <what was done>"`
3. Add a completion comment to the bone: `bn bone comment add <bone-id> "Completed by $AGENT"`
4. Close the bone: `bn done <bone-id> --reason "Completed"`
5. **Check risk-based merge requirements** before merging:
   - Check the bone's risk tag: `bn show <bone-id>` (look for `risk:low`, `risk:high`, `risk:critical` in tags)
   - **risk:low**: A review may not have been created — that's expected. Proceed directly to merge (step 6).
   - **risk:medium** (default, no tag): Standard path — review should already be LGTM before reaching finish.
   - **risk:high**: Verify the security reviewer completed the failure-mode checklist (5 questions answered in review comments) before merge. Check: `maw exec $WS -- seal review <review-id>` and confirm comments address failure modes, edge cases, rollback, monitoring, and validation.
   - **risk:critical**: Verify human approval exists. Check rite history for an approval message referencing the bone/review from a listed approver (`.edict.toml` → `project.criticalApprovers`): `rite history $EDICT_PROJECT -n 50`. If found, record the approval message ID in a bone comment: `bn bone comment add <bone-id> "Human approval received: rite message <msg-id>"`. If no approval found, do NOT merge — instead post: `rite send --agent $AGENT $EDICT_PROJECT "risk:critical bone <bone-id> awaiting human approval before merge" -L review-request` and STOP.
6. **Run checks before merging**: Run the project's check command in your workspace to verify changes compile and pass tests:
   - Check `.edict.toml` → `project.checkCommand` for the configured command
   - Run in the workspace: `maw exec $WS -- <checkCommand>` (e.g., `cargo clippy && cargo test`, `npm test`)
   - If checks fail, fix the issues before proceeding. Do NOT merge broken code. A fix committed after the LGTM needs a fresh LGTM.
   - If no `checkCommand` is configured, at minimum verify compilation succeeds.
7. **Record the review** (skip when no review exists). Seal keeps the review as an event log under `.seal/reviews/<review-id>/` in this workspace. The merge destroys the workspace, so commit the log now:
   - `maw exec $WS -- git status --porcelain --untracked-files=all` must list nothing outside `.seal/reviews/<review-id>/`. `maw ws merge` also merges uncommitted additions and deletions, but the approval covers only a commit. Commit anything else and get a fresh LGTM.
   - `maw exec $WS -- seal reviews mark-merged <review-id> --agent $AGENT` — run it while HEAD is still the approved commit. It exits 1 when code was committed after the LGTM: get a fresh LGTM instead of forcing it (see [review-response.md](review-response.md)).
   - `maw exec $WS -- git add .seal/reviews/<review-id>`
   - `maw exec $WS -- git commit -m "chore: seal review <review-id>" -- .seal/reviews/<review-id>`
   - This commit holds only the review log. It is the one commit allowed after the LGTM. Never run `mark-merged` after the merge: the workspace no longer exists.
   - Never move or delete `.seal/reviews/` to satisfy `maw ws sync` or `maw ws clean`. Commit it as above instead.
8. **Merge and destroy the workspace**. With a review, the merge command itself carries the clean check, so nothing lands between the check and the merge, and a failed `git status` stops it: `{ out=$(maw exec $WS -- git status --porcelain --untracked-files=all -- . ':(exclude).seal/reviews/<review-id>') && test -z "$out" || { echo "unreviewed changes: stop" >&2; false; }; } && maw ws merge $WS --into default --destroy --message "feat: <bone-title>"`. Without a review: `maw ws merge $WS --into default --destroy --message "feat: <bone-title>"` (where `$WS` is the workspace name from the start step — **never `default`**; use a conventional commit prefix matching your change type: `feat:`, `fix:`, `chore:`, etc.; if the workspace is change-bound, replace `default` with that change id)
   - The `--destroy` flag is required — it cleans up the workspace after merging
   - **Never merge or destroy the default workspace.** Default is where other workspaces merge into.
   - Merge creates a merge (or adoption) commit on the configured branch — it does not squash history — and the result is ready for `maw push`
   - If merge fails due to conflicts, do NOT destroy. Instead add a comment: `bn bone comment add <bone-id> "Merge conflict — workspace preserved for manual resolution"` and announce the conflict in the project channel. See [Merge Conflict Recovery](#merge-conflict-recovery) below — lead with `maw ws resolve`.
   - If the command succeeds but the workspace still exists (`maw ws list`), report: `rite send --agent $AGENT $EDICT_PROJECT "Tool issue: maw ws merge --destroy did not remove workspace $WS" -L tool-issue`
9. Release all claims held by this agent: `rite claims release --agent $AGENT --all`
10. **If pushMain is enabled** (check `.edict.toml` for `"pushMain": true`), push to GitHub main:
   - `maw push` (pushes the configured branch; use `maw push --advance` after direct commits to default)
   - If push fails, announce: `rite send --agent $AGENT $EDICT_PROJECT "Push failed for <bone-id>, manual intervention needed" -L tool-issue`
11. Announce completion in the project channel: `rite send --agent $AGENT $EDICT_PROJECT "Completed <bone-id>: <bone-title>" -L task-done`

## After Finishing a Batch of Bones

When you've completed multiple bones in a session (or a significant single bone), check if a **release** is warranted:

**Chores only** (docs, refactoring, config changes, version bumps):
- Push to main is sufficient, no release needed

**Features or fixes** (user-visible changes):
- Follow the project's release process:
  1. Bump version (Cargo.toml, package.json, etc.) using **semantic versioning**.
  2. Update changelog/release notes if the project has one.
  3. Commit the release prep in default workspace: `git add -A && git commit -m "chore: release vX.Y.Z"`
  4. Run release: `maw release vX.Y.Z`
  5. Announce on rite: `rite send --no-hooks --agent $AGENT $EDICT_PROJECT "<project> vX.Y.Z released - <summary>" -L release`

Use **conventional commits** (`feat:`, `fix:`, `docs:`, `chore:`, etc.) for clear history.

A "release" = user-visible changes shipped with a version tag. When in doubt, release — it's better to ship small incremental versions than batch up large changes.

## Merge Conflict Recovery

Conflicts are data, not failure — `maw ws merge` records conflicts as structured state rather
than aborting. If `maw ws merge` reports conflicts, the workspace is preserved (not destroyed).

### Quick fix for ledger/docs conflicts only

`.bones/` often conflicts because multiple agents update it concurrently. If your feature changes are clean and only ledger/docs paths conflict (`.bones/`, `.agents/`, `.claude/`):

```bash
maw exec $WS -- git restore --source refs/heads/main -- .bones/ .agents/ .claude/
```

Then retry the step 8 merge command, with its clean check when a review exists.

**If the review is already recorded** (step 7 ran), restore only `.bones/` and retry directly. The review covers everything else, so a restore of `.agents/`, `.claude/`, or any other path changes what merges after the approval. Commit that change, create a fresh review for the bone, and merge after its LGTM.

### Full recovery when conflicts are messy

Lead with `maw ws resolve` instead of hand-editing markers:

```bash
# 1. Inspect detailed conflicts
maw ws conflicts $WS --format json
maw ws resolve $WS --list

# 2. Pick a resolution (whole-workspace or per-file)
maw ws resolve $WS --keep epoch          # keep the rebased-onto side
maw ws resolve $WS --keep $WS            # keep your workspace's changes
maw ws resolve $WS --keep <path>=<name>  # resolve one file

# 3. Retry merge (the step 8 command; drop the { ... } && check only when no review exists)
{ out=$(maw exec $WS -- git status --porcelain --untracked-files=all -- . ':(exclude).seal/reviews/<review-id>') && test -z "$out" || { echo "unreviewed changes: stop" >&2; false; }; } && maw ws merge $WS --into default --destroy --message "feat: <bone-title>"
```

Or resolve inline at merge time with `maw ws merge $WS --into default --resolve-all=$WS` (or
`--resolve <cf-id>=<name>` per conflict).

Manual fallback (edit markers by hand, then stage):

```bash
maw exec $WS -- git status
maw exec $WS -- git add <resolved-file>
```

If a sync/merge retry is refused because of untracked scratch files, clear them first
(snapshot-first, always recoverable):

```bash
maw ws clean $WS --dry-run   # preview what would be removed
maw ws clean $WS             # remove untracked files (recovery snapshot pinned first)
```

The one hard gate: merge refuses a *source* workspace whose HEAD still contains unresolved
textual conflict markers from a prior rebase. Bypass with `--force` only when the "markers" are
legitimate content (e.g. test fixtures).

### If the merge attempt itself is stuck

A killed, OOM'd, panicked, or Ctrl-C'd `maw ws merge` can leave an orphaned merge-state (distinct
from a normal recorded conflict). Clear it with:

```bash
maw ws merge --abort
```

### To undo a COMPLETED merge

If a merge already succeeded but produced the wrong result, use the repo-level undo — **not**
`maw ws undo`, which discards the workspace's entire delta back to its base epoch (including the
work you were trying to merge):

```bash
maw ops log    # find the op id if not the most recent
maw undo       # undo the last completed merge (epoch+branch rewind, sources restored)
```

### When to escalate

If recovery takes more than 2-3 attempts, preserve the workspace and escalate:

```bash
bn bone comment add <bone-id> "Merge conflict unresolved. Workspace $WS preserved for manual resolution."
rite send --agent $AGENT $EDICT_PROJECT "Merge conflict in $WS for <bone-id>. Manual help needed." -L tool-issue
```

If the workspace was accidentally removed, recreate it with `maw ws recover $WS --to <new-name>`.

## Assumptions

- `EDICT_PROJECT` env var contains the project channel name.
- The workspace was created with `maw ws create <bone-id> --from main --description "..."` during [start](start.md). `$WS` is the bone-id used as workspace name.
