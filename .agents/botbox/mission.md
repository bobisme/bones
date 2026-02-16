# Missions

End-to-end guide for mission-based work — coordinated multi-agent tasks with shared context. A mission is a parent bead that decomposes into child beads, dispatches parallel workers, monitors progress, and synthesizes results.

## When to Use Missions

Use missions (execution level 4) for tasks that:
- Need decomposition into multiple related beads
- Benefit from shared context (outcome, constraints, stop criteria)
- Can parallelize across workers but need coordination

For simpler work: level 2 (single bead, sequential) or level 3 (parallel dispatch, independent beads). If the task fits in one reviewable change, skip missions entirely and use the [worker-loop](worker-loop.md).

## Mission Lifecycle

### 1. Create the Mission Bead

Create a bead with the `mission` label and a structured description:

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

**Description fields (all required):**
- **Outcome**: One sentence — what does "done" look like?
- **Success metric**: How to verify the outcome objectively
- **Constraints**: Scope boundaries, forbidden actions, resource limits
- **Stop criteria**: When to stop even if not everything is perfect

### 2. Decompose into Children

Create child beads for each unit of work. Each child gets a `mission:<mission-id>` label and a parent dependency:

```bash
# Create child bead
maw exec default -- br create --actor $AGENT --owner $AGENT \
  --title="Add OAuth callback handler" \
  --labels "mission:bd-abc" \
  --type=task --priority=2 \
  --description="Handle OAuth provider callbacks, exchange code for token. Acceptance: callback endpoint returns 200 with valid session."

# Wire parent dependency
maw exec default -- br dep add --actor $AGENT <child-id> <mission-id>

# Wire inter-child dependencies if needed
maw exec default -- br dep add --actor $AGENT <later-child> <earlier-child>
```

**Rules:**
- Every child has both a `mission:<id>` label and a parent dependency on the mission bead
- A child belongs to at most one mission
- Maximum children per mission: `agents.dev.missions.maxChildren` (default 12)
- Assign risk labels per child (see [planning](planning.md) for risk level guidelines)
- Look for parallelism — don't chain children linearly when they can run concurrently

Verify the dependency graph:

```bash
maw exec default -- br dep tree <mission-id>
```

Announce the plan:

```bash
bus send --agent $AGENT $BOTBOX_PROJECT "Mission <mission-id>: <title> — created N child beads" -L task-claim
```

### 3. Dispatch Workers

For each unblocked child, dispatch a worker agent. The dev-loop handles this automatically, but here is the pattern:

```bash
# Generate worker name and create workspace
WORKER=$(bus generate-name)
maw ws create --random  # → e.g., frost-castle

# Stake claims
bus claims stake --agent $AGENT "bead://$BOTBOX_PROJECT/<child-id>" -m "<child-id>"
bus claims stake --agent $AGENT "workspace://$BOTBOX_PROJECT/frost-castle" -m "<child-id>"

# Add mission context comment to child bead
maw exec default -- br comments add --actor $AGENT --author $AGENT <child-id> \
  "Mission context: <mission-id> — <outcome>. Siblings: <sibling-ids>. Workspace: frost-castle"

# Spawn worker with mission env vars
botty spawn --pass-env --timeout 600 $WORKER \
  botbox run worker-loop \
  --env "BOTBOX_BEAD=<child-id>" \
  --env "BOTBOX_WORKSPACE=frost-castle" \
  --env "BOTBOX_MISSION=<mission-id>" \
  --env "BOTBOX_MISSION_OUTCOME=Users can log in via OAuth providers" \
  --env "BOTBOX_SIBLINGS=bd-001 (Add OAuth config) [owner:none, status:open]\nbd-002 (Add callback handler) [owner:storm-raven, status:in_progress]" \
  --env "BOTBOX_FILE_HINTS=bd-001: likely edits src/config.rs\nbd-002: likely edits src/auth/callback.rs"
```

Maximum concurrent workers: `agents.dev.missions.maxWorkers` (default 4). Queue remaining children until a worker slot opens.

### 4. Monitor via Checkpoints

Run checkpoints every `agents.dev.missions.checkpointIntervalSec` seconds (default 30).

Each checkpoint:

1. **Count children by status:**
   ```bash
   maw exec default -- br list -l "mission:<mission-id>" --json
   ```
   Tally: N open, M in_progress, K closed, J blocked.

2. **Check alive workers:**
   ```bash
   botty list --format json
   ```
   Cross-reference with dispatched worker names.

3. **Poll for completions** (cursor-based — track last-seen message ID):
   ```bash
   bus history $BOTBOX_PROJECT -n 20 -L task-done --since <last-checkpoint-time>
   ```

4. **Detect dead workers:** If a worker is not in `botty list` but its bead is still `in_progress`, trigger crash recovery (see below).

5. **Dispatch queued children:** If a worker slot opened and unblocked children remain, dispatch a new worker.

6. **Post checkpoint summary:**
   ```bash
   bus send --agent $AGENT $BOTBOX_PROJECT "Mission <mission-id> checkpoint: K/N done, J blocked, M active" -L feedback
   ```

