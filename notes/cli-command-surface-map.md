# `bn` Command Surface Map

## Top-level (daily path)

- `init`
- `create`
- `list`
- `show`
- `search`
- `do`
- `done`
- `update`
- `next`
- `triage`
- `status`
- `sync`
- `bone`
- `admin`
- `data`
- `dev`
- `ui`

## `bn bone` (item-scoped operations)

- `log`, `history`, `blame`
- `agents`, `mine`
- `did`, `skip`
- `archive`, `close`, `delete`, `reopen`, `undo`
- `tag`, `untag`, `labels`, `label`
- `comment`, `comments`
- `assign`, `unassign`, `move`

## `bn triage` (report + analysis)

- `report` (or bare `bn triage`)
- `dup`, `dedup`, `similar`
- `dep`, `graph`
- `progress`, `plan`, `health`, `cycles`, `stats`

## `bn admin` (maintenance)

- `completions`
- `hooks`
- `verify`
- `redact-verify`
- `compact`
- `diagnose`
- `config`
- `migrate-format`
- `rebuild`

## `bn data` (interoperability)

- `import`
- `export`
- `migrate-from-beads`

## `bn dev` (developer tooling)

- `sim`
- `merge-tool`
- `merge-driver`

## UI naming decision

- Public command is `bn ui`.
- There is one UI entrypoint only.
- Internal UI modes are in-app behavior, not CLI subcommands.
