#!/usr/bin/env bash
# Automates the Phase 4 exit criterion from phase4-event-store.md §8:
# "the 'what changed in the last N minutes' query performs acceptably at
# a realistic event volume (benchmark at, e.g., 100k+ rows)."
#
# Seeds `events` directly via SQL (generate_series — far faster than
# going through the app one emit_event call at a time, and this script
# cares about query performance against volume, not ingest performance)
# and times a query with the same shape as events::recent_events.
#
# Prerequisites: psql on PATH, DATABASE_URL set, migrations already run
# (via `cargo run -- serve` once, or `sqlx migrate run` directly).
#
# Usage: DATABASE_URL=... ./scripts/events-benchmark.sh [row_count]
#   row_count defaults to 150000

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL before running this script}"
ROW_COUNT="${1:-150000}"

if ! command -v psql &>/dev/null; then
  echo "ERROR: psql not found on PATH" >&2
  exit 1
fi

echo "== Phase 4 events benchmark =="
echo "Target row count: $ROW_COUNT"
echo

echo "--- Seeding ---"
SEED_START=$(date +%s%N)

# Spread timestamps over the last 30 days so a "since" filter has a
# realistic distribution to query against, not everything crammed into
# the same instant. event_type cycles through the real catalog values so
# the event_type filter is exercised against genuine variety too.
psql "$DATABASE_URL" -q -c "
INSERT INTO events (agent_id, event_type, payload, occurred_at)
SELECT
    gen_random_uuid(),
    (ARRAY['PermissionAllowed','PermissionDenied','ActionProposed','ActionApproved','ActionExecuted','ActionFailed'])[1 + floor(random() * 6)::int],
    '{}'::jsonb,
    now() - (random() * interval '30 days')
FROM generate_series(1, $ROW_COUNT);
"

SEED_END=$(date +%s%N)
SEED_MS=$(( (SEED_END - SEED_START) / 1000000 ))
echo "Seeded ${ROW_COUNT} rows in ${SEED_MS}ms"
echo

TOTAL_ROWS=$(psql "$DATABASE_URL" -tAc "SELECT count(*) FROM events;")
echo "Total rows in events table (including any from prior runs): $TOTAL_ROWS"
echo

# --- Query 1: recent_events shape, agent_id + since + event_type -------
echo "--- Query: agent_id + since + event_type (most selective, realistic case) ---"
SAMPLE_AGENT_ID=$(psql "$DATABASE_URL" -tAc "SELECT agent_id FROM events ORDER BY random() LIMIT 1;")

Q1_START=$(date +%s%N)
psql "$DATABASE_URL" -q -c "
SELECT * FROM events
WHERE (agent_id = '$SAMPLE_AGENT_ID')
  AND (occurred_at > now() - interval '7 days')
  AND (event_type = 'ActionExecuted')
ORDER BY sequence DESC
LIMIT 100;
" >/dev/null
Q1_END=$(date +%s%N)
Q1_MS=$(( (Q1_END - Q1_START) / 1000000 ))
echo "Query time: ${Q1_MS}ms"
echo

# --- Query 2: recent_events shape, since only (the flagship demo query)
echo "--- Query: since only, no agent_id (the 'what changed recently' demo query) ---"
Q2_START=$(date +%s%N)
psql "$DATABASE_URL" -q -c "
SELECT * FROM events
WHERE (occurred_at > now() - interval '10 minutes')
ORDER BY sequence DESC
LIMIT 100;
" >/dev/null
Q2_END=$(date +%s%N)
Q2_MS=$(( (Q2_END - Q2_START) / 1000000 ))
echo "Query time: ${Q2_MS}ms"
echo

# --- Query 3: action_causal_chain shape ---------------------------------
echo "--- Query: action_causal_chain shape (by action_id) ---"
SAMPLE_ACTION_ID=$(psql "$DATABASE_URL" -tAc "SELECT action_id FROM events WHERE action_id IS NOT NULL LIMIT 1;")
if [[ -n "$SAMPLE_ACTION_ID" ]]; then
  Q3_START=$(date +%s%N)
  psql "$DATABASE_URL" -q -c "SELECT * FROM events WHERE action_id = '$SAMPLE_ACTION_ID' ORDER BY sequence ASC;" >/dev/null
  Q3_END=$(date +%s%N)
  Q3_MS=$(( (Q3_END - Q3_START) / 1000000 ))
  echo "Query time: ${Q3_MS}ms"
else
  echo "No action_id-linked events found in seeded data (expected — the seed script doesn't set one); skipping."
fi
echo

echo "--- Index usage check (EXPLAIN) ---"
psql "$DATABASE_URL" -c "
EXPLAIN SELECT * FROM events
WHERE agent_id = '$SAMPLE_AGENT_ID' AND occurred_at > now() - interval '7 days'
ORDER BY sequence DESC LIMIT 100;
"
echo "Confirm the plan above uses idx_events_agent_seq or idx_events_type_time,"
echo "not a sequential scan — a seq scan at this row count is the actual failure"
echo "mode this benchmark exists to catch."
echo

THRESHOLD_MS=200
if [[ "$Q1_MS" -gt "$THRESHOLD_MS" || "$Q2_MS" -gt "$THRESHOLD_MS" ]]; then
  echo "WARN: a query exceeded the ${THRESHOLD_MS}ms soft threshold at ${TOTAL_ROWS} rows."
  echo "      Check the EXPLAIN output above for a sequential scan before assuming"
  echo "      this is expected — it usually means an index isn't being used."
else
  echo "OK: all queries completed within ${THRESHOLD_MS}ms at ${TOTAL_ROWS} rows."
fi

echo
echo "== Benchmark complete =="
echo "NOTE: this script does not clean up after itself — the seeded rows"
echo "remain in the events table. Re-run against a disposable database, or"
echo "clean up with: psql \"\$DATABASE_URL\" -c 'TRUNCATE events;'"
echo "(DELETE will NOT work for cleanup — migrations/0010's append-only"
echo "trigger blocks it deliberately; TRUNCATE is the one operation that"
echo "still works, since it's a statement-level operation the row-level"
echo "trigger doesn't intercept — see the note in that migration file.)"
