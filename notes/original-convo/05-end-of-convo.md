BOB:

Cool. I just got around to the first doc you wrote. I'm trying to decide how I feel about the task sizing and the states. I don't want to set sizes, but if agents do, that's fine. We should just not attach timelines to what sizes mean because agents get it wrong. If we do sizes, we should do xxs, xs, s, m, l, xl, xxl like t-shirts. I do want some way to have a bead planned but say "don't work on this" like I can with deferred. Maybe if the urgency field were trinary: urgent, default, punted it would solve that need. Not sure of the language there but I kind of like "punt"

CLAUDE:

Yeah, I think so. The original design had four (goal, task, bug, chore) but "chore" is fuzzy — in practice, agents either file it as a task or a bug depending on their mood, and the distinction between "chore" and "task" doesn't actually change how anything gets prioritized or triaged.

**Goal**: Has children. Represents an outcome, not a unit of work. "Migrate to new auth system." You don't "do" a goal — you do the tasks under it, and the goal completes when its children do.

**Task**: A unit of work. "Implement JWT rotation endpoint." Someone picks it up, does it, marks it done.

**Bug**: A task that describes something broken. "Auth tokens don't refresh after 30s idle." The only reason to distinguish it from a task is that bugs carry an implicit "this _should_ already work" signal, which matters for triage — bugs in production code arguably deserve higher default weight than greenfield tasks.

That's it. Three kinds. Each has a clear identity test:

- Does it have children and represent an outcome? → **goal**
- Does it describe something broken? → **bug**
- Everything else → **task**

The question is whether bug needs to exist as a separate kind or if it's just a task with a `bug` label. The argument for keeping it as a kind: the triage engine can weight bugs higher by default without requiring every agent to remember to add the right label. The argument against: it's one more concept, and the line between "bug" and "task" is often blurry ("is a missing feature a bug or a task?").

I'd keep it. Three is a small number, the identity test is clear, and the triage engine benefits from the signal. But I could see an argument for just two (goal, task) with labels doing the rest.

What's your take — do you want bugs as a first-class kind or as a label convention?

BOB:

I like that. One thing I'm missing from beads is being able to easily set of phases of work and see everything that's needed for that phase to be complete. It has epics, but if you use deps to set up epics the "br ready" command gets confused. I really want to say "all these task regardless of dependencies are part of goal 'Phase 1' once they're done then the goal is done."

CLAUDE:

Right, so you want goals to be **containers**, not just dependency roots. The distinction matters:

In beads, an epic is sort of a super-task that other things block. So "Phase 1" would be an epic, and you'd set up dependencies: task A blocks Phase 1, task B blocks Phase 1, etc. But then `br ready` looks at Phase 1 and says "this is blocked by 15 things, it's not ready" — which is technically correct but useless. You didn't want to know if Phase 1 is ready to work on. You wanted to know _how close Phase 1 is to being done_.

What you actually want is a **parent-child** relationship that's distinct from the **blocks** relationship:

- `bn-a3f8` **is child of** `bn-goal-1` → "this task is part of Phase 1"
- `bn-a3f8` **blocks** `bn-c7d2` → "this task must finish before that task can start"

These are two completely different graph edges. Beads conflates them by using dependencies for both, which is why `br ready` gets confused — it can't tell "blocked because prerequisite work isn't done" from "blocked because it's a container waiting for its contents."

In Bones, goals are parents. A goal is done when all its children are done. That's it. No dependency edge needed.

```
bn create "Phase 1: Auth Migration" --kind goal
bn create "Implement JWT rotation" --parent bn-p1
bn create "Update OIDC provider" --parent bn-p1
bn create "Write migration runbook" --parent bn-p1
```

Now `bn show bn-p1` shows:

```
Phase 1: Auth Migration [goal, open]
  Progress: 1/3 (33%)
  ├── ✓ bn-a3f8  Implement JWT rotation      [done]
  ├──   bn-c7d2  Update OIDC provider        [doing]
  └──   bn-e5f6  Write migration runbook      [open]
```

And `bn next` never suggests working on the goal itself — it suggests the children. The goal auto-completes (or the agent completes it) when the last child reaches done.

Meanwhile, blocking relationships remain independent:

```
bn link bn-a3f8 --blocks bn-c7d2   # JWT rotation must finish before OIDC update
```

That blocking edge lives between two tasks within the same goal. It affects the order `bn next` suggests them, but it has nothing to do with the goal's completion status. The goal doesn't care about internal ordering — it just cares that everything under it reaches done.

This also gives you **nested goals** for free:

```
Phase 1: Auth Migration [goal]
├── Auth Backend [goal]
│   ├── Implement JWT rotation [task]
│   └── Update OIDC provider [task]
└── Auth Docs [goal]
    ├── Write migration runbook [task]
    └── Update API reference [task]
```

