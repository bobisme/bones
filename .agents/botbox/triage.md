# Triage

Find exactly one actionable bead, or determine there is no work available. Groom beads along the way to keep the backlog healthy.

## Arguments

- `$AGENT` = agent identity (required)

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, stop and instruct the user. Run `bus whoami --agent $AGENT` first to confirm; if it returns a name, use it.
2. Check inbox for new messages:
   - `bus inbox --agent $AGENT --channels $BOTBOX_PROJECT --mark-read`
   - For each message that requests work (task request, bug report, feature ask), create a bead: `maw exec default -- br create --actor $AGENT --owner $AGENT --title="..." --description="..." --labels <relevant-labels> --type=task --priority=2`
   - For messages with `-L feedback` (reports from other agents or humans):
     - If it contains a bug report, feature request, or actionable work: create a bead with `maw exec default -- br create`
     - If it references existing beads: review with `maw exec default -- br show <bead-id>`, triage (accept, adjust priority, close if duplicate/out-of-scope)
     - Acknowledge on botbus: `bus send --agent $AGENT <channel> "Triaged: <summary> @<reporter-agent>" -L triage-reply`
   - For messages that are questions or status checks, reply inline: `bus send --agent $AGENT <channel> "<response>" -L triage-reply`
3. Check for ready beads: `maw exec default -- br ready`
   - If no ready beads exist and no inbox messages created new beads, output `NO_WORK_AVAILABLE` and stop.
4. **Check tracking beads** for responses. For each bead labeled `tracking`:
   - Parse the description for the remote channel and what was posted
   - Check for responses: `bus history <channel> --from <project>-dev --since <bead-created-time> --format json`
   - If response found: add a comment with the response summary, then close the tracking bead (or follow up if needed)
   - If no response and it's been more than a day: consider re-posting to the channel
   - See [cross-channel](cross-channel.md) for full details
5. **Check blocked beads** for resolved blockers. If a bead was blocked pending information, an upstream fix, or a tool issue that has since been resolved, unblock it: `maw exec default -- br update --actor $AGENT <bead-id> --status=open` with a comment explaining why it's unblocked.
6. **Groom the ready beads.** For each bead from `maw exec default -- br ready`, run `maw exec default -- br show <bead-id>` and fix anything missing:
   - **Title**: Should be clear and actionable (imperative form, e.g., "Add /health endpoint"). If vague, update: `maw exec default -- br update --actor $AGENT <bead-id> --title="..."`
   - **Description**: Should explain what and why. If missing or vague, add context: `maw exec default -- br update --actor $AGENT <bead-id> --description="..."`
   - **Priority**: Should reflect relative importance. Adjust if wrong: `maw exec default -- br update --actor $AGENT <bead-id> --priority=<1-4>`
   - **Labels**: Add labels if the bead fits a category (see label conventions). Apply with `maw exec default -- br label add --actor $AGENT -l <label> <bead-id>` (creates label automatically if it doesn't exist).
   - **Acceptance criteria**: Description should include what "done" looks like. If missing, append criteria to the description.
   - **Testing strategy**: Description should mention how to verify the work (e.g., "run tests", "manual check", "curl endpoint"). If missing, append a brief testing note.
   - Add a comment noting what you groomed: `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Groomed by $AGENT: <what changed>"`
7. Use bv to pick exactly one task: `maw exec default -- bv --robot-next`
   - Parse the JSON output to get the recommended bead ID.
8. Check the bead size: `maw exec default -- br show <bead-id>`
   - If the bead is large (epic, or description suggests multiple distinct changes), break it down:
     - Create smaller child beads with `maw exec default -- br create --actor $AGENT --owner $AGENT` and `maw exec default -- br dep add --actor $AGENT <child> <parent>`.
     - Then run `maw exec default -- bv --robot-next` again to pick one of the children.
   - Repeat until you have exactly one small, atomic task.
9. Verify the bead is not claimed by another agent: `bus claims check --agent $AGENT "bead://$BOTBOX_PROJECT/<bead-id>"`
   - If claimed by someone else, back off and run `maw exec default -- bv --robot-next` again excluding that bead.
   - If all candidates are claimed, output `NO_WORK_AVAILABLE` and stop.
10. Output the single bead ID as the result.

**Tip**: Use `cass search "your error or problem"` to find how similar issues were solved in past sessions before starting work.

## Assumptions

- `BOTBOX_PROJECT` env var contains the project channel name.
- `bv` is available and the beads database is initialized.
- The agent will use the [start](start.md) workflow next to claim and begin work.
