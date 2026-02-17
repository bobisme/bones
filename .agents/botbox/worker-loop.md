# Worker Loop

Full worker lifecycle — triage, start, work, finish, repeat. This is the "colleague" agent: it shows up, finds work, does it, cleans up, and repeats until there is nothing left.

## Before the Loop

If you have a spec, PRD, or high-level request that needs breakdown:

1. **Scout** ([scout](scout.md)) — explore unfamiliar code to understand where changes go
2. **Plan** ([planning](planning.md)) — turn the spec into actionable beads with dependencies

Once beads exist, the worker loop takes over. Skip these steps if beads are already ready.

## Identity

If spawned by `agent-loop.sh`, your identity is provided as `$AGENT` (a random name like `storm-raven`). Otherwise, adopt `<project>-dev` as your name (e.g., `botbox-dev`). Run `bus whoami --agent $AGENT` first to confirm; if it returns a name, use it. It will generate a name if one isn't set.

Your project channel is `$BOTBOX_PROJECT`. All bus commands must include `--agent $AGENT`. All announcements go to `$BOTBOX_PROJECT` with appropriate labels (e.g., `-L task-claim`, `-L review-request`).

**Important:** Run all `br` and `bv` commands via `maw exec default --` (e.g., `maw exec default -- br update ...`). This ensures they always run in the default workspace context. Run `crit` commands via `maw exec $WS --` to target the correct workspace.

## Loop

### 0. Resume check — handle unfinished work (crash recovery)

Before triaging new work, check if you have unfinished work from a previous session that was interrupted. This handles crash recovery naturally.

**First, check for in_progress beads owned by you:**

- `maw exec default -- br list --status in_progress --assignee $AGENT --json` — shows all beads marked in_progress that you own
- If any beads are found, you have unfinished work. For each bead:
  1. Read the bead and its comments: `maw exec default -- br show <bead-id>` and `maw exec default -- br comments <bead-id>`
  2. Check if you still hold claims: `bus claims list --agent $AGENT --mine`
  3. Determine the state:
     - **If "Review requested: <review-id>" comment exists:**
       - Check review status: `maw exec $WS -- crit review <review-id>`
       - **LGTM (approved)**: Follow [merge-check](merge-check.md), then go to step 6 (Finish)
       - **Blocked (changes requested)**: Follow [review-response](review-response.md) to fix issues and re-request review. Then STOP this iteration.
       - **Pending (no new activity)**: STOP this iteration. The reviewer has not responded yet.
     - **If workspace comment exists but no review comment** (work was interrupted mid-implementation):
       - Extract workspace name and path from the "Started in workspace" comment
       - Verify workspace still exists: `maw ws list`
       - If workspace exists: Resume work in that workspace — read the code to see what's done, complete remaining work, then proceed to step 5 (Review request) or step 6 (Finish)
       - If workspace was destroyed: Create a new workspace and resume from scratch (check comments for context on what was attempted)
     - **If no workspace comment** (bead was just marked in_progress before crash):
       - This bead was claimed but work never started
       - Proceed to step 2 (Start) to create a workspace and begin implementation

**Second, check for active claims not covered by in_progress beads:**

- `bus claims list --agent $AGENT --mine` — look for `bead://` claims not already handled above
- This catches edge cases where you hold a claim but the bead status wasn't updated

**If no unfinished work found:** proceed to step 1 (Triage).

### 1. Triage — find and groom work, then pick one small task (always run this, even if you already know what to work on)

