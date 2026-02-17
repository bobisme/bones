# Finish

**Mandatory teardown** after completing work on a bead. Never skip this, even on failure paths.

All steps below are required — they clean up resources, prevent workspace leaks, and ensure the bead ledger stays synchronized. Run `br` commands via `maw exec default --` and `crit` commands via `maw exec $WS --`.

## Arguments

- `$AGENT` = agent identity (required)
- `<bead-id>` = bead to close out (required)

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, stop and instruct the user. Run `bus whoami --agent $AGENT` first to confirm; if it returns a name, use it.
2. Verify you posted at least one progress comment (`maw exec default -- br comments <bead-id>`). If not, add one now: `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Progress: <what was done>"`
3. Add a completion comment to the bead: `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Completed by $AGENT"`
4. Close the bead: `maw exec default -- br close --actor $AGENT <bead-id> --reason="Completed" --suggest-next`
5. **Check risk-based merge requirements** before merging:
   - Check the bead's risk label: `maw exec default -- br show <bead-id>` (look for `risk:low`, `risk:high`, `risk:critical` in labels)
   - **risk:low**: A review may not have been created — that's expected. Proceed directly to merge (step 6).
   - **risk:medium** (default, no label): Standard path — review should already be LGTM before reaching finish.
   - **risk:high**: Verify the security reviewer completed the failure-mode checklist (5 questions answered in review comments) before merge. Check: `maw exec $WS -- crit review <review-id>` and confirm comments address failure modes, edge cases, rollback, monitoring, and validation.
   - **risk:critical**: Verify human approval exists. Check bus history for an approval message referencing the bead/review from a listed approver (`.botbox.json` → `project.criticalApprovers`): `bus history $BOTBOX_PROJECT -n 50`. If found, record the approval message ID in a bead comment: `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Human approval received: bus message <msg-id>"`. If no approval found, do NOT merge — instead post: `bus send --agent $AGENT $BOTBOX_PROJECT "risk:critical bead <bead-id> awaiting human approval before merge" -L review-request` and STOP.
6. **Run checks before merging**: Run the project's check command in your workspace to verify changes compile and pass tests:
   - Check `.botbox.json` → `project.checkCommand` for the configured command
   - Run in the workspace: `maw exec $WS -- <checkCommand>` (e.g., `cargo clippy && cargo test`, `npm test`)
   - If checks fail, fix the issues before proceeding. Do NOT merge broken code.
   - If no `checkCommand` is configured, at minimum verify compilation succeeds.
7. **Snapshot worker files** (critical if workers don't run jj): `maw exec $WS -- jj status` — triggers jj's working-copy snapshot, capturing on-disk edits into the workspace commit. Without this, merge may see an empty commit and all worker changes are lost.
8. **Merge and destroy the workspace**: `maw ws merge $WS --destroy` (where `$WS` is the workspace name from the start step — **never `default`**)
   - The `--destroy` flag is required — it cleans up the workspace after merging
   - **Never merge or destroy the default workspace.** Default is where other workspaces merge into.
   - `maw ws merge` now produces linear history: workspace commits are rebased onto main and squashed into a single commit (as of v0.22.0)
   - Scaffolding commits are automatically abandoned; main bookmark is automatically moved and ready for push
   - If merge fails due to conflicts, do NOT destroy. Instead add a comment: `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Merge conflict — workspace preserved for manual resolution"` and announce the conflict in the project channel.
   - If the command succeeds but the workspace still exists (`maw ws list`), report: `bus send --agent $AGENT $BOTBOX_PROJECT "Tool issue: maw ws merge --destroy did not remove workspace $WS" -L tool-issue`
9. Release all claims held by this agent: `bus claims release --agent $AGENT --all`
10. Sync the beads ledger: `maw exec default -- br sync --flush-only`
11. **If pushMain is enabled** (check `.botbox.json` for `"pushMain": true`), push to GitHub main:
   - `maw push` (maw v0.24.0+ handles bookmark and push automatically)
   - If push fails, announce: `bus send --agent $AGENT $BOTBOX_PROJECT "Push failed for <bead-id>, manual intervention needed" -L tool-issue`
12. Announce completion in the project channel: `bus send --agent $AGENT $BOTBOX_PROJECT "Completed <bead-id>: <bead-title>" -L task-done`

## After Finishing a Batch of Beads

When you've completed multiple beads in a session (or a significant single bead), check if a **release** is warranted:

**Chores only** (docs, refactoring, config changes, version bumps):
- Push to main is sufficient, no release needed

**Features or fixes** (user-visible changes):
- Follow the project's release process:
  1. Run `jj new` to create a release commit, **then** bump version (Cargo.toml, package.json, etc.) using **semantic versioning**. Order matters: jj snapshots the working copy before `jj new`, so edits made before `jj new` go into the previous commit.
  2. Update changelog/release notes if the project has one
  3. Push to main
  4. Tag and push: `maw release vX.Y.Z`
  5. Announce on botbus: `bus send --no-hooks --agent $AGENT $BOTBOX_PROJECT "<project> vX.Y.Z released - <summary>" -L release`

Use **conventional commits** (`feat:`, `fix:`, `docs:`, `chore:`, etc.) for clear history.

A "release" = user-visible changes shipped with a version tag. When in doubt, release — it's better to ship small incremental versions than batch up large changes.

## Merge Conflict Recovery

If `maw ws merge` detects conflicts during the rebase:

### Quick fix for .beads conflicts only

`.beads/issues.jsonl` often conflicts because multiple agents update it concurrently. If your feature changes are clean and only `.beads/` conflicts:

```bash
maw exec $WS -- jj restore --from main .beads/
maw exec $WS -- jj squash
```

Then retry `maw ws merge $WS --destroy`.

**Note**: `.crit/` rarely conflicts with crit v2 (per-review event logs). If it does conflict, investigate rather than auto-restoring — it likely means two agents worked on the same review.

### Full recovery if merge is messy

If jj squash times out or multiple commits are tangled:

```bash
# 1. Find your feature commit (has your actual changes)
maw exec $WS -- jj log -r 'all()' --limit 15

# 2. Abandon the merge mess (empty commits, merge commits)
maw exec $WS -- jj abandon <merge-commit-id> <empty-commit-ids>

# 3. Move to your feature commit
maw exec $WS -- jj edit <feature-commit-id>

# 4. Set main and push
maw push --advance
```

### When to escalate

If recovery takes more than 2-3 attempts, preserve the workspace and escalate:

```bash
maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Merge conflict unresolved. Workspace $WS preserved for manual resolution."
bus send --agent $AGENT $BOTBOX_PROJECT "Merge conflict in $WS for <bead-id>. Manual help needed." -L tool-issue
```

## Assumptions

- `BOTBOX_PROJECT` env var contains the project channel name.
- The workspace was created with `maw ws create --random` during [start](start.md). `$WS` is the workspace name from that step.
