# Pretty Output Audit Notes

## Goal

Normalize human-readable (`--json` off) output so command responses look coherent across the CLI.

## Current inconsistencies observed

- Mixed success styles (`✓ ...`, plain sentences, table rows, boxed blocks) with no shared pattern.
- Mixed heading styles (some commands use banners/boxes, some use none).
- Error and suggestion formatting is mostly consistent via shared render helpers, but success formatting is often command-local.
- Some command groups now have clear help output, but command result bodies still vary widely.

## Proposed conventions

### 1) Success envelope

- Use a one-line lead sentence for action result.
- Follow with compact key/value bullets for details.
- Keep symbols optional and consistent when used.

### 2) Empty-state behavior

- Use explicit "No results" phrasing with one short next-step hint.

### 3) Lists and tables

- Prefer one table style for tabular output.
- Keep column ordering stable across commands that represent items.

### 4) Sectioned reports

- Use consistent section separators and naming in report-style commands.
- Keep max vertical density bounded (avoid noisy wraps for common cases).

### 5) Shared helpers

- Move repeated pretty-render patterns into reusable output helpers.
- Keep command modules focused on data selection, not terminal formatting details.

## Rollout approach

- Phase 1: highest-traffic commands (`list`, `show`, `search`, `next`, `triage`, `status`).
- Phase 2: lifecycle and metadata commands under `bn bone`.
- Phase 3: maintenance/admin/data/dev commands.

## Validation

- Add/refresh snapshot-style tests for human output where appropriate.
- Keep JSON output schema unchanged while improving pretty output only.

## Progress update

- Completed first normalization pass for high-traffic report commands:
  - `bn search`
  - `bn dup`
  - `bn similar`
  - `bn next`
  - `bn status`
- Completed second normalization pass for detail/lifecycle presentation:
  - `bn show`
  - `bn triage`
  - `bn create`
  - `bn move`
  - `bn archive`
  - batch result bodies for `bn do`, `bn done`, `bn reopen`, `bn update`, `bn delete`
- Completed additional normalization pass for lower-frequency operational commands:
  - `bn dev sim run`
  - `bn dev sim replay`
  - `bn sync` (`--config-only` and report body)
  - `bn redact-verify` (summary and single-item success)
- Improvements applied:
  - consistent section headings and separators,
  - explicit tabular column headers for ranked/list outputs,
  - clearer empty-state guidance text,
  - clearer pretty/text separation for automation-oriented commands,
  - preserved JSON output contracts.
- Verification run: `cargo test -p bones-cli --bin bn` passed after changes.