- **Mission context**: If a bead has a `mission:bd-xxx` label, you are working as part of a mission. Check the mission bead (`maw exec default -- br show <mission-id>`) for shared outcome, constraints, and sibling context before starting work.
- Check inbox: `bus inbox --agent $AGENT --channels $BOTBOX_PROJECT --mark-read`
- For messages that request work, create beads: `maw exec default -- br create --actor $AGENT --owner $AGENT --title="..." --description="..." --type=task --priority=2`
- For questions or status checks, reply directly: `bus send --agent $AGENT <channel> "<reply>" -L triage-reply`
- Check ready beads: `maw exec default -- br ready`
- If no ready beads and no new beads from inbox, stop with message "No work available."
- **Check blocked beads** for resolved blockers: if a bead was blocked pending information or an upstream fix that has since landed, unblock it with `maw exec default -- br update --actor $AGENT <id> --status=open` and a comment noting why.
- **Groom each ready bead** (`maw exec default -- br show <id>`): ensure it has a clear title, description with acceptance criteria and testing strategy, appropriate priority, and labels. Fix anything missing and comment what you changed.
- Pick one task: `maw exec default -- bv --robot-next` — parse the JSON to get the bead ID.
- If the task is large (epic or multi-step), decompose it:
  1. **Groom the parent** first — add labels, refine acceptance criteria, note any discrepancies between the description and the actual project state (e.g., bead says "use SQLite" but no DB crate in dependencies). Comment your findings on the parent bead.
  2. **Create child beads** with `maw exec default -- br create --actor $AGENT --owner $AGENT` — each one a resumable unit of work. Titles in imperative form. Descriptions must include acceptance criteria (what "done" looks like).
  3. **Set priorities** that reflect execution order — foundation subtasks get higher priority than downstream features, tests get lowest.
  4. **Wire dependencies** with `maw exec default -- br dep add --actor $AGENT <child> <parent>`. Look for parallelism — tasks that share a prerequisite but don't depend on each other should not be chained linearly.
  5. **Comment your decomposition plan** on the parent bead: what you created, why, and any decisions you made (e.g., "Using in-memory storage instead of SQLite because no DB crate available").
  6. **Verify** with `maw exec default -- br dep tree <parent>` — the graph should have at least one point where multiple tasks are unblocked simultaneously.
  7. Run `maw exec default -- bv --robot-next` again. Repeat until you have exactly one small, atomic task.
- If the bead is claimed by another agent (`bus claims check --agent $AGENT "bead://$BOTBOX_PROJECT/<id>"`), skip it and pick the next recommendation. If all are claimed, stop with "No work available."

### 2. Start — claim and set up

- `maw exec default -- br update --actor $AGENT <bead-id> --status=in_progress --owner=$AGENT`
- `bus claims stake --agent $AGENT "bead://$BOTBOX_PROJECT/<bead-id>" -m "<bead-id>"`
- `maw ws create --random` — note the workspace name (e.g., `frost-castle`). Store as `$WS`.
- **All file operations must use the workspace path** `ws/$WS/`. Use absolute paths for Read, Write, and Edit (e.g., `$PROJECT_ROOT/ws/$WS/src/file.rs`). For commands: `maw exec $WS -- <command>`.
- **Do NOT run jj commands.** Workers must never run `jj status`, `jj describe`, `jj diff`, `jj log`, or any jj command. Concurrent jj operations across workspaces cause operation forks that corrupt the repo. The lead handles all jj operations during merge.
- `bus claims stake --agent $AGENT "workspace://$BOTBOX_PROJECT/$WS" -m "<bead-id>"`
- `bus send --agent $AGENT $BOTBOX_PROJECT "Working on <bead-id>: <bead-title>" -L task-claim`

### 3. Work — implement the task

- Read the bead details: `maw exec default -- br show <bead-id>`
- Do the work using the tools available in the workspace.
- **You must add at least one progress comment** during work: `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Progress: ..."`
  - Post when you've made meaningful progress or hit a milestone
  - This is required before you can close the bead — do not skip it
  - Essential for visibility and debugging if something goes wrong

### 4. Stuck check — recognize when you are stuck

You are stuck if: you attempted the same approach twice without progress, you cannot find needed information or files, or a tool command fails repeatedly.

If stuck:
- Add a detailed comment with what you tried and where you got blocked: `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Blocked: ..."`
- Post in the project channel: `bus send --agent $AGENT $BOTBOX_PROJECT "Stuck on <bead-id>: <summary>" -L task-blocked`
- **If a tool behaved unexpectedly**, ask the responsible project for help (see [cross-channel](cross-channel.md)):
  1. Post to their channel: `bus send --agent $AGENT <tool-project> "Getting <error> when running <command>. Context: <details>. @<project>-dev" -L feedback`
  2. Create a local tracking bead: `maw exec default -- br create --actor $AGENT --owner $AGENT --title="[tracking] Asked #<project> about <issue>" --labels tracking --type=task --priority=3`
- `maw exec default -- br update --actor $AGENT <bead-id> --status=blocked`
- Release the bead claim: `bus claims release --agent $AGENT "bead://$BOTBOX_PROJECT/<bead-id>"`
- Move on to triage again (go to step 1).

**Tip**: Before declaring stuck, try `cass search "your error or problem"` to find how similar issues were solved in past sessions.

### 5. Review request — submit work for review

After completing the implementation:

- **Run quality checks before review**: Execute `maw exec $WS -- just check` (or the configured `checkCommand` from `.botbox.json`). Fix any failures before proceeding with review.
- **Check the bead's risk label** to determine review routing:
  - Get bead details: `maw exec default -- br show <bead-id>`
  - Look for `risk:low`, `risk:high`, or `risk:critical` in labels
  - No risk label = `risk:medium` (standard review)

**Risk-based branching:**

