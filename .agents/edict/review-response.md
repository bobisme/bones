# Review Response

Handle reviewer feedback on a blocked or commented review. For each thread, decide whether to fix, address, or defer.

Your identity is `$AGENT`. All seal and rite commands must include `--agent $AGENT`. Run `rite whoami --agent $AGENT` first if you need to confirm the identity.

## Arguments

- `$AGENT` = agent identity (required)
- `<review-id>` = review to respond to (required)

## When to Use

Run this when:
- `maw exec $WS -- seal inbox --agent $AGENT` shows threads with new comments on your review (check each workspace)
- `rite inbox` contains a `review-done` message indicating your review was blocked
- You previously requested review and are checking back for feedback

**Note:** All seal commands below use `maw exec $WS --` because the review exists in your workspace, not the repo root.

## Steps

1. Read the review and all threads: `maw exec $WS -- seal review <review-id>`
2. For each thread with reviewer feedback, categorize by severity and decide:

   **Fix** (CRITICAL or HIGH severity — must resolve before merge):
   - Make the code change in the workspace
   - Reply on the thread: `maw exec $WS -- seal reply <thread-id> --agent $AGENT "Fixed: <description>"`

   **Address** (reviewer concern is valid but current approach is correct):
   - Reply explaining why: `maw exec $WS -- seal reply <thread-id> --agent $AGENT "Won't fix: <rationale>"`
   - Be specific — reference docs, compiler output, or design intent

   **Defer** (good idea, but out of scope for this change):
   - Create a tracking bone: `maw exec default -- bn create --title "<title>" --tag deferred --kind task`
   - Reply: `maw exec $WS -- seal reply <thread-id> --agent $AGENT "Deferred to <bone-id> for follow-up"`

3. After handling all threads:
   a. Verify fixes compile: `maw exec $WS -- cargo check` (or equivalent for the project)
   b. Commit the fixes in your workspace:
      - `maw exec $WS -- git add -A`
      - `maw exec $WS -- git commit -m "fix: address review feedback on <review-id>"`
   c. Re-request review: `maw exec $WS -- seal reviews request <review-id> --agent $AGENT --reviewers <reviewer>`
   d. Announce (include workspace name so the reviewer can find the fixed code):
       `rite send --agent $AGENT $EDICT_PROJECT "Review feedback addressed: <review-id>, fixes in workspace $WS (ws/$WS/)" -L review-response`

## After LGTM

When the reviewer approves:

1. Verify approval: `maw exec $WS -- seal review <review-id>` — confirm LGTM vote, no blocks
2. Mark review as merged: `maw exec $WS -- seal reviews mark-merged <review-id> --agent $AGENT`
3. Continue with [finish](finish.md) to close the bone and merge the workspace

The actual code merge is handled by `maw ws merge` in the finish step — do not run manual squash commands.

## Assumptions

- `EDICT_PROJECT` env var contains the project channel name.
- You are the author of the review (the agent that created it or requested it).
- The workspace is still active — fixes are made in the workspace, not the main branch.
