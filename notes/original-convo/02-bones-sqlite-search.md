# Bones: SQLite Audit & Hybrid Search Architecture
## From Keyword Matching to Semantic Deduplication

---

## The Duplicate Problem: A Root Cause Analysis

You saw agents filing the same bead five times. That's not a UI bug — it's an **information retrieval failure** with a specific cause chain:

1. Agent wants to create "Fix authentication timeout in payment service"
2. Agent searches for existing items: `bn list --search "auth timeout"`
3. Beads/SQLite does exact substring matching or basic FTS
4. Existing item titled "Payment service auth fails after 30s" → **no match** (different words, same problem)
5. Agent creates a duplicate
6. Repeat for "Authentication timeout bug in payments," "Payment auth timing out," etc.

The core failure: **lexical search can't bridge the vocabulary gap**. "Timeout" and "fails after 30s" describe the same symptom in different words. FTS5's BM25 only matches on shared tokens — it literally cannot find the connection. No amount of stemming, trigrams, or query expansion fixes this. You need semantic understanding.

---

## What SQLite Actually Gives Us (and What It Doesn't)

### What SQLite does well as a projection layer

SQLite is excellent at what the original Bones design uses it for:

- **Structured queries**: `SELECT * FROM items WHERE state = 'open' AND kind = 'bug' ORDER BY created_at` — fast, indexed, trivial.
- **Aggregations**: `SELECT label, COUNT(*) FROM item_labels GROUP BY label` — project health dashboards.
- **Relational joins**: Items → labels, items → dependencies, items → comments — all the relational glue.
- **FTS5 with BM25**: Exact keyword search with relevance ranking. Good for `bn list --search "auth"` when you know the exact word.
- **Zero deployment**: No external services, no daemon, single file. Perfect for a CLI tool.
- **Disposable**: Rebuild from the event log in milliseconds. Delete it and nothing is lost.

### What SQLite cannot do

- **Semantic similarity**: "auth timeout" ≠ "authentication fails after 30 seconds" in any tokenizer's universe.
- **Fuzzy concept matching**: "memory leak" ≈ "RSS grows unboundedly" ≈ "OOM after long uptime" — these are the same bug described three ways.
- **Duplicate detection at creation time**: The moment an agent runs `bn create`, Bones should warn "This looks similar to bn-a3f8 (92% match)" — impossible with BM25 alone.
- **Intent-aware search**: When an agent searches "what's blocking the auth migration?", it means "find items in the auth-related dependency subgraph that are in 'open' state and block items tagged 'migration'." FTS5 treats this as bag-of-words.

### The verdict: Keep SQLite, supplement it heavily

SQLite stays as the structured projection layer. It's too good at relational queries to ditch. But search needs a parallel system that combines lexical and semantic retrieval. This is exactly the architecture FrankenSearch implements — and we can do it tailored to Bones' specific problem space.

---

## The Bones Search Architecture: Three-Layer Hybrid

Inspired by FrankenSearch's two-tier progressive search, but adapted for an issue tracker where the primary threat is **duplicate creation** rather than document retrieval:

```
                    Query: "fix auth timeout in payments"
                                    │
                    ┌───────────────┼───────────────┐
                    ▼               ▼               ▼
             ┌──────────┐   ┌──────────┐   ┌──────────────┐
             │  Layer 1  │   │  Layer 2  │   │   Layer 3    │
             │  FTS5     │   │  Vector   │   │  Structural  │
             │  BM25     │   │  Cosine   │   │  Graph Sim.  │
             │  (lexical)│   │ (semantic)│   │  (relational)│
             └────┬──────┘   └────┬──────┘   └──────┬───────┘
                  │               │                  │
                  └───────────────┼──────────────────┘
                                  ▼
                    ┌─────────────────────────┐
                    │  Reciprocal Rank Fusion  │
                    │  (RRF, K=60)            │
                    └────────────┬────────────┘
                                 ▼
                    ┌─────────────────────────┐
                    │  Unified Results         │
                    │  with similarity scores  │
                    └─────────────────────────┘
```

### Layer 1: FTS5 Lexical Search (what we have)

Keep SQLite FTS5 exactly as it is. It handles:
- Exact keyword matches ("bn-a3f8", "OOM", "CVE-2024-1234")
- Prefix search ("auth*")
- Boolean queries ("backend AND timeout NOT frontend")
- BM25 relevance ranking

