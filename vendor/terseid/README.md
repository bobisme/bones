# terseid

Adaptive-length, collision-resistant short IDs for Rust.

**For:** Applications that need compact, human-typeable identifiers with
automatic collision avoidance. **Not for:** Cryptographic identifiers, UUIDs, or
globally unique IDs across distributed systems.

## Example

```rust
use terseid::{IdConfig, IdGenerator};

let gen = IdGenerator::new(IdConfig::new("bd"));

let id = gen.generate(
    |nonce| format!("my title|{nonce}").into_bytes(),
    42,  // current item count
    |candidate| false,  // check your storage here
);
// => "bd-a7x"  (3 chars suffices for 42 items)
```

IDs grow automatically as your collection grows:

| Items | Hash length | ID space |
|-------|-------------|----------|
| 0-100 | 3 chars | 46,656 |
| ~200 | 4 chars | 1,679,616 |
| ~7,000 | 5 chars | 60,466,176 |
| ~250,000 | 6 chars | 2.18 billion |

## Status

**Experimental** (v0.1.0). API may change before 1.0.

- Platforms: anywhere Rust compiles (no platform-specific code)
- Dependencies: `sha2`, `thiserror` (2 runtime deps, no std feature gates)
- Security: not for cryptographic use; hashes are truncated SHA256 for
  distribution, not security

## Non-Goals

- Global uniqueness without a collision check function
- Cryptographic strength or unguessability
- Encoding arbitrary data in IDs
- Serde/serialization support (bring your own)

## Mental Model

```text
Seed bytes  →  SHA256  →  first 8 bytes  →  base36  →  truncate to length
                                                          ↑
                                      birthday problem picks this
```

- **ID format:** `<prefix>-<hash>[.<child>.<path>]` (e.g., `bd-a7x3q9`,
  `tk-r2m.1.3`)
- **Adaptive length:** hash length chosen by birthday problem math so collision
  probability stays below a threshold (default 25%)
- **Collision avoidance:** 4-tier fallback — nonce escalation, length extension,
  long fallback, desperate fallback
- **Storage-agnostic:** caller provides an `exists` closure; the generator never
  touches storage directly

## Quick Start

Add to `Cargo.toml`:

```toml
[dependencies]
terseid = "0.1"
```

Generate an ID:

```rust
use terseid::{IdConfig, IdGenerator};

let gen = IdGenerator::new(IdConfig::new("tk"));
let id = gen.generate(
    |nonce| format!("task-seed|{nonce}").into_bytes(),
    0,
    |_| false,
);
assert!(id.starts_with("tk-"));
```

Parse an ID:

```rust
use terseid::parse_id;

let parsed = parse_id("bd-a7x3q9.1.3").unwrap();
assert_eq!(parsed.prefix, "bd");
assert_eq!(parsed.hash, "a7x3q9");
assert_eq!(parsed.child_path, vec![1, 3]);
assert_eq!(parsed.depth(), 2);
assert_eq!(parsed.parent(), Some("bd-a7x3q9.1".to_string()));
```

Resolve partial input (for CLIs):

```rust
use terseid::{IdResolver, ResolverConfig, find_matching_ids};

let resolver = IdResolver::new(ResolverConfig::new("bd"));
let known_ids = vec!["bd-a7x3q9".to_string(), "bd-r2m4k1".to_string()];

let resolved = resolver.resolve(
    "a7x",
    |id| known_ids.iter().any(|k| k == id),
    |substr| find_matching_ids(&known_ids, substr),
).unwrap();
assert_eq!(resolved.id, "bd-a7x3q9");
```

## Usage

### Configuration

```rust
IdConfig::new("bd")                        // defaults: 3-8 chars, 25% threshold
IdConfig::new("tk").min_hash_length(4)     // start at 4 chars
IdConfig::new("ev").max_collision_prob(0.10) // tighter threshold
```

### Standalone hash

When you don't need collision avoidance:

```rust
use terseid::hash;
let h = hash("some input", 6);  // deterministic 6-char base36 string
```

### Child IDs

```rust
use terseid::{child_id, is_child_id, id_depth};

let child = child_id("bd-a7x", 1);       // "bd-a7x.1"
let nested = child_id(&child, 3);         // "bd-a7x.1.3"
assert!(is_child_id("bd-a7x.1"));         // true
assert_eq!(id_depth("bd-a7x.1.3"), 2);    // 2 levels deep
```

### Parsing rules

- Last dash separates prefix from hash (supports `my-proj-a7x3q9`)
- 3-char hashes accept any base36; 4+ chars require at least one digit (avoids
  English word false positives)
- Child path segments are dot-separated u32 integers

### Error handling

All fallible operations return `terseid::Result<T>`:

- `InvalidId` — malformed format
- `PrefixMismatch` — wrong namespace
- `AmbiguousId` — multiple substring matches during resolution
- `NotFound` — no match at any resolution stage

## For AI Agents

- All functions are pure or take closures for side effects — no hidden I/O.
- The test suite (173 unit tests + 3 doc-tests) is the source of truth for
  behavior.
- Safe edit zones: individual modules in `src/` are independent. `lib.rs` is
  just re-exports.
- Required tooling: `cargo test`, `cargo clippy`. No formatters enforced yet.

## References

- [spec.md](spec.md) — full specification with algorithm details, API docs, and
  migration guide
- [beads_rust](https://github.com/Dicklesworthstone/beads_rust) — the project
  this was extracted from
