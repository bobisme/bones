#!/usr/bin/env bash
set -euo pipefail

usage() {
	cat <<'EOF'
Generate a high-density demo Bones project.

Usage:
  scripts/generate-demo.sh [path]

Arguments:
  path    Optional destination directory. If omitted, a new /tmp path is used.

Environment:
  BN_BIN      Binary to run (default: bn)
  DEMO_AGENT  Agent identity used for mutating commands (default: demo-agent)

This generator creates a large project with:
  - ~160 items across goals, tasks, and bugs
  - rich descriptions with acceptance criteria
  - long operational comments
  - complex dependency graph (blocks + relates)
  - mixed lifecycle states (open/doing/done/archived)
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

if ((BASH_VERSINFO[0] < 4)); then
	printf 'error: bash 4+ is required for associative arrays\n' >&2
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

program_title() {
	case "$1" in
	onboarding) printf 'Onboarding v3 rollout' ;;
	reliability) printf 'Reliability and observability hardening' ;;
	search) printf 'Search relevance and dedup quality' ;;
	tui) printf 'TUI workflow and operator ergonomics' ;;
	*) printf '%s' "$1" ;;
	esac
}

phase_title() {
	case "$1" in
	phase-1-discovery) printf 'Phase 1 - discovery and guardrails' ;;
	phase-2-build) printf 'Phase 2 - implementation and migration' ;;
	phase-3-rollout) printf 'Phase 3 - rollout and stabilization' ;;
	*) printf '%s' "$1" ;;
	esac
}

track_title() {
	case "$1" in
	api) printf 'API contracts and service integration' ;;
	ui) printf 'UI surfaces and interaction model' ;;
	analytics) printf 'Analytics and instrumentation' ;;
	docs) printf 'Documentation and enablement' ;;
	sync) printf 'Sync engine and recovery behavior' ;;
	telemetry) printf 'Telemetry, tracing, and alerting' ;;
	performance) printf 'Performance profile and startup latency' ;;
	incidents) printf 'Incident playbooks and mitigation drills' ;;
	lexical) printf 'Lexical retrieval quality and query semantics' ;;
	semantic) printf 'Semantic retrieval and embedding lifecycle' ;;
	dedup) printf 'Duplicate detection precision and recall' ;;
	ranking) printf 'Hybrid ranking fusion and explainability' ;;
	navigation) printf 'Navigation model and screen flow' ;;
	details) printf 'Detail views and inline editing' ;;
	keyboard) printf 'Keyboard flows and command discoverability' ;;
	accessibility) printf 'Accessibility, contrast, and focus behavior' ;;
	*) printf '%s' "$1" ;;
	esac
}

tracks_for_program() {
	case "$1" in
	onboarding) printf '%s\n' api ui analytics docs ;;
	reliability) printf '%s\n' sync telemetry performance incidents ;;
	search) printf '%s\n' lexical semantic dedup ranking ;;
	tui) printf '%s\n' navigation details keyboard accessibility ;;
	*) return 1 ;;
	esac
}

goal_description() {
	local program="$1"
	local title
	title="$(program_title "$program")"
	cat <<EOF
Program objective: $title.

This goal intentionally contains many child items to exercise triage scoring,
dependency planning, search relevance, dedup classification, and TUI navigation
at realistic backlog density.

Acceptance criteria:
- Each phase has defined workstreams across at least four tracks.
- Dependencies reveal a critical path and non-trivial cross-track constraints.
- Search for core terms returns both direct hits and related nearby work.
- At least one-third of work items include rich operational comments.
EOF
}

phase_description() {
	local program="$1"
	local phase="$2"
	local ptitle phasetitle
	ptitle="$(program_title "$program")"
	phasetitle="$(phase_title "$phase")"
	cat <<EOF
$phasetitle for $ptitle.

Scope:
- Break down work into design, implementation, and risk-remediation streams.
- Ensure each stream has measurable acceptance criteria in child tasks.
- Capture enough context for agents to triage and execute autonomously.

Acceptance criteria:
- Child tasks cover all configured tracks for this program and phase.
- Cross-phase blockers are represented in dependency edges.
- At least one bug item documents a realistic production failure mode.
EOF
}

