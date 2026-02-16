# Bones Event Format: Design & Tradeoffs

## The Problem

Bones is git-native. The event log is committed, pushed, pulled, diffed, blamed, and merged like any other source file. This means the storage format isn't just an implementation detail — it's part of the developer experience. A bad choice here poisons everything downstream.

The format must satisfy six constraints simultaneously:

| Constraint | Why |
|---|---|
| **Git-friendly** | Meaningful diffs, auto-mergeable appends, `git blame` per-event |
| **Append-only** | New events go at the bottom. Old events are never modified. |
| **Human-readable** | `cat .bones/events` should be immediately comprehensible |
| **Machine-parseable** | Fast to parse in Rust, no ambiguity, no backtracking |
| **Grep-friendly** | `grep "bn-a3f8"` finds all events for an item, instantly |
| **Compact** | Minimize redundant bytes without sacrificing the above |

These constraints are in tension. Binary formats (MessagePack, Protobuf, CBOR) maximize compactness and parse speed but are opaque to git. Verbose formats (pretty-printed JSON, YAML) maximize readability but waste bytes and produce noisy diffs. The right answer is a format that's "just readable enough" while staying compact and tool-friendly.

---

## The Format: TSV with JSON Payload (TSJSON)

Every Bones event is a single line with five tab-separated fields:

```
{timestamp}\t{actor}\t{type}\t{item_id}\t{data}
```

Where:
- **timestamp** — Unix epoch seconds (integer)
- **actor** — Who produced this event (no whitespace, no tabs)
- **type** — Event type from the fixed catalog (e.g., `item.create`, `item.update`)
- **item_id** — The work item this event affects
- **data** — A JSON object containing the type-specific payload

### Example file

```
# bones event log v1
# fields: timestamp \t actor \t type \t item_id \t data
1708012200	claude-abc	item.create	bn-a3f8	{"title":"Fix auth retry","kind":"task","effort":"medium","labels":["backend"]}
1708012201	claude-abc	item.update	bn-a3f8	{"state":"doing"}
1708012202	gemini-xyz	item.create	bn-c7d2	{"title":"Payment schema migration","kind":"task","effort":"large"}
1708012215	claude-abc	item.link	bn-a3f8	{"blocks":"bn-c7d2"}
1708012230	claude-abc	item.comment	bn-a3f8	{"body":"Root cause is a race in token refresh. Fix incoming."}
1708012300	claude-abc	item.move	bn-a3f8	{"state":"done","reason":"Shipped in commit 9f3a2b1"}
```

### Header convention

The file begins with comment lines prefixed by `#`. These are ignored by the parser and serve as self-documentation. At minimum, the header declares the format version and field order so that a human encountering the file cold can immediately understand its structure.

---

## Why This Format

### Observation: every event has the same four fixed fields

Every Bones event, regardless of type, carries a timestamp, actor, event type, and item ID. In JSONL, these four fields are wrapped in key-value pairs with quoted keys on every single line:

```json
{"ts":1708012200,"actor":"claude-abc","type":"item.create","item_id":"bn-a3f8","data":{"title":"Fix auth retry"}}
```

That's 34 bytes of overhead per line just for `{"ts":`, `,"actor":"`, `,"type":"`, `,"item_id":"`, `,"data":`, and the closing `}`. Over 10,000 events, that's 340KB of repeated key names encoding zero information.

TSJSON eliminates this overhead entirely. The four fixed fields are positional — their meaning comes from their column position, not from embedded key names. Only the variable-payload `data` field retains JSON's self-describing flexibility, because that's where it's actually needed (different event types carry different payloads).

### The tab character is the separator, not a delimiter in the data

Tabs are chosen over commas or spaces because:
- Tabs almost never appear in the actual data (titles, descriptions, actor names). Commas appear constantly in natural language.
- No quoting needed for the fixed fields. The actor `claude-abc`, the type `item.create`, and the item ID `bn-a3f8` never contain tabs, so they need no escaping or quoting.
- The JSON payload in the fifth field can contain anything (including commas, colons, spaces, braces) without ambiguity, because it's the last field. The parser splits on the first four tabs and takes the remainder as JSON.

### Parse strategy: split first, JSON-parse only when needed

```rust
fn parse_event(line: &str) -> Event {
    // Skip comment lines
    if line.starts_with('#') { return Event::Comment; }

    // Split into at most 5 parts on tab
    let mut parts = line.splitn(5, '\t');
    let ts: i64    = parts.next().unwrap().parse().unwrap();
    let actor      = parts.next().unwrap();
    let event_type = parts.next().unwrap();
    let item_id    = parts.next().unwrap();
    let json_data  = parts.next().unwrap();

    // Only parse the JSON payload if we actually need the data
    Event { ts, actor, event_type, item_id, json_data }
}
```

