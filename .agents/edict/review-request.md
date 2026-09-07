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
vote, and releases the claim.

For a re-review after fixes, keep the same Seal review, run:

```bash
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

- Agentbus `done` is only evidence that the session answered; inspect
  `maw exec "$WS" -- seal review "$review_id" --format json` before moving on.
- If Agentbus is unresolved, blocked, unavailable, or times out, post one
  anchored `task-blocked` message, record it on the bone, release the review
  claim, and stop. Do not fall back to an @mention or a workspace scan.
- If the Seal vote blocks, use [review-response](review-response.md), then
  re-request and launch the same review with a new anchor.
- Do not close the bone, merge the workspace, or release the work claim until
  Seal records the required current approval.
