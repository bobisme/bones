# Merge Check

Verify preconditions and merge a worker's completed workspace.

## Preferred: Use protocol merge

```bash
botbox protocol merge <workspace> --agent $AGENT
```

This checks all preconditions (bone closed, review approved, no conflicts) and outputs the exact merge steps. Use `--execute` to run them directly, or `--force` to skip bone/review checks.

With `--format json`, returns structured output for automation.

## What protocol merge checks

1. **Workspace exists** and is not `default`
2. **Associated bone is closed** (found via claims)
3. **Review is approved** (if review is enabled in `.botbox.json`)
4. **No merge conflicts** (via `maw ws merge --check` pre-flight)

If any check fails, the output explains why and what to do.

## Merge steps (output by protocol merge)

1. `maw ws merge <workspace> --destroy` — merge and clean up
2. `crit reviews mark-merged <review-id>` — mark review as merged (if review exists)
3. `maw push` — push to remote (if `pushMain` is enabled)
4. `bus send` — announce merge on project channel

## Conflict recovery

If merge produces conflicts, the workspace is preserved (not destroyed). Protocol merge outputs recovery steps:

1. **Inspect conflicts**: `maw ws conflicts <ws> --format json`
2. **Sync stale workspace first** (if reported): `maw ws sync <ws>`
3. **Auto-resolve ledger/docs paths** (.bones/, .claude/, .agents/): `maw exec <ws> -- git restore --source refs/heads/main -- .bones/ .claude/ .agents/`
4. **Resolve code conflicts manually**: edit files, then stage with `maw exec <ws> -- git add <resolved-file>`
5. **Retry merge**: `maw ws merge <ws> --destroy`
6. **Undo local merge attempt**: `maw ws undo <ws>`
7. **Recover destroyed workspace**: `maw ws restore <ws>`

## Manual fallback

If `botbox protocol merge` is unavailable, check manually:

1. `maw exec $WS -- crit review <review-id>` — confirm LGTM, no blocks
2. `maw exec default -- bn show <bone-id>` — confirm bone is done
3. `maw ws merge <workspace> --check` — pre-flight conflict detection
4. `maw ws merge <workspace> --destroy` — merge
5. `bus claims release --agent $AGENT --all` — release claims
