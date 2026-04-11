#!/usr/bin/env bash
# Capture flamegraphs for bones hot-path scenarios.
#
# Uses `samply` (no sudo required) by default. Install with:
#   cargo install --locked samply
#
# Usage:
#   scripts/flamegraph.sh <scenario> [extra bn args...]
# Scenarios:
#   triage   — bn triage on the current repo
#   search   — bn search "performance"
#   list     — bn list
#   rebuild  — bn rebuild (full projection rebuild — the hottest known path)

set -euo pipefail

scenario="${1:-triage}"
shift || true

if ! command -v samply >/dev/null 2>&1; then
    echo "samply not found. Install with: cargo install --locked samply" >&2
    exit 1
fi

# Build speed-optimized binary with debug info for symbolization.
# As of bn-2qbr, [profile.release] is already opt-level=3 + thin-LTO.
cargo build --release --bin bn
bin="target/release/bn"

outdir="target/flamegraphs"
mkdir -p "$outdir"
stamp="$(date +%Y%m%d-%H%M%S)"
out="$outdir/${stamp}-${scenario}.json.gz"

case "$scenario" in
  triage)
    samply record -o "$out" --save-only -- "$bin" triage "$@"
    ;;
  search)
    samply record -o "$out" --save-only -- "$bin" search "${1:-performance}"
    ;;
  list)
    samply record -o "$out" --save-only -- "$bin" list "$@"
    ;;
  rebuild)
    samply record -o "$out" --save-only -- "$bin" admin rebuild "$@"
    ;;
  *)
    echo "unknown scenario: $scenario" >&2
    exit 2
    ;;
esac

echo "wrote $out"
echo "view with:  samply load $out"
