# Review Loop

Review loop for reviewer agents. Process pending review requests and leave feedback.

Your identity is `$AGENT`. All rite commands must include `--agent $AGENT`. Run `rite whoami --agent $AGENT` first if you need to confirm the identity.

## Loop

1. Read new review requests:
   - `rite inbox --agent $AGENT --channels $EDICT_PROJECT --mark-read`
   - `rite wait --agent $AGENT -L review-request -t 5` (optional)
2. Find open reviews by iterating workspaces: `maw ws list --format json`, then `maw exec $WS -- seal inbox --agent $AGENT --format=json` per workspace
   - The reviewer-loop script handles this iteration automatically
3. For each review, gather context before commenting. Use `maw exec $WS --` for all seal commands targeting a workspace review:
   a. Read the review and diff: `maw exec $WS -- seal review <id>` and `maw exec $WS -- seal diff <id>`
      - `maw exec $WS -- seal review <id> --format=json` includes workspace info for reading source files
   b. Read the full source files changed in the diff from the **workspace path** (e.g., `ws/$WS/src/file.rs`), not project root
   c. Read project config (e.g., `Cargo.toml`) for edition and dependency versions
   d. Run static analysis in the workspace: `maw exec $WS -- cargo clippy 2>&1` — cite warnings in your comments
   e. If unsure about framework or library behavior, use web search to verify before commenting
   f. **Cross-file consistency**: Compare functions that follow similar patterns across files. Do all handlers that validate input use the validated result consistently? Are security checks (auth, path validation, sanitization) applied uniformly? If one function does it right and another doesn't, that's a bug.
   g. **Boundary checks**: Trace each user-supplied value (query params, path params, headers, body fields) through to where it's used. Check arithmetic for edge cases: 0, 1, MAX, negative values, empty strings.
4. For each issue found, comment with a severity level:
   - **CRITICAL**: Security vulnerabilities, data loss, crashes in production
   - **HIGH**: Correctness bugs, race conditions, resource leaks
   - **MEDIUM**: Error handling gaps, missing validation at boundaries
   - **LOW**: Code quality, naming, structure
   - **INFO**: Suggestions, style preferences, minor improvements
   - `maw exec $WS -- seal comment <id> "SEVERITY: <feedback>" --file <path> --line <line-or-range>`
5. Vote:
   - `maw exec $WS -- seal block <id> --reason "..."` if any CRITICAL or HIGH issues exist
   - `maw exec $WS -- seal lgtm <id>` if no CRITICAL or HIGH issues
6. Post a summary in the project channel and tag the author: `rite send --agent $AGENT $EDICT_PROJECT "..." -L review-done`

Focus on security and correctness. Ground findings in evidence — compiler output, documentation, or source code — not assumptions about API behavior.

## Re-review

When re-review is requested after a block, the author's fixes live in their **workspace**, not on the main branch. The main branch still has the pre-fix code until merge.

1. Identify the workspace from `maw exec $WS -- seal review <id> --format=json` (workspace info is auto-detected from the change_id).
2. Read source files from the **workspace path** (e.g., `ws/$WS/src/main.rs`), not from the project root.
3. Run static analysis in the workspace: `maw exec $WS -- cargo clippy 2>&1`
4. Verify each fix against the original issue — read actual code, don't just trust thread replies.
5. If all issues are resolved: `maw exec $WS -- seal lgtm <id>`. If issues remain: `maw exec $WS -- seal reply <thread-id> --agent $AGENT "..."` explaining what's still wrong.
