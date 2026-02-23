# Worker Loop

Full worker lifecycle — triage, start, work, finish, repeat. This is the "colleague" agent: it shows up, finds work, does it, cleans up, and repeats until there is nothing left.

## Before the Loop

If you have a spec, PRD, or high-level request that needs breakdown:

1. **Scout** ([scout](scout.md)) — explore unfamiliar code to understand where changes go
2. **Plan** ([planning](planning.md)) — turn the spec into actionable bones with dependencies

Once bones exist, the worker loop takes over. Skip these steps if bones are already ready.

## Identity

If spawned by `agent-loop.sh`, your identity is provided as `$AGENT` (a random name like `storm-raven`). Otherwise, adopt `<project>-dev` as your name (e.g., `botbox-dev`). Run `bus whoami --agent $AGENT` first to confirm; if it returns a name, use it. It will generate a name if one isn't set.

Your project channel is `$BOTBOX_PROJECT`. All bus commands must include `--agent $AGENT`. All announcements go to `$BOTBOX_PROJECT` with appropriate labels (e.g., `-L task-claim`, `-L review-request`).

**Important:** Run all `bn` commands via `maw exec default --` (e.g., `maw exec default -- bn do ...`). This ensures they always run in the default workspace context. Run `crit` commands via `maw exec $WS --` to target the correct workspace.

## Loop

### 0. Resume check — handle unfinished work (crash recovery)

Before triaging new work, check if you have unfinished work from a previous session that was interrupted. This handles crash recovery naturally.

**First, check for doing bones owned by you:**

- `maw exec default -- bn list --state doing --format json` — shows all bones in doing state
- If any bones are found that you own, you have unfinished work. For each bone:
  1. Read the bone and its comments: `maw exec default -- bn show <bone-id>`
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
     - **If no workspace comment** (bone was just marked doing before crash):
       - This bone was claimed but work never started
       - Proceed to step 2 (Start) to create a workspace and begin implementation

**Second, check for active claims not covered by doing bones:**

- `bus claims list --agent $AGENT --mine` — look for `bone://` claims not already handled above
- This catches edge cases where you hold a claim but the bone state wasn't updated

**If no unfinished work found:** proceed to step 1 (Triage).

### 1. Triage — find and groom work, then pick one small task (always run this, even if you already know what to work on)

- **Mission context**: If a bone has a `mission:bd-xxx` label, you are working as part of a mission. Check the mission bone (`maw exec default -- bn show <mission-id>`) for shared outcome, constraints, and sibling context before starting work.
- Check inbox: `bus inbox --agent $AGENT --channels $BOTBOX_PROJECT --mark-read`
- For messages that request work, create bones: `maw exec default -- bn create --title "..." --description "..." --kind task`
- For questions or status checks, reply directly: `bus send --agent $AGENT <channel> "<reply>" -L triage-reply`
- Check next work: `maw exec default -- bn next`
- If no work available and no new bones from inbox, stop with message "No work available."
- **Groom each ready bone** (`maw exec default -- bn show <id>`): ensure it has a clear title, description with acceptance criteria and testing strategy, appropriate urgency, and tags. Fix anything missing and comment what you changed.
- Pick one task: `maw exec default -- bn next` — parse the output to get the bone ID.
- If the task is large (epic or multi-step), decompose it:
  1. **Groom the parent** first — add tags, refine acceptance criteria, note any discrepancies between the description and the actual project state. Comment your findings on the parent bone.
  2. **Create child bones** with `maw exec default -- bn create --title "..." --kind task` — each one a resumable unit of work. Titles in imperative form. Descriptions must include acceptance criteria (what "done" looks like).
  3. **Set urgency** that reflects execution order — foundation subtasks get higher urgency than downstream features, tests get lowest.
  4. **Wire dependencies** with `maw exec default -- bn triage dep add <earlier> --blocks <later>`. Look for parallelism — tasks that share a prerequisite but don't depend on each other should not be chained linearly.
  5. **Comment your decomposition plan** on the parent bone: what you created, why, and any decisions you made.
  6. **Verify** with `maw exec default -- bn triage graph` — the graph should have at least one point where multiple tasks are unblocked simultaneously.
  7. Run `maw exec default -- bn next` again. Repeat until you have exactly one small, atomic task.
