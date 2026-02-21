# bones

bones is a CRDT-native issue tracker for distributed human and agent collaboration.

## Installation

### From crates.io

```bash
cargo install bones-cli
```

### From source

```bash
git clone https://github.com/bobisme/bones
cd bones
cargo install --path crates/bones-cli
```

### Prebuilt binaries

Download release archives from:

- <https://github.com/bobisme/bones/releases>

Each release publishes Linux/macOS binaries for x86_64 and arm64 with SHA256 checksum files.

## Shell completions

Generate shell completions with:

```bash
bn completions bash
bn completions zsh
bn completions fish
```

Install completions locally via `just completions` (see `justfile`).

## Development

```bash
cargo test
just install
```

## Semantic acceleration

- `sqlite-vec` is bundled at build time and auto-registered as a SQLite extension.
- When available, `bn` reports vector acceleration in capability/health output.
- If unavailable, semantic search still works via Rust-side KNN over stored embeddings.
- Set `BONES_SQLITE_VEC_AUTO=0` to disable auto-registration for troubleshooting.
