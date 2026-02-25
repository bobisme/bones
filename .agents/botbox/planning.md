# Planning

Turn a spec, PRD, or high-level request into actionable bones. This happens before the worker loop — once planning is done, the output is a set of bones ready for triage.

## When to Plan

- You receive a spec, PRD, or feature request that covers multiple steps
- A user describes what they want but not how to break it down
- An epic bone needs decomposition before work can begin

If the task is already small and clear (one reviewable change), skip planning and go straight to [triage](triage.md).

## Steps

1. **Read the spec.** Understand the goal, constraints, and success criteria.
2. **Scout if needed.** If the codebase is unfamiliar, run the [scout](scout.md) workflow first.
3. **Identify units of work.** Look for:
   - Distinct changes (new file, modified function, config update)
   - Sequential dependencies (A must happen before B)
   - Parallel opportunities (C and D can happen simultaneously)
4. **Create bones.** For each unit:
   - `maw exec default -- bn create --title "..." --description "..." --kind task`
   - Title: imperative, specific (e.g., "Add OAuth callback handler", not "OAuth stuff")
   - Description: what, why, acceptance criteria, testing strategy
5. **Assign risk tags** to each bone based on:
   - **Blast radius**: How many users/systems affected if this breaks?
   - **Data sensitivity**: Does it handle PII, financial data, auth credentials, or access control?
   - **Reversibility**: Can we roll back easily, or is this irreversible (migrations, data deletion, public APIs)?
   - **Dependency uncertainty**: New dependencies, breaking changes to upstream APIs, unfamiliar libraries?

   Risk levels:
   - `risk:low` — Typo fixes, doc updates, config tweaks. Self-review only, no crit review needed.
   - `risk:medium` — Standard feature work, bug fixes. Standard crit review (current default). **This is the default if no risk tag is set — no tag needed.**
   - `risk:high` — Security-sensitive, data integrity, user-visible behavior changes. Security review + failure-mode checklist required.
   - `risk:critical` — Irreversible actions, migrations, regulated changes. Human approval required before merge.

   Apply tags: `maw exec default -- bn bone tag <bone-id> risk:<level>`

   Only add tags for `risk:low`, `risk:high`, or `risk:critical`. Default is `risk:medium` (no tag needed).

   Risk can be escalated upward by any agent. Downgrades require lead approval with justification comment.
6. **Wire dependencies.** If order matters:
   - `maw exec default -- bn triage dep add <earlier> --blocks <later>`
   - Parent bones (epics) get children via `maw exec default -- bn triage dep add <parent> --blocks <child>`
   - **Phased plans**: When a plan has phases (Phase I, II, III...), always wire phase goal bones sequentially so earlier phases block later ones: `bn triage dep add <phase-1-goal> --blocks <phase-2-goal>`. Without this, triage treats all phases as equal priority, defeating the purpose of phased planning.
7. **Verify the graph.** `maw exec default -- bn triage graph` — check that:
   - Parallel work is actually parallel (not chained when it doesn't need to be)
   - Dependencies reflect reality (you can't test without implementing)
   - **Phase ordering is present**: If the plan has phases, verify phase goals have dependency edges between them. If all phases have identical triage scores, the phase ordering is likely missing.
8. **Announce.** `bus send --agent $AGENT $BOTBOX_PROJECT "Planned <spec-name>: N bones created" -L planning`

## What Makes a Good Bone

**Right size**: One reviewable unit of work. If a session crashes after completing it, the next session knows exactly where to pick up. Too small = overhead; too large = risky (hard to review, hard to resume).

**Clear title**: Imperative form, specific action. "Add", "Fix", "Update", "Remove" — not "Implement", "Handle", or vague nouns.

**Good description**:
- What: the change being made
- Why: the motivation (links to spec, user need, bug)
- Acceptance criteria: what "done" looks like
- Testing strategy: how to verify (run tests, manual check, curl)

## One Bone vs Many

**One bone** when:
- The change is atomic (can't be split without being awkward)
- Review would be confusing if split
- Total work is under ~1 hour

**Many bones** when:
- Distinct logical steps exist
- Different skills needed (backend vs frontend vs tests)
- Parallelism is possible
- Total work is over ~2 hours

**Rule of thumb**: If you would naturally pause and say "okay, part 1 is done, now part 2", that's two bones.

## Example

Spec: "Add OAuth login support"

Bones created:
1. `Add OAuth config schema` — blocked by nothing
2. `Add OAuth callback handler` — blocked by 1
3. `Add session storage for OAuth tokens` — blocked by nothing (parallel with 1)
4. `Wire OAuth flow in login page` — blocked by 2, 3
5. `Add OAuth integration tests` — blocked by 4

Graph shows 1 and 3 are parallel, 4 waits for both, 5 is last.

## Mission Bone Conventions

A **mission** is a special bone that coordinates a group of child bones toward a shared outcome. Use missions when work is too large for a single bone but needs coherent planning and concurrent execution.

### When to Create a Mission Bone

- Large tasks needing decomposition into parallel workstreams
- Features spanning multiple components (e.g., CLI + library + docs)
- Specs or PRDs that need breakdown into individually reviewable units

If the task is small enough for one bone, skip missions entirely.

### Mission Bone Format

Create a mission bone with the `mission` tag and a structured description:

```bash
maw exec default -- bn create \
  --title "Add OAuth login support" \
  --tag mission \
  --kind task \
  --description "Outcome: Users can log in via OAuth providers (Google, GitHub).
Success metric: OAuth login flow works end-to-end in integration tests.
Constraints: No new runtime dependencies; use existing HTTP client. Max 6 child bones.
Stop criteria: Core login flow works; provider-specific edge cases can be follow-up bones."
```

**Description fields:**
- **Outcome**: One sentence — what does "done" look like?
- **Success metric**: How to verify the outcome?
- **Constraints**: Scope boundaries, forbidden actions, budget
- **Stop criteria**: When to stop even if not everything is done

### Child Bone Conventions

When creating child bones under a mission:

1. **Wire the dependency** to connect child to mission:
   ```bash
   maw exec default -- bn triage dep add <mission-id> --blocks <child-id>
   ```

2. **Tag for querying** with `mission:<mission-id>` so siblings can be discovered:
   ```bash
   maw exec default -- bn bone tag <child-id> "mission:<mission-id>"
   ```

3. **Query siblings** to see all children of a mission:
   ```bash
   maw exec default -- bn list --tag "mission:bd-xxx"
   ```

### Mission Invariants

These rules must hold for every mission:

- **Every child** has both a `mission:<id>` tag and a parent dependency on the mission bone
- **A child belongs to at most one mission** — no shared children across missions
- **Mission cannot close with open children** — all children must be closed first
- **One active worker per child bone** — enforced via `bone://` claims (no two agents work the same child)

### Coordination

See [coordination.md](coordination.md) for tag conventions (`coord:interface`, `coord:blocker`, `coord:handoff`) and sibling awareness protocols.