- If the bone is claimed by another agent (`bus claims check --agent $AGENT "bone://$BOTBOX_PROJECT/<id>"`), skip it and pick the next recommendation. If all are claimed, stop with "No work available."

### 2. Start — claim and set up

- `maw exec default -- bn do <bone-id>`
- `bus claims stake --agent $AGENT "bone://$BOTBOX_PROJECT/<bone-id>" -m "<bone-id>"`
- `maw ws create --random` — note the workspace name (e.g., `frost-castle`). Store as `$WS`.
- **All file operations must use the workspace path** `ws/$WS/`. Use absolute paths for Read, Write, and Edit (e.g., `$PROJECT_ROOT/ws/$WS/src/file.rs`). For commands: `maw exec $WS -- <command>`.
- **No `jj`**: this workflow is Git + maw. Keep workspace operations in `maw` and run `git` only via `maw exec $WS -- ...`.
- `bus claims stake --agent $AGENT "workspace://$BOTBOX_PROJECT/$WS" -m "<bone-id>"`
- `bus send --agent $AGENT $BOTBOX_PROJECT "Working on <bone-id>: <bone-title>" -L task-claim`

### 3. Work — implement the task

- Read the bone details: `maw exec default -- bn show <bone-id>`
- Do the work using the tools available in the workspace.
- **You must add at least one progress comment** during work: `maw exec default -- bn bone comment add <bone-id> "Progress: ..."`
  - Post when you've made meaningful progress or hit a milestone
  - This is required before you can close the bone — do not skip it
  - Essential for visibility and debugging if something goes wrong

### 4. Stuck check — recognize when you are stuck

You are stuck if: you attempted the same approach twice without progress, you cannot find needed information or files, or a tool command fails repeatedly.

If stuck:
- Add a detailed comment with what you tried and where you got blocked: `maw exec default -- bn bone comment add <bone-id> "Blocked: ..."`
- Post in the project channel: `bus send --agent $AGENT $BOTBOX_PROJECT "Stuck on <bone-id>: <summary>" -L task-blocked`
- **If a tool behaved unexpectedly**, ask the responsible project for help (see [cross-channel](cross-channel.md)):
  1. Post to their channel: `bus send --agent $AGENT <tool-project> "Getting <error> when running <command>. Context: <details>. @<project>-dev" -L feedback`
  2. Create a local tracking bone: `maw exec default -- bn create --title "[tracking] Asked #<project> about <issue>" --tag tracking --kind task`
- Move on to triage again (go to step 1).

**Tip**: Before declaring stuck, try `cass search "your error or problem"` to find how similar issues were solved in past sessions.

### 5. Review request — submit work for review

After completing the implementation:

- **Run quality checks before review**: Execute `maw exec $WS -- just check` (or the configured `checkCommand` from `.botbox.json`). Fix any failures before proceeding with review.
- Commit your workspace changes:
  - `maw exec $WS -- git add -A`
  - `maw exec $WS -- git commit -m "<bone-id>: <summary>"`
- **Check the bone's risk label** to determine review routing:
  - Get bone details: `maw exec default -- bn show <bone-id>`
  - Look for `risk:low`, `risk:high`, or `risk:critical` in tags
  - No risk tag = `risk:medium` (standard review)

**Risk-based branching:**

**risk:low** — Skip review entirely:
- Do NOT create a crit review
- Add self-review comment: `maw exec default -- bn bone comment add <bone-id> "Self-review: <brief what I verified>"`
- Proceed directly to step 6 (Finish)

**risk:medium** (default) — Standard review:
- Create a crit review with reviewer assignment: `maw exec $WS -- crit reviews create --agent $AGENT --title "<bone-title>" --description "For <bone-id>: <summary of changes, what was done, why>" --reviewers <reviewer>`
  - `--reviewers` assigns the reviewer in the same command (e.g., `--reviewers myproject-security`)
  - Running via `maw exec $WS --` ensures crit knows which workspace contains the changes
  - Always include the bone ID in the description so reviewers have context
  - Explain what changed and why, not just a summary