Configuration for Bones:

```sql
CREATE VIRTUAL TABLE items_fts USING fts5(
    title,
    description,
    labels,
    tokenize='porter unicode61',  -- stemming + unicode
    prefix='2,3',                  -- prefix indexes for autocomplete
    content='items',               -- external content (sync with main table)
    content_rowid='rowid'
);

-- Weighted BM25: title matters 3x, description 2x, labels 1x
SELECT item_id, bm25(items_fts, 3.0, 2.0, 1.0) as score
FROM items_fts
WHERE items_fts MATCH ?
ORDER BY score
LIMIT 20;
```

**What this catches**: Exact term overlap. "auth timeout" finds "auth timeout in production."
**What this misses**: "authentication failures after 30 seconds" — zero shared stems with "auth timeout."

### Layer 2: Vector Semantic Search (the key upgrade)

This is where the duplicate problem gets solved. Every work item gets a 384-dimensional embedding vector that captures its *meaning*, not just its words.

**Embedding model**: all-MiniLM-L6-v2 via ONNX Runtime

- 22M parameters, 384 dimensions
- ~80MB model file (quantized int8: ~23MB)
- CPU inference: ~5ms per sentence on modern hardware
- No GPU required, no external API, fully offline

**Why this model**: It's the sweet spot between quality and size. FrankenSearch uses it as its quality tier for good reason — it's the most widely deployed sentence embedding model, battle-tested on semantic similarity tasks, and fast enough for real-time use in a CLI tool. The ONNX format means we can run it via the `ort` crate in Rust without Python, PyTorch, or any heavy runtime.

**Storage**: sqlite-vec extension or a simple flat file

Two options, both good:

**Option A: sqlite-vec (simpler, unified)**
```sql
-- sqlite-vec extension: vector search inside SQLite
CREATE VIRTUAL TABLE items_vec USING vec0(
    item_id TEXT PRIMARY KEY,
    embedding FLOAT[384]
);

-- KNN search
SELECT item_id, distance
FROM items_vec
WHERE embedding MATCH ?  -- query vector
  AND k = 20;
```

Pros: Single SQLite file, transactional with the rest of the data, zero additional dependencies (pure C, no FAISS).
Cons: Brute-force KNN only (no ANN index), but for <100K items this is fine — sqlite-vec author explicitly says brute-force is the right call for most local use cases.

**Option B: Flat FSVI file (FrankenSearch-style, faster)**
```
.bones/
├── events.bin           # Columnar event log
├── bones.db             # SQLite projection (FTS5, relational)
├── vectors.fsvi         # Memory-mapped f16 vector index
└── models/
    └── minilm-l6-v2.onnx  # Embedding model (~23MB quantized)
```

Store vectors as a flat memory-mapped file of f16 values. SIMD brute-force dot product over 10,000 × 384-dim f16 vectors takes <5ms. No index structure needed.

Pros: Maximum performance, no SQLite extension dependency, works with mmap.
Cons: Separate file, need to keep in sync with SQLite.

**Recommendation**: Option A (sqlite-vec) for simplicity. The performance difference is negligible at Bones' scale (hundreds to low thousands of items). One database file is better than two. If performance becomes a problem at 50K+ items, Option B is a drop-in replacement.

**What this catches**: "auth timeout" → cosine similarity 0.87 with "authentication fails after 30 seconds." "Memory leak in worker pool" → cosine similarity 0.82 with "RSS grows unboundedly on the task executor."

**What this misses**: Structural relationships. Two items might be semantically similar in text but one is a bug and the other is a goal — they shouldn't be considered duplicates.

### Layer 3: Structural Graph Similarity (the innovation)

This layer is unique to Bones. No search system I've seen does this for issue trackers.

