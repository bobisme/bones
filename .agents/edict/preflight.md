# Preflight

Validate toolchain and environment before multi-agent work.

## Arguments

- `$AGENT` = agent identity (required)

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, adopt `<project>-dev` (e.g., `edict-dev`). Agents spawned by `edict run worker-loop` receive a random name automatically.
2. `rite whoami --agent $AGENT` — confirms identity, generates a name if not set.
3. `rite status`
4. `bn show --help`
5. `maw doctor` — fast triage
6. `maw fsck` — deep invariant check (exit 0 clean, 1 warn, 2 corruption; `--repair` applies only provably-safe fixes)
7. `seal doctor`
