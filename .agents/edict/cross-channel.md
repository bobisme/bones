# Cross-Channel Communication

How to use rite threads, ask and wait for an answer, and shape a message. Then how to communicate with other projects: ask questions, report bugs, give feedback, and track responses.

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

## Threads

Every message you send answers something or starts something. Anchor the answers.

- `rite send ... --reply-to <id>` anchors a message under a parent. A message without
  `--reply-to` is top-level.
- When a rite hook spawned you, the message that woke you is `$RITE_MESSAGE_ID`. Answer it:
  `rite send --agent $AGENT "$RITE_CHANNEL" "on it" --reply-to "$RITE_MESSAGE_ID"`.
- A lease hook can hand you a batch. It also sets `$RITE_BATCH_MESSAGE_IDS`, a
  comma-separated list in chronological order. The anchor is the LAST id in that list
  (the message that fired the hook, the same id as `$RITE_MESSAGE_ID`), not the first.
- Your prompt carries the anchor for the current turn. Use that one. Never reuse the anchor
  from an earlier turn.
- An anchor that is not in the store yet gives a warning, not an error. The reply links up
  when the parent syncs in.
- `rite history --thread <id>` reads the whole thread from any message in it, and finds the
  channel itself. A thread reported `complete:false` is a fragment: say so, and do not
  present it as the whole conversation.
- Answer a question with `--reply-to` the message that asked. A top-level answer leaves the
  asker blocked until their wait times out.

## Ask and wait

Never post a question and hope. Capture the id of the question, then block on a reply to it:

```bash
id=$(rite send --agent $AGENT <channel> "<question> @<target>" \
  -L feedback --format json | jq -r .id)

rite wait --agent $AGENT --reply-to "$id" -t 300 --format json
```

Always pass `-t`. The default is `-t 0`, which means no timeout: the wait blocks forever.

| Exit | Meaning | What to do |
|------|---------|------------|
| 0 | Answered | Read `.message.body` from the JSON. Act on it. Continue your task. |
| 1 | No answer inside the timeout | Post one `-L task-blocked` naming the anchor, record it on the bone, and move on. For another project, use a tracking bone ([step 2](#2-on-exit-1-escalate-and-record-the-anchor)). **Never re-send the request.** |
| 2 | Not a ULID, or this store never saw the id | Fix the id: `rite history <channel> --from $AGENT -n 1 --format json` returns `last_id`. Do not re-send. Add `--allow-missing-parent` only when the parent is still syncing in from another machine. |

How the wait matches:

- `--reply-to` narrows the wait, it never widens it. `--from`, `-c` and `-L` only subtract
  candidate answers.
- With no `-c`, every channel counts, so an answer sent as a DM satisfies the wait. Omit `-c`.
- An answer that arrived before the wait started still counts. There is no race.
- Your own reply never satisfies your own wait.
- Read the exchange at any time with `rite history --thread "$id"`.

## Message shape

Keep a channel message to one labelled line that leads with the bone id. The label and the bone
id already carry the context, so do not add status blocks, numbered steps, or closing actions:

- `-L task-claim "<bone-id>: <title>"`
- `-L review-request "<bone-id>: security review <review-id>"`
- `-L task-blocked "<bone-id>: blocked on <thing>, needs <what unblocks it>"`

A message with no bone, such as a cross-project question, leads with its subject instead.
Anchor an answer with `--reply-to` instead of quoting the message you answer.

## Steps: ask another project

### 1. Ask the project channel and wait for the answer

Use [Ask and wait](#ask-and-wait) against the project channel. Address the project's lead
agent:

```bash
id=$(rite send --agent $AGENT <project> \
  "Getting error X when running seal inbox. Is this expected? <details> @<project>-dev" \
  -L feedback --format json | jq -r .id)

rite wait --agent $AGENT --reply-to "$id" -t 300 --format json
```

On exit 0, act on the answer. On exit 1, go to step 2. On exit 2, fix the id as the
[exit-code table](#ask-and-wait) says.

For **bugs or feature requests**, create a bone in their repo first:
```bash
cd <repo-path> && bn create \
  --title "<clear bug/feature title>" \
  --description "<repro steps, context, your use case>" \
  --tag bug \
  --kind bug
```

Then post to their channel, and wait on the id as in [Ask and wait](#ask-and-wait):
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

- `@mention` the lead agent (e.g., `@seal-dev`) to name who should answer. Do not count on
  the mention to start an agent: the anchored wait is what brings the answer back to you
- Answer questions the same way you want to be answered (see [Threads](#threads))
- Use `-L feedback` label on rite messages so the lead agent can filter for external reports
- Include enough context for the other agent to understand and reproduce your issue
- The `#projects` channel contains the registry of all projects
- Default lead agent naming: `<project>-dev` (e.g., `vessel-dev`, `seal-dev`)
