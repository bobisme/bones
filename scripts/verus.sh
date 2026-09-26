#!/usr/bin/env bash
# Verify crates/bones-verified with Verus in a memory-capped systemd scope.
#
# cargo-verus caches results in target/verus-partial and prints nothing on a
# cache hit, so this cleans the crate first and fails unless Verus reports
# its results with zero errors.
#
# Usage: scripts/verus.sh
# Env:   VERUS_DIR (default ~/.local/verus/verus-x86-linux), VERUS_MEM
#        (default 12G), VERUS_TIMEOUT seconds (default 900)
set -uo pipefail
VERUS_DIR=${VERUS_DIR:-$HOME/.local/verus/verus-x86-linux}
MEM=${VERUS_MEM:-12G}
TIMEOUT=${VERUS_TIMEOUT:-900}
root=$(cd "$(dirname "$0")/.." && pwd)

if [ ! -x "$VERUS_DIR/cargo-verus" ]; then
  echo "cargo-verus not found in $VERUS_DIR; see docs/contributor-guide.md" >&2
  exit 1
fi

cd "$root/crates/bones-verified" || exit 1
cargo clean -q -p bones-verified --target-dir "$root/target/verus-partial"
out=$(PATH="$VERUS_DIR:$PATH" systemd-run --user --scope -q -p MemoryMax="$MEM" -p MemorySwapMax=0 \
  timeout "$TIMEOUT" cargo verus focus 2>&1)
status=$?
echo "$out" | grep -vE '^\s+(Compiling|Checking|Finished)'
result=$(grep -E 'verification results::' <<<"$out" | tail -1)
if [ $status -ne 0 ] || [ -z "$result" ] || ! grep -q ' 0 errors' <<<"$result"; then
  echo "verus: FAILED (exit $status)" >&2
  exit 1
fi
