# Review Request

Create a Seal review against the exact workspace, then explicitly launch the
reviewer that owns that exact review. A `--reviewers` assignment is a Seal gate;
it is not an agent-discovery mechanism and it does not create a Rite hook.

## Arguments

- `$AGENT` — author identity;
- `$WS` — workspace containing the change;
- `<review-id>` — exact Seal review id; and
- `<bone-id>` — tracked work item.

All Seal commands run through `maw exec $WS --`. The reviewer identity remains
`$EDICT_PROJECT-security` so that Seal's approval gate is stable, but never
mention `@$EDICT_PROJECT-security`: the ambient mention hook is retired.

## Before you request review

Commit everything in the workspace first:

```bash
maw exec "$WS" -- git add -A
maw exec "$WS" -- git commit -m "<bone-id>: <summary>"
```

A review covers commits, and an approval covers one commit. Uncommitted changes are not
in the review, but `maw ws merge` merges them anyway. After the LGTM, the only commit
allowed is the review log (see
[merge-check.md](merge-check.md#the-review-log-and-the-clean-check)).

## What a review covers

A review covers the range `<base>..<target>`:

- **Base.** `seal reviews create` finds the fork point of your workspace, so the review
  covers every commit of the feature. `--base <rev>` sets the base yourself (exclusive);
  `--base HEAD~1` reviews only the current commit.
- **Check the range.** `seal reviews create` prints the range and the commit count. If the
  count is not what you expect, fix it before you launch a reviewer.
- **The target does not move by itself.** After fixes, `seal reviews retarget` moves the
  review's target to the new HEAD (only the author can retarget). A retarget clears the
  votes, so the new code needs a fresh decision. See the re-review steps below.
- **The base is persisted.** A retarget extends the range from the same base instead of
  shifting it, so a re-review covers the whole feature, not only the fix. (After a rebase
  onto a newer trunk, or with an explicit `--base`, a retarget moves the base too.)
- To see what the approval covers now, run
  `maw exec "$WS" -- seal diff <review-id> --format json`. It reports `approval_stale`,
  `approved_commit` and `uncovered_commits` (see
  [review-response.md](review-response.md#commit-no-code-after-the-lgtm)).

## Risk routing

- **risk:low** — do not create a review; record the self-review on the bone.
- **risk:medium** — create the configured Seal review. For its security role,
  use the dedicated Daybreak launch below.
- **risk:high** — create the security review with the five failure-mode
  questions in its description and use the dedicated Daybreak launch.
- **risk:critical** — do the same as high risk and request the separately
  required human approval. The Daybreak LGTM does not replace that gate.

## Create and launch an exact security review

```bash
maw exec "$WS" -- seal reviews create --agent "$AGENT" \
  --title "<bone-id>: <title>" --description "<summary>" \
  --reviewers "$EDICT_PROJECT-security"

# Copy the printed review id into review_id, then retain crash-recovery data.
bn bone comment add <bone-id> \
  "Review created: $review_id in workspace $WS (.maw/workspaces/$WS)"

request_anchor=$(rite send --agent "$AGENT" "$EDICT_PROJECT" \
  "Dedicated security review requested: $review_id for <bone-id> in $WS" \
  -L review-request --format json | jq -r .id)
bn bone comment add <bone-id> "Review anchor: $request_anchor for $review_id"

kind=review-request
```

Immediately follow [security-review](security-review.md)'s **Launch contract**
with these variables. It creates a uniquely named and labelled Vessel Codex
session using `gpt-daybreak-blue-latest`, assigns
`review://$EDICT_PROJECT/$review_id`, waits with Agentbus, verifies the Seal
vote, reports the anchored Rite verdict, terminates the dedicated Vessel
session, and only then releases the claim.

For a re-review after fixes, keep the same Seal review. First retarget it to
the fixed commits — `seal reviews request` alone leaves the review's target
commit pinned at the old, pre-fix anchor — then re-request:

```bash
maw exec "$WS" -- seal reviews retarget "$review_id" --agent "$AGENT"
maw exec "$WS" -- seal reviews request "$review_id" \
  --reviewers "$EDICT_PROJECT-security" --agent "$AGENT"
request_anchor=$(rite send --agent "$AGENT" "$EDICT_PROJECT" \
  "Dedicated security re-review requested: $review_id in $WS" \
  -L review-response --format json | jq -r .id)
kind=review-response
```

Then run the same direct launch with the fresh `request_anchor` and current
workspace `head`. Never start a second review for ordinary feedback fixes.

## Terminal rules

- Agentbus completion is not approval. `done` is only evidence that the session
  answered; inspect `maw exec "$WS" -- seal review "$review_id" --format json` and
  confirm the vote before moving on. The same holds for a subagent's report.
- If Agentbus is unresolved, blocked, unavailable, or times out, post one
  anchored `task-blocked` message, record it on the bone, snapshot and
  terminate the dedicated Vessel session, release the review claim, and stop.
  If termination fails, keep the review claim held and report the operational
  blocker. Do not fall back to an @mention or a workspace scan.
- If the Seal vote blocks, use [review-response](review-response.md), then
  retarget, re-request, and launch the same review with a new anchor.
- Do not close the bone, merge the workspace, or release the work claim until
  Seal records the required current approval.