Two items are structurally similar if they:
- Share the same parent (same goal)
- Share dependencies (blocked by the same things)
- Share labels (same subsystem)
- Share assignees (same person's work)
- Are in the same graph neighborhood (within 2 hops in the dependency graph)

```rust
fn structural_similarity(a: &Item, b: &Item) -> f64 {
    let label_jaccard = jaccard(&a.labels, &b.labels);
    let dep_jaccard = jaccard(&a.blocked_by, &b.blocked_by);
    let assignee_jaccard = jaccard(&a.assignees, &b.assignees);
    let same_parent = if a.parent == b.parent && a.parent.is_some() { 1.0 } else { 0.0 };
    let graph_distance = shortest_path_distance(a.id, b.id); // 0.0 if unreachable
    let graph_proximity = if graph_distance > 0 { 1.0 / graph_distance as f64 } else { 0.0 };

    // Weighted combination
    0.30 * label_jaccard
    + 0.25 * dep_jaccard
    + 0.15 * assignee_jaccard
    + 0.15 * same_parent
    + 0.15 * graph_proximity
}
```

**What this catches**: An item tagged [backend, auth, security] with parent "Auth Migration Epic" is structurally similar to other items in that same cluster — even if the text is completely different (e.g., "rotate JWT signing keys" vs. "update OIDC provider config"). These might not be duplicates, but they're *related*, and an agent should know about them.

### Fusion: Reciprocal Rank Fusion (RRF)

RRF is the right combiner here (same choice as FrankenSearch) because it's **rank-based** — it doesn't require calibrating the wildly different score scales of BM25, cosine similarity, and Jaccard coefficients.

```
RRF(item) = Σ_layer  1 / (K + rank(item, layer))

where K = 60 (standard), and we sum over all three layers
```

An item that ranks #1 in lexical, #3 in semantic, and #8 in structural gets:
```
RRF = 1/61 + 1/63 + 1/68 = 0.0164 + 0.0159 + 0.0147 = 0.0470
```

An item that ranks #2 in semantic and #1 in structural but doesn't appear in lexical results:
```
RRF = 0 + 1/62 + 1/61 = 0 + 0.0161 + 0.0164 = 0.0325
```

The first item wins — it appeared across multiple signals. RRF naturally rewards cross-signal consensus.

---

## The Killer Feature: Duplicate Prevention at Creation Time

This is the main event. The entire search architecture exists to power this:

```bash
$ bn create "Fix authentication timeout in payment service"

⚠ Similar items found:
  bn-a3f8  "Payment service auth fails after 30s"     (92% match, state: open)
  bn-c7d2  "Auth token expiry causes payment drops"    (78% match, state: doing)
  bn-e5f6  "Investigate payment timeouts"              (71% match, state: done)

Create anyway? [y/N/link]
  y     - Create as new item
  N     - Cancel
  link  - Create and link as related to bn-a3f8
```

**For agents** (the 80% use case), this is the `--json` version:

```bash
$ bn create "Fix authentication timeout in payment service" --json --dry-run

{
  "action": "create",
  "title": "Fix authentication timeout in payment service",
  "similar_items": [
    {
      "id": "bn-a3f8",
      "title": "Payment service auth fails after 30s",
      "similarity": 0.92,
      "match_signals": {
        "lexical": 0.34,
        "semantic": 0.91,
        "structural": 0.67
      },
      "state": "open",
      "recommendation": "likely_duplicate"
    },
    {
      "id": "bn-c7d2",
      "title": "Auth token expiry causes payment drops",
      "similarity": 0.78,
      "match_signals": {
        "lexical": 0.12,
        "semantic": 0.84,
        "structural": 0.55
      },
      "state": "doing",
      "recommendation": "possibly_related"
    }
  ],
  "duplicate_risk": "high"
}
```

The agent can then make an informed decision: skip creation, or create with `--relates bn-a3f8` to establish the connection.

**Threshold recommendations**:
- similarity ≥ 0.90 → `likely_duplicate` (recommend skipping)
- similarity 0.70-0.89 → `possibly_related` (recommend linking)
- similarity 0.50-0.69 → `maybe_related` (inform, don't block)
- similarity < 0.50 → no warning

These thresholds are tunable in `.bones/config.yaml` and should be calibrated per-project.

---

## Embedding Lifecycle: When and How Vectors Get Created

The embedding model adds weight to the binary (~23MB quantized). Here's the lifecycle that keeps it transparent:

### Model management

```bash
# First run: model downloads automatically (or ships with binary)
$ bn create "First item"
Downloading embedding model (23MB)... done.

# Or pre-download
$ bn setup --download-model

# Or disable entirely
$ bn create "First item" --no-semantic
# Or in config.yaml:
# search:
#   semantic: false
```

### Embedding computation

Embeddings are computed **lazily and incrementally**:

1. On `bn create`: Embed the new item's title + description. Cost: ~5ms. Trivially fast.
2. On `bn rebuild`: Re-embed all items. Cost: ~5ms × N items. 1000 items = 5 seconds. Acceptable.
3. On `item.update` (title or description change): Re-embed that single item. Cost: ~5ms.
4. On `item.compact`: Re-embed from the compacted summary.

Embeddings are **not** stored in the event log. They're a projection — part of the SQLite database (or FSVI file). They're recomputable from the event log at any time, just like all other projections. This means:
- The event log stays pure (no model-dependent data)
- Upgrading the embedding model just requires `bn rebuild`
- Different replicas can use different models (convergence is on events, not embeddings)

### What gets embedded

The embedding input is a structured concatenation:

```
"{title}. {description_first_500_chars}. Labels: {labels_joined}. Kind: {kind}."
```

For example:
```
"Fix authentication timeout in payment service. When the payment service
processes high-volume transactions, auth tokens expire before the request
completes, causing 504 errors. Labels: backend, auth, payments. Kind: bug."
```

This gives the embedding model maximum semantic signal in a single pass.

---

## Search Commands: The Full Interface

```bash
# Basic search (uses all three layers)
$ bn search "auth timeout"

# Semantic-only search (useful when you don't know exact terms)
$ bn search "things that cause payment failures" --semantic

# Lexical-only search (useful for exact IDs, error codes)
$ bn search "CVE-2024-1234" --lexical

# Find duplicates of an existing item
$ bn similar bn-a3f8

# Duplicate check before creation (non-interactive)
$ bn create "Fix auth timeout" --check-duplicates --json

# Bulk duplicate scan (find all likely duplicates in the project)
$ bn dedup
⚠ Found 7 likely duplicate pairs:
  bn-a3f8 ↔ bn-x1y2  (0.94)  "Payment auth timeout" ↔ "Auth fails in payments"
  bn-c7d2 ↔ bn-z9w8  (0.91)  "Update user docs" ↔ "Refresh user documentation"
  ...

# Agent-friendly: search with structured output
$ bn search "memory leak" --json --limit 5
```

---

## Architecture Decision: What Replaces What

Here's the final picture of what stays, what gets added, and what role each plays:

```
┌─────────────────────────────────────────────────────────────────┐
│                          bones.db (SQLite)                       │
│                                                                  │
│  ┌─────────────────┐  ┌──────────────────┐  ┌───────────────┐  │
│  │  items table     │  │  items_fts (FTS5) │  │  items_vec    │  │
│  │  (relational     │  │  (lexical search  │  │  (sqlite-vec  │  │
│  │   projection,    │  │   with BM25,      │  │   384-dim     │  │
│  │   structured     │  │   stemming,       │  │   embeddings, │  │
│  │   queries)       │  │   prefix)         │  │   KNN)        │  │
│  └────────┬────────┘  └────────┬─────────┘  └──────┬────────┘  │
│           │                    │                     │           │
│           └────────────────────┼─────────────────────┘           │
│                                │                                 │
│                    ┌───────────▼───────────┐                    │
│                    │  RRF Fusion + Graph   │                    │
│                    │  Structural Similarity │                    │
│                    └───────────────────────┘                    │
└─────────────────────────────────────────────────────────────────┘
                                │
                        Query results with
                        unified similarity scores
```

**SQLite is not replaced. It's augmented.** The relational engine handles structured queries (filters, joins, aggregations). FTS5 handles keyword search. sqlite-vec handles semantic search. The graph similarity layer sits on top and uses the dependency data already in SQLite. RRF fuses everything.

All of it lives in a single `bones.db` file. All of it is disposable and rebuiltable from the event log.

---

## The FrankenSearch Influence: What We Take and What We Change

FrankenSearch does several things brilliantly that Bones should adopt:

**Adopted from FrankenSearch:**
- **Two-tier progressive search**: Fast initial results, quality refinement. In Bones: FTS5 returns in <1ms (fast tier), semantic + structural follows in <20ms (quality tier).
- **RRF fusion**: Rank-based combination of heterogeneous retrieval signals. No score calibration needed.
- **f16 vector storage**: Half-precision is more than sufficient for similarity search. Halves storage and doubles throughput vs. f32.
- **Graceful degradation**: If the embedding model isn't available, fall back to FTS5 only. Search still works, just without semantic matching. This is critical for a CLI tool that might run in constrained environments.
- **SIMD brute-force for KNN**: At Bones' scale (<100K items), brute-force with SIMD is faster than building an ANN index. sqlite-vec takes the same approach.

**Changed from FrankenSearch:**
- **No Tantivy**: FrankenSearch uses Tantivy for BM25. Bones uses SQLite FTS5 instead because we already have SQLite for relational queries, and adding Tantivy would mean a second full-text index of the same data. FTS5's BM25 is good enough for keyword matching, and it's free with SQLite.
- **Structural layer added**: FrankenSearch is a general-purpose search engine. Bones knows the structure of its data — dependencies, labels, parents, assignees. This third signal layer is the main innovation over FrankenSearch's architecture.
- **Duplicate prevention focus**: FrankenSearch optimizes for retrieval quality. Bones optimizes for a specific decision: "is this item a duplicate?" The threshold system, risk scoring, and creation-time warnings are purpose-built for this use case.
- **Embedded model, not configurable stack**: FrankenSearch has a sophisticated model fallback chain (fastembed → model2vec → hash). Bones ships one model (MiniLM-L6-v2 ONNX, quantized) and that's it. Simplicity over flexibility. One model that works well beats a stack of options an agent has to configure.

---

## Performance Budget

For `bn create` with duplicate checking (the hot path):

| Step | Time | Notes |
|------|------|-------|
| Parse title + description | <0.1ms | String ops |
| Compute embedding (MiniLM-L6-v2 ONNX, int8 quantized) | ~3-5ms | CPU, single sentence |
| FTS5 BM25 search (top 20) | <1ms | Already indexed |
| sqlite-vec KNN search (top 20) | <5ms | Brute-force, 10K items |
| Structural similarity (top 20 candidates) | <2ms | Jaccard on sets |
| RRF fusion + ranking | <0.1ms | Arithmetic |
| **Total** | **~12ms** | **Imperceptible** |

For `bn search` (interactive):
| Step | Time |
|------|------|
| Phase 1 (FTS5 only) | <1ms |
| Phase 2 (semantic + structural + fusion) | ~10ms |
| **Total** | **~11ms** |

For `bn dedup` (all-pairs scan, 1000 items):
| Step | Time |
|------|------|
| Compute all-pairs cosine similarity matrix | ~200ms |
| Filter pairs above threshold | <10ms |
| Structural similarity for filtered pairs | <50ms |
| **Total** | **~260ms** |

For `bn rebuild` (full re-embed, 1000 items):
| Step | Time |
|------|------|
| Replay events → SQLite | ~100ms |
| Embed all items | ~5 seconds |
| Build FTS5 index | ~50ms |
| **Total** | **~5.2 seconds** |

The rebuild is the only slow operation, and it's rare (model upgrade, corruption recovery). Everything else is sub-20ms.

---

## Configuration

```yaml
# .bones/config.yaml
search:
  semantic: true                    # Enable semantic search (requires model)
  model: "minilm-l6-v2-int8"       # Embedding model
  duplicate_threshold: 0.85         # Similarity above this = likely duplicate
  related_threshold: 0.65           # Similarity above this = possibly related
  structural_weight: 0.20           # Weight of structural similarity in fusion
  semantic_weight: 0.50             # Weight of semantic similarity in fusion
  lexical_weight: 0.30              # Weight of lexical similarity in fusion
  warn_on_create: true              # Show duplicate warnings during creation
  block_on_create: false            # Block creation if duplicate found (strict mode)
```

---

## Implementation Priority

1. **Phase 1**: SQLite FTS5 with BM25 + weighted columns. This alone is a big improvement over substring search. Implement `bn search` and `bn create --check-duplicates` with lexical matching only.

2. **Phase 2**: Add MiniLM-L6-v2 ONNX embedding + sqlite-vec. Implement semantic search layer and RRF fusion. This is where the duplicate problem actually gets solved.

3. **Phase 3**: Add structural similarity layer. Implement `bn dedup` for bulk duplicate detection. Implement `bn similar <item-id>`.

4. **Phase 4**: Thompson Sampling feedback loop (from the advanced research addendum). Agents that consistently skip certain similar-item suggestions provide negative signal; agents that accept suggestions provide positive signal. The similarity thresholds and layer weights adapt per-agent over time.
