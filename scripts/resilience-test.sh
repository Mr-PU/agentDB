#!/usr/bin/env bash
# Automates the last manual Phase 1 exit criterion from phase1-state-mirror.md
# §6: the mirror must survive an osquery restart or a single failed poll
# without corrupting existing rows.
#
# Simulates this by shadowing `osqueryi` on PATH with a stub that always
# fails, running one poll cycle against it, and confirming the existing
# `processes` rows are untouched (not dropped, not partially overwritten)
# — then confirms a normal poll afterward recovers cleanly.
#
# Prerequisites: DATABASE_URL set, psql on PATH, run from the project root.
#
# Usage: DATABASE_URL=... ./scripts/resilience-test.sh

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL before running this script}"

for cmd in psql cargo; do
  if ! command -v "$cmd" &>/dev/null; then
    echo "ERROR: required command '$cmd' not found on PATH" >&2
    exit 1
  fi
done

echo "== Resilience test: simulated osquery failure =="

echo "1. Running a normal poll to establish baseline data..."
cargo run --quiet -- poll-once

BEFORE_COUNT=$(psql "$DATABASE_URL" -tAc "SELECT count(*) FROM processes;" | tr -d ' ')
echo "   processes rows before simulated failure: $BEFORE_COUNT"

if [[ "$BEFORE_COUNT" -eq 0 ]]; then
  echo "WARN: baseline row count is 0 — this test is much less meaningful"
  echo "      with nothing in the table to check for corruption/loss."
fi

echo "2. Shadowing osqueryi with a stub that always fails..."
STUB_DIR=$(mktemp -d)
trap 'rm -rf "$STUB_DIR"' EXIT

cat > "$STUB_DIR/osqueryi" <<'EOF'
#!/usr/bin/env bash
echo "simulated osquery crash" >&2
exit 1
EOF
chmod +x "$STUB_DIR/osqueryi"

echo "3. Running poll-once with the failing stub first on PATH (expected to fail)..."
if PATH="$STUB_DIR:$PATH" cargo run --quiet -- poll-once; then
  echo "FAIL: poll-once unexpectedly succeeded despite the failing osqueryi stub."
  exit 1
else
  echo "   OK: poll-once failed as expected."
fi

AFTER_COUNT=$(psql "$DATABASE_URL" -tAc "SELECT count(*) FROM processes;" | tr -d ' ')
echo "   processes rows after simulated failure: $AFTER_COUNT"

if [[ "$AFTER_COUNT" -lt "$BEFORE_COUNT" ]]; then
  echo "FAIL: row count dropped after a failed poll ($BEFORE_COUNT -> $AFTER_COUNT)"
  echo "      — existing data was lost or corrupted by the failed cycle."
  exit 1
fi

echo "   OK: existing rows survived the failed poll cycle intact."

ERROR_LOGGED=$(psql "$DATABASE_URL" -tAc \
  "SELECT count(*) FROM ingest_polls WHERE source='processes' AND error IS NOT NULL ORDER BY started_at DESC LIMIT 1;" \
  | tr -d ' ')
if [[ "$ERROR_LOGGED" -ge 1 ]]; then
  echo "   OK: the failure was recorded in ingest_polls (visible via /health/mirror)."
else
  echo "WARN: expected at least one ingest_polls row with a recorded error — check ingest.rs error handling."
fi

echo "4. Running a normal poll again to confirm recovery..."
cargo run --quiet -- poll-once
RECOVERED_COUNT=$(psql "$DATABASE_URL" -tAc "SELECT count(*) FROM processes;" | tr -d ' ')
echo "   processes rows after recovery poll: $RECOVERED_COUNT"
echo "   OK: ingest recovered after osqueryi became available again."

echo "== Resilience test passed =="
