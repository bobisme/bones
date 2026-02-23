# Start

Start a bone using the standard botbox flow: claim the work, set up a workspace, announce.

## Arguments

- `$AGENT` = agent identity (required)
- `<bone-id>` = bone to start (required)

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, stop and instruct the user. Run `bus whoami --agent $AGENT` first to confirm; if it returns a name, use it.
2. `maw exec default -- bn do <bone-id>`
3. `bus claims stake --agent $AGENT "bone://$BOTBOX_PROJECT/<bone-id>" -m "<bone-id>"`
4. Create a workspace: `maw ws create --random` — note the workspace name from the output. Store as `$WS`.
5. **All file edits must use the workspace path** `ws/$WS/` (e.g., `$PROJECT_ROOT/ws/frost-castle/`). Use absolute paths for Read, Write, and Edit tools. For commands: `maw exec $WS -- <command>`. Run `bn` commands via `maw exec default -- bn ...`.
6. **No `jj`**: botbox now uses Git worktrees through maw. Keep workspace actions in `maw` commands (and `git` only inside `maw exec` when needed).
7. `bus claims stake --agent $AGENT "workspace://$BOTBOX_PROJECT/$WS" -m "<bone-id>"`
8. Announce: `bus send --agent $AGENT $BOTBOX_PROJECT "Working on <bone-id>: <bone-title>" -L task-claim`

## Assumptions

- `BOTBOX_PROJECT` env var contains the project channel name.
- `maw` workspaces are used with Git worktrees.
