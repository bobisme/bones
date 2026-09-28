# Finish

**Mandatory teardown** after completing work on a bone. Never skip this, even on failure paths.

All steps below are required — they clean up resources, prevent workspace leaks, and ensure the bone ledger stays consistent. Run `bn` commands directly at the repo root and `seal` commands via `maw exec $WS --`.

## Arguments

- `$AGENT` = agent identity (required)
- `<bone-id>` = bone to close out (required)

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, stop and instruct the user. Run `rite whoami --agent $AGENT` first to confirm; if it returns a name, use it.
2. Verify you posted at least one progress comment (`bn show <bone-id>`). If not, add one now: `bn bone comment add <bone-id> "Progress: <what was done>"`
3. Add a completion comment to the bone: `bn bone comment add <bone-id> "Completed by $AGENT"`. Do not close the bone yet: `edict protocol finish` refuses a bone that is already done, and its steps close it.
4. **Check risk-based merge requirements** before finishing:
   - Check the bone's risk tag: `bn show <bone-id>` (look for `risk:low`, `risk:high`, `risk:critical` in tags)
   - **risk:low**: A review may not have been created — that's expected.
   - **risk:medium** (default, no tag): Standard path — review should already be LGTM before reaching finish.
   - **risk:high**: Verify the security reviewer completed the failure-mode checklist (5 questions answered in review comments) before merge. Check: `maw exec $WS -- seal review <review-id>` and confirm comments address failure modes, edge cases, rollback, monitoring, and validation.
   - **risk:critical**: Verify human approval exists. Check rite history for an approval message referencing the bone/review from a listed approver (`.edict.toml` → `project.criticalApprovers`): `rite history $EDICT_PROJECT -n 50`. If found, record the approval message ID in a bone comment: `bn bone comment add <bone-id> "Human approval received: rite message <msg-id>"`. If no approval found, do NOT merge — instead post: `rite send --agent $AGENT $EDICT_PROJECT "risk:critical bone <bone-id> awaiting human approval before merge" -L review-request` and STOP. Even with the approval, the worker does not merge a `risk:critical` bone: `edict protocol finish` refuses to. Finish with `--no-merge` in step 6 and tell the lead the approved work is ready; the lead merges it by hand (see [merge-check](merge-check.md)).
5. **Run checks before finishing**: Run the project's check command in your workspace to verify changes compile and pass tests:
   - Check `.edict.toml` → `project.checkCommand` for the configured command
   - Run in the workspace: `maw exec $WS -- <checkCommand>` (e.g., `cargo clippy && cargo test`, `npm test`)
   - If checks fail, fix the issues before proceeding. Do NOT merge broken code. A fix committed after the LGTM needs a fresh LGTM.
   - If no `checkCommand` is configured, at minimum verify compilation succeeds.
6. **Run the finish protocol** and run the steps it prints, in order. Reviewed work merges only through these steps: a hand-run `maw ws merge` skips the review log, the clean check, and the `risk:critical` gate.
   - **Standalone worker**: `edict protocol finish <bone-id> --agent $AGENT`
   - **Dispatched worker** (the lead assigned the bone and workspace): `edict protocol finish <bone-id> --agent $AGENT --no-merge`. The lead's `edict protocol merge` records the review and merges.
   - **risk:low with no review**: add `--force`, since no review exists to gate.
   - The protocol checks the Seal verdict before it prints any merge step. With a review, a standalone worker's steps are:
     - Record the review: a clean check that refuses anything uncommitted outside `.seal/reviews/<review-id>/`, then `seal reviews mark-merged <review-id>` while HEAD is still the approved commit, then a commit of only `.seal/reviews/<review-id>`. That is the one commit allowed after the LGTM. If `mark-merged` exits 1, code was committed after the LGTM: get a fresh LGTM instead of forcing it (see [review-response.md](review-response.md#commit-no-code-after-the-lgtm)).
     - Merge and destroy the workspace, with the clean check repeated in the same command, so nothing lands between the check and the merge.
     - Close the bone, announce, and release your claims.
   - Why the steps run in this order, and the rules around the review log (never `mark-merged` after the merge, never move or delete `.seal/reviews/`): see [merge-check.md](merge-check.md#the-review-log-and-the-clean-check).
   - If the status is not Ready (NeedsReview, Blocked), follow its diagnostics. Do not merge.
   - **Never merge or destroy the default workspace.** Default is where other workspaces merge into.
   - If the merge step reports conflicts, do NOT destroy. Instead add a comment: `bn bone comment add <bone-id> "Merge conflict — workspace preserved for manual resolution"` and announce the conflict in the project channel. See [Merge Conflict Recovery](#merge-conflict-recovery) below — lead with `maw ws resolve`.
   - If the merge succeeds but the workspace still exists (`maw ws list`), report: `rite send --agent $AGENT $EDICT_PROJECT "Tool issue: maw ws merge --destroy did not remove workspace $WS" -L tool-issue`
7. **If `edict protocol finish` is unavailable** (exit 1):
   - **With a review**: do not merge by hand. Confirm the verdict with `maw exec $WS -- seal review <review-id> --format json`, close the bone (`bn done <bone-id> --reason "Completed"`), comment `bn bone comment add <bone-id> "Review <review-id> approved; merge left to the lead"`, and leave the workspace for the lead's `edict protocol merge`.
   - **Without a review**: commit (`maw exec $WS -- git add -A` and `maw exec $WS -- git commit -m "<bone-id>: <summary>"`), then `maw ws merge $WS --into default --destroy --message "feat: <bone-title>"` (a bare merge is only for work with no review; `$WS` is the workspace name from the start step, **never `default`**; use a conventional commit prefix; if the workspace is change-bound, replace `default` with that change id), then `bn done <bone-id> --reason "Completed"`.
   - Release all claims held by this agent: `rite claims release --agent $AGENT --all`
   - Announce completion in the project channel: `rite send --agent $AGENT $EDICT_PROJECT "Completed <bone-id>: <bone-title>" -L task-done`
8. **Do not push.** Workers never push, even when `pushMain` is enabled: the lead merges and pushes.

## After Finishing a Batch of Bones

This section is for the lead, who merges and pushes. Workers skip it. When the lead has merged multiple bones in a session (or a significant single bone), check if a **release** is warranted:

**Chores only** (docs, refactoring, config changes, version bumps):
- The lead's push to main is sufficient, no release needed

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

Then retry the merge step exactly as `edict protocol finish` printed it, with its clean check. Without a review, retry the step 7 bare merge.

**If the review is already recorded** (the record steps ran), restore only `.bones/` and retry directly. The review covers everything else, so a restore of `.agents/`, `.claude/`, or any other path changes what merges after the approval. Commit that change, create a fresh review for the bone, and merge after its LGTM.

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

# 3. Retry the merge step exactly as `edict protocol finish` printed it (it carries the clean check).
#    Do not rerun `edict protocol finish` after the record steps ran: a recorded review no longer
#    passes its gate. Without a review, retry the step 7 bare merge.
```

Or resolve inline at merge time: add `--resolve-all=$WS` (or `--resolve <cf-id>=<name>` per conflict)
to that merge step.

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

If the merge is refused (a stale source, recorded conflicts, or conflict placeholders in HEAD),
see [Merge gates](merge-check.md#merge-gates) for what each refusal means and which one
`--force` can bypass.

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