story_description() {
	local program="$1"
	local phase="$2"
	local track="$3"
	local flavor="$4"
	local ptitle phasetitle ttitle
	ptitle="$(program_title "$program")"
	phasetitle="$(phase_title "$phase")"
	ttitle="$(track_title "$track")"
	cat <<EOF
$phasetitle :: $ptitle :: $ttitle :: $flavor.

Context:
We need this item to pressure-test planning, dependency resolution, and search
quality in a dense project. The work should be specific enough for execution,
yet broad enough to create overlap with nearby work for dedup/similar checks.

Acceptance criteria:
- Define explicit scope boundaries, owners, and rollback strategy.
- Document verification steps and observable success signals.
- Capture known risks and at least one mitigation path.
- Leave artifacts that can be inspected from CLI and TUI detail views.
EOF
}

register_item() {
	local id="$1"
	local kind="$2"
	local title="$3"
	ALL_ITEMS+=("$id")
	ITEM_KIND["$id"]="$kind"
	ITEM_TITLE["$id"]="$title"
	ITEM_STATE["$id"]="open"
	if [[ "$kind" != "goal" ]]; then
		WORK_ITEMS+=("$id")
	fi
}

create_story_bundle() {
	local program="$1"
	local phase="$2"
	local track="$3"
	local parent_id="$4"
	local key title spec impl bug

	key="$program|$phase|$track"
	title="$(track_title "$track")"

	spec="$(create_item \
		--title "[$(phase_title "$phase")] $title planning" \
		--kind task \
		--parent "$parent_id" \
		--size m \
		--label "$program" \
		--label "$phase" \
		--label "$track" \
		--description "$(story_description "$program" "$phase" "$track" "planning")")"
	STORY_SPEC["$key"]="$spec"
	register_item "$spec" "task" "[$(phase_title "$phase")] $title planning"

	impl="$(create_item \
		--title "[$(phase_title "$phase")] $title implementation" \
		--kind task \
		--parent "$parent_id" \
		--size l \
		--urgency urgent \
		--label "$program" \
		--label "$phase" \
		--label "$track" \
		--label critical-path \
		--description "$(story_description "$program" "$phase" "$track" "implementation")")"
	STORY_IMPL["$key"]="$impl"
	register_item "$impl" "task" "[$(phase_title "$phase")] $title implementation"

	bug="$(create_item \
		--title "[$(phase_title "$phase")] Regression risk in $title" \
		--kind bug \
		--parent "$parent_id" \
		--size m \
		--urgency urgent \
		--label "$program" \
		--label "$phase" \
		--label "$track" \
		--label regression \
		--description "$(story_description "$program" "$phase" "$track" "bug remediation")")"
	STORY_BUG["$key"]="$bug"
	register_item "$bug" "bug" "[$(phase_title "$phase")] Regression risk in $title"
}

story_spec_id() {
	local program="$1"
	local phase="$2"
	local track="$3"
	local key="$program|$phase|$track"
	printf '%s' "${STORY_SPEC[$key]}"
}

story_impl_id() {
	local program="$1"
	local phase="$2"
	local track="$3"
	local key="$program|$phase|$track"
	printf '%s' "${STORY_IMPL[$key]}"
}

story_bug_id() {
	local program="$1"
	local phase="$2"
	local track="$3"
	local key="$program|$phase|$track"
	printf '%s' "${STORY_BUG[$key]}"
}

phase_goal_id() {
	local program="$1"
	local phase="$2"
	local key="$program|$phase"
	printf '%s' "${PHASE_GOAL[$key]}"
}

declare -a PROGRAMS PHASES ALL_ITEMS WORK_ITEMS ASSIGNEES
declare -A ROOT_GOAL PHASE_GOAL STORY_SPEC STORY_IMPL STORY_BUG ITEM_KIND ITEM_TITLE ITEM_STATE

PROGRAMS=(onboarding reliability search tui)
PHASES=(phase-1-discovery phase-2-build phase-3-rollout)
ASSIGNEES=(alice bob carol dani erin frank gina henry infra-bot ops-bot qa-bot)

pushd "$TARGET_DIR" >/dev/null

if command -v git >/dev/null 2>&1; then
	git init -q
