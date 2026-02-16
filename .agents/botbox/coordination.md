# Mission Coordination

When working as part of a mission (multiple workers on related beads), use these conventions to coordinate with siblings.

## Coordination Labels

Use these labels on bus messages alongside `mission:<mission-id>`:

| Label | Purpose | Example |
|-------|---------|---------|
| `coord:interface` | Share API shape, types, or contracts that siblings depend on | `bus send --agent $AGENT $PROJECT "Interface: createUser(name, email) returns User" -L coord:interface -L "mission:bd-xxx"` |
| `coord:blocker` | Flag a blocking dependency on a sibling's work | `bus send --agent $AGENT $PROJECT "Blocked by bd-yyy: need User type exported" -L coord:blocker -L "mission:bd-xxx"` |
| `coord:handoff` | Transfer partial work or context to another worker | `bus send --agent $AGENT $PROJECT "Handoff bd-yyy: auth middleware done, needs route wiring" -L coord:handoff -L "mission:bd-xxx"` |

## Sibling Awareness

When dispatched as part of a mission, your prompt includes:
- **Mission outcome**: What the overall mission is trying to achieve
- **Sibling beads**: Other beads in the mission with their owners and status
- **File ownership hints**: Advisory list of which files other workers are likely editing

**Respect file ownership**: If a sibling is working on a file, avoid editing it. If you must, post a `coord:interface` message first and wait for acknowledgment.

## Checkpoint Protocol

The lead dev agent runs periodic checkpoints during missions:
1. Counts children by status (open/in_progress/closed/blocked)
2. Checks for alive workers via `botty list`
3. Reads completion signals from bus history
4. Posts checkpoint summaries: "Mission bd-xxx checkpoint: 3/5 done, 1 blocked"

Workers don't need to do anything special for checkpoints — just keep working and post progress comments on your bead.

## When to Use Coordination

- **Interface changes**: Always post `coord:interface` when you define or change a public API, type, or contract that other beads might consume
- **Blocking on siblings**: Post `coord:blocker` rather than silently waiting. The lead dev can reassign or reprioritize
- **Handoffs**: Use `coord:handoff` when you've done partial work that another worker should continue
- **Completion**: Always use `task-done` label with `mission:<id>` so checkpoints detect it
