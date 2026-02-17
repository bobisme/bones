
Based on the Bones design + implementation plan you shared. 

## 30 improvement ideas

1. **Make the event format fully self-sufficient**: explicitly encode ITC, parents/frontier, and a normative event-hash algorithm in TSJSON.
2. **Fix timestamp semantics**: move from “unix seconds” examples to a real monotonic clock (µs + seq or HLC) and document it.
3. **Crash-consistent append protocol**: define atomic-write, fsync policy, and torn-write recovery for `.events` shards.
4. **Cross-process locking**: formalize file locking for event appends + SQLite projection so multiple `bn` processes can’t corrupt state.
5. **Shard manifests should be seal-only**: commit manifests for frozen shards; treat active shards differently to avoid constant churn/staleness.
6. **Event validation layer**: schema-validate every event type + semantic validation (state machine, id format) with `bn validate`.
7. **Versioned JSON API contract**: freeze `--json` output schemas per command + generate JSON Schema + add golden tests.
8. **Triage explainability**: `bn next --explain` should show score decomposition + “why not X” + minimal unblocking path.
9. **Secret prevention**: pre-write secret scanning + explicit redaction limitations + embedding privacy rules.
10. **Optional event signing**: ed25519 signatures + trust policy config (verify provenance, not just integrity).
11. **Incremental projection**: cursor-based replay into SQLite and binary cache; make common commands O(delta).
12. **`bn upgrade` workflow**: formalize upgrade/migration for on-disk formats (cache/db/config/schema changes).
13. **`bn doctor`**: a diagnostics command for common failure modes (corruption, model missing, bad config, perf pathologies).
14. **Core vs experimental split**: feature flags and a “labs” namespace to prevent advanced research features from bloating MVP.
15. **Fuzzing harness**: `cargo fuzz` targets for TSJSON parsing, event application, graph normalization, and SQLite ingestion.
16. **Normative test vectors**: publish canonical event streams + expected projections to enforce cross-version determinism.
17. **Batch/transaction events**: group multi-step commands (`create+link+tag`) into a single atomic intent.
18. **Define concurrency semantics for delete/snapshot/redact**: ensure compaction/redaction cannot change merge results.
19. **Schema evolution policy**: reserved fields + extension strategy so future additions don’t break old clients.
20. **Earlier compaction strategy**: move snapshot/compaction earlier than Phase 4 to control repo growth sooner.
21. **Expose fairness metrics**: quantify starvation/dup-assignment risk in multi-agent scheduling output.
22. **Working-set primitives**: `bn focus`, `bn inbox`, `bn snooze` to reduce overwhelm and make humans productive.
23. **Rust library API**: a supported crate API (in addition to CLI) for embedding Bones into other tools.
24. **Optional git metadata ingestion**: reversible mapping between commits and events (not auto-commit; just linkage).
25. **Configurable “templates”**: structured fields for bugs/goals to improve search quality and reporting.
26. **Graph lint + CI mode**: `bn lint` with rules, suitable for CI gating.
27. **Earlier remote sync spec**: pin assumptions needed for Prolly Tree / non-git sync earlier to avoid rework.
28. **Privacy mode for semantic search**: per-item opt-out + no-embed for redacted/private content.
29. **Profiling + perf regression CI**: flamegraphs + performance budgets integrated into CI.
30. **Threat model + operational docs**: explicit trust boundaries, failure modes, and recovery playbooks.

---

## Critical evaluation of each idea

