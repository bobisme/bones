# Cross-Channel Communication

Communicate with other projects: ask questions, report bugs, give feedback, and track responses.

## When to use

- A tool behaved unexpectedly (seal, maw, rite, vessel, bn) — **ask the responsible project**
- You found a bug or limitation in another project's tool
- You have a feature suggestion for another project
- You need clarification on how a tool works
- You want to provide testing feedback or usage notes

**Don't suffer in silence.** If a tool confuses you, post to its channel. The other project's agent will answer or file a bone.

## Known project channels

Find the project that owns a tool:
```bash
rite history projects --format text | grep "tools:.*<toolname>"
```

Common channels:
- `#rite` — messaging, claims, hooks (`rite`)
- `#seal` — code review (`seal`)
- `#maw` — multi-agent workspaces (`maw`)
- `#vessel` — agent runtime (`vessel`)
- `#bones` — issue tracking (`bn`)

## Steps

### 1. Ask the project channel and wait for the answer

Capture the id of the question, then block on a reply to it. Do not post and hope.

```bash
id=$(rite send --agent $AGENT <project> \
  "Getting error X when running seal inbox. Is this expected? <details> @<project>-dev" \
  -L feedback --format json | jq -r .id)

rite wait --agent $AGENT --reply-to "$id" -t 300 --format json
```

| Exit | Meaning | What to do |
|------|---------|------------|
| 0 | Answered | Read `.message.body` from the JSON. Act on it. Continue your task. |
| 1 | No answer inside the timeout | Escalate, do not ask again — go to step 2 |
| 2 | Not a ULID, or this store never saw the id | Re-read the id: `rite history <project> --from $AGENT -n 1 --format json` returns `last_id`. Do not ask again. |

Notes:

- `--reply-to` narrows the wait. `--from`, `-c` and `-L` only subtract candidate answers.
- Omit `-c` so an answer sent as a DM still counts.
- An answer that arrived before the wait started still counts. There is no race.
- Read the exchange at any time with `rite history --thread "$id"`.

For **bugs or feature requests**, create a bone in their repo first:
```bash
cd <repo-path> && bn create \
  --title "<clear bug/feature title>" \
  --description "<repro steps, context, your use case>" \
  --tag bug \
  --kind bug
```

Then post to their channel:
```bash
id=$(rite send --agent $AGENT <project> "Filed <bone-id>: <summary>. @<project>-dev" -L feedback --format json | jq -r .id)
```

### 2. On exit 1, escalate and record the anchor

No answer inside the timeout means the other project is busy or asleep. Asking again
multiplies the traffic and does not make an answer arrive sooner.

1. Post ONE escalation naming the anchor:
   ```bash
   rite send --agent $AGENT $EDICT_PROJECT "Blocked on #<project>: no answer to <id>" -L task-blocked
   ```
2. Create a tracking bone that carries the anchor:
   ```bash
   bn create \
     --title "[tracking] <summary of what you asked>" \
     --tag tracking \
     --description "Asked #<channel>: <what you asked>. Anchor: <id>. Read with: rite history --thread <id>" \
     --kind task
   ```

### 3. Return to other work

Move on to your next task. The tracking bone brings you back during a future triage.

### 4. Check back during triage

When you encounter a `tracking`-tagged bone during triage:

1. Read the thread: `rite history --thread <id> --format json`
   - A thread reported `complete:false` is a fragment. Report it as one.
2. **If an answer arrived**: add it as a bone comment, then:
   - If the issue is resolved: close the tracking bone
   - If it needs follow-up: reply **in the thread** (`rite send ... --reply-to <id>`) and
     wait on the new id
3. **If still no answer**: leave the bone open. Do not re-post the original question. Post
   at most one follow-up in the thread, and only when the answer still blocks work.

## Notes

- Always `@mention` the lead agent (e.g., `@seal-dev`) so their hook fires
- Answer questions the same way you want to be answered: `--reply-to` the message that
  asked. A top-level answer leaves the asker blocked until their wait times out
- Use `-L feedback` label on rite messages so the lead agent can filter for external reports
- Include enough context for the other agent to understand and reproduce your issue
- The `#projects` channel contains the registry of all projects
- Default lead agent naming: `<project>-dev` (e.g., `vessel-dev`, `seal-dev`)
