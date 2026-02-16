# Scout

Explore an unfamiliar codebase to build enough understanding for effective planning. This is reconnaissance, not implementation — the output is knowledge, not code.

## When to Scout

- You're new to a codebase and need to create beads
- A spec references parts of the code you haven't seen
- You're unsure where a change should go

Skip scouting if you already know the codebase well or the task is self-contained (e.g., "add a new CLI command" in a codebase you built).

## Steps

1. **Read the docs first.** Check for:
   - `CLAUDE.md` or `AGENTS.md` — project-specific instructions
   - `README.md` — overview, setup, architecture
   - Any `docs/` or `notes/` directories
2. **Map the structure.** Get a sense of layout:
   - `ls -la` at the root
   - Look for `src/`, `lib/`, `packages/`, `cmd/` — where does code live?
   - Check for monorepo structure (`packages/`, `apps/`)
3. **Find entry points.** Where does execution start?
   - CLI: look for `bin/`, `cli/`, `main`, or `index` files
   - Server: look for `server`, `app`, `main` files
   - Library: look for `lib/` or exported modules in `package.json`
4. **Understand patterns.** Grep for conventions:
   - How are tests organized? (`*.test.*`, `__tests__/`, `test/`)
   - How is config handled? (env vars, config files, flags)
   - What's the module style? (ESM, CJS, TypeScript)
5. **Read key files.** Based on the spec, identify 2-3 files most relevant to the change and read them fully.
6. **Note conventions.** Document what you find:
   - Naming conventions (camelCase, snake_case, kebab-case)
   - Error handling patterns
   - Logging/debugging approach
   - Testing patterns

## Tools

| Task | Tool |
|------|------|
| Find files by name | `Glob` with pattern (e.g., `**/*.mjs`, `**/config*`) |
| Search file contents | `Grep` with pattern (e.g., `"function main"`, `"export.*Router"`) |
| Read a specific file | `Read` tool |
| List directory contents | `ls` via Bash |
| Check project config | Read `package.json`, `Cargo.toml`, `go.mod`, etc. |

## Output

After scouting, you should be able to answer:

1. **Where does this change go?** Which files/modules are affected?
2. **What patterns should I follow?** How does similar code look?
3. **What are the dependencies?** What does this code rely on?
4. **How do I test it?** Where do tests live, how are they run?
5. **Are there gotchas?** Special conventions, known issues, dragons?

You don't need to document this formally — the knowledge enables better beads. If the codebase has unusual conventions, consider adding a comment to the parent bead or spec.

## Example

Task: "Add rate limiting to the API"

Scout findings:
- API handlers are in `src/handlers/` with one file per resource
- Middleware lives in `src/middleware/` — there's already auth middleware to reference
- Tests are colocated (`*.test.mjs` next to source)
- Config uses env vars loaded in `src/config.mjs`
- No existing rate limiting — will need new middleware

This enables planning: create config schema bead, create middleware bead, wire middleware bead, add tests bead.
