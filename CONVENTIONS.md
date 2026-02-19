# Bones Coding Conventions

This document defines project-wide coding patterns for all Bones crates and binaries.

These conventions are intended to reduce merge friction when multiple agents work in parallel.

## Scope

Applies to:
- CLI/application code (`crates/bn-cli`, command handlers, orchestration)
- Library code (`crates/*` reusable crates)
- Tests and examples

When this document conflicts with existing code, follow this document for new code and refactors.

---

## 1) Error Handling

### Rules

1. **Application code uses `anyhow::Result<T>`**
   - Use in CLI entrypoints, command handlers, and app orchestration.
   - Always attach context at fallible boundaries.

2. **Library code uses typed errors via `thiserror`**
   - Use explicit error enums/structs in reusable crates (e.g. core/state/search layers).
   - Do not expose `anyhow` in public library APIs.

3. **Every typed error has:**
   - A **user-facing message** (`Display`/`#[error("...")]`)
   - A **machine-readable error code** (e.g. `ErrorCode` enum + accessor)

4. **No bare `unwrap()` on user-reachable paths**
   - Prefer `?` with `.context("...")`.
   - `expect()` is acceptable only when proving internal invariants in non-user paths/tests.

### Example (compiles)

```rust
use anyhow::{Context, Result};

fn load_config(path: &str) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {path}"))
}
```

```rust
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidItemId,
    StorageCorrupt,
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid item id: {id}")]
    InvalidItemId { id: String },
    #[error("storage is corrupt")]
    StorageCorrupt,
}

impl CoreError {
    pub fn code(&self) -> ErrorCode {
        match self {
            CoreError::InvalidItemId { .. } => ErrorCode::InvalidItemId,
            CoreError::StorageCorrupt => ErrorCode::StorageCorrupt,
        }
    }
}
```

---

## 2) Logging

### Rules

1. **Use `tracing` (not `log`)**
2. Prefer **structured fields** over interpolated strings.
3. Use log levels consistently:
   - `error`: user-visible failures or invariant breakage
   - `warn`: recoverable anomalies and degraded behavior
   - `info`: key lifecycle operations
   - `debug`: detailed control-flow information
   - `trace`: per-event/per-line hot-path diagnostics

### Example (compiles)

```rust
fn emit_event(item_id: &str, event_type: &str) {
    tracing::info!(item_id = %item_id, event_type = %event_type, "event appended");
}
```

---

## 3) Module Organization

### Rules

1. **Organize by logical component**, not one file per struct.
2. Keep **public API surfaced from module roots** (`mod.rs` or top-level module file), with implementation in submodules.
3. Place `#[cfg(test)] mod tests` at the **bottom** of each source file.
4. Keep visibility minimal:
   - prefer `pub(crate)` over `pub` for crate-internal APIs.

### Suggested pattern

```text
crates/bones-core/src/
  error.rs
  ids.rs
  state/
    mod.rs          # public API re-exports
    merge.rs
    validate.rs
```

---

## 4) Testing

### Rules

1. **Unit tests**: in-file `#[cfg(test)] mod tests`.
2. **Integration tests**: crate-level `tests/` directory.
3. **Property tests**: use `proptest`.
   - Local/dev baseline: **10,000** iterations
   - CI/stress profile: **1,000,000** iterations
4. **Every public function gets at least one test** (unit or integration).
5. Test naming format:
   - `test_<function>_<scenario>`
   - Example: `test_parse_line_valid_create_event`

### Example (compiles)

```rust
pub fn parse_flag(input: &str) -> bool {
    input == "yes"
}

#[cfg(test)]
mod tests {
    use super::parse_flag;

    #[test]
    fn test_parse_flag_yes_true() {
        assert!(parse_flag("yes"));
    }
}
```

---

## 5) Naming

### Rules

- Types/traits/enums: `PascalCase` (`EventType`, `WorkItem`, `LwwRegister`)
- Functions/methods/modules/variables: `snake_case`
- Constants/statics: `SCREAMING_SNAKE_CASE`
- Acronyms in types follow project-preferred forms:
  - `ItemId` (not `ItemID`)
  - `FTS` (not `Fts`) when used as an acronym token

---

## 6) Serialization

### Rules

1. Use `serde` for serializable data types.
2. JSON field names are `snake_case`.
3. Timestamps are `u64` unix epoch seconds unless a schema explicitly requires microseconds.
4. Hashes are lowercase hex strings.

### Example (compiles)

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct EventRef {
    item_id: String,
    wall_ts_secs: u64,
    event_hash: String,
}
```

---

## 7) Dependencies

### Rules

1. Prefer bundled/vendored dependencies when practical (example: `rusqlite` with bundled SQLCipher/SQLite features as needed).
2. Pin major versions in `Cargo.toml` (avoid unconstrained compatibility ranges for core crates).
3. Any new dependency requires a short justification in the bead/review notes:
   - why existing dependencies are insufficient
   - expected maintenance/security impact
   - impact on binary size/build time (if relevant)

---

## 8) Checklist for New Code

Before marking a task done:
- [ ] App code returns `anyhow::Result` with context.
- [ ] Library errors use `thiserror` + machine-readable codes.
- [ ] No `unwrap()` in user-reachable paths.
- [ ] Logging uses `tracing` structured fields.
- [ ] Module/API visibility is minimal (`pub(crate)` by default).
- [ ] Public behavior is tested.
- [ ] Serde JSON uses snake_case fields.
- [ ] New deps are justified.

---

## 9) Alignment with `notes/plan.md`

This conventions document intentionally matches the plan's baseline direction:
- error handling (`anyhow`, `thiserror`)
- tracing-based structured logging
- strong testing culture including property tests
- consistent type/function naming and serde usage
- dependency hygiene for reliable multi-agent development
