# Start

Start a bead using the standard botbox flow: claim the work, set up a workspace, announce.

## Arguments

- `$AGENT` = agent identity (required)
- `<bead-id>` = bead to start (required)

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, stop and instruct the user. Run `bus whoami --agent $AGENT` first to confirm; if it returns a name, use it.
2. `maw exec default -- br update --actor $AGENT <bead-id> --status=in_progress --owner=$AGENT`
3. `bus claims stake --agent $AGENT "bead://$BOTBOX_PROJECT/<bead-id>" -m "<bead-id>"`
4. Create a workspace: `maw ws create --random` — note the workspace name from the output. Store as `$WS`.
5. **All file edits must use the workspace path** `ws/$WS/` (e.g., `$PROJECT_ROOT/ws/frost-castle/`). Use absolute paths for Read, Write, and Edit tools. For commands: `maw exec $WS -- <command>`. Run `br` commands via `maw exec default -- br ...`. **Do NOT run jj commands** — the lead handles all jj operations during merge.
6. `bus claims stake --agent $AGENT "workspace://$BOTBOX_PROJECT/$WS" -m "<bead-id>"`
7. Announce: `bus send --agent $AGENT $BOTBOX_PROJECT "Working on <bead-id>: <bead-title>" -L task-claim`

## Assumptions

- `BOTBOX_PROJECT` env var contains the project channel name.
- `maw` workspaces are used. Workers do not run jj directly — the lead handles jj during merge.
