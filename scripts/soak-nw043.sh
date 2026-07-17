#!/usr/bin/env bash
# nw-043 soak: hammer the isolation-anomaly test until failure or N passes.
# Usage: scripts/soak-nw043.sh [iterations=500] [logdir=/tmp/nw043-soak]
set -u
ITER="${1:-500}"
LOG="${2:-/tmp/nw043-soak}"
mkdir -p "$LOG"
cd "$(dirname "$0")/.."

BIN=$(cargo test --test server_test --no-run --message-format=json 2>/dev/null \
      | jq -r 'select(.target.name=="server_test") | .executable // empty' | tail -1)
[ -x "$BIN" ] || { echo "server_test binary not found"; exit 2; }
echo "binary: $BIN, iterations: $ITER, logs: $LOG"

for i in $(seq 1 "$ITER"); do
  # Alternate threading modes: odd = isolated, even = suite-parallel pressure
  if [ $((i % 2)) -eq 1 ]; then
    ARGS=(hybrid_flow_trace_auto_detects_cross_repo_boundary --exact --test-threads=1 --nocapture)
  else
    ARGS=(hybrid_ --nocapture)   # all hybrid tests, default parallelism
  fi
  if ! RUST_LOG=nw043=trace "$BIN" "${ARGS[@]}" >"$LOG/run-$i.log" 2>&1; then
    echo "FAILURE at iteration $i (mode: ${ARGS[0]}) — log: $LOG/run-$i.log"
    exit 1
  fi
  rm -f "$LOG/run-$i.log"
  [ $((i % 25)) -eq 0 ] && echo "pass $i/$ITER"
done
echo "no recurrence in $ITER iterations"
