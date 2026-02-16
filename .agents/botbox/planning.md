# Planning

Turn a spec, PRD, or high-level request into actionable beads. This happens before the worker loop — once planning is done, the output is a set of beads ready for triage.

## When to Plan

- You receive a spec, PRD, or feature request that covers multiple steps
- A user describes what they want but not how to break it down
- An epic bead needs decomposition before work can begin

If the task is already small and clear (one reviewable change), skip planning and go straight to [triage](triage.md).

## Steps

1. **Read the spec.** Understand the goal, constraints, and success criteria.
2. **Scout if needed.** If the codebase is unfamiliar, run the [scout](scout.md) workflow first.
3. **Identify units of work.** Look for:
   - Distinct changes (new file, modified function, config update)
   - Sequential dependencies (A must happen before B)
   - Parallel opportunities (C and D can happen simultaneously)
4. **Create beads.** For each unit:
   - `maw exec default -- br create --actor $AGENT --owner $AGENT --title="..." --description="..." --type=task --priority=<1-4>`
   - Title: imperative, specific (e.g., "Add OAuth callback handler", not "OAuth stuff")
   - Description: what, why, acceptance criteria, testing strategy
5. **Assign risk labels** to each bead based on:
   - **Blast radius**: How many users/systems affected if this breaks?
   - **Data sensitivity**: Does it handle PII, financial data, auth credentials, or access control?
   - **Reversibility**: Can we roll back easily, or is this irreversible (migrations, data deletion, public APIs)?
   - **Dependency uncertainty**: New dependencies, breaking changes to upstream APIs, unfamiliar libraries?

   Risk levels:
   - `risk:low` — Typo fixes, doc updates, config tweaks. Self-review only, no crit review needed.
   - `risk:medium` — Standard feature work, bug fixes. Standard crit review (current default). **This is the default if no risk label is set — no label needed.**
   - `risk:high` — Security-sensitive, data integrity, user-visible behavior changes. Security review + failure-mode checklist required.
   - `risk:critical` — Irreversible actions, migrations, regulated changes. Human approval required before merge.

   Apply labels: `maw exec default -- br label add --actor $AGENT -l risk:<level> <bead-id>`

   Only add labels for `risk:low`, `risk:high`, or `risk:critical`. Default is `risk:medium` (no label needed).

   Risk can be escalated upward by any agent. Downgrades require lead approval with justification comment.
6. **Wire dependencies.** If order matters:
   - `maw exec default -- br dep add --actor $AGENT <later> <earlier>`
   - Parent beads (epics) get children via `maw exec default -- br dep add --actor $AGENT <child> <parent>`
7. **Verify the graph.** `maw exec default -- br dep tree <root-bead>` — check that:
   - Parallel work is actually parallel (not chained when it doesn't need to be)
   - Dependencies reflect reality (you can't test without implementing)
8. **Announce.** `bus send --agent $AGENT $BOTBOX_PROJECT "Planned <spec-name>: N beads created" -L planning`

## What Makes a Good Bead

**Right size**: One reviewable unit of work. If a session crashes after completing it, the next session knows exactly where to pick up. Too small = overhead; too large = risky (hard to review, hard to resume).

**Clear title**: Imperative form, specific action. "Add", "Fix", "Update", "Remove" — not "Implement", "Handle", or vague nouns.

**Good description**:
- What: the change being made
- Why: the motivation (links to spec, user need, bug)
- Acceptance criteria: what "done" looks like
- Testing strategy: how to verify (run tests, manual check, curl)

**Appropriate priority**:
- P1: Blocking other work or critical path
- P2: Normal priority (default)
- P3: Nice to have, not urgent
- P4: Backlog, maybe never

## One Bead vs Many

**One bead** when:
- The change is atomic (can't be split without being awkward)
- Review would be confusing if split
- Total work is under ~1 hour

**Many beads** when:
- Distinct logical steps exist
- Different skills needed (backend vs frontend vs tests)
- Parallelism is possible
- Total work is over ~2 hours

**Rule of thumb**: If you would naturally pause and say "okay, part 1 is done, now part 2", that's two beads.

## Example

Spec: "Add OAuth login support"

Beads created:
1. `Add OAuth config schema` (P2) — blocked by nothing
2. `Add OAuth callback handler` (P2) — blocked by 1
3. `Add session storage for OAuth tokens` (P2) — blocked by nothing (parallel with 1)
4. `Wire OAuth flow in login page` (P2) — blocked by 2, 3
5. `Add OAuth integration tests` (P3) — blocked by 4

Graph shows 1 and 3 are parallel, 4 waits for both, 5 is last.

## Mission Bead Conventions

A **mission** is a special bead that coordinates a group of child beads toward a shared outcome. Use missions when work is too large for a single bead but needs coherent planning and concurrent execution.

### When to Create a Mission Bead

- Large tasks needing decomposition into parallel workstreams
- Features spanning multiple components (e.g., CLI + library + docs)
- Specs or PRDs that need breakdown into individually reviewable units

If the task is small enough for one bead, skip missions entirely.

### Mission Bead Format

Create a mission bead with the `mission` label and a structured description:

```bash
maw exec default -- br create --actor $AGENT --owner $AGENT \
  --title="Add OAuth login support" \
  --labels mission \
  --type=task --priority=2 \
  --description="Outcome: Users can log in via OAuth providers (Google, GitHub).
Success metric: OAuth login flow works end-to-end in integration tests.
Constraints: No new runtime dependencies; use existing HTTP client. Max 6 child beads.
Stop criteria: Core login flow works; provider-specific edge cases can be follow-up beads."
```

**Description fields:**
- **Outcome**: One sentence — what does "done" look like?
- **Success metric**: How to verify the outcome?
- **Constraints**: Scope boundaries, forbidden actions, budget
- **Stop criteria**: When to stop even if not everything is done

### Child Bead Conventions

When creating child beads under a mission:

1. **Wire the dependency** with `--parent` to connect child to mission:
   ```bash
   maw exec default -- br dep add --actor $AGENT <child-id> <mission-id>
   ```

2. **Label for querying** with `mission:<mission-id>` so siblings can be discovered:
   ```bash
   maw exec default -- br label add --actor $AGENT -l "mission:<mission-id>" <child-id>
   ```

3. **Query siblings** to see all children of a mission:
   ```bash
   maw exec default -- br list --label "mission:bd-xxx"
   ```

### Mission Invariants

These rules must hold for every mission:

- **Every child** has both a `mission:<id>` label and a `--parent` dependency on the mission bead
- **A child belongs to at most one mission** — no shared children across missions
- **Mission cannot close with open children** — all children must be closed first
- **One active worker per child bead** — enforced via `bead://` claims (no two agents work the same child)

### Coordination

See [coordination.md](coordination.md) for label conventions (`coord:interface`, `coord:blocker`, `coord:handoff`) and sibling awareness protocols.
