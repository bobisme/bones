# Preflight

Validate toolchain and environment before multi-agent work.

## Arguments

- `$AGENT` = agent identity (required)

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, adopt `<project>-dev` (e.g., `botbox-dev`). Agents spawned by `agent-loop.sh` receive a random name automatically.
2. `bus whoami --agent $AGENT` — confirms identity, generates a name if not set.
3. `bus status`
4. `maw exec default -- br where`
5. `maw doctor`
6. `maw exec default -- crit doctor`
