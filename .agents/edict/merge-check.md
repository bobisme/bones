# Merge Check

Verify preconditions and merge a worker's completed workspace.

## Preferred: Use protocol merge

```bash
edict protocol merge <workspace> --agent $AGENT
```

This checks all preconditions (bone closed, review approved, no conflicts) and outputs the exact merge steps. Use `--execute` to run them directly, or `--force` to skip bone/review checks.

With `--format json`, returns structured output for automation.

## What protocol merge checks

1. **Workspace exists** and is not `default`
2. **Associated bone is closed** (found via claims)
3. **Review is approved** (if review is enabled in `.edict.toml`)
4. **No merge conflicts** (via `maw ws merge --into default --check` pre-flight)

If any check fails, the output explains why and what to do.

## Merge steps (output by protocol merge)

1. `maw ws merge <workspace> --into default --destroy --message "feat: <bone-title>"` — merge and clean up (use conventional commit prefix: `feat:`, `fix:`, `chore:`, etc.; swap `default` for a change id when needed)
2. `maw exec $WS -- seal reviews mark-merged <review-id>` — mark review as merged (if review exists)
3. `maw push` — push to remote (if `pushMain` is enabled)
4. `rite send` — announce merge on project channel

## Conflict recovery

Conflicts are data, not failure — merge auto-syncs stale sources and records conflicts as
structured state rather than aborting. If merge produces conflicts, the workspace is preserved
(not destroyed). Protocol merge outputs recovery steps:

1. **Inspect conflicts**: `maw ws conflicts <ws> --format json` and `maw ws resolve <ws> --list`
2. **Auto-resolve ledger/docs paths** (.bones/, .claude/, .agents/): `maw exec <ws> -- git restore --source refs/heads/main -- .bones/ .claude/ .agents/`
3. **Resolve code conflicts**: `maw ws resolve <ws> --keep epoch|<ws>|both|union` (or `--keep <path>=<name>` per file), or resolve inline at merge time with `maw ws merge <ws> --into default --resolve-all=<ws>` (or `--resolve <cf-id>=<name>`). Manual fallback: edit markers by hand, then `maw exec <ws> -- git add <resolved-file>`.
4. **Untracked scratch blocking a retry?** `maw ws clean <ws> --dry-run` then `maw ws clean <ws>` (snapshot-first removal of untracked files).
5. **Retry merge**: `maw ws merge <ws> --into default --destroy --message "feat: <bone-title>"`
6. **Merge attempt itself stuck** (killed/OOM'd/Ctrl-C'd mid-merge): `maw ws merge --abort` clears the orphaned merge-state.
7. **Undo a COMPLETED merge** (not a stuck attempt): repo-level `maw undo` (see `maw ops log` to pick an op id). Do **not** use `maw ws undo <ws>` here — that discards the workspace's entire delta, including the work being merged.
8. **Recover destroyed workspace**: `maw ws recover <ws> --to <new-name>`

## Manual fallback

If `edict protocol merge` is unavailable, check manually:

1. `maw exec $WS -- seal review <review-id>` — confirm LGTM, no blocks
2. `bn show <bone-id>` — confirm bone is done
3. `maw ws merge <workspace> --into default --check` — pre-flight conflict detection
4. `maw ws merge <workspace> --into default --destroy --message "feat: <bone-title>"` — merge (use conventional commit prefix)
5. `rite claims release --agent $AGENT --all` — release claims