- Add a comment to the bone: `maw exec default -- bn bone comment add <bone-id> "Review requested: <review-id>, workspace: $WS (ws/$WS/)"`
- **If requesting a specialist reviewer** (e.g., security):
  - Announce with @mention to trigger spawn: `bus send --agent $AGENT $BOTBOX_PROJECT "Review requested: <review-id> for <bone-id>, @<reviewer>" -L review-request`
  - The @mention triggers auto-spawn hooks
- **If requesting a general code review**:
  - Spawn a subagent to perform the review
  - Announce: `bus send --agent $AGENT $BOTBOX_PROJECT "Review requested: <review-id> for <bone-id>, spawned subagent for review" -L review-request`
- **STOP this iteration.** Do NOT close the bone, merge the workspace, or release claims. The reviewer will process the review, and you will resume in the next iteration via step 0.

**risk:high** — Security review with failure-mode checklist:
- Create crit review with security reviewer: `maw exec $WS -- crit reviews create --agent $AGENT --title "<bone-title>" --description "For <bone-id>: <summary>. risk:high — failure-mode checklist required. Please answer: 1) What failure modes exist? 2) What edge cases need validation? 3) How can we roll back if this breaks? 4) What monitoring/alerts should we add? 5) What input validation is needed?" --reviewers $BOTBOX_PROJECT-security`
- Add comment to bone: `maw exec default -- bn bone comment add <bone-id> "Review requested: <review-id>, workspace: $WS (ws/$WS/)"`
- Announce with @mention: `bus send --agent $AGENT $BOTBOX_PROJECT "Review requested: <review-id> for <bone-id>, @$BOTBOX_PROJECT-security" -L review-request`
- **STOP this iteration.**

**risk:critical** — Security review + human approval:
- Create crit review with security reviewer: `maw exec $WS -- crit reviews create --agent $AGENT --title "<bone-title>" --description "For <bone-id>: <summary>. risk:critical — requires human approval before merge." --reviewers $BOTBOX_PROJECT-security`
- Add comment to bone: `maw exec default -- bn bone comment add <bone-id> "Review requested: <review-id>, workspace: $WS (ws/$WS/)"`
- Post to bus requesting human approval: `bus send --agent $AGENT $BOTBOX_PROJECT "risk:critical review for <bone-id>: requires human approval before merge. Review: <review-id> @<approver>" -L review-request`
  - List of approvers from `.botbox.json` → `project.criticalApprovers`
  - If no `criticalApprovers` configured, use project lead: `@$BOTBOX_PROJECT-lead`
- **STOP this iteration.**

See [review-request](review-request.md) for full details.

### 6. Finish — mandatory teardown (never skip)

If a review was conducted:
- Verify approval: `maw exec $WS -- crit review <review-id>` — confirm LGTM, no blocks
- Mark review as merged: `maw exec $WS -- crit reviews mark-merged <review-id> --agent $AGENT`

Then proceed with teardown:
- `maw exec default -- bn bone comment add <bone-id> "Completed by $AGENT"`
- `maw exec default -- bn done <bone-id> --reason "Completed"`
- `maw ws merge $WS --destroy` (if merge conflict, preserve workspace and announce; maw v0.22.0+ produces linear squashed history and auto-moves main)
- `maw push` (if pushMain enabled in `.botbox.json`; maw v0.24.0+ handles bookmark and push)
- `bus claims release --agent $AGENT --all`
- `bus send --agent $AGENT $BOTBOX_PROJECT "Completed <bone-id>: <bone-title>" -L task-done`

### 7. Release check — lead responsibility

Workers do NOT perform releases. The lead dev agent handles version bumps, tagging, and pushing after merging worker workspaces. Skip this step.

### 8. Repeat

Go back to step 0. The loop ends when triage finds no work and no reviews are pending.

## Key Rules

- **Exactly one small task at a time.** Never work on multiple bones concurrently.
- **Always finish or release before picking new work.** Context must be clear.
- **If claim is denied, back off and pick something else.** Never force or wait.
- **All bus commands use `--agent $AGENT`.**
