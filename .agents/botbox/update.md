# Update

Post a bone state update and notify the project channel.

## Arguments

- `$AGENT` = agent identity (required)
- `<bone-id>` = bone to update (required)
- `<state>` = new state (required): open | doing | done

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, stop and instruct the user. Run `bus whoami --agent $AGENT` first to confirm; if it returns a name, use it.
2. For `doing`: `maw exec default -- bn do <bone-id>`
   For `done`: `maw exec default -- bn done <bone-id>`
   For `open`: update the bone state as needed
3. `bus send --agent $AGENT $BOTBOX_PROJECT "<bone-id> -> <state>" -L task-update`

## Assumptions

- `BOTBOX_PROJECT` env var contains the project channel name.
