# Groom

Groom a set of ready bones to improve backlog quality. Use this when you need to clean up bones without necessarily working on them.

## Arguments

- `$AGENT` = agent identity (optional)

## Steps

1. Check next work: `maw exec default -- bn next`
2. For each bone, run `maw exec default -- bn show <bone-id>` and fix anything missing:
   - **Title**: Should be clear and actionable (imperative form, e.g., "Add /health endpoint"). If vague, update it.
   - **Description**: Should explain what and why. If missing or vague, add context.
   - **Risk tag**: Does the bone have an appropriate risk tag? Assess based on blast radius (how many users/systems affected), data sensitivity (PII, financial, auth), reversibility (can we roll back easily?), and dependency uncertainty (new deps, upstream changes). Add `risk:low`, `risk:high`, or `risk:critical` as appropriate using `maw exec default -- bn bone tag <bone-id> risk:<level>`. No tag = `risk:medium` default.
   - **Tags**: Add tags if the bone fits a category (see Tag Conventions below). Apply with `maw exec default -- bn bone tag <bone-id> <tag>`.
   - **Acceptance criteria**: Description should include what "done" looks like. If missing, append criteria to the description.
   - **Testing strategy**: Description should mention how to verify the work (e.g., "run tests", "manual check", "curl endpoint"). If missing, append a brief testing note.
   - Add a comment noting what you groomed: `maw exec default -- bn bone comment add <bone-id> "Groomed by $AGENT: <what changed>"`
3. **Set dependencies between bones.** This is critical — without dependencies, multiple agents can be dispatched to work on bones that must be sequential, causing conflicts and wasted work. For each pair of ready bones, ask: "does one need to land before the other?" Common dependency patterns:
   - **Interface before consumer**: If bone A adds/changes a function, type, or API that bone B uses → `bn triage dep add <A> --blocks <B>`
   - **Schema before code**: If bone A changes a data format, config schema, or database structure that bone B relies on → `bn triage dep add <A> --blocks <B>`
   - **Core before extension**: If bone A adds base functionality and bone B extends it → `bn triage dep add <A> --blocks <B>`
   - **Shared file conflict**: If two bones will edit the same file in overlapping regions, sequence them to avoid merge conflicts → `bn triage dep add <earlier> --blocks <later>`
   - **Phased plans**: If bones come from a phased plan (Phase I, II, III...), verify that phase goal bones are wired sequentially (`bn triage dep add <phase-1-goal> --blocks <phase-2-goal>`). Without this, triage treats all phases as equal priority.
   - Use `maw exec default -- bn triage graph` to visualize the full dependency graph across all open bones — verify there are no missing edges or unintended isolation. If all phases have identical triage scores, the phase ordering is likely missing.
   - Add a comment when adding a dependency to explain why: `maw exec default -- bn bone comment add <blocked-id> "Blocked by <blocker-id>: <reason>"`
4. Check bone size: each bone should be one resumable unit of work — if a session crashes after completing it, the next session knows exactly where to pick up. If a bone covers multiple distinct steps, break it down:
   - Create smaller child bones with `maw exec default -- bn create --title "..." --kind task` and `maw exec default -- bn triage dep add <earlier> --blocks <later>`.
   - Add sibling dependencies where order matters (see step 3 patterns).
   - Add a comment to the parent: `maw exec default -- bn bone comment add <parent-id> "Broken down into smaller tasks: <child-id>, ..."`
5. Announce if you groomed multiple bones: `bus send --agent $AGENT $BOTBOX_PROJECT "Groomed N bones: <summary>" -L grooming`

## Acceptance Criteria

- All ready bones have clear, actionable titles
- Descriptions include acceptance criteria and testing strategy
- **Dependencies are set between bones that must be sequenced** (shared files, interface/consumer, schema/code)
- Large bones are broken into smaller, atomic tasks
- Bones with the same owner/context are tagged consistently

## When to Use

- Before a dev agent starts a work cycle (ensures picking work is fast)
- After filing a batch of new bones (get them ready for triage)
- When you notice vague or overlapping bones (preventive cleanup)
- As a standalone task when other work is blocked

## Tag Conventions

Tags categorize bones for filtering, reporting, and prioritization. Use tags consistently to make the backlog navigable.

### When to Tag

Apply tags during grooming when a bone clearly fits a category. Don't over-tag — only use tags that add useful filtering value.

### Tag Categories

**Organizational**
- `epic` — Parent tracking issue that aggregates related bones (typically has child dependencies)

**Component/Area** (where the work happens)
- `cli` — CLI code changes
- `docs` — Documentation updates
- `skill` — Skill implementation or changes
- `eval` — Evaluation framework or test runs

**Review-Related**
- `review` — Review process improvements or review-related tasks
- `review-finding` — Issues discovered during code review
- `must-fix` — Critical issues from reviews (blocks merge)
- `should-fix` — Non-blocking review feedback (nice-to-have improvements)

**Risk**
- `risk:low` — Typo fixes, doc updates, config tweaks (self-review, no crit review)
- `risk:high` — Security-sensitive, data integrity, user-visible changes (security review + checklist)
- `risk:critical` — Irreversible actions, migrations, regulated changes (human approval required)
- Note: `risk:medium` is the default — no tag needed for standard work

### Naming Conventions

- Use lowercase, singular form
- Use hyphens for multi-word tags (kebab-case): `review-finding`, not `review_finding` or `reviewFinding`
- Keep tags short and descriptive (1-2 words)
- Be specific but not too granular (prefer `eval` over `eval-level-2-test-case-3`)

### Creating New Tags

Before creating a new tag:
1. Check existing tags: `maw exec default -- bn bone tag list`
2. Reuse an existing tag if it fits (prefer consistency over perfect naming)
3. Only create a new tag if you expect to use it for multiple bones
4. Apply with `maw exec default -- bn bone tag <bone-id> <tag>`

### Listing and Filtering

- Filter bones by tag: `maw exec default -- bn list --tag <name>`

### Examples

Good tagging:
- A bone about adding OAuth support to the CLI → `cli`
- An epic tracking review evals → `epic`, `eval`, `review`
- A bug found during review that must be fixed → `review-finding`, `must-fix`
- Documentation for the report-issue workflow → `docs`

Over-tagging (avoid):
- A CLI bug that's also urgent → just `cli` (urgency is a field, not a tag)
- A one-off task specific to a single bone → no new tag needed
