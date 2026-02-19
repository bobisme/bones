# ADR-001: Item ID Generation Scheme

## Status
Accepted

## Context

Item IDs appear in every command, event, and merge. The ID scheme must satisfy these requirements:

1. **Collision-safe offline**: Agents generate IDs independently without central coordination
2. **Stable across repos**: Same seed produces deterministic IDs, enabling reproducibility
3. **Ergonomic for humans and agents**: Short enough for CLI copy/paste, readable in logs and diffs
4. **Deterministic identity**: ID encodes content deterministically; same issue = same ID
5. **Scalable**: Adaptive length supports projects from 10 to 100k+ items without explosion
6. **Namespace clarity**: Prefix identifies artifact type and project scope
7. **Child ID support**: Goals and compacted items have child IDs with hierarchical numbering

Without a principled ID scheme, Bones commands become fragile (ambiguous references), test fixtures become brittle (IDs change), and agent scripts become unconfident (fear of collisions).

## Decision

Use **terseid** ([github.com/bobisme/terseid](https://github.com/bobisme/terseid)) for all item ID generation.

### Format

```
bn-<base36hash>[.<child>.<path>]
```

- **Prefix**: `bn-` (identifier for Bones in this project)
- **Body**: adaptive-length base36 hash (lowercase alphanumeric `[a-z0-9]+`)
- **Child notation**: dot-separated numeric suffixes for hierarchical IDs
  - Parent: `bn-3m6` 
  - First child: `bn-3m6.1`
  - Nested: `bn-3m6.1.3`

Examples:
- `bn-3m6` (task)
- `bn-a7x.1` (goal's first child)
- `bn-a3f8.1.3` (nested item)

### Seed Function

**Goal**: Same content usually produces the same ID prefix, but collisions are rare and handled gracefully.

**Inputs**:
- `title` (string)
- `description` (string)  
- `nonce` (integer, starts at 0)

**Algorithm**:
1. Concatenate: `seed = title + "|" + description + "|" + nonce` (as UTF-8 bytes)
2. SHA256 hash the seed
3. Take first 8 bytes of hash
4. Encode as base36
5. Truncate to adaptive length (see Collision Avoidance, tier 1)
6. Verify uniqueness (see Collision Avoidance)

**Determinism guarantee**: Same title + description + nonce → same ID on all replicas.

**Fully random fallback**: Callers may omit description and nonce for fully random generation (useful for batch operations where determinism isn't needed).

### Collision Avoidance (4-Tier Escalation)

If a generated ID already exists in the repository:

#### Tier 1: Nonce Escalation
- Increment nonce: 0 → 1 → 2 → ...
- Rehash with new nonce
- Typical: collides ~once per 10k items

#### Tier 2: Length Extension
- Keep nonce, use more base36 digits
- Example: `a7x` (3 chars) → `a7x4` (4 chars)
- Dramatically increases candidate space

#### Tier 3: Long Fallback
- Use full 32 base36 digits (SHA256 full encoding)
- Example: `a7x4gq8k...` (32 chars)
- Collision probability becomes mathematically negligible

#### Tier 4: Desperate Fallback
- Append random suffix if all above fail (shouldn't happen in practice)
- Example: `a7x4gq8k-xyz123`
- Recovery flag set for monitoring

**Caller responsibility**: Provide `exists_closure` to check uniqueness. terseid never touches storage directly.

```rust
// Example: caller provides existence check
let exists = |candidate: &str| -> bool {
    db.id_exists(candidate)
};

let id = generator.generate(seed_fn, item_count, exists);
```

### CLI Resolution

**Partial matching**: Users and agents can reference items by prefix.

- User types: `bn show a7x`
- Resolver finds: `bn-a7x` (if unique)
- Returns: full ID `bn-a7x`

**Ambiguous resolution** (two items: `bn-a7x1` and `bn-a7x2`):
- User types: `bn show a7x`
- Resolver returns: error with listing
  ```
  Ambiguous: 'a7x' matches multiple items:
    bn-a7x1   "Fix auth timeout"
    bn-a7x2   "Auth token refresh"
  Use a longer prefix or full ID.
  ```

**IdResolver implementation**: Available in terseid, integrated into CLI argument parsing.

### Child ID Generation

For goal children and compacted items:

```rust
fn child_id(parent: &str, index: usize) -> String {
    format!("{}.{}", parent, index)
}

// Examples:
child_id("bn-a3f8", 1)  // → "bn-a3f8.1"
child_id("bn-a3f8", 2)  // → "bn-a3f8.2"
child_id("bn-a3f8.1", 1) // → "bn-a3f8.1.1"
```

Child IDs are **not** stored as separate items in the ID table. They are computed on-demand and always valid. No collision check needed.

### Grammar (Normative)

**ABNF**:
```
item-id       = "bn-" id-body [ child-suffix ]
id-body       = 1*id-char
child-suffix  = 1*( "." 1*DIGIT )
id-char       = DIGIT / %x61-7a  ; [0-9a-z]
```

**Regex** (for validators and error messages):
```regex
^bn-[a-z0-9]+(\.[0-9]+)*$
```

**Validation**:
- All lowercase
- No hyphens within body (only at prefix)
- Dots separate numeric child indices only
- Pattern must match regex above
- Total length: 3-255 characters
- Child indices: 1+ (zero-indexed not used)

### Examples

Valid IDs:
- `bn-3m6`
- `bn-a7x1b`
- `bn-xyz.1`
- `bn-xyz.1.3`
- `bn-a3f8.1.3.5`

Invalid IDs:
- `bn` (missing body)
- `BN-a3f8` (uppercase)
- `bn-a3f8-secondary` (hyphen in body)
- `bn-a3f8.` (trailing dot)
- `bn-a3f8.0` (zero-indexed child)
- `bn-a3f8..1` (double dot)

## Alternatives Considered

### Alternative 1: UUIDv7

**Format**: `bn-f47ac10b58cc4372a5670e4b66b41591` (36 chars)

**Pros**:
- Standardized (RFC 9562)
- Time-sortable variant (UUIDv7)
- Zero collision risk
- Works offline

**Cons**:
- Too long for CLI ergonomics (36 chars makes commands unwieldy)
- Not human-memorable
- Git diffs show random-looking strings instead of content-derived IDs
- No determinism from seed; every generation is random
- Partial matching not applicable

**Example command** (vs. terseid):
```bash
# UUIDv7: unwieldy
bn show f47ac10b-58cc-4372-a567-0e4b66b41591

# terseid: ergonomic
bn show a7x
```

**Rejected because**: UUID ergonomics are incompatible with command-line scripting and agent tooling. Terseid's 3-4 char IDs reduce copy-paste errors and cognitive load.

### Alternative 2: ULID

**Format**: `01ARZ3NDEKTSV4RRFFQ` (26 chars, Crockford base32)

**Pros**:
- Sortable (timestamp prefix)
- Shorter than UUID
- Crockford base32 avoids ambiguous chars

**Cons**:
- Still 26 characters (too long for frequent CLI use)
- Not derived from content (not deterministic)
- Limited partial matching applicability (timestamp prefix means ambiguity even with unique content)
- No namespace or project scope

**Example**:
```bash
# ULID: still unwieldy
bn show 01ARZ3NDEKTSV4RRF

# terseid: better
bn show a7x
```

**Rejected because**: Still prioritizes sortability over ergonomics. Terseid achieves 3-4 char IDs at the cost of needing to handle collisions explicitly, which is a better trade for human/agent CLI workflows.

### Alternative 3: Short Content Hash (Hand-Rolled)

**Format**: `bn-3m6` (adaptive-length, no collision handling)

**Pros**:
- Same short format as terseid
- Deterministic from content
- Simple to implement

**Cons**:
- **No collision avoidance strategy**: a hand-rolled hash to ID7 without nonce escalation cannot guarantee uniqueness
- No library support for resolution
- Child ID strategy unclear
- Validators must hardcode logic

**Rejected because**: Terseid is a battle-tested library that solves collision avoidance, CLI resolution, and child ID semantics. Reinventing it risks silent collisions and unresolved bugs. Dependency cost is negligible vs. correctness gain.

### Alternative 4: Prefixed Deterministic IDs (Sequential + Hash)

**Format**: `bn-001-a7x` (sequence number + hash)

**Pros**:
- Deterministic for same project
- Humans can see ordinal position

**Cons**:
- Requires global sequencer (breaks offline-first guarantee)
- Merge conflict zone in distributed teams
- Longer than terseid (9+ chars vs. 3-4)
- Hash portion still needed for collision avoidance

**Rejected because**: Violates offline-first principle. Bones is designed for agent swarms with intermittent connectivity; a global sequencer introduces a Single Point of Failure and coordination overhead that terseid eliminates.

## Collision Probability & Safety

### Birthday Problem Analysis

Terseid uses the **birthday problem** to compute safe truncation lengths:

- **3 chars (base36)**: safe for ~100 items (birthday collision ≈ 1 in 46k)
- **4 chars**: safe for ~1000 items  
- **5 chars**: safe for ~7000 items
- **6 chars**: safe for ~46k items
- **7 chars**: safe for ~287k items

Adaptive truncation uses collection size as input to `generator.generate(item_count)`, automatically selecting safe length per dataset.

### Nonce Escalation Math

If a base36 3-char space has ~7776 possible values (36³), and 1% are taken, nonce collision is rare. Most teams stay below 1000 items, where 3 chars suffice. Nonce escalation handles the rare 1% case without bloat.

### Empirical Data

No known terseid deployments have hit tier 3 (long fallback) with normal usage. Tier 1 (nonce escalation) is the expected path on collision.

## Consequences

1. **All code must use terseid** for ID generation. No ad-hoc ID schemes.
   - `bn create` → terseid.generate()
   - `bn snapshot` → terseid.child_id()
   - Imports: `use terseid::{IdGenerator, IdResolver, ...};`

2. **Parser must validate ID grammar**:
   - Reject invalid formats early with clear error messages
   - Use regex `^bn-[a-z0-9]+(\.[0-9]+)*$` in validators

3. **IdResolver must be available in CLI commands**:
   - `bn show a7x` resolves via IdResolver
   - Multi-match errors show candidates clearly
   - Library: `terseid::IdResolver`

4. **Collision is structurally very unlikely** but handled by 4-tier escalation:
   - Tier 1 (nonce): increments and rehashes  
   - Tier 2 (length): expands base36 representation
   - Tier 3 (long fallback): uses full hash
   - Tier 4 (desperate): random suffix (shouldn't happen)

5. **Child IDs are computed, not stored**:
   - No ID table entry needed
   - Always valid if parent is valid
   - Enables free hierarchical namespacing

6. **Determinism guarantee enables reproducible scripting**:
   - Agent scripts can `bn create "Title" "Description"` and get same ID every time
   - Test fixtures remain stable
   - Git diffs show content-derived IDs

7. **Dependency**: Add terseid to `Cargo.toml`:
   ```toml
   terseid = { git = "https://github.com/bobisme/terseid.git" }
   ```

## Links & Dependencies

- **Blocks**: 
  - bn-x2e (Implement TSJSON event format parser and writer — needs ID grammar validation)
  - bn-3m6 (SQLite schema design — needs ID type/unique constraints)

- **Related**:
  - bn-3rr.1 (Write architecture decision records — this ADR is part of that effort)
  - bn-yot.4 (CLI invalid-ID error workflows — uses ADR grammar for validation)
  - bn-1js, bn-m80 (Diagnostics/log artifacts — reference this ADR for ID format)

## Testing Strategy

### Unit Tests

1. **terseid integration**:
   - Deterministic generation: `(title, desc, nonce) → same ID`
   - Nonce escalation on collision
   - Adaptive length for dataset size

2. **ID validation** (bn-x2e):
   - Valid patterns: `bn-3m6`, `bn-a7x.1`, `bn-xyz.1.3`
   - Invalid patterns: `BN-a7x`, `bn-a7x-x`, `bn-a7x.0`
   - Regex matches all valid, rejects all invalid

3. **CLI resolution** (bn-x2e):
   - Unique prefix resolves: `a7x` → `bn-a7x`
   - Ambiguous prefix errors clearly: `a7` matches `bn-a7x`, `bn-a7x1`
   - Full ID always works: `bn-a7x` resolves

### E2E Tests (bn-yot.4)

- Create item, reference by partial ID in subsequent commands
- Invalid ID arguments error with clear messages
- Child IDs work correctly in goal hierarchies

### Property Tests (Phase 0)

- **Determinism**: Same seed (title + desc + nonce) always produces same ID
- **Uniqueness**: No two items with same ID in repo
- **Collision handling**: Nonce escalation produces valid, distinct IDs under synthetic collision load

## Appendix: Seed Function Examples

```python
# Python pseudocode for seed construction

def make_seed(title: str, description: str, nonce: int) -> bytes:
    """Combine title, description, nonce into seed bytes."""
    parts = [title, description, str(nonce)]
    return "|".join(parts).encode("utf-8")

# Example:
seed = make_seed("Fix auth timeout", "Token refresh race in payment service", 0)
# → b"Fix auth timeout|Token refresh race in payment service|0"

# SHA256 hash this seed, take first 8 bytes, encode as base36, truncate.
```

```rust
// Rust implementation sketch

use terseid::{IdGenerator, IdConfig};
use sha2::{Sha256, Digest};

fn generate_item_id(
    title: &str,
    description: &str,
    db: &Database, // provides id_exists() check
) -> Result<String, IdError> {
    let config = IdConfig::new("bn");
    let generator = IdGenerator::new(config);
    
    let item_count = db.count_items();
    let seed_fn = |nonce: usize| {
        format!("{}|{}|{}", title, description, nonce).into_bytes()
    };
    
    generator.generate(
        seed_fn,
        item_count,
        |candidate| db.id_exists(candidate),
    )
}
```

---

**ADR-001 Approved**: Use terseid for item ID generation. Short, deterministic, collision-safe offline, ergonomic for CLI/agents.
