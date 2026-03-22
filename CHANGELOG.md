# Changelog

## v0.23.0 - 2026-03-22

### Windows support

- Made `semantic-ort` (ONNX Runtime) an opt-in feature, resolving CRT linking conflicts on Windows (MD vs MT `RuntimeLibrary` mismatch between `ort` and `esaxx-rs`).
- Added `model2vec` embedding backend (`safetensors` + `tokenizers`, no ONNX) for Windows-friendly semantic search with real embeddings.
- Added hash embedder as a zero-dependency semantic baseline — always available regardless of feature flags.
- Added `windows` convenience feature: `cargo install bones-cli --no-default-features --features windows`.
- Backend priority chain: ORT > model2vec > hash embed, with automatic fallback.

### Fixes

- Fixed hardcoded 384-dimension requirement in `knn_search` that rejected non-ORT embedding backends.
- Content hash now includes backend ID, so switching backends triggers re-embedding instead of silently using stale vectors.
- Embedding dimensions are now dynamic per-backend instead of hardcoded to MiniLM's 384.

## v0.22.11 - 2026-03-11

- Added `bn create --from-file <path>` with YAML, JSON, and TOML support for creating one or many bones from structured input files.
- Stopped surfacing goal bones in executable triage recommendations so `bn triage` and `bn next` focus on actionable work items.
