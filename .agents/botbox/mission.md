# Missions

End-to-end guide for mission-based work — coordinated multi-agent tasks with shared context. A mission is a parent bone that decomposes into child bones, dispatches parallel workers, monitors progress, and synthesizes results.

## When to Use Missions

Use missions (execution level 4) for tasks that:
- Need decomposition into multiple related bones
- Benefit from shared context (outcome, constraints, stop criteria)
- Can parallelize across workers but need coordination

For simpler work: level 2 (single bone, sequential) or level 3 (parallel dispatch, independent bones). If the task fits in one reviewable change, skip missions entirely and use the [worker-loop](worker-loop.md).

## Mission Lifecycle

### 1. Create the Mission Bone

Create a bone with the `mission` tag and a structured description:

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

**Description fields (all required):**
- **Outcome**: One sentence — what does "done" look like?
- **Success metric**: How to verify the outcome objectively
- **Constraints**: Scope boundaries, forbidden actions, resource limits
- **Stop criteria**: When to stop even if not everything is perfect

### 2. Decompose into Children

Create child bones for each unit of work. Each child gets a `mission:<mission-id>` tag and a parent dependency:

```bash
# Create child bone
maw exec default -- bn create \
  --title "Add OAuth callback handler" \
  --tag "mission:bd-abc" \
  --kind task \
  --description "Handle OAuth provider callbacks, exchange code for token. Acceptance: callback endpoint returns 200 with valid session."

# Wire parent dependency
maw exec default -- bn triage dep add <mission-id> --blocks <child-id>

# Wire inter-child dependencies if needed
maw exec default -- bn triage dep add <earlier-child> --blocks <later-child>
```

**Rules:**
- Every child has both a `mission:<id>` tag and a parent dependency on the mission bone
- A child belongs to at most one mission
- Maximum children per mission: `agents.dev.missions.maxChildren` (default 12)
- Assign risk tags per child (see [planning](planning.md) for risk level guidelines)
- Look for parallelism — don't chain children linearly when they can run concurrently

Verify the dependency graph:

```bash
maw exec default -- bn triage graph
```

Announce the plan:

```bash
bus send --agent $AGENT $BOTBOX_PROJECT "Mission <mission-id>: <title> — created N child bones" -L task-claim
```

### 3. Dispatch Workers

For each unblocked child, dispatch a worker agent. The dev-loop handles this automatically, but here is the pattern:

```bash
# Generate worker name and create workspace
WORKER=$(bus generate-name)
maw ws create --random  # → e.g., frost-castle

# Stake claims
bus claims stake --agent $AGENT "bone://$BOTBOX_PROJECT/<child-id>" -m "<child-id>"
bus claims stake --agent $AGENT "workspace://$BOTBOX_PROJECT/frost-castle" -m "<child-id>"

# Add mission context comment to child bone
maw exec default -- bn bone comment add <child-id> \
  "Mission context: <mission-id> — <outcome>. Siblings: <sibling-ids>. Workspace: frost-castle"

# Spawn worker with mission env vars
botty spawn --pass-env --timeout 600 $WORKER \
  botbox run worker-loop \
  --env "BOTBOX_BONE=<child-id>" \
  --env "BOTBOX_WORKSPACE=frost-castle" \
  --env "BOTBOX_MISSION=<mission-id>" \
  --env "BOTBOX_MISSION_OUTCOME=Users can log in via OAuth providers" \
  --env "BOTBOX_SIBLINGS=bd-001 (Add OAuth config) [owner:none, state:open]\nbd-002 (Add callback handler) [owner:storm-raven, state:doing]" \
  --env "BOTBOX_FILE_HINTS=bd-001: likely edits src/config.rs\nbd-002: likely edits src/auth/callback.rs"
```

Maximum concurrent workers: `agents.dev.missions.maxWorkers` (default 4). Queue remaining children until a worker slot opens.

### 4. Monitor via Checkpoints

Run checkpoints every `agents.dev.missions.checkpointIntervalSec` seconds (default 30).

Each checkpoint:

1. **Count children by state:**
   ```bash
   maw exec default -- bn list --tag "mission:<mission-id>" --format json
   ```
   Tally: N open, M doing, K done.

2. **Check alive workers:**
   ```bash
   botty list --format json
   ```
   Cross-reference with dispatched worker names.

