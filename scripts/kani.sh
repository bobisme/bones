#!/usr/bin/env bash
# Run each Kani harness in its own memory-capped systemd scope, one at a time.
#
# CBMC can exhaust host RAM on a bad harness (one reached 72 GB and triggered
# the host OOM killer). The cap confines that to the harness's own scope.
#
# Usage: scripts/kani.sh [harness-name ...]    (default: all harnesses)
# Env:   KANI_MEM (default 12G), KANI_TIMEOUT seconds (default 600)
set -u
MEM=${KANI_MEM:-12G}
TIMEOUT=${KANI_TIMEOUT:-600}
FILES=(
  crates/bones-core/src/cache/codec.rs
  crates/bones-core/src/clock/text.rs
  crates/bones-core/src/crdt/lww.rs
)

declare -A module_of=()
for f in "${FILES[@]}"; do
  mod=$(sed -e 's|crates/bones-core/src/||' -e 's|\.rs$||' -e 's|/|::|g' <<<"$f")
  for h in $(grep -A3 'kani::proof' "$f" | grep -oP 'fn \K\w+'); do
    module_of[$h]="$mod::kani_proofs::$h"
  done
done

if [ ${#module_of[@]} -eq 0 ]; then
  echo "no Kani harnesses found; run from the repo root" >&2
  exit 1
fi

status=0
for h in ${@:-${!module_of[@]}}; do
  path=${module_of[$h]:-}
  if [ -z "$path" ]; then echo "unknown harness: $h" >&2; status=1; continue; fi
  out=$(systemd-run --user --scope -q -p MemoryMax="$MEM" -p MemorySwapMax=0 \
        timeout "$TIMEOUT" cargo kani -p bones-core --exact --harness "$path" 2>&1)
  rc=$?
  verdict=$(grep -oE 'VERIFICATION:- (SUCCESSFUL|FAILED)' <<<"$out" | tail -1)
  t=$(grep -oE 'Verification Time: [0-9.]+s' <<<"$out" | tail -1)
  case $rc in
    124) note="timed out after ${TIMEOUT}s" ;;
    137|143) note="killed (memory cap $MEM?)" ;;
    *) note="" ;;
  esac
  printf '%-40s %s %s %s\n' "$h" "${verdict:-NO-VERDICT}" "$t" "$note"
  if [ "$rc" != 0 ]; then
    status=1
    grep -E '^Failed Checks' <<<"$out" | head -5
  fi
done
exit $status
