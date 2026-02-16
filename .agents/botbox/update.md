# Update

Post a bead status update and notify the project channel.

## Arguments

- `$AGENT` = agent identity (required)
- `<bead-id>` = bead to update (required)
- `<status>` = new status (required): open | in_progress | blocked | done

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, stop and instruct the user. Run `bus whoami --agent $AGENT` first to confirm; if it returns a name, use it.
2. `maw exec default -- br update --actor $AGENT <bead-id> --status=<status>`
3. `bus send --agent $AGENT $BOTBOX_PROJECT "<bead-id> -> <status>" -L task-update`

## Assumptions

- `BOTBOX_PROJECT` env var contains the project channel name.
