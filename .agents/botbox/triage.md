# Triage

Find exactly one actionable bone, or determine there is no work available. Groom bones along the way to keep the backlog healthy.

## Arguments

- `$AGENT` = agent identity (required)

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, stop and instruct the user. Run `bus whoami --agent $AGENT` first to confirm; if it returns a name, use it.
2. Check inbox for new messages:
   - `bus inbox --agent $AGENT --channels $BOTBOX_PROJECT --mark-read`
   - For each message that requests work (task request, bug report, feature ask), create a bone: `maw exec default -- bn create --title "..." --description "..." --tag <relevant-tags> --kind task`
   - For messages with `-L feedback` (reports from other agents or humans):
     - If it contains a bug report, feature request, or actionable work: create a bone with `maw exec default -- bn create`
     - If it references existing bones: review with `maw exec default -- bn show <bone-id>`, triage (accept, adjust urgency, close if duplicate/out-of-scope)
     - Acknowledge on botbus: `bus send --agent $AGENT <channel> "Triaged: <summary> @<reporter-agent>" -L triage-reply`
   - For messages that are questions or status checks, reply inline: `bus send --agent $AGENT <channel> "<response>" -L triage-reply`
3. Check for next work: `maw exec default -- bn next`
   - If no work available and no inbox messages created new bones, output `NO_WORK_AVAILABLE` and stop.
4. **Check tracking bones** for responses. For each bone tagged `tracking`:
   - Parse the description for the remote channel and what was posted
   - Check for responses: `bus history <channel> --from <project>-dev --since <bone-created-time> --format json`
   - If response found: add a comment with the response summary, then close the tracking bone (or follow up if needed)
   - If no response and it's been more than a day: consider re-posting to the channel
   - See [cross-channel](cross-channel.md) for full details
5. **Groom the ready bones.** For each bone from `maw exec default -- bn next`, run `maw exec default -- bn show <bone-id>` and fix anything missing:
   - **Title**: Should be clear and actionable (imperative form, e.g., "Add /health endpoint"). If vague, update it.
   - **Description**: Should explain what and why. If missing or vague, add context.
   - **Tags**: Add tags if the bone fits a category (see tag conventions).
   - **Acceptance criteria**: Description should include what "done" looks like. If missing, append criteria to the description.
   - **Testing strategy**: Description should mention how to verify the work (e.g., "run tests", "manual check", "curl endpoint"). If missing, append a brief testing note.
   - Add a comment noting what you groomed: `maw exec default -- bn bone comment add <bone-id> "Groomed by $AGENT: <what changed>"`
6. Pick exactly one task from `maw exec default -- bn next`.
7. Check the bone size: `maw exec default -- bn show <bone-id>`
   - If the bone is large (epic, or description suggests multiple distinct changes), break it down:
     - Create smaller child bones with `maw exec default -- bn create --title "..." --kind task` and `maw exec default -- bn triage dep add <earlier> --blocks <later>`.
     - Then run `maw exec default -- bn next` again to pick one of the children.
   - Repeat until you have exactly one small, atomic task.
8. Verify the bone is not claimed by another agent: `bus claims check --agent $AGENT "bone://$BOTBOX_PROJECT/<bone-id>"`
   - If claimed by someone else, back off and run `maw exec default -- bn next` again excluding that bone.
   - If all candidates are claimed, output `NO_WORK_AVAILABLE` and stop.
9. Output the single bone ID as the result.

**Tip**: Use `cass search "your error or problem"` to find how similar issues were solved in past sessions before starting work.

## Assumptions

- `BOTBOX_PROJECT` env var contains the project channel name.
- `bn` is available and the bones database is initialized.
- The agent will use the [start](start.md) workflow next to claim and begin work.
