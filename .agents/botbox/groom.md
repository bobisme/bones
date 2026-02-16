# Groom

Groom a set of ready beads to improve backlog quality. Use this when you need to clean up beads without necessarily working on them.

## Arguments

- `$AGENT` = agent identity (optional)

## Steps

1. Check ready beads: `maw exec default -- br ready`
2. For each bead from `maw exec default -- br ready`, run `maw exec default -- br show <bead-id>` and fix anything missing:
   - **Title**: Should be clear and actionable (imperative form, e.g., "Add /health endpoint"). If vague, update: `maw exec default -- br update --actor $AGENT <bead-id> --title="..."`
   - **Description**: Should explain what and why. If missing or vague, add context: `maw exec default -- br update --actor $AGENT <bead-id> --description="..."`
   - **Priority**: Should reflect relative importance. Adjust if wrong: `maw exec default -- br update --actor $AGENT <bead-id> --priority=<1-4>`
   - **Risk label**: Does the bead have an appropriate risk label? Assess based on blast radius (how many users/systems affected), data sensitivity (PII, financial, auth), reversibility (can we roll back easily?), and dependency uncertainty (new deps, upstream changes). Add `risk:low`, `risk:high`, or `risk:critical` as appropriate using `maw exec default -- br label add --actor $AGENT -l risk:<level> <bead-id>`. No label = `risk:medium` default.
   - **Labels**: Add labels if the bead fits a category (see Label Conventions below). Apply with `maw exec default -- br label add --actor $AGENT -l <label> <bead-id>` (creates label if it doesn't exist).
   - **Acceptance criteria**: Description should include what "done" looks like. If missing, append criteria to the description.
   - **Testing strategy**: Description should mention how to verify the work (e.g., "run tests", "manual check", "curl endpoint"). If missing, append a brief testing note.
   - Add a comment noting what you groomed: `maw exec default -- br comments add --actor $AGENT --author $AGENT <bead-id> "Groomed by $AGENT: <what changed>"`
3. **Set dependencies between beads.** This is critical — without dependencies, multiple agents can be dispatched to work on beads that must be sequential, causing conflicts and wasted work. For each pair of ready beads, ask: "does one need to land before the other?" Common dependency patterns:
   - **Interface before consumer**: If bead A adds/changes a function, type, or API that bead B uses → `br dep add --actor $AGENT <B> <A>`
   - **Schema before code**: If bead A changes a data format, config schema, or database structure that bead B relies on → `br dep add --actor $AGENT <B> <A>`
   - **Core before extension**: If bead A adds base functionality and bead B extends it → `br dep add --actor $AGENT <B> <A>`
   - **Shared file conflict**: If two beads will edit the same file in overlapping regions, sequence them to avoid merge conflicts → `br dep add --actor $AGENT <later> <earlier>`
   - Use `maw exec default -- br graph --all` to visualize the full dependency graph across all open beads — verify there are no missing edges or unintended isolation. Use `br graph <id>` to inspect a single bead's tree.
   - Add a comment when adding a dependency to explain why: `maw exec default -- br comments add --actor $AGENT --author $AGENT <blocked-id> "Blocked by <blocker-id>: <reason>"`
4. Check bead size: each bead should be one resumable unit of work — if a session crashes after completing it, the next session knows exactly where to pick up. If a bead covers multiple distinct steps, break it down:
   - Create smaller child beads with `maw exec default -- br create --actor $AGENT --owner $AGENT` and `maw exec default -- br dep add --actor $AGENT <child> <parent>`.
   - Add sibling dependencies where order matters (see step 3 patterns).
   - Add a comment to the parent: `maw exec default -- br comments add --actor $AGENT --author $AGENT <parent-id> "Broken down into smaller tasks: <child-id>, ..."`
5. Announce if you groomed multiple beads: `bus send --agent $AGENT $BOTBOX_PROJECT "Groomed N beads: <summary>" -L grooming`

## Acceptance Criteria

- All ready beads have clear, actionable titles
- Descriptions include acceptance criteria and testing strategy
- Priority levels make sense relative to each other
- **Dependencies are set between beads that must be sequenced** (shared files, interface/consumer, schema/code)
- Large beads are broken into smaller, atomic tasks
- Beads with the same owner/context are labeled consistently

## When to Use

- Before a dev agent starts a work cycle (ensures picking work is fast)
- After filing a batch of new beads (get them ready for triage)
- When you notice vague or overlapping beads (preventive cleanup)
- As a standalone task when other work is blocked

## Label Conventions

Labels categorize beads for filtering, reporting, and prioritization. Use labels consistently to make the backlog navigable.

### When to Label

Apply labels during grooming when a bead clearly fits a category. Don't over-label — only use labels that add useful filtering value.

### Label Categories

**Organizational**
- `epic` — Parent tracking issue that aggregates related beads (typically has child dependencies)

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
- Note: `risk:medium` is the default — no label needed for standard work

### Naming Conventions

- Use lowercase, singular form
- Use hyphens for multi-word labels (kebab-case): `review-finding`, not `review_finding` or `reviewFinding`
- Keep labels short and descriptive (1-2 words)
- Be specific but not too granular (prefer `eval` over `eval-level-2-test-case-3`)

### Creating New Labels

Before creating a new label:
1. Check existing labels: `maw exec default -- br label list`
2. Reuse an existing label if it fits (prefer consistency over perfect naming)
3. Only create a new label if you expect to use it for multiple beads
4. Apply with `maw exec default -- br label add --actor $AGENT -l <name> <bead-id>` (creates label automatically if it doesn't exist — no separate creation command needed)

### Project-Specific vs Cross-Project

Labels are project-scoped (stored in each project's beads database). Use naming that makes sense for your project. If you work across multiple projects, use similar conventions for consistency, but don't try to share labels between projects.

### Listing and Filtering

- List all labels: `maw exec default -- br label list`
- Filter beads by label: `maw exec default -- br list --label <name>`
- Filter ready beads: `maw exec default -- br ready` (shows all labels in output)

### Examples

Good labeling:
- A bead about adding OAuth support to the CLI → `cli`
- An epic tracking review evals → `epic`, `eval`, `review`
- A bug found during review that must be fixed → `review-finding`, `must-fix`
- Documentation for the report-issue workflow → `docs`

Over-labeling (avoid):
- A CLI bug that's also a P0 priority → just `cli` (priority is a field, not a label)
- A one-off task specific to a single bead → no new label needed