For operations that scan the log but don't need the payload — like counting events per actor, filtering by type, or finding all events for a specific item — the JSON never needs to be parsed at all. This is a significant performance advantage for operations like `bn stats` or `bn log bn-a3f8`.

---

## Git Behavior

### Diffs

When two agents create work concurrently, the diff is clean and informative:

```diff
  1708012200	claude-abc	item.create	bn-a3f8	{"title":"Fix auth retry"}
  1708012201	claude-abc	item.update	bn-a3f8	{"state":"doing"}
+ 1708012202	gemini-xyz	item.create	bn-c7d2	{"title":"Payment schema migration"}
+ 1708012203	gemini-xyz	item.link	bn-c7d2	{"blocks":"bn-a3f8"}
```

Every added line is a complete, self-contained event. The diff tells a story: Gemini created a new task and linked it as blocking the auth fix.

### Merges

Because the file is append-only, git's default merge strategy handles the common case automatically. Both branches add lines to the end of the file — git concatenates them. No merge driver needed.

The rare pathological case: two branches both add events at the exact same byte offset (the last line of the shared ancestor). Git may report a conflict here, but it's trivially resolvable: keep both sets of lines. A future Bones merge driver could automate this, but it shouldn't be necessary in practice because agents typically work on different branches at different times.

### Blame

`git blame .bones/events/current.events` shows which commit introduced each event. This is the full audit trail: who committed the event, when, and in what context.

### Log

`git log -p -- .bones/events/` shows the history of all event additions. Because each line is a complete event, `git log -p -S "bn-a3f8"` finds every commit that added or removed an event mentioning item bn-a3f8.

---

## Unix Tool Compatibility

The tab-separated fixed fields make the format a first-class citizen of the Unix pipeline:

```bash
# All events for a specific item
grep 'bn-a3f8' .bones/events/current.events

# Count events by type
awk -F'\t' '{print $3}' .bones/events/*.events | sort | uniq -c | sort -rn

# All events by a specific actor
awk -F'\t' '$2 == "claude-abc"' .bones/events/*.events

# Events in the last hour (assuming current unix time ~1708015800)
awk -F'\t' '$1 > 1708012200' .bones/events/current.events

# Most active items
awk -F'\t' '{print $4}' .bones/events/*.events | sort | uniq -c | sort -rn | head -20

# Sort all events chronologically across all shards
sort -t$'\t' -k1n .bones/events/*.events

# Find all state transitions
grep 'item.move' .bones/events/*.events
```

None of these require a JSON parser, a Bones binary, or any special tooling. Standard Unix tools work because the high-value fields are positional and unquoted.

---

## File Organization: Sharding

A single event file that grows forever causes two problems: git diffs get slower as the file grows, and atomic appends contend when multiple processes write concurrently. Bones shards the event log by time:

```
.bones/
├── events/
│   ├── 2026-01.events     # January events (frozen, never modified)
│   ├── 2026-02.events     # February events (append-only during February)
│   └── current.events     # Symlink or redirect to the active shard
├── bones.db               # Gitignored — SQLite projection
└── cache/
    └── events.bin          # Gitignored — binary columnar cache
```

Frozen shards are immutable. Git stores them once and never diffs them again. Only the active shard appears in `git diff`, keeping diffs fast. When a shard is frozen at month's end, git sees one final diff (the last events of the month) and then the file is static forever.

The binary columnar cache (`events.bin`) is gitignored and serves purely as a performance optimization. It's rebuilt from the `.events` files on demand and discarded without consequence.

---

## Alternatives Considered

### JSONL (one JSON object per line)

```json
{"ts":1708012200,"actor":"claude-abc","type":"item.create","item_id":"bn-a3f8","data":{"title":"Fix auth retry"}}
```

**Pros:**
- Universal. Every language has a JSON parser. Every developer has seen JSONL.
- Self-describing. Each line contains its own key names — no external schema needed.
- Ecosystem tooling: `jq`, `fx`, `gron`, `python -m json.tool` all work natively.

**Cons:**
- Repetitive. The key names `"ts"`, `"actor"`, `"type"`, `"item_id"`, `"data"` are repeated on every line, encoding zero information.
- Requires full JSON parsing to extract any field. Can't `awk` by column.
- ~130 bytes per typical event (vs. ~80 for TSJSON). ~38% larger.
- Noisy diffs. The structural JSON characters (`{`, `}`, `:`, `"`) dilute the actual content in a diff.