fi
run_bn init --force >/dev/null

# Program and phase goals.
for program in "${PROGRAMS[@]}"; do
	root_title="Program goal: $(program_title "$program")"
	root_id="$(create_item \
		--title "$root_title" \
		--kind goal \
		--label program \
		--label "$program" \
		--description "$(goal_description "$program")")"
	ROOT_GOAL["$program"]="$root_id"
	register_item "$root_id" "goal" "$root_title"

	for phase in "${PHASES[@]}"; do
		phase_title_value="$(phase_title "$phase"): $(program_title "$program")"
		phase_id="$(create_item \
			--title "$phase_title_value" \
			--kind goal \
			--parent "$root_id" \
			--label phase \
			--label "$phase" \
			--label "$program" \
			--description "$(phase_description "$program" "$phase")")"
		PHASE_GOAL["$program|$phase"]="$phase_id"
		register_item "$phase_id" "goal" "$phase_title_value"

		while IFS= read -r track; do
			create_story_bundle "$program" "$phase" "$track" "$phase_id"
		done < <(tracks_for_program "$program")
	done
done

# Dependency graph: within-track, within-phase, and cross-phase chains.
for program in "${PROGRAMS[@]}"; do
	for phase in "${PHASES[@]}"; do
		mapfile -t tracks < <(tracks_for_program "$program")
		prev_impl=""
		for track in "${tracks[@]}"; do
			key="$program|$phase|$track"
			spec_id="${STORY_SPEC[$key]}"
			impl_id="${STORY_IMPL[$key]}"
			bug_id="${STORY_BUG[$key]}"

			run_bn triage dep add "$spec_id" --blocks "$impl_id" >/dev/null
			run_bn triage dep add "$bug_id" --blocks "$impl_id" >/dev/null

			if [[ -n "$prev_impl" ]]; then
				run_bn triage dep add "$prev_impl" --blocks "$spec_id" >/dev/null
			fi
			prev_impl="$impl_id"
		done
	done

	for phase_idx in 1 2; do
		prev_phase="${PHASES[$((phase_idx - 1))]}"
		next_phase="${PHASES[$phase_idx]}"
		while IFS= read -r track; do
			prev_impl="$(story_impl_id "$program" "$prev_phase" "$track")"
			next_spec="$(story_spec_id "$program" "$next_phase" "$track")"
			next_bug="$(story_bug_id "$program" "$next_phase" "$track")"
			run_bn triage dep add "$prev_impl" --blocks "$next_spec" >/dev/null
			run_bn triage dep add "$prev_impl" --relates "$next_bug" >/dev/null
		done < <(tracks_for_program "$program")
	done
done

# Cross-program relationships for realistic coupling.
run_bn triage dep add "$(story_impl_id onboarding phase-2-build api)" --blocks "$(story_impl_id search phase-2-build semantic)" >/dev/null
run_bn triage dep add "$(story_impl_id search phase-2-build semantic)" --blocks "$(story_impl_id tui phase-2-build details)" >/dev/null
run_bn triage dep add "$(story_impl_id reliability phase-2-build telemetry)" --blocks "$(story_impl_id search phase-3-rollout ranking)" >/dev/null
run_bn triage dep add "$(story_impl_id tui phase-1-discovery keyboard)" --blocks "$(story_impl_id tui phase-2-build navigation)" >/dev/null
run_bn triage dep add "$(story_bug_id search phase-3-rollout dedup)" --blocks "$(story_impl_id onboarding phase-3-rollout docs)" >/dev/null
run_bn triage dep add "$(story_bug_id reliability phase-3-rollout incidents)" --blocks "$(story_impl_id onboarding phase-3-rollout api)" >/dev/null
run_bn triage dep add "$(story_impl_id onboarding phase-3-rollout analytics)" --relates "$(story_impl_id search phase-3-rollout ranking)" >/dev/null
run_bn triage dep add "$(story_impl_id tui phase-3-rollout accessibility)" --relates "$(story_impl_id reliability phase-3-rollout telemetry)" >/dev/null

