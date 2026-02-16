# Merge Check

Ensure review approval and clean claims before merge.

## Arguments

- `$AGENT` = agent identity (required)
- `<review-id>` = review to check (required)
- `<bead-id>` = bead to verify (optional)

## Steps

1. Resolve agent identity: use `--agent` argument if provided, otherwise `$AGENT` env var. If neither is set, stop and instruct the user. Run `bus whoami --agent $AGENT` first to confirm; if it returns a name, use it.
2. `maw exec $WS -- crit review <review-id>` and confirm approval (LGTM) or no blocks.
3. If bead-id provided, ensure the bead is closed: `maw exec default -- br show <bead-id>`.
4. Ensure claims are released: `bus claims --agent $AGENT` and `bus claims release --agent $AGENT --all` if needed.