1. **KEEP** — Event format/hashing is foundational; current plan mentions Merkle-DAG + ITC but TSJSON doesn’t clearly encode them.
2. **KEEP** — Timestamp ambiguity will bite debugging, ordering, and determinism edge-cases; cheap to fix now.
3. **KEEP** — Append-only logs are only “simple” if crash semantics are explicit; otherwise you’ll ship foot-guns.
4. **KEEP** — Multi-process writes happen in practice (TUI + CLI + agents); locking is mandatory for correctness.
5. **KEEP** — “Committed manifest for active shard” is operationally awkward; seal-only manifests reduce churn and failure modes.
6. **KEEP** — Validation is your firewall against corrupted/malicious/buggy events; also enables better error reporting.
7. **KEEP** — `--json` is your API; without a contract you’ll break agents constantly.
8. **KEEP** — “Graph knows priority” is only usable if it can explain itself; otherwise humans ignore it.
9. **KEEP** — Redaction doesn’t delete git history; you need prevention + explicit semantics, especially with embeddings.
10. **REJECT (for now)** — Signatures add key management + UX complexity; integrity is already partly covered by hashes/manifests. Revisit once you have real multi-party trust issues.
11. **KEEP** — Without incremental projection, you’ll either be slow or constantly rebuilding; cursor replay is high leverage.
12. **REJECT** — `bn upgrade` is useful, but if you implement versioning + rebuildable caches correctly, you can defer a formal upgrader until you actually change formats.
13. **REJECT** — `bn doctor` is good polish, but it’s downstream of validation + verify + better errors; not “excellent” vs core correctness work.
14. **KEEP** — The plan includes very advanced features; feature flags + a “labs” track prevent MVP collapse.
15. **KEEP** — Parsers and graph code are fuzz magnets; fuzzing finds real bugs early at low marginal cost.
16. **REJECT** — You already have deterministic simulation + property tests; test vectors are nice but not top-tier ROI until you have multiple implementations/users.
17. **REJECT** — Batch events are elegant, but you can get 80% by emitting multiple events with a shared `txn_id` field later.
18. **KEEP** — Snapshot/compaction can silently break CRDT semantics if done wrong; this needs explicit design now.
19. **REJECT** — Important, but can be folded into (1)/(6)/(7) without being its own initiative.
20. **REJECT** — Early compaction is premature until you have a correct baseline and real data; keep it planned but not promoted.
21. **REJECT** — Fairness metrics are nice but only matter after multi-agent scheduling is real and used.
22. **REJECT** — Working-set UX is valuable, but it’s a product feature; correctness and API stability come first.
23. **REJECT** — A library API is scope creep; `--json` CLI already serves agents well.
24. **REJECT** — Git metadata linkage is nice-to-have and risks reintroducing “git-dependent” complexity.
25. **REJECT** — Templates contradict the explicit “templates not needed” philosophy; revisit only if search quality truly suffers.
26. **REJECT** — CI lint rules are downstream of real usage patterns; premature rules become noise.
27. **REJECT** — Remote sync spec is explicitly “future”; locking format assumptions now may slow iteration.
28. **REJECT** — Privacy mode for embeddings is a subset of (9); keep it inside the security initiative, not standalone.
29. **REJECT** — Profiling/perf CI is good, but you already have benchmark tiers; add profiling once hot paths exist.
30. **REJECT** — Threat model/docs are good, but (9) covers the most critical security reality; full ops docs can follow.

**Ideas kept:** 1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 14, 15, 18.

---

## Detailed plans for the ideas that survived

### 1) Complete, self-sufficient event format and hashing

**What it is**
Right now the plan references ITC clocks, Merkle-DAG integrity, and deterministic tie-breaking, but the TSJSON example fields don’t explicitly carry: (a) ITC, (b) causal parents/frontier, (c) a normative event-hash algorithm. Fix this by defining **Event Log v1** (or v2 if you consider the current text v1) as a complete wire format.

**Concrete plan**

1. **Define the exact TSJSON fields** (still tab-separated, still “partial parse” friendly):

   ```
   # bones event log v2
   # fields: wall_ts_us \t agent \t itc \t parents \t type \t item_id \t data_json \t event_hash
   1708012200123456  claude-abc  itc:...  h1,h2  item.create  bn-a3f8  {...}  blake3:...
   ```

   * `wall_ts_us`: i64 microseconds since epoch (see idea 2)
   * `itc`: a canonical text form (or base64) of ITC clock for that event
   * `parents`: comma-separated list of parent hashes (sorted lexicographically); empty for roots
   * `data_json`: **compact JSON**, no whitespace (see below)
   * `event_hash`: the hash of the event header+payload **excluding** this field