# Assign ownership and tags broadly.
for idx in "${!WORK_ITEMS[@]}"; do
	id="${WORK_ITEMS[$idx]}"
	assignee="${ASSIGNEES[$((idx % ${#ASSIGNEES[@]}))]}"
	run_bn bone assign "$id" "$assignee" >/dev/null
	run_bn bone tag "$id" demo dense-backlog >/dev/null
done

# Rich comments to stress timeline rendering and text search.
for program in "${PROGRAMS[@]}"; do
	for phase in "${PHASES[@]}"; do
		while IFS= read -r track; do
			impl_id="$(story_impl_id "$program" "$phase" "$track")"
			bug_id="$(story_bug_id "$program" "$phase" "$track")"

			run_bn bone comment add "$impl_id" "Execution checkpoint: implementation spec is approved, migration sequencing is documented, and rollback criteria are attached. Pending gate is a 24-hour staging soak with error budget under 0.2% and no P1 alerts." >/dev/null

			if [[ "$phase" != "phase-1-discovery" ]]; then
				run_bn bone comment add "$bug_id" "Incident simulation notes: reproduced failure under realistic load profile, captured trace + heap snapshot, and documented three mitigations. We will promote the least risky fix behind a feature flag before broad rollout." >/dev/null
			fi
		done < <(tracks_for_program "$program")
	done
done

# Mixed lifecycle states.
for idx in "${!WORK_ITEMS[@]}"; do
	id="${WORK_ITEMS[$idx]}"
	if ((idx % 3 == 0)); then
		run_bn do "$id" >/dev/null
		ITEM_STATE["$id"]="doing"
	fi
done

for idx in "${!WORK_ITEMS[@]}"; do
	id="${WORK_ITEMS[$idx]}"
	if ((idx % 5 == 0)); then
		run_bn done "$id" --reason "Completed in demo generation pass with verification notes recorded" >/dev/null
		ITEM_STATE["$id"]="done"
	fi
done

for idx in "${!WORK_ITEMS[@]}"; do
	id="${WORK_ITEMS[$idx]}"
	if ((idx % 11 == 0)); then
		if [[ "${ITEM_STATE[$id]}" != "done" ]]; then
			run_bn done "$id" --reason "Closed after consolidation and superseded by newer rollout work" >/dev/null
			ITEM_STATE["$id"]="done"
		fi
		run_bn bone archive "$id" >/dev/null
		ITEM_STATE["$id"]="archived"
	fi
done

# Mark early phase goals as done to create phase progression signal.
for program in "${PROGRAMS[@]}"; do
	phase_id="$(phase_goal_id "$program" phase-1-discovery)"
	run_bn done "$phase_id" --reason "Discovery artifacts accepted and implementation approved" >/dev/null
	ITEM_STATE["$phase_id"]="done"
done

# Seed deliberate near-duplicate clusters so dedup/similar demos are reliable.
dedup_seed_parent="$(phase_goal_id search phase-2-build)"

dedup_a="$(create_item \
	--title "Auth callback timeout during onboarding token exchange" \
	--kind bug \
	--parent "$dedup_seed_parent" \
	--size m \
	--urgency urgent \
	--label search \
	--label dedup \
	--label auth \
	--description $'Production traces show intermittent timeout while exchanging onboarding callback tokens with the auth service.\n\nAcceptance criteria:\n- Repro script captures at least three timeouts in five minutes.\n- Candidate fix is validated in staging under synthetic load.\n- Incident note includes rollback plan and owner handoff.')"
register_item "$dedup_a" "bug" "Auth callback timeout during onboarding token exchange"

dedup_b="$(create_item \
	--title "Onboarding auth callback token exchange timeout" \
	--kind bug \
	--parent "$dedup_seed_parent" \
	--size m \
	--urgency urgent \
	--label search \
	--label dedup \
	--label auth \
	--description $'Users intermittently hit callback timeout in onboarding when auth token exchange exceeds service SLA.\n\nAcceptance criteria:\n- Logs correlate timeout spikes with callback path latency.\n- Proposed mitigation includes timeout budget and retry cap.\n- Verification checklist is attached to incident review.')"
register_item "$dedup_b" "bug" "Onboarding auth callback token exchange timeout"

