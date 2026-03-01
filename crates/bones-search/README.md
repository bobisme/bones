# bones-search

Hybrid search engine for the [bones](https://github.com/bobisme/bones) issue tracker, combining lexical BM25, semantic embeddings, and structural graph proximity via reciprocal rank fusion (RRF).

## What this crate provides

- **Hybrid search**: fuses three independent ranking signals with RRF
  - Lexical (FTS5/BM25): stemming, prefix search, boolean operators
  - Semantic (optional): ONNX embedding model via `ort`, KNN over stored vectors
  - Structural: graph proximity to lexical seed items via dependency edges
- **Semantic model**: loads an ONNX sentence-transformer model; gracefully degrades to lexical+structural when unavailable
- **Structural similarity**: shared label/parent/dependency scoring between items
- **Duplicate detection**: multi-signal fusion used by `bn create` to surface near-duplicates

## Features

- `semantic-ort` — enable semantic search via ONNX Runtime (requires `ort` and `tokenizers`)
- `bundled-model` — embed a default model at build time (implies `semantic-ort`)

## Usage

This crate is an internal dependency of [`bones-cli`](https://crates.io/crates/bones-cli). See the [bones repository](https://github.com/bobisme/bones) for the full project.