2. **Specify the event-hash algorithm** as a byte-level spec:

   * Hash input is the UTF-8 bytes of:
     `"{wall_ts_us}\t{agent}\t{itc}\t{parents}\t{type}\t{item_id}\t{data_json}\n"`
   * Hash function: BLAKE3 (fast, available, stable).
   * Encoding: `blake3:<hex>` (hex lowercase).
   * `parents` field uses `,` separator and **must be sorted** before hashing.

3. **Define JSON canonical rules you control**:

   * Always serialize `data_json` with:

     * no pretty printing
     * object keys sorted lexicographically (recursively)
   * This is primarily to keep diffs stable; hashing is already defined over bytes-on-disk.

4. **Implement a single “codec” module** that is the only way to read/write events.

**Rust sketch**

```rust
use blake3;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct EventLine {
    pub wall_ts_us: i64,
    pub agent: String,
    pub itc: String,
    pub parents: Vec<String>, // "blake3:..." strings
    pub typ: String,
    pub item_id: String,
    pub data: Value,
    pub event_hash: String, // computed
}

fn canonicalize_json(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                out.insert(k.clone(), canonicalize_json(&map[&k]));
            }
            Value::Object(out)
        }
        Value::Array(xs) => Value::Array(xs.iter().map(canonicalize_json).collect()),
        _ => v.clone(),
    }
}

fn encode_for_hash(ev: &EventLine) -> String {
    let mut parents = ev.parents.clone();
    parents.sort();

    let data = canonicalize_json(&ev.data);
    let data_json = serde_json::to_string(&data).expect("json encode");

    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        ev.wall_ts_us,
        ev.agent,
        ev.itc,
        parents.join(","),
        ev.typ,
        ev.item_id,
        data_json,
    )
}

fn compute_hash(encoded: &str) -> String {
    let h = blake3::hash(encoded.as_bytes());
    format!("blake3:{}", h.to_hex())
}
```

**Why this improves the plan**

* Removes an implementation hole: Merkle-DAG + ITC can’t be real unless encoded.
* Makes “deterministic tie-breaking” actually implementable across platforms.
* Gives you a stable foundation for `bn verify`, `item.redact` (hash reference), and sync/diffing.

**Downsides**

* Adds 2–3 extra TSJSON fields (slightly larger logs).
* Forces you to commit to an encoding for ITC and parents early.

**Confidence: 95%**
This is foundational correctness. It’s extremely likely you otherwise hit a spec gap mid-implementation.

---

### 2) Real timestamp semantics with monotonicity

**What it is**
Your TSJSON examples use second-resolution timestamps; the tie-break spec uses `wall_ts`. In practice you want:

* enough resolution to avoid collisions
* monotonicity for human-friendly ordering and debugging
* deterministic behavior when multiple events are created quickly

**Concrete plan**

1. Define `wall_ts_us` (microseconds) as the timestamp field.
2. Guarantee monotonicity **per repo** (not per agent) by maintaining a local “last timestamp” file under lock:

   * `.bones/cache/clock`
3. On each write:

   * `now = system_time_us()`
   * `wall_ts_us = max(now, last+1)`
   * persist `wall_ts_us`

**Rust sketch**

```rust
use std::{fs, io, path::Path};

fn now_us() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    (dur.as_secs() as i64) * 1_000_000 + (dur.subsec_micros() as i64)
}

fn next_wall_ts_us(clock_path: &Path) -> io::Result<i64> {
    let last = fs::read_to_string(clock_path)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);

    let ts = now_us().max(last.saturating_add(1));
    fs::write(clock_path, ts.to_string())?;
    Ok(ts)
}
```

**Why this improves the plan**

* Makes logs readable and stable.
* Eliminates annoying collisions in tie-breaking edge cases.
* Prevents “time went backwards” anomalies in projections.

**Downsides**

* Requires a lock discipline (see idea 4).
* If system time is wildly wrong, timestamps still wrong (but monotonic).

**Confidence: 85%**
This is a pragmatic improvement with little risk; the only tradeoff is a small amount of local state.

---

### 3) Crash-consistent append protocol for `.events`

**What it is**
“Append-only” doesn’t automatically mean “safe.” You need explicit rules for:

* atomicity of writes
* fsync durability policy
* how to recover from torn/truncated lines

**Concrete plan**

