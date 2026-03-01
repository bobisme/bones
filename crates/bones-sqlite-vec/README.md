# bones-sqlite-vec

SQLite vector extension loader for semantic search in [bones](https://github.com/bobisme/bones).

## What this crate provides

Loads and registers the [`sqlite-vec`](https://github.com/asg017/sqlite-vec) extension into a `rusqlite` connection, enabling KNN vector search over stored embeddings. Used by `bones-search` for semantic ranking.

Gracefully no-ops if the extension is unavailable; bones falls back to lexical+structural search in that case.

Set `BONES_SQLITE_VEC_AUTO=0` to disable auto-registration.

## Usage

This crate is an internal dependency of [`bones-cli`](https://crates.io/crates/bones-cli). See the [bones repository](https://github.com/bobisme/bones) for the full project.