**Verdict:** The safe choice. If Bones were a library consumed by third parties who need to parse the event log, JSONL would be the right call because it minimizes format-specific documentation. But Bones is a CLI tool that owns its parser, and agents interact via `--json` output, not by reading the event log directly. The universality of JSONL is less valuable here than the readability and performance of TSJSON.

### Compact JSONL (short keys)

```json
{"t":"item.create","i":"bn-a3f8","ts":1708012200,"a":"claude-abc","d":{"title":"Fix auth retry"}}
```

**Pros:**
- Still JSON. All JSON tooling works.
- ~95 bytes per event. ~27% smaller than verbose JSONL.

**Cons:**
- Short keys are cryptic. What's `"t"` vs `"ts"`? Requires documentation.
- Still has the quoting overhead for every key and string value.
- Still requires full JSON parsing to extract any field.
- Worst of both worlds: sacrifices readability without reaching TSJSON's compactness or parseability.

**Verdict:** A half-measure. If you're going to deviate from standard JSONL, go further and get the real benefits. If you're going to stay with JSON, stay with readable keys.

### CSV/TSV (fully columnar)

```
1708012200	claude-abc	item.create	bn-a3f8	title	Fix auth retry
1708012200	claude-abc	item.create	bn-a3f8	kind	task
1708012200	claude-abc	item.create	bn-a3f8	effort	medium
```

**Pros:**
- Maximally compact. ~60 bytes per field.
- Pure TSV — every tool in the Unix universe handles it.
- Each field is independently greppable and awk-able.

**Cons:**
- A single logical event (item.create with title, kind, effort, labels) explodes into multiple lines. This breaks the "one line = one event" invariant, making diffs confusing and blame useless.
- Variable-length fields (descriptions with newlines, JSON arrays for labels) require escaping that kills readability.
- No natural way to represent nested data (labels as a list, comment body as multi-line text).
- Schema rigidity. Adding a new field to the event data requires changing the column structure.

**Verdict:** Great for fixed-schema tabular data, terrible for semi-structured events with variable payloads. The one-event-per-line invariant is non-negotiable for git friendliness, and full TSV violates it.

### Binary formats (MessagePack, CBOR, Protobuf, FlatBuffers)

**Pros:**
- Maximum compactness (2-10 bytes per event).
- Maximum parse speed.
- Schema evolution support (Protobuf, FlatBuffers).