1. Define append invariants:

   * An event is exactly one newline-terminated line.
   * Files must always end with `\n` after a successful append.
2. Append algorithm:

   * Acquire the repo write lock (idea 4).
   * Open active shard with `O_APPEND`.
   * Write the full line bytes in one `write_all`.
   * `flush()` and optionally `sync_data()` depending on config:

     * default: `flush()` (fast)
     * `--durable`: `sync_data()` (safer)
3. Recovery algorithm on startup or before replay:

   * Scan from end to find last `\n`.
   * If last line is incomplete or fails parse, truncate back to last known-good newline.
   * Emit a diagnostic warning: “torn write repaired.”

**Rust sketch**

```rust
use std::{
    fs::OpenOptions,
    io::{self, Write, Seek, SeekFrom, Read},
    path::Path,
};

fn truncate_to_last_newline(path: &Path) -> io::Result<()> {
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    let len = f.metadata()?.len();
    if len == 0 { return Ok(()); }

    let mut pos = len;
    let mut buf = [0u8; 4096];

    while pos > 0 {
        let read_len = std::cmp::min(pos as usize, buf.len());
        pos -= read_len as u64;
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut buf[..read_len])?;
        if let Some(i) = buf[..read_len].iter().rposition(|&b| b == b'\n') {
            let new_len = pos + (i as u64) + 1;
            f.set_len(new_len)?;
            return Ok(());
        }
    }

    // No newline found; file is garbage or single torn line. Truncate to 0.
    f.set_len(0)?;
    Ok(())
}

fn append_line(path: &Path, line: &[u8], durable: bool) -> io::Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line)?;
    f.write_all(b"\n")?;
    f.flush()?;
    if durable {
        f.sync_data()?;
    }
    Ok(())
}
```

**Why this improves the plan**

* Prevents “one bad line kills the repo.”
* Makes durability an explicit knob instead of an implicit promise.
* Enables robust operation under power loss / kill -9 / disk pressure.

**Downsides**

* Slight overhead if `--durable` is used frequently.
* You have to decide how noisy repair warnings should be.

**Confidence: 90%**
If you ship without this, you’ll eventually get corruption reports you can’t reproduce.

---

### 4) Formal cross-process locking for writes and projection

**What it is**
You need a single answer for “what happens if two `bn` processes run concurrently,” including:

* appending events
* updating SQLite projection
* rebuilding caches

**Concrete plan**

1. Introduce a single lock file: `.bones/lock` (advisory).
2. Lock tiers:

   * **Write lock**: required for any mutating command (`create`, `do`, `done`, `link`, etc.) and for `rebuild`.
   * **Read lock**: optional; for high safety you can have a shared lock for readers, but not necessary if readers tolerate lag.
3. SQLite rules:

   * Use WAL mode.
   * For projection updates, hold the same write lock to avoid “events appended but projection behind” races (or store a cursor and tolerate lag; see idea 11).

**Rust sketch**

```rust
use std::{fs::OpenOptions, io, path::Path};
use fs2::FileExt;

pub struct RepoLock {
    f: std::fs::File,
}

impl RepoLock {
    pub fn acquire_exclusive(lock_path: &Path) -> io::Result<Self> {
        let f = OpenOptions::new().create(true).read(true).write(true).open(lock_path)?;
        f.lock_exclusive()?;
        Ok(Self { f })
    }
}
impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = self.f.unlock();
    }
}
```

**Why this improves the plan**

* Prevents corrupted shards / broken cursors / partial projections.
* Makes behavior deterministic under real workloads (TUI + agents + humans).

**Downsides**

* Advisory locks behave differently on some filesystems; you’ll need tests across platforms.
* Long-running commands holding the lock can block others (mitigate by keeping critical sections short).

**Confidence: 92%**
Concurrency bugs are brutal; a single lock is the simplest robust answer.

---

### 5) Shard manifests should be seal-only

**What it is**
The plan says each shard has a committed manifest and Bones verifies before replay. If manifests are committed for the active shard, you get constant churn and/or failures when events exist but manifest wasn’t updated/committed.

**Concrete plan**

