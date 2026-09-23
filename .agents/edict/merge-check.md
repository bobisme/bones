# Merge Check

Verify preconditions and merge a worker's completed workspace.

## Preferred: Use protocol merge

```bash
edict protocol merge <workspace> --agent $AGENT
```

This checks all preconditions (bone closed, review approved, no conflicts) and outputs the exact merge steps. Use `--execute` to run them directly, or `--force` to skip the bone-state and review checks. `--force` still needs a bone bound to this exact workspace (its claim memo, or a workspace named after a bone you hold): without that bone's labels the protocol cannot rule out `risk:critical`, so it blocks.

With `--format json`, returns structured output for automation.

## What protocol merge checks

1. **Workspace exists** and is not `default`
2. **Associated bone is closed** (found via claims)
3. **Review is approved** (if review is enabled in `.edict.toml`)
4. **No merge conflicts** (via `maw ws merge --into default --check` pre-flight)
5. **Not risk:critical**. A `risk:critical` bone needs an authorized human approval that no protocol state records, so protocol merge (and protocol finish) block it, even with `--force`. Verify the approval, record it on the bone, then use the manual fallback below.

If any check fails, the output explains why and what to do.

## Merge steps (output by protocol merge)

1. Record the review in the workspace (if a review exists). These steps must come before the merge, because the merge destroys the workspace and its `.seal/reviews/<review-id>/` log:
   - `{ out=$(maw exec $WS -- git status --porcelain --untracked-files=all -- . ':(exclude).seal/reviews/<review-id>') && test -z "$out" || { echo "unreviewed changes: stop" >&2; false; }; }` — refuse when anything outside the review log is uncommitted. `maw ws merge` merges uncommitted additions and deletions too, and no reviewer saw them.
   - `maw exec $WS -- seal reviews mark-merged <review-id>` — mark review as merged while HEAD is still the approved commit.
     Exits 1 when code landed after the approval. Get a fresh LGTM rather than
     forcing it; `--allow-stale-approval` is the deliberate override. See
     [review-response.md](review-response.md).
   - `maw exec $WS -- git add .seal/reviews/<review-id>`
   - `maw exec $WS -- git diff --cached --quiet -- .seal/reviews/<review-id> || maw exec $WS -- git commit -m "chore: seal review <review-id>" -- .seal/reviews/<review-id>` — commit only the review log (skipped when it is already committed)
2. `{ out=$(maw exec $WS -- git status --porcelain --untracked-files=all -- . ':(exclude).seal/reviews/<review-id>') && test -z "$out" || { echo "unreviewed changes: stop" >&2; false; }; } && maw ws merge $WS --into default --destroy --message "feat: <bone-title>"` — repeat the clean check in the same command as the merge (drop the check when no review exists), then merge and clean up (use conventional commit prefix: `feat:`, `fix:`, `chore:`, etc.; swap `default` for a change id when needed)
3. `maw push` — push to remote (if `pushMain` is enabled)
4. `rite send` — announce merge on project channel

After step 2, `seal reviews list` at the repo root shows the review with status `merged`.
If step 2 reports conflicts, the review is already merged, so resolving them needs care:

- A retry with no resolution, or a resolution that changes only `.seal/` or `.bones/`
  (for example the ledger restore below), merges only reviewed code. Retry the step 2 command
  directly, with its clean check. Do not rerun `edict protocol merge`: a merged review no longer passes its gate.
- Any other resolution is code that no reviewer saw. Commit it, create a fresh review for the
  bone, get its LGTM, and run `edict protocol merge` again.

## Conflict recovery

Conflicts are data, not failure — merge auto-syncs stale sources and records conflicts as
structured state rather than aborting. If merge produces conflicts, the workspace is preserved
(not destroyed). Protocol merge outputs recovery steps:

1. **Inspect conflicts**: `maw ws conflicts <ws> --format json` and `maw ws resolve <ws> --list`
2. **Auto-resolve ledger/docs paths** (.bones/, .claude/, .agents/): `maw exec <ws> -- git restore --source refs/heads/main -- .bones/ .claude/ .agents/`. Once the review is recorded, restore only `.bones/`: a change to `.claude/` or `.agents/` after the approval needs a fresh review.
3. **Resolve code conflicts**: `maw ws resolve <ws> --keep epoch|<ws>|both|union` (or `--keep <path>=<name>` per file), or resolve inline at merge time with `maw ws merge <ws> --into default --resolve-all=<ws>` (or `--resolve <cf-id>=<name>`). Manual fallback: edit markers by hand, then `maw exec <ws> -- git add <resolved-file>`.
4. **Untracked scratch blocking a retry?** `maw ws clean <ws> --dry-run` then `maw ws clean <ws>` (snapshot-first removal of untracked files).
5. **Retry merge**: before the review is recorded, rerun `edict protocol merge <ws>` so the record steps run first. After it is recorded, use the step 2 command with its clean check: `{ out=$(maw exec $WS -- git status --porcelain --untracked-files=all -- . ':(exclude).seal/reviews/<review-id>') && test -z "$out" || { echo "unreviewed changes: stop" >&2; false; }; } && maw ws merge $WS --into default --destroy --message "feat: <bone-title>"`. Without a review: `maw ws merge <ws> --into default --destroy --message "feat: <bone-title>"`
6. **Merge attempt itself stuck** (killed/OOM'd/Ctrl-C'd mid-merge): `maw ws merge --abort` clears the orphaned merge-state.
7. **Undo a COMPLETED merge** (not a stuck attempt): repo-level `maw undo` (see `maw ops log` to pick an op id). Do **not** use `maw ws undo <ws>` here — that discards the workspace's entire delta, including the work being merged.
8. **Recover destroyed workspace**: `maw ws recover <ws> --to <new-name>`

## Manual fallback

If `edict protocol merge` is unavailable, check manually:

1. `maw exec $WS -- seal review <review-id>` — confirm LGTM, no blocks
2. `bn show <bone-id>` — confirm bone is done
3. `maw ws merge <workspace> --into default --check` — pre-flight conflict detection
4. If a review exists, confirm `maw exec $WS -- git status --porcelain --untracked-files=all` lists nothing outside `.seal/reviews/<review-id>/`, then record it: `maw exec $WS -- seal reviews mark-merged <review-id>`, then `maw exec $WS -- git add .seal/reviews/<review-id>` and `maw exec $WS -- git commit -m "chore: seal review <review-id>" -- .seal/reviews/<review-id>`
5. Merge. With a review, carry the clean check in the same command: `{ out=$(maw exec $WS -- git status --porcelain --untracked-files=all -- . ':(exclude).seal/reviews/<review-id>') && test -z "$out" || { echo "unreviewed changes: stop" >&2; false; }; } && maw ws merge $WS --into default --destroy --message "feat: <bone-title>"`. Without a review: `maw ws merge <workspace> --into default --destroy --message "feat: <bone-title>"` (use conventional commit prefix)
6. `rite claims release --agent $AGENT --all` — release claims