**Cons:**
- `git diff` shows "Binary files differ." Full stop.
- `git merge` requires a custom merge driver.
- `cat`, `grep`, `awk`, `less` are all useless.
- `git blame` is meaningless.
- Recreates the exact problem Bones exists to solve (beads's SQLite-as-source-of-truth had the same opacity issue).

**Verdict:** Rejected for the git-tracked source of truth. Appropriate only for gitignored caches and projections.

### YAML stream (documents separated by `---`)

```yaml
---
ts: 1708012200
actor: claude-abc
type: item.create
item_id: bn-a3f8
data:
  title: Fix auth retry
  kind: task
```

**Pros:**
- Most human-readable of all options.
- Handles nested data naturally.

**Cons:**
- One event = 6-10 lines. Diffs become very tall. Blame is per-field, not per-event.
- Whitespace-sensitive. A trailing space or wrong indentation can silently change semantics.
- YAML parsing is notoriously slow and the spec is enormous (80+ pages).
- Append-only files with `---` separators are uncommon and tool support is spotty.
- Multi-line strings (descriptions) require block scalars with specific indentation — fragile in diffs.

**Verdict:** Optimizes for the wrong thing. The event log is read by machines 99% of the time. Maximizing human readability at the cost of everything else is the wrong tradeoff.

### S-expressions

```
(item.create bn-a3f8 1708012200 claude-abc (title "Fix auth retry") (kind task))
```

**Pros:**
- Extremely compact. ~70 bytes per event.
- Trivially parseable (recursive descent in 20 lines of code).
- One line per event.

**Cons:**
- Nobody outside the Lisp world recognizes the format on sight.
- No standard for how to represent maps/objects (is it `(key value)` pairs? Association lists?).
- No ecosystem tooling (`jq` equivalent doesn't exist).
- Would require extensive documentation for contributors.

**Verdict:** Intellectually appealing, practically a dead end. The cognitive overhead for every new contributor isn't worth the marginal compactness gain over TSJSON.

---

## Comparison Matrix

| Property | JSONL | Compact JSONL | TSJSON | Full TSV | Binary | YAML |
|---|---|---|---|---|---|---|
| Bytes per event (typical) | ~130 | ~95 | ~80 | ~60* | ~5 | ~180 |
| Git diff quality | Good | Good | Excellent | Poor* | None | Poor |
| Git merge (append) | Auto | Auto | Auto | Auto | Manual | Auto |
| Git blame usefulness | Per-event | Per-event | Per-event | Per-field* | None | Per-field |
| Human readability | Good | Fair | Very good | Good | None | Excellent |
| Parse speed (full) | Medium | Medium | Fast | Fast | Fastest | Slow |
| Partial parse (skip payload) | No | No | Yes | Yes | No | No |
| Unix tool compatibility | Via `jq` | Via `jq` | Native | Native | None | Weak |
| Schema flexibility | Full | Full | Full | Rigid | Full | Full |
| One event = one line | Yes | Yes | Yes | No* | N/A | No |
| Ecosystem tooling | Excellent | Good | Minimal | Excellent | Varies | Good |
| Documentation needed | None | Some | Some | None | Extensive | None |

\* Full TSV scores are for the multi-line-per-event variant. If you could fit events on one line, TSV would score well — but you can't, because of variable payloads.

---

## The TSJSON Specification (v1)

### File extension

`.events`

### Encoding

UTF-8. No BOM.

### Line format

Each non-comment line is a single event with exactly five tab-separated fields:

```
TIMESTAMP \t ACTOR \t TYPE \t ITEM_ID \t DATA \n
```

| Field | Type | Constraints |
|---|---|---|
| TIMESTAMP | Integer | Unix epoch seconds. No fractional part. |
| ACTOR | String | No whitespace. Identifies the agent or human. |
| TYPE | String | One of the fixed event type catalog. No whitespace. |
| ITEM_ID | String | Bones item identifier (e.g., `bn-a3f8`). No whitespace. |
| DATA | JSON object | Valid JSON object. Must not contain unescaped newlines. |

### Comment lines

Lines beginning with `#` are comments. They are ignored by the parser and may appear anywhere in the file (though by convention they appear only at the top as a header).

### Newline handling in DATA

The DATA field is a single-line JSON object. If a value within the JSON contains a newline (e.g., a multi-line description), it must be represented as `\n` within the JSON string, per the JSON specification. The literal line-feed character (0x0A) must never appear within a DATA field.

### Ordering

Events within a shard should be in approximately chronological order (by TIMESTAMP), but strict ordering is not required. The CRDT merge guarantees convergence regardless of event ordering. Consumers must not assume sorted order.

### Event type catalog

```
item.create     — Create a new work item
item.update     — Update fields (title, description, effort, labels, etc.)
item.move       — Transition to a new state (open → doing → done → archived)
item.assign     — Assign/unassign an actor
item.comment    — Add a comment or note
item.link       — Add a dependency or relationship
item.unlink     — Remove a dependency or relationship
item.delete     — Soft-delete (tombstone)
item.compact    — Replace description with summary (memory decay)
item.snapshot   — Lattice-compacted state for a completed item
```

---

## Migration Path

If Bones starts with JSONL (the conservative choice) and later wants to switch to TSJSON, the migration is mechanical and lossless:

```bash
# Convert JSONL → TSJSON
jq -r '[.ts, .actor, .type, .item_id, (.data | tojson)] | @tsv' events.jsonl > events.events

# Convert TSJSON → JSONL (for export/interop)
awk -F'\t' '{printf "{\"ts\":%s,\"actor\":\"%s\",\"type\":\"%s\",\"item_id\":\"%s\",\"data\":%s}\n", $1, $2, $3, $4, $5}' events.events
```

Both directions are a single-pass stream transformation. No data is lost. The event log can be converted back and forth freely.

---

## Recommendation

**Use TSJSON (`.events` format) as the canonical event log format.**

The format sits at the sweet spot of the constraint space: git-friendly, human-readable, machine-fast, grep-native, and ~40% more compact than JSONL. The tradeoff — requiring a trivial custom parser instead of a generic JSON parser — is negligible for a Rust binary that owns its entire read/write path.

For interoperability, `bn export --jsonl` produces standard JSONL from the event log. Agents and external tools that need to read events programmatically should use `bn log --json`, which outputs structured JSON to stdout regardless of the underlying storage format.

The event log format is an internal implementation detail, not a public API. The public API is the CLI with `--json` output.