1. Split shards into:

   * **Active shard**: append-only, no committed manifest requirement.
   * **Sealed shard**: immutable, must have a committed manifest.
2. Add `bn seal`:

   * closes current shard (e.g., `2026-02.events`)
   * writes `2026-02.manifest` and marks it sealed
   * opens a new shard (`2026-03.events`), updates `current.events` symlink
3. `bn verify` behavior:

   * Sealed shards: verify `byte_len`, `event_count`, and `file_hash` (and later Merkle).
   * Active shard: run parse sanity + optionally a local rolling hash file in `.bones/cache/active.manifest` (gitignored).

**Manifest format suggestion**
Keep it simple and deterministic:

```json
{
  "version": 1,
  "shard": "2026-02",
  "event_count": 12345,
  "byte_len": 987654,
  "file_hash": "blake3:...."
}
```

**Rust sketch**

```rust
use blake3;
use std::{fs, io, path::Path};

fn blake3_file(path: &Path) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut f = fs::File::open(path)?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = io::Read::read(&mut f, &mut buf)?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}
```

**Why this improves the plan**

* Eliminates an operational trap where normal local work fails verification.
* Keeps “integrity checks” strong where they matter most (frozen data).
* Reduces git noise.

**Downsides**

* Active shard has weaker guarantees until sealed (acceptable if documented).
* Adds a new workflow concept (`bn seal`), but it maps cleanly to “freeze shards are immutable.”

**Confidence: 88%**
This is primarily about preventing a likely workflow mismatch.

---

### 6) Event validation layer with `bn validate`

**What it is**
Right now “invalid transitions rejected” is stated, but not operationalized. You want:

* type-level schema validation
* semantic validation
* deterministic handling of invalid events

**Concrete plan**

1. Define typed payload structs per event:

   * use `#[serde(flatten)] extra: BTreeMap<String, Value>` to allow forward-compatible unknown fields
2. Add a validation module:

   * `validate_line_syntax` (TSJSON shape, hash match)
   * `validate_event_schema` (payload shape)
   * `validate_event_semantics` (state machine, kind/state enums, link constraints)
3. Define deterministic invalid-event behavior:

   * For replay/CRDT: invalid events become **no-ops** (but recorded as errors).
   * For `bn verify` / `bn validate`: surface them clearly with line number + shard.

**Rust sketch**

```rust
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Deserialize)]
pub struct ItemMoveData {
    pub state: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

fn validate_item_move(data: &ItemMoveData) -> anyhow::Result<()> {
    match data.state.as_str() {
        "open" | "doing" | "done" | "archived" => Ok(()),
        _ => anyhow::bail!("invalid state {}", data.state),
    }
}
```

**Why this improves the plan**

* Turns “determinism” into enforceable rules.
* Hardens against partial corruption and buggy agents.
* Makes `bn verify` meaningful beyond “hash matches.”

**Downsides**

* You must define a stance on forward-compatibility vs strictness.
* Validation code can become a maintenance surface (mitigate with shared enums and schema gen).

**Confidence: 93%**
Validation prevents whole classes of failure; it’s very likely to pay off.

---

### 7) Versioned, testable `--json` API contract

**What it is**
Agents depend on `--json`. If output changes ad hoc, the ecosystem breaks.

**Concrete plan**

1. Every `--json` output includes:

   * `api_version`
   * `command`
   * `data` (command-specific)
2. For each command, define a Rust struct for the JSON shape.
3. Generate JSON Schema with `schemars` and expose:

   * `bn schema <command>` (prints schema)
4. Add golden tests:

   * run command on a fixed fixture repo
   * compare output to `tests/golden/<command>.json` (or schema-validate)

**Rust sketch**

```rust
use serde::Serialize;
use schemars::JsonSchema;

#[derive(Serialize, JsonSchema)]
pub struct JsonEnvelope<T> {
    pub api_version: u32,
    pub command: String,
    pub data: T,
}

#[derive(Serialize, JsonSchema)]
pub struct NextData {
    pub recommendations: Vec<Recommendation>,
}

#[derive(Serialize, JsonSchema)]
pub struct Recommendation {
    pub item_id: String,
    pub score: f64,
    pub blocked: bool,
}
```

