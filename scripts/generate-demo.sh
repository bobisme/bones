#!/usr/bin/env bash
set -euo pipefail

usage() {
	cat <<'EOF'
Generate a realistic demo Bones project.

Usage:
  scripts/generate-demo.sh [path]

Arguments:
  path    Optional destination directory. If omitted, a new /tmp path is used.

Environment:
  BN_BIN      Binary to run (default: bn)
  DEMO_AGENT  Agent identity used for mutating commands (default: demo-agent)
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
	usage
	exit 0
fi

if [[ $# -gt 1 ]]; then
	usage
	exit 1
fi

BN_BIN="${BN_BIN:-bn}"
if ! command -v "$BN_BIN" >/dev/null 2>&1; then
	printf 'error: bn binary not found: %s\n' "$BN_BIN" >&2
	printf 'hint: install via `just install` or set BN_BIN=/path/to/bn\n' >&2
	exit 1
fi

DEMO_AGENT="${DEMO_AGENT:-demo-agent}"

TARGET_DIR="${1:-}"
if [[ -z "$TARGET_DIR" ]]; then
	TARGET_DIR="$(mktemp -d /tmp/bones-demo.XXXXXX)"
else
	mkdir -p "$TARGET_DIR"
fi
TARGET_DIR="$(cd "$TARGET_DIR" && pwd)"

if [[ -e "$TARGET_DIR/.bones" ]]; then
	printf 'error: %s already contains a .bones project\n' "$TARGET_DIR" >&2
	exit 1
fi

run_bn() {
	"$BN_BIN" --agent "$DEMO_AGENT" "$@"
}

create_item() {
	local payload id
	payload="$(run_bn create --json "$@")"
	id="$(printf '%s\n' "$payload" | awk -F'"' '/^[[:space:]]*"id"[[:space:]]*:/ { print $4; exit }')"
	if [[ -z "$id" ]]; then
		printf 'error: failed to parse item id from create output\n' >&2
		printf '%s\n' "$payload" >&2
		exit 1
	fi
	printf '%s\n' "$id"
}

pushd "$TARGET_DIR" >/dev/null

if command -v git >/dev/null 2>&1; then
	git init -q
fi
run_bn init --force >/dev/null

goal_launch="$(create_item --title "Launch onboarding v2" --kind goal --label roadmap --description "Coordinate API, UI, analytics, and docs for onboarding rollout")"
goal_reliability="$(create_item --title "Improve reliability and observability" --kind goal --label platform --description "Harden sync behavior and improve runtime visibility")"

task_design="$(create_item --title "Design onboarding flow" --parent "$goal_launch" --size m --urgency urgent --label product)"
task_api="$(create_item --title "Implement onboarding API" --parent "$goal_launch" --size l --label backend)"
task_ui="$(create_item --title "Build onboarding UI" --parent "$goal_launch" --size l --label frontend)"
task_analytics="$(create_item --title "Add onboarding analytics" --parent "$goal_launch" --size s --label analytics)"
bug_signup="$(create_item --title "Fix signup token race" --kind bug --parent "$goal_launch" --urgency urgent --label auth)"
task_docs="$(create_item --title "Write onboarding docs" --parent "$goal_launch" --size s --label docs)"
task_integrate="$(create_item --title "Integrate onboarding with auth service" --parent "$goal_launch" --size m --label backend)"

task_logging="$(create_item --title "Add structured logging pipeline" --parent "$goal_reliability" --size m --label observability)"
bug_sync="$(create_item --title "Fix panic in sync retry loop" --kind bug --parent "$goal_reliability" --urgency urgent --label sync)"
task_dashboard="$(create_item --title "Add health-check dashboard" --parent "$goal_reliability" --size m --label observability)"
task_latency="$(create_item --title "Reduce startup latency" --parent "$goal_reliability" --size m --label perf)"
chore_cleanup="$(create_item --title "Clean up deprecated migration notes" --size xs --label docs)"

run_bn triage dep add "$task_design" --blocks "$task_api" >/dev/null
run_bn triage dep add "$task_design" --blocks "$task_ui" >/dev/null
run_bn triage dep add "$task_api" --blocks "$task_analytics" >/dev/null
run_bn triage dep add "$task_api" --blocks "$task_integrate" >/dev/null
run_bn triage dep add "$bug_signup" --blocks "$task_ui" >/dev/null
run_bn triage dep add "$task_logging" --blocks "$task_dashboard" >/dev/null
run_bn triage dep add "$bug_sync" --blocks "$task_latency" >/dev/null
run_bn triage dep add "$bug_sync" --blocks "$task_api" >/dev/null
run_bn triage dep add "$task_integrate" --relates "$task_latency" >/dev/null

run_bn bone assign "$task_api" alice >/dev/null
run_bn bone assign "$task_ui" bob >/dev/null
run_bn bone assign "$bug_sync" infra-bot >/dev/null
run_bn bone assign "$task_dashboard" ops-bot >/dev/null

run_bn bone tag "$task_api" api critical-path >/dev/null
run_bn bone tag "$task_ui" ux critical-path >/dev/null
run_bn bone tag "$bug_sync" flaky hotfix >/dev/null

run_bn bone comment add "$task_api" "Waiting on auth contract review." >/dev/null
run_bn bone comment add "$task_ui" "Skeleton screens are in place; copy pending." >/dev/null
run_bn bone comment add "$bug_sync" "Panic reproduced under retry storm; patch queued." >/dev/null

run_bn do "$task_api" >/dev/null
run_bn do "$task_ui" >/dev/null

run_bn done "$task_design" --reason "Reviewed and accepted by product" >/dev/null
run_bn done "$task_docs" --reason "Initial draft published" >/dev/null
run_bn done "$bug_sync" --reason "Retry loop panic fixed" >/dev/null

run_bn done "$chore_cleanup" --reason "No longer relevant after migration" >/dev/null
run_bn bone archive "$chore_cleanup" >/dev/null

popd >/dev/null

cat <<EOF
Demo project created: $TARGET_DIR
Agent used: $DEMO_AGENT

Try these commands:
  cd "$TARGET_DIR"
  $BN_BIN list
  $BN_BIN status
  $BN_BIN triage
  $BN_BIN triage graph
  $BN_BIN triage plan "$goal_launch"
  $BN_BIN show "$task_api"

Tips:
  - Use '$BN_BIN list --json' to capture IDs quickly.
  - Run '$BN_BIN triage dedup' and '$BN_BIN triage similar <id>' on onboarding items.
EOF
