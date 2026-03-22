#!/usr/bin/env bash
# TUI soak test: spawn bn ui in vessel, simulate user actions, monitor RSS.
#
# Usage: ./tui_soak.sh [project_dir] [duration_secs]
#   project_dir: path to a bones project (default: ~/src/ward/ws/default)
#   duration_secs: how long to run (default: 300 = 5 minutes)
#
# Requires: vessel, bn, jq

set -euo pipefail

PROJECT_DIR="${1:-$HOME/src/ward/ws/default}"
DURATION="${2:-300}"
AGENT_ID="bn-ui-soak-$$"
INTERVAL=5  # seconds between actions

echo "=== bn ui Soak Test ==="
echo "Project: $PROJECT_DIR"
echo "Duration: ${DURATION}s"
echo "Agent: $AGENT_ID"
echo ""

# Kill any leftover soak agents
vessel kill -l soak-test 2>/dev/null || true

# Spawn bn ui
vessel spawn -n "$AGENT_ID" -l soak-test --rows 40 --cols 120 \
  --cwd "$PROJECT_DIR" -- bn ui
sleep 2

get_rss_mb() {
  vessel list --format json | jq -r \
    ".agents[] | select(.id == \"$AGENT_ID\") | .rss_bytes" \
    | awk '{printf "%.1f", $1 / 1048576}'
}

get_rss_bytes() {
  vessel list --format json | jq -r \
    ".agents[] | select(.id == \"$AGENT_ID\") | .rss_bytes"
}

RSS_START=$(get_rss_bytes)
echo "RSS at start: $(echo "$RSS_START" | awk '{printf "%.1f MB", $1/1048576}')"
echo ""
echo "time_s  rss_mb  action"
echo "------  ------  ------"

START_TIME=$(date +%s)
CYCLE=0

while true; do
  ELAPSED=$(( $(date +%s) - START_TIME ))
  if [ "$ELAPSED" -ge "$DURATION" ]; then
    break
  fi

  # Cycle through different actions to simulate real usage
  ACTION_IDX=$(( CYCLE % 12 ))
  case $ACTION_IDX in
    0)  # Navigate down 5 items
        ACTION="nav-down-5"
        for _ in $(seq 5); do vessel send-keys "$AGENT_ID" j; sleep 0.1; done
        ;;
    1)  # Open detail pane
        ACTION="open-detail"
        vessel send-keys "$AGENT_ID" l
        ;;
    2)  # Scroll detail down
        ACTION="scroll-detail"
        for _ in $(seq 10); do vessel send-keys "$AGENT_ID" ctrl-d; sleep 0.1; done
        ;;
    3)  # Navigate down more
        ACTION="nav-down-10"
        for _ in $(seq 10); do vessel send-keys "$AGENT_ID" j; sleep 0.1; done
        ;;
    4)  # Scroll detail up
        ACTION="scroll-detail-up"
        for _ in $(seq 5); do vessel send-keys "$AGENT_ID" ctrl-u; sleep 0.1; done
        ;;
    5)  # Close detail pane
        ACTION="close-detail"
        vessel send-keys "$AGENT_ID" h
        ;;
    6)  # Navigate up
        ACTION="nav-up-10"
        for _ in $(seq 10); do vessel send-keys "$AGENT_ID" k; sleep 0.1; done
        ;;
    7)  # Open detail again
        ACTION="open-detail-2"
        vessel send-keys "$AGENT_ID" l
        ;;
    8)  # Toggle done items
        ACTION="toggle-done"
        vessel send-keys "$AGENT_ID" D
        ;;
    9)  # Navigate around
        ACTION="nav-mixed"
        for _ in $(seq 5); do vessel send-keys "$AGENT_ID" j; sleep 0.1; done
        for _ in $(seq 3); do vessel send-keys "$AGENT_ID" k; sleep 0.1; done
        ;;
    10) # Go to top
        ACTION="go-top"
        vessel send-keys "$AGENT_ID" g g
        ;;
    11) # Go to bottom and back
        ACTION="go-bottom"
        vessel send-keys "$AGENT_ID" G
        sleep 0.5
        vessel send-keys "$AGENT_ID" g g
        ;;
  esac

  sleep "$INTERVAL"

  RSS_NOW=$(get_rss_bytes)
  RSS_MB=$(echo "$RSS_NOW" | awk '{printf "%.1f", $1/1048576}')
  RSS_DELTA=$(echo "$RSS_NOW $RSS_START" | awk '{printf "%.1f", ($1-$2)/1048576}')
  printf "%5ds  %6s  %s (+%s MB)\n" "$ELAPSED" "$RSS_MB" "$ACTION" "$RSS_DELTA"

  CYCLE=$(( CYCLE + 1 ))
done

echo ""
RSS_END=$(get_rss_bytes)
echo "=== Results ==="
echo "Duration: ${DURATION}s"
echo "Cycles: $CYCLE"
echo "RSS start: $(echo "$RSS_START" | awk '{printf "%.1f MB", $1/1048576}')"
echo "RSS end:   $(echo "$RSS_END" | awk '{printf "%.1f MB", $1/1048576}')"
echo "RSS delta: $(echo "$RSS_END $RSS_START" | awk '{printf "%.1f MB", ($1-$2)/1048576}')"
echo "RSS growth rate: $(echo "$RSS_END $RSS_START $DURATION" | awk '{printf "%.2f MB/min", ($1-$2)/1048576/($3/60)}')"

# Cleanup
vessel kill "$AGENT_ID" 2>/dev/null || true