**risk:low** — Skip review entirely:
- Do NOT create a crit review
- Add self-review comment: `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Self-review: <brief what I verified>"`
- Proceed directly to step 6 (Finish)

**risk:medium** (default) — Standard review:
- Create a crit review with reviewer assignment: `maw exec $WS -- crit reviews create --agent $AGENT --title "<bead-title>" --description "For <bead-id>: <summary of changes, what was done, why>" --reviewers <reviewer>`
  - `--reviewers` assigns the reviewer in the same command (e.g., `--reviewers myproject-security`)
  - Running via `maw exec $WS --` ensures crit knows which workspace contains the changes
  - Always include the bead ID in the description so reviewers have context
  - Explain what changed and why, not just a summary
- Add a comment to the bead: `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Review requested: <review-id>, workspace: $WS (ws/$WS/)"`
- **If requesting a specialist reviewer** (e.g., security):
  - Announce with @mention to trigger spawn: `bus send --agent $AGENT $BOTBOX_PROJECT "Review requested: <review-id> for <bead-id>, @<reviewer>" -L review-request`
  - The @mention triggers auto-spawn hooks
- **If requesting a general code review**:
  - Spawn a subagent to perform the review
  - Announce: `bus send --agent $AGENT $BOTBOX_PROJECT "Review requested: <review-id> for <bead-id>, spawned subagent for review" -L review-request`
- **STOP this iteration.** Do NOT close the bead, merge the workspace, or release claims. The reviewer will process the review, and you will resume in the next iteration via step 0.

**risk:high** — Security review with failure-mode checklist:
- Create crit review with security reviewer: `maw exec $WS -- crit reviews create --agent $AGENT --title "<bead-title>" --description "For <bead-id>: <summary>. risk:high — failure-mode checklist required. Please answer: 1) What failure modes exist? 2) What edge cases need validation? 3) How can we roll back if this breaks? 4) What monitoring/alerts should we add? 5) What input validation is needed?" --reviewers $BOTBOX_PROJECT-security`
- Add comment to bead: `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Review requested: <review-id>, workspace: $WS (ws/$WS/)"`
- Announce with @mention: `bus send --agent $AGENT $BOTBOX_PROJECT "Review requested: <review-id> for <bead-id>, @$BOTBOX_PROJECT-security" -L review-request`
- **STOP this iteration.**

**risk:critical** — Security review + human approval:
- Create crit review with security reviewer: `maw exec $WS -- crit reviews create --agent $AGENT --title "<bead-title>" --description "For <bead-id>: <summary>. risk:critical — requires human approval before merge." --reviewers $BOTBOX_PROJECT-security`
- Add comment to bead: `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Review requested: <review-id>, workspace: $WS (ws/$WS/)"`
- Post to bus requesting human approval: `bus send --agent $AGENT $BOTBOX_PROJECT "risk:critical review for <bead-id>: requires human approval before merge. Review: <review-id> @<approver>" -L review-request`
  - List of approvers from `.botbox.json` → `project.criticalApprovers`
  - If no `criticalApprovers` configured, use project lead: `@$BOTBOX_PROJECT-lead`
- **STOP this iteration.**

See [review-request](review-request.md) for full details.

### 6. Finish — mandatory teardown (never skip)

If a review was conducted:
- Verify approval: `maw exec $WS -- crit review <review-id>` — confirm LGTM, no blocks
- Mark review as merged: `maw exec $WS -- crit reviews mark-merged <review-id> --agent $AGENT`

Then proceed with teardown:
- `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Completed by $AGENT"`
- `maw exec default -- br close --actor $AGENT <bead-id> --reason="Completed" --suggest-next`
- `maw ws merge $WS --destroy` (if merge conflict, preserve workspace and announce; maw v0.22.0+ produces linear squashed history and auto-moves main)
- `maw push` (if pushMain enabled in `.botbox.json`; maw v0.24.0+ handles bookmark and push)
- `bus claims release --agent $AGENT --all`
- `maw exec default -- br sync --flush-only`
- `bus send --agent $AGENT $BOTBOX_PROJECT "Completed <bead-id>: <bead-title>" -L task-done`

### 7. Release check — lead responsibility

Workers do NOT perform releases. The lead dev agent handles version bumps, tagging, and pushing after merging worker workspaces. Skip this step.

### 8. Repeat

Go back to step 0. The loop ends when triage finds no work and no reviews are pending.

## Key Rules

- **Exactly one small task at a time.** Never work on multiple beads concurrently.
- **Always finish or release before picking new work.** Context must be clear.
- **If claim is denied, back off and pick something else.** Never force or wait.
- **All bus commands use `--agent $AGENT`.**
