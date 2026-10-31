#!/usr/bin/env bash
# Automates the Phase 1 exit-criteria checks from phase1-state-mirror.md §6:
#   1. Query latency: agentdb mirror vs. raw osqueryi
#   2. Row-for-row coverage: every process osquery sees appears in the mirror
#   3. Staleness bound: no result older than poll_interval + ingest_time
#
# Prerequisites: the server must already be running (`cargo run -- serve`),
# osqueryi must be on PATH, and `jq` and `curl` must be installed.
#
# Usage: ./scripts/benchmark.sh [base_url] [api_key]
#   base_url defaults to http://localhost:8080
#   api_key  defaults to $API_KEY if set, else requests are sent unauthenticated

set -euo pipefail

BASE_URL="${1:-http://localhost:8080}"
API_KEY="${2:-${API_KEY:-}}"

AUTH_HEADER=()
if [[ -n "$API_KEY" ]]; then
  AUTH_HEADER=(-H "X-API-Key: $API_KEY")
fi

echo "== AgentDB Phase 1 Benchmark =="
echo "Target: $BASE_URL"
echo

for cmd in curl jq osqueryi; do
  if ! command -v "$cmd" &>/dev/null; then
    echo "ERROR: required command '$cmd' not found on PATH" >&2
    exit 1
  fi
done

# --- 1. Latency: mirror vs. raw osqueryi ------------------------------
echo "--- Latency ---"

MIRROR_START=$(date +%s%N)
curl -sf "${AUTH_HEADER[@]}" "$BASE_URL/processes?limit=500" >/dev/null
MIRROR_END=$(date +%s%N)
MIRROR_MS=$(( (MIRROR_END - MIRROR_START) / 1000000 ))

OSQUERY_START=$(date +%s%N)
osqueryi --json "SELECT pid, name FROM processes;" >/dev/null
OSQUERY_END=$(date +%s%N)
OSQUERY_MS=$(( (OSQUERY_END - OSQUERY_START) / 1000000 ))

echo "Mirror (/processes):     ${MIRROR_MS}ms"
echo "Raw osqueryi:             ${OSQUERY_MS}ms"

if [[ "$MIRROR_MS" -gt $(( OSQUERY_MS * 3 + 100 )) ]]; then
  echo "WARN: mirror latency is more than 3x raw osquery + 100ms margin — investigate."
else
  echo "OK: mirror latency is within a reasonable margin of raw osquery."
fi
echo

# --- 2. Coverage: every osquery-visible pid should appear in the mirror
echo "--- Coverage ---"

OSQUERY_PIDS=$(osqueryi --json "SELECT pid FROM processes;" | jq -r '.[].pid' | sort -n)
MIRROR_PIDS=$(curl -sf "${AUTH_HEADER[@]}" "$BASE_URL/processes?limit=500" | jq -r '.[].pid' | sort -n)

OSQUERY_COUNT=$(echo "$OSQUERY_PIDS" | grep -c . || true)
MIRROR_COUNT=$(echo "$MIRROR_PIDS" | grep -c . || true)

MISSING=$(comm -23 <(echo "$OSQUERY_PIDS") <(echo "$MIRROR_PIDS") | wc -l | tr -d ' ')

echo "osquery process count: $OSQUERY_COUNT"
echo "mirror process count (capped at 500): $MIRROR_COUNT"
echo "pids in osquery but missing from mirror: $MISSING"

if [[ "$MISSING" -gt 0 ]]; then
  echo "NOTE: some missing pids are expected if the mirror's 30s freshness"
  echo "window (see api.rs) excludes very recently-exited processes, or if"
  echo "the process count exceeds the /processes limit=500 cap. Missing"
  echo "pids that are NOT recently exited and NOT past the limit indicate"
  echo "a real ingest gap."
else
  echo "OK: full coverage on this sample."
fi
echo

# --- 3. Staleness bound -------------------------------------------------
echo "--- Staleness bound ---"

HEALTH=$(curl -sf "${AUTH_HEADER[@]}" "$BASE_URL/health/mirror")
echo "$HEALTH" | jq -r '.[] | "\(.source): last poll started \(.last_poll_started // "never"), finished \(.last_poll_finished // "never"), rows=\(.last_row_count // "n/a"), error=\(.last_error // "none")"'

NOW_EPOCH=$(date +%s)
STALE_SOURCES=$(echo "$HEALTH" | jq -r --argjson now "$NOW_EPOCH" '
  .[] | select(.last_poll_finished != null) |
  select((($now - (.last_poll_finished | sub("\\..*Z$";"Z") | fromdateiso8601)) > 30)) |
  .source
')

if [[ -n "$STALE_SOURCES" ]]; then
  echo "WARN: the following sources have not completed a poll in the last 30s: $STALE_SOURCES"
else
  echo "OK: all sources within the expected staleness bound (poll_interval + ingest_time <= 30s)."
fi
echo

echo "== Benchmark complete =="
