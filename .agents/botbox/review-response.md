# Review Response

Handle reviewer feedback on a blocked or commented review. For each thread, decide whether to fix, address, or defer.

Your identity is `$AGENT`. All crit and bus commands must include `--agent $AGENT`. Run `bus whoami --agent $AGENT` first if you need to confirm the identity.

## Arguments

- `$AGENT` = agent identity (required)
- `<review-id>` = review to respond to (required)

## When to Use

Run this when:
- `maw exec $WS -- crit inbox --agent $AGENT` shows threads with new comments on your review (check each workspace)
- `bus inbox` contains a `review-done` message indicating your review was blocked
- You previously requested review and are checking back for feedback

**Note:** All crit commands below use `maw exec $WS --` because the review exists in your workspace, not the repo root.

## Steps

1. Read the review and all threads: `maw exec $WS -- crit review <review-id>`
2. For each thread with reviewer feedback, categorize by severity and decide:

   **Fix** (CRITICAL or HIGH severity — must resolve before merge):
   - Make the code change in the workspace
   - Reply on the thread: `maw exec $WS -- crit reply <thread-id> --agent $AGENT "Fixed: <description>"`

   **Address** (reviewer concern is valid but current approach is correct):
   - Reply explaining why: `maw exec $WS -- crit reply <thread-id> --agent $AGENT "Won't fix: <rationale>"`
   - Be specific — reference docs, compiler output, or design intent

   **Defer** (good idea, but out of scope for this change):
   - Create a tracking bead: `maw exec default -- br create --actor $AGENT --owner $AGENT "<title>" --label deferred`
   - Reply: `maw exec $WS -- crit reply <thread-id> --agent $AGENT "Deferred to <bead-id> for follow-up"`

3. After handling all threads:
   a. Verify fixes compile: `maw exec $WS -- cargo check` (or equivalent for the project)
   b. Re-request review: `maw exec $WS -- crit reviews request <review-id> --agent $AGENT --reviewers <reviewer>`
   d. Announce (include workspace name so the reviewer can find the fixed code):
      `bus send --agent $AGENT $BOTBOX_PROJECT "Review feedback addressed: <review-id>, fixes in workspace $WS (ws/$WS/)" -L review-response`

## After LGTM

When the reviewer approves:

1. Verify approval: `maw exec $WS -- crit review <review-id>` — confirm LGTM vote, no blocks
2. Mark review as merged: `maw exec $WS -- crit reviews mark-merged <review-id> --agent $AGENT`
3. Continue with [finish](finish.md) to close the bead and merge the workspace

The actual code merge is handled by `maw ws merge` in the finish step (maw v0.22.0+ rebases onto main and squashes into a single commit) — do not run `jj squash` manually.

## Assumptions

- `BOTBOX_PROJECT` env var contains the project channel name.
- You are the author of the review (the agent that created it or requested it).
- The workspace is still active — fixes are made in the workspace, not the main branch.