3. **Poll for completions** (cursor-based — track last-seen message ID):
   ```bash
   bus history $BOTBOX_PROJECT -n 20 -L task-done --since <last-checkpoint-time>
   ```

4. **Detect dead workers:** If a worker is not in `botty list` but its bone is still `doing`, trigger crash recovery (see below).

5. **Dispatch queued children:** If a worker slot opened and unblocked children remain, dispatch a new worker.

6. **Post checkpoint summary:**
   ```bash
   bus send --agent $AGENT $BOTBOX_PROJECT "Mission <mission-id> checkpoint: K/N done, M active" -L feedback
   ```

Exit the checkpoint loop when all children are done, or no workers are alive and all remaining bones are stuck.

### 5. Handle Failures

**One-retry-then-block policy** for crashed workers:

1. Worker dies (not in `botty list`, bone still `doing`) with no `RETRY:1` marker in comments:
   - Comment: `"Worker <name> died. RETRY:1 — reassigning."`
   - Check if workspace still exists; create new one if destroyed
   - Re-dispatch with a new worker name

2. Worker dies again (`RETRY:1` marker already exists):
   - Comment: `"Worker died again after retry. Marking done with failure."`
   - Destroy workspace if it exists: `maw ws destroy <ws>`
   - Release claims: `bus claims release --agent $AGENT "bone://$BOTBOX_PROJECT/<child-id>"`
   - Announce: `bus send --agent $AGENT $BOTBOX_PROJECT "Bone <child-id> failed: worker died twice" -L task-blocked`

### 6. Close the Mission

When all children are done:

1. **Verify:** `maw exec default -- bn list --tag "mission:<mission-id>"` — all should be done.

2. **Write synthesis log** as a bone comment:
   ```bash
   maw exec default -- bn bone comment add <mission-id> \
     "Mission complete.\n\nChildren: N total, all done.\nKey decisions: <what changed during execution>\nWhat worked: <patterns that succeeded>\nWhat to avoid: <patterns that failed>\nKey artifacts: <files/modules created or modified>"
   ```

3. **Close the mission bone:**
   ```bash
   maw exec default -- bn done <mission-id> --reason "All children completed"
   ```

4. **Announce:**
   ```bash
   bus send --agent $AGENT $BOTBOX_PROJECT "Mission <mission-id> complete: <title> — N children, all done" -L task-done
   ```

## Risk Tags in Missions

Each child bone gets its own risk tag independently. The mission bone itself does not have a risk tag — risk is assessed per child.

- **risk:low** children skip review and merge directly after self-review
- **risk:medium** (default) children go through standard crit review
- **risk:high** children require security review with failure-mode checklist
- **risk:critical** children require human approval before merge, even within a mission

The dev-loop does not override child risk levels. If a child is `risk:critical`, the mission pauses on that child until human approval is received.

See [planning](planning.md) for how to assign risk tags.

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
| `maxChildren` | `12` | Maximum child bones per mission |
| `checkpointIntervalSec` | `30` | Seconds between checkpoint polls |

## Environment Variables

Workers receive mission context via environment variables set by the dev-loop at dispatch time.

| Variable | Set by | Read by | Value |
|----------|--------|---------|-------|
| `BOTBOX_MISSION` | dev-loop | agent-loop | Mission bone ID (e.g., `bd-abc`) |
| `BOTBOX_BONE` | dev-loop | agent-loop | Assigned child bone ID — skip triage, work this bone |
| `BOTBOX_WORKSPACE` | dev-loop | agent-loop | Pre-created workspace name — skip workspace creation |
| `BOTBOX_MISSION_OUTCOME` | dev-loop | agent-loop | Outcome line from mission description — shared context |
| `BOTBOX_SIBLINGS` | dev-loop | agent-loop | One line per sibling: `<id> (<title>) [owner:<name>, state:<state>]` |
| `BOTBOX_FILE_HINTS` | dev-loop | agent-loop | Advisory file ownership: `<id>: likely edits <files>` per line |

When `BOTBOX_BONE` and `BOTBOX_WORKSPACE` are set, agent-loop skips triage and starts working immediately on the assigned bone in the given workspace.

When `BOTBOX_MISSION` is set, agent-loop reads the mission bone for shared context and includes `mission:<id>` labels on bus messages.