**Why this improves the plan**

* Makes agents reliable.
* Enables backward compatibility discipline from day one.
* Gives you a safe way to evolve output without breaking clients.

**Downsides**

* You must maintain schema compatibility rules.
* Some CLI freedom is lost (but that’s the point).

**Confidence: 90%**
Given “agents first,” this is a direct alignment improvement.

---

### 8) Triage explainability as a first-class output

**What it is**
The plan has advanced metrics and a composite score, but explainability is the difference between “cool” and “used.”

**Concrete plan**

1. Extend triage computation to retain per-item feature values:

   * `cp`, `pr`, `bc`, `u`, `d`
2. When producing recommendations:

   * include `score_breakdown` = each feature * weight
   * include `suppression_reasons` (e.g., blocked, punt, archived)
   * include `top_unblocked_path` (a short list of items that become available if this is done)
3. CLI:

   * `bn next --explain`
   * `bn why <item_id>`: prints why it was/wasn’t recommended, with deltas vs current top pick.

**Rust sketch**

```rust
#[derive(serde::Serialize)]
pub struct ScoreBreakdown {
    pub cp: f64,
    pub pr: f64,
    pub bc: f64,
    pub u: f64,
    pub d: f64,
    pub total: f64,
}

fn breakdown(f: Features, w: Weights) -> ScoreBreakdown {
    let cp = w.alpha * f.cp;
    let pr = w.beta  * f.pr;
    let bc = w.gamma * f.bc;
    let u  = w.delta * f.u;
    let d  = w.eps   * f.d;
    ScoreBreakdown { cp, pr, bc, u, d, total: cp+pr+bc+u+d }
}
```

**Why this improves the plan**

* Increases trust and adoption.
* Makes tuning (and Thompson feedback learning) observable instead of magical.
* Helps debug weird graph behavior (cycles, SCC collapse effects).

**Downsides**

* More computation and more data in JSON output (but still cheap).
* Risk of overfitting explanations to metrics you later change (mitigate with versioned API).

**Confidence: 87%**
High product leverage; low technical risk.

---

### 9) Secret prevention and explicit security semantics

**What it is**
Your `item.redact` is useful, but it does **not** remove secrets from git history. Without prevention + clear semantics, you’ll eventually leak credentials into the event log and your only real fix will be git history rewriting.

**Concrete plan**

1. Add a “Security reality” section to docs:

   * Redaction hides content in projections, but the raw event remains in the log.
   * If a real secret is committed, you must rotate the secret and possibly rewrite git history.
2. Implement **pre-write secret scanning** for fields that can contain free text:

   * `title`, `description`, `comment.body`, etc.
   * If match found: block by default and require `--allow-secret` (non-interactive) or interactive confirmation.
3. Embedding privacy rules:

   * If an event is redacted, embeddings for affected items are deleted/recomputed without the secret.
   * Never embed content marked as `private` or matching secret patterns (configurable).
4. Optional future: “encrypted blobs” for truly sensitive notes, stored as attachments (but keep that optional).

**Rust sketch**

```rust
use regex::Regex;

pub struct SecretScanner {
    patterns: Vec<Regex>,
}

impl SecretScanner {
    pub fn default() -> Self {
        Self {
            patterns: vec![
                Regex::new(r"-----BEGIN (RSA|EC|OPENSSH) PRIVATE KEY-----").unwrap(),
                Regex::new(r"ghp_[A-Za-z0-9]{30,}").unwrap(), // GitHub classic token shape
                Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),     // AWS access key id shape
            ],
        }
    }

    pub fn find(&self, s: &str) -> Option<&Regex> {
        self.patterns.iter().find(|re| re.is_match(s))
    }
}
```

**Why this improves the plan**

* Prevents irreversible mistakes in a git-native store.
* Makes “redact” honest rather than misleading.
* Keeps semantic search from becoming a secondary leak vector.

**Downsides**

* False positives; you’ll need allowlists and config toggles.
* Adds friction for some workflows (but that’s appropriate for high-risk strings).

**Confidence: 86%**
Secret leaks are common; prevention beats cleanup.

---

### 11) Incremental projection and replay cursors