Exit the checkpoint loop when all children are closed, or no workers are alive and all remaining beads are blocked.

### 5. Handle Failures

**One-retry-then-block policy** for crashed workers:

1. Worker dies (not in `botty list`, bead still `in_progress`) with no `RETRY:1` marker in comments:
   - Comment: `"Worker <name> died. RETRY:1 — reassigning."`
   - Check if workspace still exists; create new one if destroyed
   - Re-dispatch with a new worker name

2. Worker dies again (`RETRY:1` marker already exists):
   - Comment: `"Worker died again after retry. Blocking bead."`
   - `maw exec default -- br update --actor $AGENT <child-id> --status=blocked`
   - Destroy workspace if it exists: `maw ws destroy <ws>`
   - Release claims: `bus claims release --agent $AGENT "bead://$BOTBOX_PROJECT/<child-id>"`
   - Announce: `bus send --agent $AGENT $BOTBOX_PROJECT "Bead <child-id> blocked: worker died twice" -L task-blocked`

**Blocked children:** If all remaining children are blocked, investigate. Check bead comments for details, consider rescoping or splitting the blocked work.

### 6. Close the Mission

When all children are closed:

1. **Verify:** `maw exec default -- br list -l "mission:<mission-id>"` — all should be closed.

2. **Write synthesis log** as a bead comment:
   ```bash
   maw exec default -- br comments add --actor $AGENT --author $AGENT <mission-id> \
     "Mission complete.\n\nChildren: N total, all closed.\nKey decisions: <what changed during execution>\nWhat worked: <patterns that succeeded>\nWhat to avoid: <patterns that failed>\nKey artifacts: <files/modules created or modified>"
   ```

3. **Close the mission bead:**
   ```bash
   maw exec default -- br close --actor $AGENT <mission-id> --reason="All children completed"
   ```

4. **Announce:**
   ```bash
   bus send --agent $AGENT $BOTBOX_PROJECT "Mission <mission-id> complete: <title> — N children, all done" -L task-done
   ```

## Risk Labels in Missions

Each child bead gets its own risk label independently. The mission bead itself does not have a risk label — risk is assessed per child.

- **risk:low** children skip review and merge directly after self-review
- **risk:medium** (default) children go through standard crit review
- **risk:high** children require security review with failure-mode checklist
- **risk:critical** children require human approval before merge, even within a mission

The dev-loop does not override child risk levels. If a child is `risk:critical`, the mission pauses on that child until human approval is received.

See [planning](planning.md) for how to assign risk labels.

## Coordination Labels

Workers in a mission coordinate via labeled bus messages. Always include `mission:<mission-id>` alongside the coordination label.

| Label | When to use |
|-------|-------------|
| `coord:interface` | You define or change a public API, type, or contract that siblings consume |
| `coord:blocker` | You are blocked on a sibling's output |
| `coord:handoff` | You are transferring partial work to another worker |

Example:

```bash
bus send --agent $AGENT $BOTBOX_PROJECT "Interface: createUser(name, email) returns User" \
  -L coord:interface -L "mission:bd-abc"
```

See [coordination](coordination.md) for the full protocol including sibling awareness and file ownership.

## Configuration

Mission settings in `.botbox.json` under `agents.dev.missions`:

```json
{
  "agents": {
    "dev": {
      "missions": {
        "enabled": false,
        "maxWorkers": 4,
        "maxChildren": 12,
        "checkpointIntervalSec": 30
      }
    }
  }
}
```

| Key | Default | Purpose |
|-----|---------|---------|
| `enabled` | `false` | Enable mission dispatch (safe rollout — must opt in) |
| `maxWorkers` | `4` | Maximum concurrent worker agents per mission |
| `maxChildren` | `12` | Maximum child beads per mission |
| `checkpointIntervalSec` | `30` | Seconds between checkpoint polls |

## Environment Variables

Workers receive mission context via environment variables set by the dev-loop at dispatch time.

| Variable | Set by | Read by | Value |
|----------|--------|---------|-------|
| `BOTBOX_MISSION` | dev-loop | agent-loop | Mission bead ID (e.g., `bd-abc`) |
| `BOTBOX_BEAD` | dev-loop | agent-loop | Assigned child bead ID — skip triage, work this bead |
| `BOTBOX_WORKSPACE` | dev-loop | agent-loop | Pre-created workspace name — skip workspace creation |
| `BOTBOX_MISSION_OUTCOME` | dev-loop | agent-loop | Outcome line from mission description — shared context |
| `BOTBOX_SIBLINGS` | dev-loop | agent-loop | One line per sibling: `<id> (<title>) [owner:<name>, status:<status>]` |
| `BOTBOX_FILE_HINTS` | dev-loop | agent-loop | Advisory file ownership: `<id>: likely edits <files>` per line |

When `BOTBOX_BEAD` and `BOTBOX_WORKSPACE` are set, agent-loop skips triage and starts working immediately on the assigned bead in the given workspace.

When `BOTBOX_MISSION` is set, agent-loop reads the mission bead for shared context and includes `mission:<id>` labels on bus messages.
