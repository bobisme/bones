# Start

Start a bone using the standard edict flow: claim the work, set up a workspace, announce.

## Arguments

- `$AGENT` = agent identity (required)
- `<bone-id>` = bone to start (required)

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, stop and instruct the user. Run `rite whoami --agent $AGENT` first to confirm; if it returns a name, use it.
2. `bn do <bone-id>`
3. `rite claims stake --agent $AGENT "bone://$EDICT_PROJECT/<bone-id>" -m "<bone-id>"`
4. Create a workspace named after the bone: `maw ws create <bone-id> --from main --description "<bone-title>"`. Store the bone-id as `$WS`. If the bone is tied to an existing change, add `--change <change-id>` instead of `--from main`.
5. **All file edits must use the workspace path** `.maw/workspaces/$WS/` (e.g., `$PROJECT_ROOT/.maw/workspaces/bn-2kj9/`). Use absolute paths for Read, Write, and Edit tools. For commands: `maw exec $WS -- <command>`. Run `bn` commands via `bn ...`.
6. `rite claims stake --agent $AGENT "workspace://$EDICT_PROJECT/$WS" -m "<bone-id>"`
7. Announce: `rite send --agent $AGENT $EDICT_PROJECT "Working on <bone-id>: <bone-title>" -L task-claim`

## Assumptions

- `EDICT_PROJECT` env var contains the project channel name.
- `maw` workspaces are used with Git worktrees.