**What it is**
You already say SQLite is disposable and rebuildable. Good. But you still need:

* fast normal operations
* projection that tracks the event tail incrementally

**Concrete plan**

1. Add `.bones/cache/projection.cursor.json` (gitignored) storing per-shard cursors:

   * shard name
   * byte offset
   * last processed event hash
2. On startup / before commands that need projection:

   * For each shard in order:

     * If sealed and hash matches manifest: skip to end.
     * If active: seek to cursor offset and process new lines.
   * If cursor inconsistent (file truncated/hash mismatch): fall back to rebuild (or rescan shard).
3. Apply events to SQLite in batches within transactions.
4. Same cursor concept applies to `.bones/cache/events.bin` if you keep it appendable.

**Rust sketch**

```rust
use std::{fs::File, io::{self, BufRead, BufReader, Seek, SeekFrom}, path::Path};

fn replay_from_offset(path: &Path, offset: u64, mut on_line: impl FnMut(&str) -> anyhow::Result<()>) -> anyhow::Result<u64> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut rdr = BufReader::new(f);
    let mut buf = String::new();
    let mut pos = offset;

    loop {
        buf.clear();
        let n = rdr.read_line(&mut buf)?;
        if n == 0 { break; }
        pos += n as u64;
        let line = buf.trim_end_matches('\n');
        if line.starts_with('#') || line.is_empty() { continue; }
        on_line(line)?;
    }
    Ok(pos)
}
```

**Why this improves the plan**

* Makes `bn` feel instant even on large repos.
* Keeps rebuild as a recovery tool, not a normal path.
* Creates a clean boundary: “event log is truth; projection is a cached read model.”

**Downsides**

* Cursor invalidation logic is subtle; you must treat rebuild as a safe fallback.
* Needs careful transactional handling in SQLite.

**Confidence: 91%**
Almost all event-sourced systems end up here; doing it explicitly early avoids pain.

---

### 14) Core vs experimental feature split with feature flags

**What it is**
Your plan is ambitious: persistent homology, Whittle indices, spectral sparsification, etc. These are high-risk and can sink shipping if treated as required. You need a structural way to keep the MVP lean.

**Concrete plan**

1. Define “Bones Core” (1.0) explicitly:

   * event log + replay + convergence
   * projection + FTS lexical search
   * basic triage: readiness + simple graph heuristics
   * verify/validate
2. Define “Bones Labs”:

   * persistent homology analysis
   * Whittle scheduling
   * advanced centrality suites beyond what’s needed for `bn next` quality
   * semantic embeddings
3. Implement gating:

   * compile-time Cargo features: `labs`, `semantic`, `topology`
   * runtime config toggles: `triage.mode = "core" | "labs"`
4. CLI UX:

   * labs commands live under `bn labs ...` or require `--enable-labs`

**Rust sketch**

```rust
#[cfg(feature = "labs")]
mod topology;

pub fn run_health(opts: HealthOpts) -> anyhow::Result<()> {
    match opts.topology {
        TopologyMode::Basic => run_basic_health(),
        TopologyMode::Advanced => {
            #[cfg(feature = "labs")]
            { topology::run_advanced() }
            #[cfg(not(feature = "labs"))]
            { anyhow::bail!("advanced topology requires feature 'labs'") }
        }
    }
}
```

**Why this improves the plan**

* Prevents research features from blocking productization.
* Lets you ship value early and iterate with real feedback.
* Keeps the codebase maintainable.

**Downsides**

* Feature flag complexity and test matrix expansion.
* Risk that “labs” becomes neglected or fragmented (mitigate by clear ownership and milestones).

**Confidence: 89%**
Given the scope, this is a strong risk reducer.

---

### 15) Fuzzing for parsing and event application

**What it is**
TSJSON parsing + event validation + graph algorithms are classic fuzz targets. Fuzzing will find panics, hangs, and pathological cases faster than manual tests.

**Concrete plan**

1. Add `cargo fuzz` to the repo.
2. Fuzz targets:

   * `parse_event_line` (never panic; either Ok or structured error)
   * `apply_event_to_state`
   * `apply_event_to_sqlite` (with in-memory SQLite)
   * `graph_normalize` (SCC condensation + transitive reduction should never panic)
