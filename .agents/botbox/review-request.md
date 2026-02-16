# Review Request

Request a review using crit and announce it in the project channel.

## Arguments

- `$AGENT` = agent identity (required)
- `<review-id>` = review to request (required)
- `<reviewer>` = reviewer agent name (optional)
  - Specialist reviewers follow the pattern: `<project>-<role>`
  - Example: `myproject-security`
  - Common role: `security`

## How Reviewer Spawning Works

**Important**: Creating a review with `--reviewers` assigns the reviewer in crit (metadata), but does NOT spawn them. You still need an @mention in a bus message to trigger the spawn hook.

The botbus hook system watches for @mentions. When you send a message containing `@myproject-security`, the hook spawns the security reviewer agent.

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, stop and instruct the user. Run `bus whoami --agent $AGENT` first to confirm; if it returns a name, use it.

2. **Check the bead's risk label** to determine review routing:
   - Get bead details: `maw exec default -- br show <bead-id>`
   - Look for `risk:low`, `risk:high`, or `risk:critical` in labels
   - No risk label = `risk:medium` (standard review)

3. **Risk-based review routing**:

   **risk:low** — Skip review entirely:
   - Do NOT create a crit review
   - Add self-review comment: `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Self-review: <brief what I verified>"`
   - Proceed directly to finish (skip remaining steps)

   **risk:medium** (default) — Standard review:
   - Follow existing review process (step 4 for specialist or step 5 for general review)

   **risk:high** — Security review with failure-mode checklist:
   - MUST request security reviewer
   - Create crit review with note in description: "risk:high — failure-mode checklist required. Please answer: 1) What failure modes exist? 2) What edge cases need validation? 3) How can we roll back if this breaks? 4) What monitoring/alerts should we add? 5) What input validation is needed?"
   - Request security reviewer (see step 4)

   **risk:critical** — Security review + human approval:
   - MUST request security reviewer
   - Create crit review (see step 4)
   - Post to bus requesting human approval: `bus send --agent $AGENT $BOTBOX_PROJECT "risk:critical review for <bead-id>: requires human approval before merge. Review: <review-id> @<approver>" -L review-request`
   - List of approvers from `.botbox.json` → `project.criticalApprovers`
   - If no `criticalApprovers` configured, use project lead or fallback: `@$BOTBOX_PROJECT-lead`

4. If requesting a **specialist reviewer** (e.g., security):
   ```bash
   # Step 1: Create review with reviewer assignment (one command)
   maw exec $WS -- crit reviews create --agent $AGENT --title "<title>" --description "<summary>" --reviewers $BOTBOX_PROJECT-security

   # Step 2: Announce with @mention (TRIGGERS THE SPAWN)
   bus send --agent $AGENT $BOTBOX_PROJECT "Review requested: <review-id> @$BOTBOX_PROJECT-security" -L review-request
   ```

   If the review already exists (re-request after fixes), use `crit reviews request` instead:
   ```bash
   maw exec $WS -- crit reviews request <review-id> --reviewers $BOTBOX_PROJECT-security --agent $AGENT
   ```

   The reviewer name MUST match the project pattern: `<project>-<role>` (e.g., `myproject-security`, `botbus-security`). Do NOT use generic names like `security-reviewer` — those won't match any hooks.

5. **Post review details to the bead** for crash recovery:
   ```bash
   maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Review created: <review-id> in workspace <ws-name> (ws/<ws-name>)"
   ```
   Include: review ID and workspace name. This lets another agent find the review and workspace if the session crashes.

6. If requesting a **general code review** (no specific specialist):
   - Spawn a subagent to perform the code review
   - Announce: `bus send --agent $AGENT $BOTBOX_PROJECT "Review requested: <review-id>, spawned subagent for review" -L review-request`

The reviewer-loop finds open reviews via `crit reviews list` and processes them automatically.

## Common Mistakes

- Using `--reviewer security` or `--reviewers security-reviewer` — these generic names don't match any hooks
- Forgetting the @mention in the bus message — without it, no reviewer spawns
- Using the wrong project prefix — reviewer must be `<project>-<role>` where `<project>` matches the channel

## Assumptions

- `BOTBOX_PROJECT` env var contains the project channel name.