Phase 1 is done when Auth Backend and Auth Docs are both done. Auth Backend is done when both its tasks are done. The completion rolls up naturally through the tree.

The key commands:

```bash
bn create "Phase 1" --kind goal              # Create a goal
bn create "JWT rotation" --parent bn-p1      # Add a task under it
bn move bn-a3f8 --parent bn-p1              # Reparent an existing task into a goal
bn progress bn-p1                            # Show completion status
bn plan bn-p1                                # Parallel execution plan for this goal's children
```

And for agents with `--json`:

```json
{
  "id": "bn-p1",
  "title": "Phase 1: Auth Migration",
  "kind": "goal",
  "progress": { "done": 1, "total": 3, "percent": 33 },
  "children": ["bn-a3f8", "bn-c7d2", "bn-e5f6"],
  "blocked_children": ["bn-c7d2"],
  "ready_children": ["bn-e5f6"]
}
```

So the two graph types in Bones are:

- **Parent-child** (containment): "this task is part of this goal." Determines completion. No ordering implied.
- **Blocks** (dependency): "this task must finish before that task can start." Determines scheduling order. No containment implied.

They're orthogonal. A task can be part of Goal A and blocked by a task in Goal B. That's fine — it just means Goal A can't finish until that cross-goal dependency is resolved, which is exactly the kind of thing the triage engine should surface.

Does that match what you had in mind?

BOB:

Yeah, does that fit with what you had on mind?

CLAUDE:

Yeah, perfectly. The original design already had `parent` as an LWW Register on every work item, and the CRDT layer in the advanced research doc proposed Loro's movable tree for the hierarchy. This just makes explicit what was implicit — that the parent-child edge is a _containment_ relationship, not a dependency, and goals are defined by that containment.

The one thing I want to nail down: **does a goal auto-transition to done when its last child completes, or does someone explicitly close it?**

I'm leaning toward explicit. Here's why:

An agent marks the last task in Phase 1 as done. If the goal auto-closes, there's no moment where someone looks at the whole and says "yes, Phase 1 is actually complete." Maybe the last task got marked done prematurely, or maybe finishing those three tasks revealed a fourth task that needs to be added. Auto-close removes that checkpoint.

But we should make it _obvious_ that the goal is ready to close. So the model would be:

- Goal stays `open` even when all children are `done`
- `bn next` surfaces it: "All children of bn-p1 are done. Close it? `bn done bn-p1`"
- `bn health` flags goals at 100% that haven't been closed as needing attention
- The agent or human makes the explicit call

This also means a goal at 100% children done but still `open` is a natural place to do a retrospective, add notes, or realize "actually we need one more task."

The other option — auto-close — is simpler and probably fine for agent-heavy workflows where nobody's doing retrospectives. Could be a config toggle:

```yaml
goals:
  auto_complete: false # default: require explicit close
```

What do you think — explicit close, auto-close, or configurable?

BOB:

Default auto-close, but I like configurable.

CLAUDE:

Makes sense. Agents don't want to babysit goal completion — if all the children are done, the goal is done. Humans who want the checkpoint can flip the config.

So the event flow for auto-close: when an agent runs `bn done bn-e5f6` and that's the last open child of `bn-p1`, Bones emits two events:

```
1708012300 claude-abc item.move bn-e5f6 {"state":"done"}
1708012300 bones item.move bn-p1 {"state":"done","reason":"all children complete"}
```

The second event has actor `bones` (the system itself), not the agent. This makes it clear in the audit trail that the goal was auto-closed, not explicitly closed by someone. And if `auto_complete: false`, that second event simply doesn't get emitted.

One edge case to handle: what if someone adds a new child to an already-auto-closed goal? The goal should reopen:

```
1708012400 gemini-xyz item.create bn-f7g8 {"title":"Fix token edge case","parent":"bn-p1"}
1708012400 bones item.move bn-p1 {"state":"open","reason":"new child added"}
```

That feels right — the goal reopens because it's no longer complete. Same thing if a done child gets reopened.

OK, I think we've converged on the full work item model. Want me to write the revised model section as a clean doc? It would cover kinds (3), states (4), urgency (3), sizes (optional, 7), and the parent-child containment model with auto-close. That way you have one definitive reference instead of the decisions being scattered across our conversation.

BOB:

I would love it if you projected the original doc through the whole conversation, the advanced research, the storage, all the changes. Add on to the plan that we might want a migrate-from-beads utility and a ratatui tui via 'bn ui' based on <https://github.com/bobisme/beads-tui> (~/src/beads-tui/) that I created.