dedup_c="$(create_item \
	--title "Investigate onboarding callback timeout in auth service" \
	--kind task \
	--parent "$dedup_seed_parent" \
	--size m \
	--urgency urgent \
	--label search \
	--label dedup \
	--label auth \
	--description $'Investigate repeated callback timeout reports tied to onboarding token exchange path.\n\nAcceptance criteria:\n- Produce comparison of failing and healthy request traces.\n- Document the top three hypotheses with confidence levels.\n- Recommend one immediate mitigation and one long-term fix.')"
register_item "$dedup_c" "task" "Investigate onboarding callback timeout in auth service"

dedup_d="$(create_item \
	--title "Search ranking misses auth callback timeout incidents" \
	--kind task \
	--parent "$dedup_seed_parent" \
	--size s \
	--label search \
	--label dedup \
	--label ranking \
	--description $'Search and dedup ranking should cluster callback timeout incidents instead of scattering near-identical tickets.\n\nAcceptance criteria:\n- Similar incident variants appear in the same top dedup cluster.\n- Ranking explanation surfaces lexical and semantic overlap clearly.\n- Regression test covers the seeded incident variants.')"
register_item "$dedup_d" "task" "Search ranking misses auth callback timeout incidents"

for id in "$dedup_a" "$dedup_b" "$dedup_c" "$dedup_d"; do
	run_bn bone assign "$id" qa-bot >/dev/null
	run_bn bone tag "$id" demo dense-backlog dedup-seed >/dev/null
done

run_bn bone comment add "$dedup_a" "Seeded duplicate family A: mirrors callback timeout wording used by incident responders and should be grouped with close lexical/semantic siblings." >/dev/null
run_bn bone comment add "$dedup_b" "Seeded duplicate family B: intentionally rephrased to validate dedup clustering and similar-item retrieval under natural variation." >/dev/null
run_bn bone comment add "$dedup_d" "Seeded ranking task: expected to reference both duplicate bug variants as close neighbors in dedup and similar workflows." >/dev/null

run_bn triage dep add "$dedup_a" --relates "$dedup_b" >/dev/null
run_bn triage dep add "$dedup_c" --blocks "$dedup_d" >/dev/null
run_bn triage dep add "$dedup_d" --relates "$dedup_a" >/dev/null

run_bn admin rebuild >/dev/null

goal_count=0
task_count=0
bug_count=0
open_count=0
doing_count=0
done_count=0
archived_count=0

for id in "${ALL_ITEMS[@]}"; do
	kind="${ITEM_KIND[$id]}"
	state="${ITEM_STATE[$id]}"
	case "$kind" in
	goal) ((goal_count += 1)) ;;
	task) ((task_count += 1)) ;;
	bug) ((bug_count += 1)) ;;
	esac
	case "$state" in
	open) ((open_count += 1)) ;;
	doing) ((doing_count += 1)) ;;
	done) ((done_count += 1)) ;;
	archived) ((archived_count += 1)) ;;
	esac
done

launch_goal="${ROOT_GOAL[onboarding]}"
search_goal="${ROOT_GOAL[search]}"
sample_impl="$(story_impl_id onboarding phase-2-build api)"
sample_bug="$(story_bug_id search phase-3-rollout dedup)"

popd >/dev/null

cat <<EOF
Demo project created: $TARGET_DIR
Agent used: $DEMO_AGENT

Generated dataset:
  Total items:      ${#ALL_ITEMS[@]}
  Goals:            $goal_count
  Tasks:            $task_count
  Bugs:             $bug_count
  Open:             $open_count
  Doing:            $doing_count
  Done:             $done_count
  Archived:         $archived_count

Try these commands:
  cd "$TARGET_DIR"
  $BN_BIN list --all
  $BN_BIN triage
  $BN_BIN triage graph
  $BN_BIN triage plan "$launch_goal"
  $BN_BIN triage plan "$search_goal"
  $BN_BIN show "$sample_impl"
  $BN_BIN show "$sample_bug"
  $BN_BIN search "semantic ranking rollout"
  $BN_BIN search "keyboard navigation"

Tips:
  - Use '$BN_BIN triage dedup' to inspect high-overlap work clusters.
  - Use '$BN_BIN triage similar <id>' for near-neighbor exploration.
  - Open '$BN_BIN tui' and inspect comments/dependencies in dense phases.
EOF
