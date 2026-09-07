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
   - Create a tracking bone: `bn create --title "<title>" --tag deferred --kind task`
   - Reply: `maw exec $WS -- seal reply <thread-id> --agent $AGENT "Deferred to <bone-id> for follow-up"`

3. After handling all threads:
   a. Verify fixes compile: `maw exec $WS -- cargo check` (or equivalent for the project)
   b. Commit the fixes in your workspace:
      - `maw exec $WS -- git add -A`
      - `maw exec $WS -- git commit -m "fix: address review feedback on <review-id>"`
   c. Re-request review: `maw exec $WS -- seal reviews request <review-id> --agent $AGENT --reviewers <reviewer>`
   d. Create a NEW anchor and launch one exact Daybreak re-review. Do not use an
      @mention or wait for a hook:
      ```bash
      req=$(rite send --agent $AGENT $EDICT_PROJECT \
        "Dedicated security re-review requested: <review-id>, fixes in workspace $WS (.maw/workspaces/$WS/)" \
        -L review-response --format json | jq -r .id)

      ```

      Set `review_id=<review-id>`, `ws=$WS`, `request_anchor=$req`, and
      `kind=review-response`, then follow [security-review](security-review.md)'s
      **Launch contract**. Agentbus failure or a missing Seal vote blocks the work;
      do not announce again or substitute another review.

      Each round of fixes gets its own anchor. Never wait on the anchor of the previous round.

## After LGTM

When the reviewer approves:

1. Verify approval: `maw exec $WS -- seal review <review-id>` — confirm LGTM vote, no blocks
2. Mark review as merged: `maw exec $WS -- seal reviews mark-merged <review-id> --agent $AGENT`
3. Continue with [finish](finish.md) to close the bone and merge the workspace

The actual code merge is handled by `maw ws merge` in the finish step — do not run manual squash commands.

### Commit nothing after the LGTM

An approval records the commit it applied to. Any commit you add afterwards —
a lint fix, a conflict resolution, a last "small" change — puts the approval
behind the code, and `seal reviews mark-merged` exits 1:

```
Error: Cannot merge <review-id>: the approval does not cover the current code.
  Approved at:  <commit>
  Target now:   <commit> (N commits)
```

This is not a glitch to work around. Fix it in this order:

1. **Ask for a fresh LGTM.** Re-request the reviewer as in step 3 above. A repeat
   LGTM moves the approval onto the new commit and clears the block. This is the
   normal path and the only one that keeps the merge honest.
2. `--allow-stale-approval` merges past the check. Use it only when you can say
   why the new commits are outside what was reviewed, and record that reason in a
   bone comment.

Check coverage before you try to merge:
`maw exec $WS -- seal diff <review-id> --format json` reports `approval_stale`,
`approved_commit` and `uncovered_commits`.

## Assumptions

- `EDICT_PROJECT` env var contains the project channel name.
- You are the author of the review (the agent that created it or requested it).
- The workspace is still active — fixes are made in the workspace, not the main branch.