3. Add “invariants under fuzz”:

   * applying same event twice is idempotent where expected
   * invalid events produce deterministic no-ops

**Fuzz target sketch**

```rust
// fuzz_targets/parse_line.rs
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = bones::codec::parse_line(s); // must not panic
    }
});
```

**Why this improves the plan**

* Finds real crashes before users do.
* Hardens the system against corrupted repos and malicious inputs.

**Downsides**

* Some fuzz failures are “expected errors”; you must define what’s acceptable.
* Needs CI resources/time (can run fuzzing nightly if needed).

**Confidence: 84%**
Low cost, high payoff for parser-heavy systems.

---

### 18) Define correct concurrency semantics for delete, snapshot, and redaction

**What it is**
This is the most subtle correctness risk in your plan:
If `item.snapshot` is implemented as a normal “update event with a new clock,” it can **change merge outcomes** by incorrectly dominating concurrent events that weren’t observed at compaction time. That would violate “compaction is semantics-preserving.”

**Concrete plan**

1. Treat `item.snapshot` as a **lattice element**, not a normal update:

   * Snapshot payload must include, for every LWW field, the **winning (clock, value)** pair.
   * Applying a snapshot means: `state = join(state, snapshot_state)` (field-wise join), not “overwrite with snapshot event clock.”
2. Define `item.delete` as a CRDT field:

   * `deleted: LWW<bool>` or `deleted_at: LWW<Option<WallTs>>`
   * Default queries hide deleted items; but merging remains deterministic.
3. Define `item.redact` as a projection rule with explicit semantics:

   * Redaction targets an event hash.
   * Projection must replace payload with `[redacted]`.
   * Embeddings/indexes must be recomputed accordingly.
   * Compaction must never reintroduce redacted content.
4. Document deterministic behavior when operations occur concurrently:

   * delete vs update
   * snapshot vs update
   * redact vs snapshot

**Rust sketch for semantics-preserving snapshot of LWW fields**

```rust
#[derive(Clone, Debug)]
pub struct Clock {
    pub itc: String,       // placeholder; use real ITC type
    pub wall_ts_us: i64,
    pub agent: String,
    pub event_hash: String,
}

#[derive(Clone, Debug)]
pub struct Lww<T> {
    pub clock: Clock,
    pub value: T,
}

fn clock_cmp(a: &Clock, b: &Clock) -> std::cmp::Ordering {
    // Implement the normative order:
    // 1) ITC dominance (not shown)
    // 2) wall_ts
    // 3) agent lexicographic
    // 4) event_hash lexicographic
    a.wall_ts_us.cmp(&b.wall_ts_us)
        .then_with(|| a.agent.cmp(&b.agent))
        .then_with(|| a.event_hash.cmp(&b.event_hash))
}

fn lww_join<T: Clone>(a: &Lww<T>, b: &Lww<T>) -> Lww<T> {
    match clock_cmp(&a.clock, &b.clock) {
        std::cmp::Ordering::Less => b.clone(),
        _ => a.clone(),
    }
}

#[derive(Clone, Debug)]
pub struct ItemState {
    pub title: Lww<String>,
    pub description: Lww<String>,
    pub deleted: Lww<bool>,
    // plus OR-set fields, etc.
}

fn apply_snapshot(mut cur: ItemState, snap: ItemState) -> ItemState {
    cur.title = lww_join(&cur.title, &snap.title);
    cur.description = lww_join(&cur.description, &snap.description);
    cur.deleted = lww_join(&cur.deleted, &snap.deleted);
    cur
}
```

**Why this improves the plan**

* Prevents compaction from violating CRDT semantics.
* Makes delete/redact behavior deterministic and explainable.
* Avoids “ghost bugs” where history compaction changes outcomes.

**Downsides**

* Snapshot payloads become more complex (must carry per-field clocks and OR-set internal state).
* You’ll need careful design for OR-set snapshot representation (e.g., representing the OR-set as a lattice element too).

**Confidence: 96%**
This is a correctness landmine. Making semantics explicit now is extremely likely to save you from a catastrophic “compaction broke convergence” class of bugs later.
